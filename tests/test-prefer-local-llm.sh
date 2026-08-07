#!/usr/bin/env bash
# test-prefer-local-llm.sh — Tests for hooks/prefer-local-llm.sh
#
# Verifies:
#   - Each intercepted verb prefix produces a deny JSON whose
#     permissionDecisionReason names check_output (unqualified — see below)
#   - The "# raw-output" escape hatch lets an otherwise-intercepted command
#     through (no deny)
#   - Non-build commands produce no output and exit 0
#   - JSONL intercept log is written with matched:true/false correctly
#   - Malformed stdin → exit 0 silently (fail-open)
#   - Reachability fail-open: missing scout binary, or an unreachable
#     endpoint (ping fails), both let the raw command through instead of
#     denying into a dead end — the ported fix for the ct issue where a
#     hard deny with no live redirect target bricked the Bash tool.
#   - macOS/Linux portability (no GNU-only constructs)
#
# Usage:
#   ./tests/test-prefer-local-llm.sh
#   ./tests/test-prefer-local-llm.sh --verbose

# Note: -e intentionally omitted — tests capture non-zero exits via $?
set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
HOOK="$PROJECT_DIR/hooks/prefer-local-llm.sh"

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

# ── Helpers ──────────────────────────────────────────────────────────────────

TMPDIR_TEST=$(mktemp -d)
trap 'rm -rf "$TMPDIR_TEST"' EXIT

# Override HOME so the log goes to a temp dir we control
export HOME="$TMPDIR_TEST"
mkdir -p "$TMPDIR_TEST/.claude"
INTERCEPT_LOG="$TMPDIR_TEST/.claude/scout-intercepts.jsonl"

# A working scout binary stub: `run --ping` succeeds (reachable). Used as the
# default CLAUDE_PLUGIN_DATA for every test in this file except the
# reachability fail-open tests, which point at a missing/broken one instead.
GOOD_DATA="$TMPDIR_TEST/scout-data-good"
mkdir -p "$GOOD_DATA/bin"
cat > "$GOOD_DATA/bin/scout" <<'EOF'
#!/usr/bin/env bash
case "$1 $2" in
  "run --ping") exit 0 ;;
  *) exit 0 ;;
esac
EOF
chmod +x "$GOOD_DATA/bin/scout"
export CLAUDE_PLUGIN_DATA="$GOOD_DATA"

# A stub binary that exists but whose endpoint is unreachable (`run --ping`
# fails), for the reachability fail-open tests.
DEAD_DATA="$TMPDIR_TEST/scout-data-dead"
mkdir -p "$DEAD_DATA/bin"
cat > "$DEAD_DATA/bin/scout" <<'EOF'
#!/usr/bin/env bash
case "$1 $2" in
  "run --ping") exit 1 ;;
  *) exit 1 ;;
esac
EOF
chmod +x "$DEAD_DATA/bin/scout"

# A CLAUDE_PLUGIN_DATA dir with no scout binary at all, for the missing-binary
# fail-open test.
MISSING_DATA="$TMPDIR_TEST/scout-data-missing"
mkdir -p "$MISSING_DATA"

# Build a PreToolUse JSON payload for a Bash command
make_payload() {
  local cmd="$1"
  jq -n --arg cmd "$cmd" '{
    tool_name: "Bash",
    tool_input: {command: $cmd, cwd: "/tmp"}
  }'
}

# Run hook with a given command; capture stdout and exit code
run_hook() {
  local cmd="$1"
  make_payload "$cmd" | "$HOOK" 2>/dev/null
}

# Return the last matched value from the log (true or false)
last_log_matched() {
  jq -r '.matched' "$INTERCEPT_LOG" 2>/dev/null | tail -1
}

last_log_reason() {
  jq -r '.reason' "$INTERCEPT_LOG" 2>/dev/null | tail -1
}

# ── Tests: intercepted verbs → deny whose permissionDecisionReason names check_output ─
#
# Asserts the UNQUALIFIED tool name: the fully-qualified name carries an
# install-dependent prefix (mcp__plugin_<plugin>_<server>__ under a plugin,
# mcp__<server>__ from .mcp.json), so pinning a literal here would pin the
# hook to one install method.

for cmd in \
  "cargo build" \
  "cargo build --release" \
  "cargo test" \
  "cargo test --quiet" \
  "cargo check" \
  "cargo clippy" \
  "go build ./..." \
  "go test ./..." \
  "go vet ./..." \
  "npx tsc" \
  "npx tsc --noEmit" \
  "tsc --noEmit" \
  "tsc --watch" \
  "npm test" \
  "npm run test" \
  "npm build" \
  "npm run build" \
  "python -m pytest" \
  "python -m pytest tests/" \
  "pytest" \
  "pytest -v tests/"; do

  output=$(run_hook "$cmd")

  # Must emit valid JSON
  if ! echo "$output" | jq . >/dev/null 2>&1; then
    fail "[$cmd] output is not valid JSON" "got: $output"
    continue
  fi

  decision=$(echo "$output" | jq -r '.hookSpecificOutput.permissionDecision // empty')
  if [ "$decision" = "deny" ]; then
    pass "[$cmd] permissionDecision=deny"
  else
    fail "[$cmd] expected deny, got '$decision'"
  fi

  reason=$(echo "$output" | jq -r '.hookSpecificOutput.permissionDecisionReason // empty')
  if echo "$reason" | grep -q "check_output"; then
    pass "[$cmd] permissionDecisionReason names check_output"
  else
    fail "[$cmd] permissionDecisionReason missing check_output" "reason: $reason"
  fi

  # And it must point at ToolSearch, which is how the model resolves the
  # install-dependent prefix into a callable name.
  if echo "$reason" | grep -q "ToolSearch"; then
    pass "[$cmd] permissionDecisionReason points at ToolSearch for name resolution"
  else
    fail "[$cmd] permissionDecisionReason missing ToolSearch pointer" "reason: $reason"
  fi

  if echo "$reason" | grep -q "first_error"; then
    pass "[$cmd] permissionDecisionReason includes return shape (first_error)"
  else
    fail "[$cmd] permissionDecisionReason missing return shape" "reason: $reason"
  fi

  matched=$(last_log_matched)
  if [ "$matched" = "true" ]; then
    pass "[$cmd] log entry matched=true"
  else
    fail "[$cmd] expected log matched=true, got '$matched'"
  fi
done

# ── Tests: non-build commands → no output, exit 0 ────────────────────────────

for cmd in \
  "cargo add serde" \
  "cargo fmt" \
  "cargo clean" \
  "go fmt ./..." \
  "go mod tidy" \
  "go mod download" \
  "npm install" \
  "npm install --save-dev" \
  "npm ci" \
  "ls -la" \
  "echo cargo test" \
  "git status"; do

  output=$(run_hook "$cmd")
  rc=$?

  if [ $rc -ne 0 ]; then
    fail "[$cmd] expected exit 0, got $rc"
  else
    pass "[$cmd] exits 0"
  fi

  if [ -z "$output" ]; then
    pass "[$cmd] no output"
  else
    fail "[$cmd] unexpected output" "got: $output"
  fi

  matched=$(last_log_matched)
  if [ "$matched" = "false" ]; then
    pass "[$cmd] log entry matched=false"
  else
    fail "[$cmd] expected log matched=false, got '$matched'"
  fi
done

# ── Test: non-Bash tool → no output, exit 0 ──────────────────────────────────

output=$(jq -n '{tool_name: "Read", tool_input: {file_path: "/tmp/foo"}}' | "$HOOK" 2>/dev/null)
rc=$?
assert_eq "$rc" "0" "[non-Bash tool] exits 0"
assert_eq "$output" "" "[non-Bash tool] no output"

# ── Test: malformed stdin → exit 0 silently (fail-open) ──────────────────────

output=$(printf 'not json at all\n' | "$HOOK" 2>/dev/null)
rc=$?
assert_eq "$rc" "0" "[malformed stdin] exits 0"
assert_eq "$output" "" "[malformed stdin] no output"

output=$(printf '' | "$HOOK" 2>/dev/null)
rc=$?
assert_eq "$rc" "0" "[empty stdin] exits 0"
assert_eq "$output" "" "[empty stdin] no output"

# ── Test: edge cases — empty, whitespace, leading whitespace ─────────────────

# Empty command: BUILD_RE requires a verb after ^\s*, so empty never matches
output=$(make_payload "" | "$HOOK" 2>/dev/null)
assert_eq "$output" "" "[empty command] no output (no false-positive deny)"
assert_eq "$(last_log_matched)" "false" "[empty command] log matched=false"

# Whitespace-only command: ^\s* matches but verb alternation doesn't follow
output=$(make_payload "   " | "$HOOK" 2>/dev/null)
assert_eq "$output" "" "[whitespace command] no output (no false-positive deny)"
assert_eq "$(last_log_matched)" "false" "[whitespace command] log matched=false"

# Leading whitespace before build verb — ^\s* must absorb it
output=$(make_payload "  cargo test" | "$HOOK" 2>/dev/null)
decision=$(echo "$output" | jq -r '.hookSpecificOutput.permissionDecision // empty' 2>/dev/null)
assert_eq "$decision" "deny" "[leading whitespace] cargo test still intercepted"
assert_eq "$(last_log_matched)" "true" "[leading whitespace] log matched=true"

# ── Test: intercepted command appears in permissionDecisionReason ────────────

output=$(run_hook "cargo test --quiet")
reason=$(echo "$output" | jq -r '.hookSpecificOutput.permissionDecisionReason // empty')
if echo "$reason" | grep -q "cargo test --quiet"; then
  pass "[permissionDecisionReason] includes the intercepted command"
else
  fail "[permissionDecisionReason] does not include the intercepted command" "reason: $reason"
fi

# ── Tests: escape hatch (# raw-output) lets an intercepted command through ────

# Marker present → no deny, exit 0, command runs normally under the usual flow
output=$(run_hook "cargo test # raw-output")
rc=$?
assert_eq "$rc" "0" "[escape hatch] exits 0"
assert_eq "$output" "" "[escape hatch] no deny output"
assert_eq "$(last_log_matched)" "true" "[escape hatch] log matched=true (build verb still recognized)"
assert_eq "$(jq -r '.escaped' "$INTERCEPT_LOG" 2>/dev/null | tail -1)" "true" "[escape hatch] log escaped=true"

# A normal intercepted command logs escaped=false
run_hook "cargo build" >/dev/null
assert_eq "$(jq -r '.escaped' "$INTERCEPT_LOG" 2>/dev/null | tail -1)" "false" "[no marker] log escaped=false"

# The deny reason advertises the escape hatch so Claude can discover it
output=$(run_hook "cargo test")
reason=$(echo "$output" | jq -r '.hookSpecificOutput.permissionDecisionReason // empty')
if echo "$reason" | grep -q "raw-output"; then
  pass "[deny reason] documents the # raw-output escape hatch"
else
  fail "[deny reason] missing escape-hatch instructions" "reason: $reason"
fi

# The deny reason hints how to load the tool when it is deferred
if echo "$reason" | grep -q "ToolSearch"; then
  pass "[deny reason] hints ToolSearch for the deferred tool"
else
  fail "[deny reason] missing deferred-tool ToolSearch hint" "reason: $reason"
fi

# ── Tests: reachability fail-open (scout-specific; not present in ct's hook) ─
# A deny with no working redirect target bricks the Bash tool for all
# build/test commands. Both failure modes below must fail OPEN: no deny
# output, exit 0, raw command allowed through.

# Missing binary: CLAUDE_PLUGIN_DATA points at a dir with no bin/scout.
# NOTE: the CLAUDE_PLUGIN_DATA override must live only in the pipeline below
# (as a prefix to the hook invocation) — assigning it as a bare statement in
# this shell would permanently overwrite the already-exported $CLAUDE_PLUGIN_DATA
# for every later run_hook call in this file.
output=$(make_payload "cargo build" | CLAUDE_PLUGIN_DATA="$MISSING_DATA" "$HOOK" 2>/dev/null)
rc=$?
assert_eq "$rc" "0" "[missing binary] exits 0"
assert_eq "$output" "" "[missing binary] no deny output (fail-open)"
assert_eq "$(last_log_reason)" "missing-binary" "[missing binary] log reason=missing-binary"

# Endpoint unreachable: binary exists but `run --ping` fails.
output=$(make_payload "cargo test" | CLAUDE_PLUGIN_DATA="$DEAD_DATA" "$HOOK" 2>/dev/null)
rc=$?
assert_eq "$rc" "0" "[endpoint unreachable] exits 0"
assert_eq "$output" "" "[endpoint unreachable] no deny output (fail-open)"
assert_eq "$(last_log_reason)" "endpoint-unreachable" "[endpoint unreachable] log reason=endpoint-unreachable"

# Sanity: with the GOOD stub restored, the same command still denies normally
# (proves the fail-open tests above aren't just a broken hook).
output=$(run_hook "cargo build")
decision=$(echo "$output" | jq -r '.hookSpecificOutput.permissionDecision // empty' 2>/dev/null)
assert_eq "$decision" "deny" "[reachable] cargo build still denied when scout is reachable"

# ── Results ──────────────────────────────────────────────────────────────────

print_results
