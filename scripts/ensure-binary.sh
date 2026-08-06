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

CONTEXT="scout (local-LLM helper) plugin is active. Binary status: $STATUS. The MCP server 'scout' currently exposes a ping tool for wiring verification; check_output, extract, and grep arrive as the extraction from ct proceeds."

printf '{"hookSpecificOutput":{"hookEventName":"SessionStart","additionalContext":"%s"}}\n' "$CONTEXT"
