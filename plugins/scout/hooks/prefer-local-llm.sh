#!/usr/bin/env bash
# prefer-local-llm.sh — PreToolUse hook: redirect build/test commands to
# scout's check_output MCP tool.
#
# Emits a deny when the Bash command matches a known build/test verb, carrying a
# permissionDecisionReason that names the check_output tool and its call
# pattern. The reason deliberately does NOT hardcode a fully-qualified tool
# name: the prefix (mcp__plugin_<plugin>_<server>__) is derived from the plugin
# and server names rather than declared where this hook can read it, and a stale
# literal would deny into a dead end. It names the unqualified tool and points at
# ToolSearch to resolve it. Deny (not advisory allow) because raw build output flooding the
# conversation context is the failure mode to prevent, not merely a suboptimal
# choice. NOTE: the redirect text MUST ride in permissionDecisionReason — Claude
# Code silently drops a field named "reason", leaving only a bare "denied".
#
# Escape hatch: when the full raw log is genuinely needed (e.g. the classifier
# could not parse the output), re-run the SAME command with a "# raw-output"
# marker appended; the hook recognizes the marker and lets the command through.
# Output stays out of context by default, with a deliberate one-step opt-in.
#
# Intercepted, but ONLY in command position — as the head of a simple command
# the shell will actually run:
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
# Matching is two-stage (see SPEC-command-matching.md):
#
#   Stage 1 — an unanchored grep for any verb's leading word. Runs on every
#     Bash call, so it stays to one grep; it over-matches deliberately and has
#     no false negatives by construction, which is what makes it safe as a
#     pre-filter. No hit → log and exit, zero further subprocesses.
#   Stage 2 — `scout classify-command`, reached only on a stage-1 hit. It lexes
#     the command (quotes, heredocs, comments, command substitution) and
#     reports whether a verb sits in command position and whether the escape
#     marker is in a real comment.
#
#   HISTORY: this used to be one anchored regex over the raw command string,
#   with a comment claiming `^\s*` kept the verbs out of echo/printf strings.
#   That claim was FALSE — grep is line-oriented, so `^` anchors to the start of
#   any line inside the command, including a heredoc body. It blocked commit
#   messages that merely mentioned `cargo test` while letting
#   `cd foo && cargo test` run raw. Position in the string was never the
#   property that mattered; command position is, and that needs a lexer.
#
#   Stage 2 lives in the Rust binary rather than in bash: the hook already
#   refuses to deny without a working `scout` (see invariant 2), so it adds no
#   new dependency, and the lexer's test matrix belongs under `cargo test`.
#   Version skew — an older installed binary with no `classify-command` — is
#   just another fail-open path (reason `classify-failure`).
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
# Same resolution order that script and bin/scout use. The PATH fallback is
# load-bearing even though the plugin is the only Claude Code install: it covers
# a CLI install (make install) and any context where CLAUDE_PLUGIN_DATA is not
# exported — running this hook by hand, or from a test harness.
SCOUT_BIN="${CLAUDE_PLUGIN_DATA:-$HOME/.claude/plugins/data/scout-scout}/bin/scout"
[ -x "$SCOUT_BIN" ] || SCOUT_BIN="$(command -v scout 2>/dev/null || true)"
SUBPROCESS_TIMEOUT_SECS=6

# Wrapper for the two scout subprocesses this hook spawns (`classify-command`
# and `run --ping`): use timeout/gtimeout if available, otherwise run bare
# (`run --ping` has its own internal ~5s HTTP timeout and classify-command is
# pure local lexing; a missing timeout cmd is not fatal, just less defensive
# against a hung process).
_timeout() {
  if command -v timeout >/dev/null 2>&1; then
    timeout "$SUBPROCESS_TIMEOUT_SECS" "$@"
  elif command -v gtimeout >/dev/null 2>&1; then
    gtimeout "$SUBPROCESS_TIMEOUT_SECS" "$@"
  else
    "$@"
  fi
}

# Stage-1 pre-filter: the leading word of every entry in the verb table, plus
# the bare-verb entries. Unanchored and deliberately loose — a hit means only
# "worth asking the classifier", never "intercept". The one hard requirement is
# that it must not under-match relative to the table, so every leading word
# above appears here. Word boundaries are spelled out with POSIX bracket
# expressions rather than \b, which BSD grep -E does not support.
PREFILTER_RE='(^|[^[:alnum:]_])(cargo|go|tsc|npx|npm|pytest|python)([^[:alnum:]_]|$)'

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

# ── Stage 1: cheap pre-filter ─────────────────────────────────────────────────
# The overwhelming majority of Bash commands mention no build verb at all and
# leave here having spawned exactly one grep.
if ! printf '%s' "$COMMAND" | grep -qE "$PREFILTER_RE" 2>/dev/null; then
  _log false
  exit 0
fi

# ── Stage 2: command-position classification ──────────────────────────────────
# Needs the scout binary, which the deny path requires anyway (see invariant 2),
# so check for it first and reuse the existing missing-binary fail-open.
if [ ! -x "$SCOUT_BIN" ]; then
  _log true false "missing-binary"
  exit 0
fi

# The command goes in on stdin: it can contain quotes, newlines and heredoc
# bodies, and stdin sidesteps every quoting hazard argv would introduce.
CLASSIFY=$(printf '%s' "$COMMAND" | _timeout "$SCOUT_BIN" classify-command 2>/dev/null) || CLASSIFY=""
INTERCEPT=$(printf '%s' "$CLASSIFY" | jq -r 'if (.intercept | type) == "boolean" then .intercept else empty end' 2>/dev/null) || INTERCEPT=""
ESCAPED=$(printf '%s' "$CLASSIFY" | jq -r 'if (.escape | type) == "boolean" then .escape else empty end' 2>/dev/null) || ESCAPED=""

# Non-zero exit, empty output, or anything that isn't the expected JSON — which
# includes an older installed binary that has no classify-command subcommand.
# We cannot tell whether this command should be intercepted, so fail open.
if [ -z "$INTERCEPT" ] || [ -z "$ESCAPED" ]; then
  _log true false "classify-failure"
  exit 0
fi

if [ "$INTERCEPT" != "true" ]; then
  _log false
  exit 0
fi

# Escape hatch: an explicit "# raw-output" marker — in a real comment, not in a
# quoted string or heredoc body — means Claude has decided it needs the full
# log. Let the command run unmodified (exit 0 → normal permission flow).
if [ "$ESCAPED" = "true" ]; then
  _log true true
  exit 0
fi

# ── Reachability check — fail open before denying ─────────────────────────────
# A deny with no working redirect target is worse than no hook at all: it
# blocks every build/test command with no sanctioned way to run them. The
# binary is already known to exist; confirm its endpoint answers too.
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
    permissionDecisionReason: ("Build/test output floods conversation context. Use the scout check_output MCP tool instead, with:\n\n  command=\"" + $cmd + "\"\n\nIts fully-qualified name carries a plugin-derived prefix (mcp__plugin_<plugin>_<server>__check_output). If it is not already in your loaded toolset, run ToolSearch for \"check_output\" to resolve the name and load its schema, then call it.\n\nReturns: {ok: bool, summary: string, first_error: {...}|null, suggested_next_step: string}\n\nNeed the full raw log — e.g. ok=false AND first_error=null (the classifier could not parse the output), or detail it dropped? Re-run the SAME command with a \"# raw-output\" marker appended to bypass this redirect:\n\n  " + $cmd + " # raw-output")
  }
}'
