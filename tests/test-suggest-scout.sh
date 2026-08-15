#!/usr/bin/env bash
# test-suggest-scout.sh — Tests for hooks/suggest-scout.sh
#
# Verifies:
#   - Invariant 1: NO output ever carries a permissionDecision. This hook is
#     advisory only; if it ever learns to deny or allow, that is a regression,
#     not a feature.
#   - Read: fires above the size threshold, silent below it, silent on binary
#     extensions and on files that do not exist
#   - Bash: fires on a broad uncapped recursive search, stays silent on a
#     capped one (-l, -c, -m, | head) and on a narrow single-file grep
#   - The throttle suppresses a second nudge of the same kind inside the
#     window, and the two kinds throttle independently
#   - Fail-open and silent: malformed stdin, unknown tool, missing binary
#   - Emitted JSON is well-formed and names the unqualified tool only —
#     never a fully-qualified mcp__plugin_* literal (CLAUDE.md)
#
# Usage:
#   ./tests/test-suggest-scout.sh
#   ./tests/test-suggest-scout.sh --verbose

# Note: -e intentionally omitted — tests capture non-zero exits via $?
set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
HOOK="$PROJECT_DIR/plugins/scout/hooks/suggest-scout.sh"

export VERBOSE=false
[ "${1:-}" = "--verbose" ] && VERBOSE=true

# shellcheck source=tests/lib-test.sh
source "$SCRIPT_DIR/lib-test.sh"

# ── Prerequisites ────────────────────────────────────────────────────────────

if ! command -v jq >/dev/null 2>&1; then
  echo "SKIP: jq is required but not found" >&2
  exit 0
fi

if [ ! -x "$HOOK" ]; then
  echo "SKIP: hook not found or not executable: $HOOK" >&2
  exit 0
fi

# ── Isolated environment ─────────────────────────────────────────────────────
# Unlike prefer-local-llm.sh, this hook never shells out to scout — it only
# stats the binary — so a stub is sufficient and correct here, no real build
# needed. $HOME is redirected so the throttle stamps land in the sandbox.

TMPROOT="$(mktemp -d)"
trap 'rm -rf "$TMPROOT"' EXIT

export HOME="$TMPROOT/home"
mkdir -p "$HOME"

STUB_DIR="$TMPROOT/stub/bin"
mkdir -p "$STUB_DIR"
printf '#!/bin/sh\nexit 0\n' > "$STUB_DIR/scout"
chmod +x "$STUB_DIR/scout"
export CLAUDE_PLUGIN_DATA="$TMPROOT/stub"

# Throttle off by default; the throttle tests re-enable it explicitly.
export SCOUT_SUGGEST_THROTTLE_SECS=0
export SCOUT_SUGGEST_STAMP_DIR="$TMPROOT/stamps"

# A curated PATH holding every tool the hook needs and nothing else — in
# particular, no `scout`. Emptying PATH outright does not work: the shebang is
# `#!/usr/bin/env bash`, so the script cannot even start and the test reports
# 127 for the wrong reason. This also closes the isolation gap the other two
# suites have (see CLAUDE.md): with no scout reachable by `command -v`, the
# missing-binary branch is genuinely exercised rather than skipped.
CLEAN_PATH_DIR="$TMPROOT/cleanpath"
mkdir -p "$CLEAN_PATH_DIR"
for tool in env bash sh jq date mkdir cat wc tr grep; do
  tool_path="$(command -v "$tool" 2>/dev/null)" || continue
  ln -sf "$tool_path" "$CLEAN_PATH_DIR/$tool"
done
if [ -e "$CLEAN_PATH_DIR/scout" ]; then
  echo "SKIP: sandbox PATH unexpectedly contains scout" >&2
  exit 0
fi

FIXTURES="$TMPROOT/fixtures"
mkdir -p "$FIXTURES"

BIG="$FIXTURES/big.log"
: > "$BIG"
i=0
while [ "$i" -lt 2000 ]; do
  printf 'line %d: some log content padding it out to a realistic width\n' "$i" >> "$BIG"
  i=$((i + 1))
done

SMALL="$FIXTURES/small.toml"
printf 'key = "value"\n' > "$SMALL"

BIG_BINARY="$FIXTURES/big.png"
cp "$BIG" "$BIG_BINARY"

# ── Helpers ──────────────────────────────────────────────────────────────────

# run_read FILE — emit a Read PreToolUse payload, return the hook's stdout.
run_read() {
  jq -n --arg f "$1" '{tool_name:"Read",tool_input:{file_path:$f}}' | "$HOOK" 2>/dev/null
}

# run_bash COMMAND — emit a Bash PreToolUse payload, return the hook's stdout.
run_bash() {
  jq -n --arg c "$1" '{tool_name:"Bash",tool_input:{command:$c}}' | "$HOOK" 2>/dev/null
}

# assert_fires OUTPUT LABEL — non-empty, valid JSON, carries additionalContext.
assert_fires() {
  local out="$1" label="$2"
  if [ -z "$out" ]; then
    fail "$label" "expected a nudge, got no output"
    return
  fi
  if ! printf '%s' "$out" | jq -e '.hookSpecificOutput.additionalContext' >/dev/null 2>&1; then
    fail "$label" "output is not valid JSON with additionalContext: $out"
    return
  fi
  pass "$label"
}

# assert_silent OUTPUT LABEL
assert_silent() {
  if [ -z "$1" ]; then
    pass "$2"
  else
    fail "$2" "expected silence, got: $1"
  fi
}

reset_stamps() { rm -rf "$SCOUT_SUGGEST_STAMP_DIR"; }

# ── Invariant 1: never emits a permission decision ───────────────────────────

echo "Invariant: advisory only, never a permission decision"

ALL_OUTPUT=""
ALL_OUTPUT="$ALL_OUTPUT$(run_read "$BIG")"
ALL_OUTPUT="$ALL_OUTPUT$(run_bash 'grep -r TODO .')"
ALL_OUTPUT="$ALL_OUTPUT$(run_bash 'rg pattern')"

if printf '%s' "$ALL_OUTPUT" | grep -q 'permissionDecision'; then
  fail "no permissionDecision in any emitted output" "found one: $ALL_OUTPUT"
else
  pass "no permissionDecision in any emitted output"
fi

if printf '%s' "$ALL_OUTPUT" | grep -q 'mcp__plugin'; then
  fail "no fully-qualified MCP name in nudge text" "found a stale-prone literal"
else
  pass "no fully-qualified MCP name in nudge text"
fi

# ── Read ─────────────────────────────────────────────────────────────────────

echo ""
echo "Read: size threshold and file-type filtering"

reset_stamps
assert_fires "$(run_read "$BIG")" "large text file fires"

reset_stamps
assert_silent "$(run_read "$SMALL")" "small file is silent"

reset_stamps
assert_silent "$(run_read "$BIG_BINARY")" "large binary-extension file is silent"

reset_stamps
assert_silent "$(run_read "$FIXTURES/does-not-exist.txt")" "nonexistent file is silent"

reset_stamps
out=$(run_read "$BIG")
if printf '%s' "$out" | jq -r '.hookSpecificOutput.additionalContext' | grep -q 'extract'; then
  pass "Read nudge names extract"
else
  fail "Read nudge names extract" "got: $out"
fi

# Threshold is configurable and respected in both directions.
reset_stamps
assert_silent "$(SCOUT_SUGGEST_MIN_BYTES=99999999 run_read "$BIG")" "raised threshold silences a large file"

reset_stamps
assert_fires "$(SCOUT_SUGGEST_MIN_BYTES=1 run_read "$SMALL")" "lowered threshold fires on a small file"

# ── Bash ─────────────────────────────────────────────────────────────────────

echo ""
echo "Bash: breadth signal and output caps"

for cmd in \
  'grep -r TODO .' \
  'grep -rn "func " src/' \
  'grep -R pattern .' \
  'rg pattern' \
  'cd /tmp && grep -r needle .'
do
  reset_stamps
  assert_fires "$(run_bash "$cmd")" "broad uncapped search fires: $cmd"
done

for cmd in \
  'grep -rl TODO .' \
  'grep -rc TODO .' \
  'grep -rm 5 TODO .' \
  'grep -r TODO . | head -20' \
  'rg --files-with-matches pattern' \
  'rg pattern | wc -l' \
  'grep TODO src/main.rs' \
  'ls -la' \
  'cargo build'
do
  reset_stamps
  assert_silent "$(run_bash "$cmd")" "no nudge: $cmd"
done

reset_stamps
out=$(run_bash 'grep -r TODO .')
if printf '%s' "$out" | jq -r '.hookSpecificOutput.additionalContext' | grep -q 'intent'; then
  pass "grep nudge names the intent parameter"
else
  fail "grep nudge names the intent parameter" "got: $out"
fi

# ── Throttle ─────────────────────────────────────────────────────────────────

echo ""
echo "Throttle"

reset_stamps
first=$(SCOUT_SUGGEST_THROTTLE_SECS=600 run_read "$BIG")
second=$(SCOUT_SUGGEST_THROTTLE_SECS=600 run_read "$BIG")
assert_fires "$first" "first Read nudge fires under throttle"
assert_silent "$second" "second Read nudge suppressed inside window"

reset_stamps
r=$(SCOUT_SUGGEST_THROTTLE_SECS=600 run_read "$BIG")
g=$(SCOUT_SUGGEST_THROTTLE_SECS=600 run_bash 'grep -r TODO .')
assert_fires "$r" "Read kind fires"
assert_fires "$g" "grep kind fires independently of Read kind"

reset_stamps
a=$(SCOUT_SUGGEST_THROTTLE_SECS=0 run_read "$BIG")
b=$(SCOUT_SUGGEST_THROTTLE_SECS=0 run_read "$BIG")
assert_fires "$a" "throttle=0 fires once"
assert_fires "$b" "throttle=0 fires again immediately"

# ── Fail-open ────────────────────────────────────────────────────────────────

echo ""
echo "Fail-open"

reset_stamps
out=$(printf 'not json at all' | "$HOOK" 2>/dev/null); rc=$?
assert_silent "$out" "malformed stdin is silent"
assert_eq "$rc" "0" "malformed stdin exits 0"

reset_stamps
out=$(jq -n '{tool_name:"Write",tool_input:{file_path:"/tmp/x"}}' | "$HOOK" 2>/dev/null); rc=$?
assert_silent "$out" "unhandled tool is silent"
assert_eq "$rc" "0" "unhandled tool exits 0"

reset_stamps
out=$(jq -n '{tool_name:"Read"}' | "$HOOK" 2>/dev/null); rc=$?
assert_silent "$out" "Read with no file_path is silent"
assert_eq "$rc" "0" "Read with no file_path exits 0"

# These two cases make the hook exit before it drains stdin, so the payload is
# built FIRST and fed to the hook alone. Piping `jq | hook` here would hand jq a
# SIGPIPE and — under `set -o pipefail` — report jq's 141 as the hook's status,
# testing the harness instead of the hook.
READ_PAYLOAD=$(jq -n --arg f "$BIG" '{tool_name:"Read",tool_input:{file_path:$f}}')

# Missing binary: nothing to point at, so say nothing. The curated PATH is used
# here because the hook falls back to `command -v scout` when the plugin-data
# path misses — pointing CLAUDE_PLUGIN_DATA at nothing is not sufficient on its
# own if a real scout happens to be installed on the tester's PATH.
reset_stamps
out=$(printf '%s' "$READ_PAYLOAD" | \
  env CLAUDE_PLUGIN_DATA="$TMPROOT/nonexistent" PATH="$CLEAN_PATH_DIR" "$HOOK" 2>/dev/null); rc=$?
assert_silent "$out" "missing scout binary is silent"
assert_eq "$rc" "0" "missing scout binary exits 0"

reset_stamps
out=$(printf '%s' "$READ_PAYLOAD" | env -u HOME "$HOOK" 2>/dev/null); rc=$?
assert_silent "$out" "missing HOME is silent"
assert_eq "$rc" "0" "missing HOME exits 0"

print_results
