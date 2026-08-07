#!/usr/bin/env bash
# prefer-local-llm.sh — PreToolUse hook: redirect build/test commands to
# mcp__scout__check_output.
#
# Emits a deny when the Bash command matches a known build/test verb, carrying a
# permissionDecisionReason that names mcp__scout__check_output and its exact
# call pattern. Deny (not advisory allow) because raw build output flooding the
# conversation context is the failure mode to prevent, not merely a suboptimal
# choice. NOTE: the redirect text MUST ride in permissionDecisionReason — Claude
# Code silently drops a field named "reason", leaving only a bare "denied".
#
# Escape hatch: when the full raw log is genuinely needed (e.g. the classifier
# could not parse the output), re-run the SAME command with a "# raw-output"
# marker appended; the hook recognizes the marker and lets the command through.
# Output stays out of context by default, with a deliberate one-step opt-in.
#
# Intercepted (anchored ^\s* to avoid false positives):
#   cargo build|test|check|clippy
#   go build|test|vet
#   npx tsc
#   tsc --
#   npm [run] build|test
#   python -m pytest
#   pytest
#
# Not intercepted: cargo add, cargo fmt, go fmt, go mod, npm install, etc.
#
# Hard invariants:
#   1. Fail-open on the hook's own errors. Any error or parse failure → exit 0
#      → Bash runs normally.
#   2. Fail-open when the redirect target is unreachable. This hook denies —
#      unlike shell-safety.sh, which only ever adds an allow — so it carries
#      extra responsibility: a deny into a dead end (redirect target not
#      callable) bricks the user's Bash tool for every build/test command,
#      standalone plugin or not. Before emitting deny, we verify the scout
#      binary exists AND its local-LLM endpoint responds to a quick ping.
#      Missing binary or unreachable endpoint → log the reason, exit 0, let
#      the raw command run. (Ported fix for a known ct issue: "hard-denies
#      builds with no fallback when the ct MCP server is down" — the
#      reachability-check option from that writeup, adapted from an MCP-
#      server-liveness check to a scout-binary/local-LLM-endpoint check,
#      since scout bundles both in one binary rather than depending on an
#      external always-on daemon.)
#
# Ordering: this hook emits deny; shell-safety.sh emits allow at most.
# Claude Code applies deny > ask > allow, so ordering between the two doesn't
# matter.

set -euo pipefail

# Fail-open on missing HOME (unusual CI environments, test harnesses).
[ -z "${HOME:-}" ] && exit 0
INTERCEPT_LOG="${HOME}/.claude/scout-intercepts.jsonl"

# The scout binary, installed by scripts/ensure-binary.sh at SessionStart.
# Same resolution order that script and .mcp.json use.
SCOUT_BIN="${CLAUDE_PLUGIN_DATA:-$HOME/.claude/plugins/data/scout}/bin/scout"
PING_TIMEOUT_SECS=6

# Wrapper: use timeout/gtimeout if available, otherwise run bare (`scout run
# --ping` has its own internal ~5s HTTP timeout; missing timeout cmd is not
# fatal, just less defensive against a hung process).
_timeout() {
  if command -v timeout >/dev/null 2>&1; then
    timeout "$PING_TIMEOUT_SECS" "$@"
  elif command -v gtimeout >/dev/null 2>&1; then
    gtimeout "$PING_TIMEOUT_SECS" "$@"
  else
    "$@"
  fi
}

# Build/test intercept pattern. Anchored with ^\s* to avoid matching these verbs
# inside echo/printf strings or variable assignments. Verb-level anchoring prevents
# false positives like "cargo add serde" matching "cargo".
BUILD_RE='^\s*(cargo\s+(build|test|check|clippy)\b|go\s+(build|test|vet)\b|npx\s+tsc\b|tsc\s+--|npm(\s+run)?\s+(build|test)\b|python\s+-m\s+pytest\b|pytest\b)'

# Escape-hatch marker: an explicit opt-in to run an otherwise-intercepted command
# and see its full raw output. POSIX class [[:space:]] for macOS/Linux portability
# (BSD grep lacks \s). The "#" makes it a harmless shell comment when the cmd runs.
ESCAPE_RE='#[[:space:]]*raw-output'

COMMAND=""
CWD=""

_log() {
  # _log MATCHED [ESCAPED] [REASON] — append JSONL record; never fails.
  local matched="$1"
  local escaped="${2:-false}"
  local reason="${3:-}"
  local TS CMD_JSON CWD_JSON REASON_JSON
  TS=$(date -u +%Y-%m-%dT%H:%M:%SZ 2>/dev/null) || TS=""
  CMD_JSON=$(printf '%s' "$COMMAND" | jq -Rs '.' 2>/dev/null) || CMD_JSON='""'
  CWD_JSON=$(printf '%s' "$CWD" | jq -Rs '.' 2>/dev/null) || CWD_JSON='""'
  REASON_JSON=$(printf '%s' "$reason" | jq -Rs '.' 2>/dev/null) || REASON_JSON='""'
  printf '{"ts":"%s","command":%s,"cwd":%s,"matched":%s,"escaped":%s,"reason":%s}\n' \
    "$TS" "$CMD_JSON" "$CWD_JSON" "$matched" "$escaped" "$REASON_JSON" \
    >> "$INTERCEPT_LOG" 2>/dev/null || true
}

# ── Parse PreToolUse payload ──────────────────────────────────────────────────
INPUT=$(cat)
TOOL_NAME=$(printf '%s' "$INPUT" | jq -r '.tool_name // empty' 2>/dev/null) || true
[ "$TOOL_NAME" = "Bash" ] || exit 0

COMMAND=$(printf '%s' "$INPUT" | jq -r '.tool_input.command // empty' 2>/dev/null) || true
CWD=$(printf '%s' "$INPUT" | jq -r '.tool_input.cwd // empty' 2>/dev/null) || true
[ -z "$CWD" ] && CWD="$(pwd)"

# ── Apply intercept pattern ───────────────────────────────────────────────────
if ! printf '%s' "$COMMAND" | grep -qE "$BUILD_RE" 2>/dev/null; then
  _log false
  exit 0
fi

# Escape hatch: an explicit "# raw-output" marker means Claude has decided it needs
# the full log. Let the command run unmodified (exit 0 → normal permission flow).
if printf '%s' "$COMMAND" | grep -qE "$ESCAPE_RE" 2>/dev/null; then
  _log true true
  exit 0
fi

# ── Reachability check — fail open before denying ─────────────────────────────
# A deny with no working redirect target is worse than no hook at all: it
# blocks every build/test command with no sanctioned way to run them. Verify
# the redirect target is actually usable first.
if [ ! -x "$SCOUT_BIN" ]; then
  _log true false "missing-binary"
  exit 0
fi

if ! _timeout "$SCOUT_BIN" run --ping >/dev/null 2>&1; then
  _log true false "endpoint-unreachable"
  exit 0
fi

_log true false

# ── Emit deny with redirect message (rides permissionDecisionReason — see header) ─
jq -n --arg cmd "$COMMAND" '{
  hookSpecificOutput: {
    hookEventName: "PreToolUse",
    permissionDecision: "deny",
    permissionDecisionReason: ("Build/test output floods conversation context. Use mcp__scout__check_output instead:\n\n  mcp__scout__check_output(command=\"" + $cmd + "\")\n\nIf that tool is not in your loaded toolset, it is a deferred MCP tool — run ToolSearch for \"check_output\" to load its schema, then call it.\n\nReturns: {ok: bool, summary: string, first_error: {...}|null, suggested_next_step: string}\n\nNeed the full raw log — e.g. ok=false AND first_error=null (the classifier could not parse the output), or detail it dropped? Re-run the SAME command with a \"# raw-output\" marker appended to bypass this redirect:\n\n  " + $cmd + " # raw-output")
  }
}'
