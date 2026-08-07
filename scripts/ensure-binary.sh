#!/bin/sh
# SessionStart hook: make sure the scout binary is installed and current in
# ${CLAUDE_PLUGIN_DATA}/bin, then inject usage guidance into the session
# context. Always emits valid JSON on stdout; fail-open — a missing binary
# is reported in the context, never a blocked session.

set -u

PLUGIN_ROOT="${CLAUDE_PLUGIN_ROOT:-$(cd "$(dirname "$0")/.." && pwd)}"
DATA_DIR="${CLAUDE_PLUGIN_DATA:-$HOME/.claude/plugins/data/scout}"
BIN_DIR="$DATA_DIR/bin"
BIN="$BIN_DIR/scout"

plugin_version() {
  sed -n 's/.*"version"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' \
    "$PLUGIN_ROOT/.claude-plugin/plugin.json" | head -1
}

WANT="$(plugin_version)"
HAVE=""
if [ -x "$BIN" ]; then
  HAVE="$("$BIN" --version 2>/dev/null | awk '{print $2}')"
fi

STATUS="ok ($HAVE)"
if [ -z "$HAVE" ] || [ "$HAVE" != "$WANT" ]; then
  mkdir -p "$BIN_DIR"
  if [ -x "$PLUGIN_ROOT/target/release/scout" ]; then
    # Dev checkout: use the locally built binary.
    if cp "$PLUGIN_ROOT/target/release/scout" "$BIN"; then
      STATUS="installed from local build"
    else
      STATUS="error: copy from local build failed"
    fi
  elif command -v cargo >/dev/null 2>&1; then
    # TODO: prefer prebuilt GitHub release binaries once releases exist;
    # cargo install is the fallback (crate is scout-llm, binary is scout).
    if cargo install scout-llm --quiet --root "$DATA_DIR" >/dev/null 2>&1; then
      STATUS="installed via cargo install scout-llm"
    else
      STATUS="missing: no local build and cargo install scout-llm failed"
    fi
  else
    STATUS="missing: run cargo build --release in the plugin repo"
  fi
fi

# Seed the default config on first run. Lives in the XDG config dir — NOT
# CLAUDE_PLUGIN_DATA — because the same file serves the CLI (`scout grep ...`
# in a terminal) and the hooks; the plugin-data dir holds only the binary.
# Never overwrites, and stays out of the way when SCOUT_CONFIG points
# elsewhere. Resolution matches src/config.rs and hooks/shell-safety.sh.
CFG="${XDG_CONFIG_HOME:-$HOME/.config}/scout/config.toml"
CONFIG_NOTE="config: $CFG"
if [ -n "${SCOUT_CONFIG:-}" ]; then
  CONFIG_NOTE="config: \$SCOUT_CONFIG override ($SCOUT_CONFIG)"
elif [ ! -f "$CFG" ]; then
  if mkdir -p "$(dirname "$CFG")" 2>/dev/null &&
     cp "$PLUGIN_ROOT/config.example.toml" "$CFG" 2>/dev/null; then
    CONFIG_NOTE="config: seeded default at $CFG — edit [llm].endpoint and [llm].model to match your local LLM host"
  else
    CONFIG_NOTE="config: missing ($CFG) and could not seed default"
  fi
fi

# Guidance injection (PLAN.md §6): plugins have no CLAUDE.md equivalent, so
# the delegation-table content that used to live there is injected here as
# SessionStart additionalContext instead. Layered with MCP tool descriptions
# (passive discovery) and the PreToolUse hooks (active steering), this is the
# three-layer fix for "MCP server alone sits unused."
GUIDANCE="Prefer scout over raw Bash/Read/Grep for token-heavy work: it runs the job against a local model and only a short summary enters this conversation.

  check_output(command)    run a build/test command, get {ok, summary, first_error, suggested_next_step}. CLI: scout check \"<cmd>\"
  extract(file, question)  answer a specific question about a large file instead of reading it whole. CLI: scout extract <file> \"<question>\"
  grep(pattern, intent)    intent-filtered grep when a raw pattern match would return too many irrelevant hits. CLI: scout grep <pattern> \"<intent>\"
  scout task \"<prompt>\"                ad-hoc escape hatch straight to the local LLM
  scout run --preset <name> --arg k=v  raw preset invocation (used by hooks/scripts, e.g. quality_review, test_review)

Those three are MCP tools; the names above are unqualified. Their full names carry a prefix that depends on how scout was installed — mcp__plugin_<plugin>_<server>__check_output under a plugin install, mcp__<server>__check_output from a .mcp.json entry. If a scout tool is not already in your loaded toolset, it is also deferred: run ToolSearch for its unqualified name (e.g. \"check_output\") to resolve the full name and load its schema.

A PreToolUse hook denies bare build/test Bash commands (cargo build|test|check|clippy, go build|test|vet, npm/npx tsc, python -m pytest, pytest) and redirects to check_output so raw output never floods context. If you already know you need the full raw log (e.g. the classifier could not parse prior output), re-run the SAME command with a \"# raw-output\" marker appended once to bypass the redirect.

A second PreToolUse hook silently auto-allows confidently-safe Bash commands via local classification — it only ever adds an allow, never blocks; on any error it is a no-op and the normal permission prompt applies."

if command -v jq >/dev/null 2>&1; then
  jq -n --arg status "$STATUS" --arg config "$CONFIG_NOTE" --arg guidance "$GUIDANCE" \
    '{hookSpecificOutput:{hookEventName:"SessionStart",additionalContext:("scout (local-LLM helper) plugin is active. Binary status: " + $status + ". " + $config + "\n\n" + $guidance)}}'
else
  # jq missing: degrade to a single-line, special-character-free status
  # report rather than risk emitting malformed JSON from hand-escaping a
  # multi-line, quote-and-backtick-laden guidance block. Fail-open in
  # spirit: SessionStart never blocks, so worst case here is thinner
  # guidance, never a broken session.
  SAFE_STATUS=$(printf '%s' "$STATUS" | tr -d '"\\' | tr '\n' ' ')
  printf '{"hookSpecificOutput":{"hookEventName":"SessionStart","additionalContext":"scout (local-LLM helper) plugin is active. Binary status: %s. (jq not found: install jq for full usage guidance.)"}}\n' "$SAFE_STATUS"
fi
