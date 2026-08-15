#!/bin/sh
# SessionStart hook: report binary status and inject usage guidance.
#
# This used to be ensure-binary.sh, and used to install the binary and seed the
# config. Both jobs moved:
#
#   - The binary now lives in the payload at ${CLAUDE_PLUGIN_ROOT}/bin/scout,
#     put there by `make build`, and the MCP server is declared against that
#     path directly. Nothing has to be copied before the server can spawn, so
#     the first-session spawn failure is gone. See SPEC-grok-plugin.md §3.2.
#   - Config seeding moved into the binary (src/config.rs), because a hook is
#     the one place it could not run: not for `make install` users, not for
#     `cargo install` users, and not at all under Grok, which never executes
#     plugin hooks (SPEC-grok-plugin.md §2.5).
#
# What is left is Claude-only by nature: additionalContext reaches no other
# harness. The same guidance ships to Grok as skills/scout/SKILL.md.
#
# Always emits valid JSON on stdout; fail-open — a missing binary is reported
# in the context, never a blocked session.

set -u

PLUGIN_ROOT="${CLAUDE_PLUGIN_ROOT:-$(cd "$(dirname "$0")/.." && pwd)}"
BIN="$PLUGIN_ROOT/bin/scout"

if [ -x "$BIN" ]; then
  VERSION="$("$BIN" --version 2>/dev/null | awk '{print $2}')"
  STATUS="ok (${VERSION:-unknown version})"
else
  STATUS="MISSING at $BIN — run 'make build' in the scout checkout, then restart this session"
fi

GUIDANCE="Prefer scout over raw Bash/Read/Grep for token-heavy work: it runs the job against a local model and only a short summary enters this conversation.

  check_output(command)    run a build/test command, get {ok, summary, first_error, suggested_next_step}. CLI: scout check \"<cmd>\"
  extract(file, question)  answer a specific question about a large file instead of reading it whole. CLI: scout extract <file> \"<question>\"
  grep(pattern, intent)    intent-filtered grep when a raw pattern match would return too many irrelevant hits. CLI: scout grep <pattern> \"<intent>\"
  scout task \"<prompt>\"                ad-hoc escape hatch straight to the local LLM
  scout run --preset <name> --arg k=v  raw preset invocation (used by hooks/scripts, e.g. quality_review, test_review)

Those three are MCP tools; the names above are unqualified. Their full names carry a plugin-derived prefix (mcp__plugin_<plugin>_<server>__check_output). If a scout tool is not already in your loaded toolset, it is also deferred: run ToolSearch for its unqualified name (e.g. \"check_output\") to resolve the full name and load its schema.

Where extract/grep pay: text no code index covers, and match sets too large to skim — logs and run output, generated or vendored trees, long prose and config, plus intent-filtering a big result set. If a structural code tool (LSP, tree-sitter indexer) is available, prefer it for indexed source: outlines, call graphs and \"find every caller\" have exact answers and need no model. See the scout skill for the full boundary.

A PreToolUse hook denies bare build/test Bash commands (cargo build|test|check|clippy, go build|test|vet, npm/npx tsc, python -m pytest, pytest) and redirects to check_output so raw output never floods context. If you already know you need the full raw log (e.g. the classifier could not parse prior output), re-run the SAME command with a \"# raw-output\" marker appended once to bypass the redirect.

A second PreToolUse hook silently auto-allows confidently-safe Bash commands via local classification — it only ever adds an allow, never blocks; on any error it is a no-op and the normal permission prompt applies.

A third PreToolUse hook is purely advisory: on a large Read or a broad uncapped grep it adds a note pointing at extract/grep. It emits no permission decision at all and changes nothing about what runs — take it or ignore it on the merits above."

if command -v jq >/dev/null 2>&1; then
  jq -n --arg status "$STATUS" --arg guidance "$GUIDANCE" \
    '{hookSpecificOutput:{hookEventName:"SessionStart",additionalContext:("scout (local-LLM helper) plugin is active. Binary: " + $status + "\n\n" + $guidance)}}'
else
  # jq missing: degrade to a single-line, special-character-free status report
  # rather than risk emitting malformed JSON from hand-escaping a multi-line,
  # quote-and-backtick-laden guidance block. Fail-open in spirit: SessionStart
  # never blocks, so worst case here is thinner guidance, never a broken
  # session.
  SAFE_STATUS=$(printf '%s' "$STATUS" | tr -d '"\\' | tr '\n' ' ')
  printf '{"hookSpecificOutput":{"hookEventName":"SessionStart","additionalContext":"scout (local-LLM helper) plugin is active. Binary: %s. (jq not found: install jq for full usage guidance.)"}}\n' "$SAFE_STATUS"
fi
