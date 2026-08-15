#!/usr/bin/env bash
# shell-safety.sh — PreToolUse hook: auto-approve confidently-safe Bash commands.
#
# Decision order (every step free except the last):
#   1. No shell expansion in command → log "no-expansion", exit (normal
#      allowlist handles it — the bulk of commands, zero LLM call).
#   2. Baked-in deny-list floor → log "deny-floor", exit (no LLM call).
#   2b. Config deny extensions ([shell_safety] deny=[...] in config.toml) →
#      log "deny-config", exit (no LLM call). Config can extend but not
#      shrink the floor; malformed/missing config falls to the floor safely.
#   2c. Fast-path allowlist: all $() subs are known-safe patterns AND no $VAR
#      refs → log "allow-fastpath", emit allow, exit (zero LLM call).
#   2c-bis. Trusted plugin-script fast-path: command does nothing but invoke
#      one of a plugin's own vetted scripts ($CLAUDE_PLUGIN_ROOT/scripts/*.sh)
#      with no other unknowable expansion → log "allow-fastpath" (trusted
#      plugin script), emit allow.
#   3. Expansion present, not deny-listed → resolve the command's referenced
#      env vars from the live environment (known_vars), then call the local
#      LLM via the scout binary (`scout run --preset shell_safety`):
#        verdict "allow"  → emit permissionDecision:allow + log "allow"
#        anything else    → log verdict, exit (fail-to-ask)
#
# Shadow mode: set $SCOUT_SHELL_SAFETY_SHADOW, or touch
# ~/.claude/scout-shell-safety.shadow, and step 3's "allow" is logged as
# "allow-shadow" without being emitted — the command falls through to the
# harness's own permission decision instead. Everything else (classification,
# logging, the deny floor) behaves identically. This exists to measure what the
# hook is actually buying now that Claude Code has an auto-approve mode of its
# own: every "allow-shadow" row is a command this hook would have approved, so
# a prompt seen on one is a case the harness did not cover by itself. Shadow is
# strictly less permissive than normal operation, so it is safe to leave on.
#
# Hard invariant: fail-open. Any error, timeout, missing binary, or parse
# failure → exit 0, emit nothing → normal permission prompt. The hook only
# ever *adds* an allow — it never denies or blocks. A missing/unreachable
# scout binary therefore just forfeits the auto-approve optimization; it can
# never brick the Bash tool (see prefer-local-llm.sh for the hook that
# denies, and how IT stays fail-open too).

set -euo pipefail

# Fail-open if HOME is unset (unusual CI environments, test harnesses).
[ -z "${HOME:-}" ] && exit 0
AUDIT_LOG="${HOME}/.claude/scout-shell-safety.jsonl"
LLM_TIMEOUT_SECS=5

# ── Resolve the scout binary ─────────────────────────────────────────────────
# Payload first, then the legacy data dir, then PATH. See the fuller comment on
# the identical block in prefer-local-llm.sh for why each entry is there and why
# this is duplicated rather than sourced. Keep the three copies byte-identical.
SCOUT_BIN="${CLAUDE_PLUGIN_ROOT:+${CLAUDE_PLUGIN_ROOT}/bin/scout}"
[ -x "$SCOUT_BIN" ] || SCOUT_BIN="${CLAUDE_PLUGIN_DATA:-$HOME/.claude/plugins/data/scout-scout}/bin/scout"
[ -x "$SCOUT_BIN" ] || SCOUT_BIN="$(command -v scout 2>/dev/null || true)"

# Wrapper: use timeout/gtimeout if available, otherwise run bare (the LLM
# call's own config timeout still applies; missing timeout cmd is not fatal).
_timeout() {
  if command -v timeout >/dev/null 2>&1; then
    timeout "$LLM_TIMEOUT_SECS" "$@"
  elif command -v gtimeout >/dev/null 2>&1; then
    gtimeout "$LLM_TIMEOUT_SECS" "$@"
  else
    "$@"
  fi
}

_log() {
  # _log DECISION [key val ...] — append JSONL record; never fails.
  local decision="$1"; shift
  local DECISION_JSON CMD_JSON CWD_JSON TS_VAL
  TS_VAL=$(date -u +%Y-%m-%dT%H:%M:%SZ 2>/dev/null) || TS_VAL=""
  CMD_JSON=$(printf '%s' "$COMMAND" | jq -Rs '.' 2>/dev/null) || CMD_JSON='""'
  CWD_JSON=$(printf '%s' "$CWD"     | jq -Rs '.' 2>/dev/null) || CWD_JSON='""'
  DECISION_JSON=$(printf '%s' "$decision" | jq -Rs '.' 2>/dev/null) || DECISION_JSON='"unknown"'

  # Build optional extra fields from key/val pairs
  local extras=""
  while [ $# -ge 2 ]; do
    local k="$1" v="$2"; shift 2
    local vj
    vj=$(printf '%s' "$v" | jq -Rs '.' 2>/dev/null) || vj='""'
    extras="${extras},\"${k}\":${vj}"
  done

  printf '{"ts":"%s","command":%s,"cwd":%s,"expansion":%s,"decision":%s%s}\n' \
    "$TS_VAL" "$CMD_JSON" "$CWD_JSON" "$HAS_EXPANSION" "$DECISION_JSON" "$extras" \
    >> "$AUDIT_LOG" 2>/dev/null || true
}

# _unwrap_json TEXT — print the JSON object embedded in an LLM reply.
#
# `scout run` prints the model's reply verbatim (src/run_cmd.rs) because it is a
# generic preset runner and has no business massaging preset output — so peeling
# a fenced or prose-wrapped reply is this hook's job. The shell counterpart of
# src/select.rs::parse_selector_json, and needed for the same reason: the preset
# forbids fences, and the model emits them anyway. The audit log had 513
# `parse-failure` rows whose recorded output began "```json\n{\"verdict\":
# \"allow\"" — valid verdicts thrown away for their wrapping.
#
# Drops <think>...</think> blocks and fence lines, then keeps the first '{'
# through the last '}'. Prints nothing when there is no brace pair, which the
# caller treats as an unparsable reply (fail-to-ask), same as before.
#
# Known limitation: the <think> strip is line-wise and greedy, so two complete
# think spans on one line take the text between them with it. Real replies put
# the block on its own lines, and anything this mangles was already a
# parse-failure — it can only turn non-answers into answers, never the reverse.
_unwrap_json() {
  printf '%s' "$1" | awk '
    { lines[NR] = $0 }
    END {
      body = ""; think = 0
      for (i = 1; i <= NR; i++) {
        l = lines[i]
        if (think) {
          if (l ~ /<\/think>/) { sub(/^.*<\/think>/, "", l); think = 0 } else continue
        }
        while (l ~ /<think>.*<\/think>/) sub(/<think>.*<\/think>/, "", l)
        if (l ~ /<think>/) { sub(/<think>.*$/, "", l); think = 1 }
        if (l ~ /^[ \t]*```/) continue
        body = body l "\n"
      }
      first = index(body, "{")
      if (first == 0) exit
      last = 0
      for (i = length(body); i > 0; i--) {
        if (substr(body, i, 1) == "}") { last = i; break }
      }
      if (last <= first) exit
      printf "%s", substr(body, first, last - first + 1)
    }
  ' 2>/dev/null || true
}

# Load [shell_safety].deny list from config.toml; prints one entry per line.
# Returns nothing (not an error) when config is absent or unparsable — the
# baked-in floor remains the only gate; config can only extend, never shrink.
_load_config_deny() {
  local cfg="${SCOUT_CONFIG:-${XDG_CONFIG_HOME:-${HOME}/.config}/scout/config.toml}"
  [ -f "$cfg" ] || return 0
  python3 - "$cfg" 2>/dev/null <<'PYEOF' || true
import sys
cfg_path = sys.argv[1]
try:
    import tomllib
except ImportError:
    try:
        import tomli as tomllib  # pip install tomli for Python < 3.11
    except ImportError:
        sys.exit(0)
try:
    with open(cfg_path, "rb") as f:
        cfg = tomllib.load(f)
    for item in cfg.get("shell_safety", {}).get("deny", []):
        if isinstance(item, str) and item.strip():
            print(item)
except Exception:
    sys.exit(0)
PYEOF
}

# ── Parse PreToolUse payload ───────────────────────────────────────────────────
INPUT=$(cat)
TOOL_NAME=$(printf '%s' "$INPUT" | jq -r '.tool_name // empty' 2>/dev/null) || true

[ "$TOOL_NAME" = "Bash" ] || exit 0

COMMAND=$(printf '%s' "$INPUT" | jq -r '.tool_input.command // empty' 2>/dev/null) || true
CWD=$(printf '%s' "$INPUT" | jq -r '.tool_input.cwd // empty' 2>/dev/null) || true
[ -z "$CWD" ] && CWD="$(pwd)"

# ── Step 1: Detect shell expansion ────────────────────────────────────────────
# $() / backticks = command substitution; ${VAR} / $VAR = variable ref; * ? [ = glob.
HAS_EXPANSION=false
if printf '%s' "$COMMAND" | grep -qE '\$\(|`|\$\{|\$[A-Za-z_]|\*|\?|\[' 2>/dev/null; then
  HAS_EXPANSION=true
fi

if [ "$HAS_EXPANSION" = false ]; then
  _log "no-expansion"
  exit 0
fi

# ── Step 2: Baked-in deny-list floor ──────────────────────────────────────────
# These patterns always fall through to the normal prompt — no LLM call.
# The floor cannot be weakened by config; step 2b only makes it extensible.
DENY_REASON=""

# rm with any recursive flag (-r, -R, --recursive, or combined like -rf)
if printf '%s' "$COMMAND" | grep -qE '\brm\b' 2>/dev/null &&
   printf '%s' "$COMMAND" | grep -qE '(^|\s)-[a-zA-Z]*[rR][a-zA-Z]*(\s|$)|--recursive' 2>/dev/null; then
  DENY_REASON="rm recursive"
# git push (any form including --force)
elif printf '%s' "$COMMAND" | grep -qE '\bgit\s+push\b' 2>/dev/null; then
  DENY_REASON="git push"
# dd writing to device/file
elif printf '%s' "$COMMAND" | grep -qE '\bdd\b.*\bof=' 2>/dev/null; then
  DENY_REASON="dd write"
# chmod with recursive flag
elif printf '%s' "$COMMAND" | grep -qE '\bchmod\b' 2>/dev/null &&
     printf '%s' "$COMMAND" | grep -qE '(^|\s)-[a-zA-Z]*[rR][a-zA-Z]*(\s|$)|--recursive' 2>/dev/null; then
  DENY_REASON="chmod recursive"
# pipe-to-shell: curl/wget piped to bash/sh
elif printf '%s' "$COMMAND" | grep -qiE '\b(curl|wget)\b.*\|\s*(bash|sh)\b' 2>/dev/null; then
  DENY_REASON="pipe-to-shell"
# eval
elif printf '%s' "$COMMAND" | grep -qE '\beval\b' 2>/dev/null; then
  DENY_REASON="eval"
# source via process substitution: source <(...) or . <(...)
elif printf '%s' "$COMMAND" | grep -qE '\b(source|\.)(\s+)<\(' 2>/dev/null; then
  DENY_REASON="source process-substitution"
# write to system paths — except always-safe device files. Redirecting to
# /dev/null et al. is ubiquitous (e.g. `2>/dev/null`) and harmless, so it must
# not trip the floor. Extract every redirect target into a system dir, drop the
# safe devices, and deny only if an unsafe target remains.
elif printf '%s' "$COMMAND" | grep -qE '>\s*/(etc|usr|bin|sbin|dev)/' 2>/dev/null; then
  # Safe device files: /dev/null, /dev/zero, /dev/stdout, /dev/stderr,
  # /dev/tty, /dev/full, /dev/fd/N. Double-anchored (^>\s* start, $ end) so
  # crafted paths like >/dev/sda/dev/null that end with /dev/null are NOT
  # treated as safe — the ^ prevents suffix-only matching.
  SAFE_DEV_RE='^>\s*/dev/(null|zero|stdout|stderr|tty|full|fd/[0-9]+)$'
  SYS_WRITE_TARGETS=$(printf '%s' "$COMMAND" \
    | grep -oE '>\s*/(etc|usr|bin|sbin|dev)/[^[:space:];|&>)]*' 2>/dev/null) || SYS_WRITE_TARGETS=""
  UNSAFE_WRITE=$(printf '%s' "$SYS_WRITE_TARGETS" \
    | grep -vE "$SAFE_DEV_RE" 2>/dev/null | head -1) || UNSAFE_WRITE=""
  if [ -n "$UNSAFE_WRITE" ]; then
    DENY_REASON="write to system path"
  fi
fi

if [ -n "$DENY_REASON" ]; then
  _log "deny-floor" reason "$DENY_REASON"
  exit 0  # emit nothing → normal prompt = ask
fi

# ── Step 2b: Config deny extensions ───────────────────────────────────────────
# User-supplied literal strings from [shell_safety].deny in config.toml.
# Matched with grep -F (fixed strings — no regex metacharacter surprises for users).
CONFIG_DENY=$(_load_config_deny)
if [ -n "$CONFIG_DENY" ]; then
  MATCHED_PATTERN=""
  while IFS= read -r pattern; do
    if printf '%s' "$COMMAND" | grep -qF "$pattern" 2>/dev/null; then
      MATCHED_PATTERN="$pattern"
      break
    fi
  done <<< "$CONFIG_DENY"
  if [ -n "$MATCHED_PATTERN" ]; then
    _log "deny-config" reason "matches config deny pattern: $MATCHED_PATTERN"
    exit 0
  fi
fi

# ── Step 2c: Fast-path allowlist ───────────────────────────────────────────────
# Auto-allow commands whose only expansions are known-safe read-only
# substitutions. Zero LLM round-trip — and the ONLY step in this hook that
# emits `allow`, so it is deliberately paranoid. Fail toward ask: a missed
# auto-approve costs one permission prompt, a wrong allow is a hole.
#
# Same rigor as step 2c-bis: a SINGLE simple command built from literal words
# and safe substitutions. Any command separator (; | & && ||), subshell,
# backtick, real-file redirection, bare $VAR, or newline falls through to the
# LLM (step 3). Conditions, all required:
#   (1) no bare $VAR reference — unknowable value,
#   (2) at least one $( — otherwise step 1 already handled it,
#   (3) no backtick ANYWHERE — `...` is a substitution this test cannot see
#       into, and it can ride along beside a genuinely safe $(),
#   (4) every substitution matches SAFE_SUB_RE end to end,
#   (5) nothing but literal words and trusted redirection left over.
#
# Safe substitution set: git rev-parse/describe, pwd, date, basename, dirname,
# whoami, uname, echo — zero side effects. Tight list; only extend it with
# verbs that read. The pattern is anchored at BOTH ends of the substitution
# (\$\( … \)) and its argument body excludes every shell metacharacter, so no
# payload can ride behind a safe-looking prefix:
#   $(echo hi; curl …)         — `;` is not an argument character
#   $(git rev-parse $(curl …)) — nor are `$` and `(`, so the outer sub never
#                                matches and its `$(` survives into the residue
#   $(echofoo)                 — the verb must end at a blank or `)`, so the
#                                verb list matches whole words only
#   $(cat x > /tmp/y)          — `>` is not an argument character either
SAFE_SUB_RE='\$\((git[[:blank:]]+(rev-parse|describe)|pwd|date|basename|dirname|whoami|uname|echo)([[:blank:]]+[^;&|`()<>$[:space:]]+)*\)'

HAS_VAR_REF=false
printf '%s' "$COMMAND" | grep -qE '\$\{|\$[A-Za-z_]' 2>/dev/null && HAS_VAR_REF=true

HAS_SUB=false
case "$COMMAND" in *'$('*) HAS_SUB=true ;; esac

if [ "$HAS_VAR_REF" = false ] && [ "$HAS_SUB" = true ]; then
  FASTPATH_OK=true
  # (3) Backtick substitution — never fast-path, at any position.
  case "$COMMAND" in *'`'*) FASTPATH_OK=false ;; esac
  # A newline starts a second command.
  case "$COMMAND" in *"
"*) FASTPATH_OK=false ;; esac

  # (4)+(5) Blank out every WHOLE safe substitution, strip the only redirections
  # we trust — fd duplications (2>&1, >&2) and /dev/null-family targets, same
  # set as step 2c-bis — then reject if ANY control operator, parenthesis,
  # further redirection, or leftover `$` survives. An unsafe or nested
  # substitution cannot be blanked out, so its `$(` is exactly what trips this.
  # Fail closed if sed errors.
  RESIDUE=$(printf '%s' "$COMMAND" | sed -E "s#${SAFE_SUB_RE}#__SAFE_SUB__#g") || FASTPATH_OK=false
  RESIDUE=$(printf '%s' "$RESIDUE" \
    | sed -E 's/[0-9]?>&[0-9]//g' \
    | sed -E 's#[0-9]?(>>?|<)[[:space:]]*/dev/(null|zero|stdout|stderr|tty|full|fd/[0-9]+)##g') || FASTPATH_OK=false
  printf '%s' "$RESIDUE" | grep -qE '[;|&`()<>$]' 2>/dev/null && FASTPATH_OK=false

  if [ "$FASTPATH_OK" = true ]; then
    _log "allow-fastpath"
    jq -n '{
      hookSpecificOutput: {
        hookEventName: "PreToolUse",
        permissionDecision: "allow"
      }
    }'
    exit 0
  fi
fi

# ── Step 2c-bis: Trusted plugin-script fast-path ──────────────────────────────
# Any plugin's own scripts under $CLAUDE_PLUGIN_ROOT/scripts (the convention
# scout's own scripts/session-context.sh follows) are vetted, version-controlled
# code, and CLAUDE_PLUGIN_ROOT is set by Claude Code — not the model. Neutralize
# that one trusted token in a copy of the command, then apply the SAME
# expansion-safety test as step 2c. If locating a trusted script was the *only*
# thing that made the command "expanding", it is safe to fast-path. Deny-floor
# (step 2) has already removed destructive commands. This is a pure text match —
# it does not depend on CLAUDE_PLUGIN_ROOT actually being set.
#
# Strict, because deny-floor does NOT block a plain `rm file` chained after the
# script: the fast-path requires a SINGLE simple command — the trusted script
# with literal/word arguments, at most fd-duplication (2>&1) or /dev/null-family
# redirection. Any command separator (; | & && ||), subshell, backtick, real-
# file redirection, leftover $VAR, glob, or newline falls through to the LLM
# (step 3), which now sees the resolved path via known_vars anyway.
TRUSTED_SCRIPT_RE='"?(\$\{CLAUDE_PLUGIN_ROOT(:-[^}]*)?\}|\$CLAUDE_PLUGIN_ROOT)/scripts/[A-Za-z0-9._-]+\.sh"?'
SANITIZED=$(printf '%s' "$COMMAND" | sed -E "s#${TRUSTED_SCRIPT_RE}#__TRUSTED_SCRIPT__#g") || SANITIZED="$COMMAND"

if [ "$SANITIZED" != "$COMMAND" ]; then
  FASTPATH_OK=true
  # (a) Single line only — a newline starts a second command.
  case "$SANITIZED" in *"
"*) FASTPATH_OK=false ;; esac
  # (b) No leftover variable reference or glob (unknowable / blast radius).
  printf '%s' "$SANITIZED" | grep -qE '\$\{|\$[A-Za-z_]|\*|\?|\[' 2>/dev/null && FASTPATH_OK=false
  # (c) Strip the only redirections we trust — fd duplications (2>&1, >&2) and
  #     /dev/null-family targets — then reject if ANY control operator or
  #     further redirection survives: ; | & ` ( ) < > or $( all mean the line is
  #     more than a lone trusted-script invocation. Fail closed if sed errors.
  SAFE_RED=$(printf '%s' "$SANITIZED" \
    | sed -E 's/[0-9]?>&[0-9]//g' \
    | sed -E 's#[0-9]?(>>?|<)[[:space:]]*/dev/(null|zero|stdout|stderr|tty|full|fd/[0-9]+)##g') || FASTPATH_OK=false
  printf '%s' "$SAFE_RED" | grep -qE '[;|&`()<>]|\$\(' 2>/dev/null && FASTPATH_OK=false
  # (d) The trusted script must be the COMMAND, not an argument — the token must
  #     be the first word. Otherwise a write-capable command the deny-floor does
  #     not cover (cp, mv, install, tee …) targeting a plugin-script path would
  #     fast-path and silently overwrite vetted code, e.g.
  #     `cp /tmp/evil.sh "$CLAUDE_PLUGIN_ROOT/scripts/x.sh"`.
  LEAD="${SANITIZED#"${SANITIZED%%[![:space:]]*}"}"  # strip leading whitespace
  case "$LEAD" in
    __TRUSTED_SCRIPT__ | __TRUSTED_SCRIPT__[!A-Za-z0-9_]*) : ;;  # token is the executable
    *) FASTPATH_OK=false ;;
  esac

  if [ "$FASTPATH_OK" = true ]; then
    _log "allow-fastpath" reason "trusted plugin script"
    jq -n '{
      hookSpecificOutput: {
        hookEventName: "PreToolUse",
        permissionDecision: "allow"
      }
    }'
    exit 0
  fi
fi

# ── Step 3: Call local LLM via the scout binary ───────────────────────────────
# Fail-open: a missing scout binary just skips the auto-approve optimization —
# this hook never blocks, so there is no bricking risk here (contrast
# prefer-local-llm.sh, which denies and therefore needs its own reachability
# check before doing so).
if [ ! -x "$SCOUT_BIN" ]; then
  _log "missing-binary"
  exit 0
fi

# Gather conditional context: dir_listing for glob commands, git_status for
# destructive-looking commands. Keep it minimal — irrelevant context degrades
# classifier accuracy and adds injection surface.
DIR_LISTING_ARG=""
GIT_STATUS_ARG=""

if printf '%s' "$COMMAND" | grep -qE '\*|\?|\[' 2>/dev/null; then
  LISTING=$(ls -1 "$CWD" 2>/dev/null | head -50) || LISTING=""
  [ -n "$LISTING" ] && DIR_LISTING_ARG="Dir contents:
$LISTING
"
fi

if printf '%s' "$COMMAND" | grep -qE '\brm\b|\bmv\b|\bgit\s+(reset|checkout|restore|clean)\b' 2>/dev/null; then
  GIT_ST=$(git -C "$CWD" status --short 2>/dev/null | head -20) || GIT_ST=""
  [ -n "$GIT_ST" ] && GIT_STATUS_ARG="Git status:
$GIT_ST
"
fi

# Resolve ONLY the variables this command actually references, from the hook's
# own live environment — it shares Claude Code's environment, so the values
# (CLAUDE_PLUGIN_ROOT, HOME, …) are exactly what the command will see. Targeted,
# never a static list: vars the command does not name are never resolved, and
# unset/unexported ones are omitted so they stay genuinely unknowable. Pure
# name lookup via printenv — no eval, no $() expansion, no injection surface.
KNOWN_VARS_ARG=""
VAR_NAMES=$(printf '%s' "$COMMAND" | grep -oE '\$\{?[A-Za-z_][A-Za-z0-9_]*' 2>/dev/null \
  | sed -E 's/^\$\{?//' | sort -u) || VAR_NAMES=""
if [ -n "$VAR_NAMES" ]; then
  RESOLVED=""
  while IFS= read -r vname; do
    [ -z "$vname" ] && continue
    vval=$(printenv "$vname" 2>/dev/null) || continue  # unset/unexported → skip
    # Never leak credential-looking values into the classifier's input (and thus
    # its process/logs). Redact by name; the LLM then treats the value as
    # unknowable and leans "ask", which is the safe outcome.
    # tr, not ${vname^^}: stock macOS bash is 3.2, where ^^ is a fatal
    # "bad substitution" under set -e — the hook would die right here.
    vname_uc=$(printf '%s' "$vname" | tr '[:lower:]' '[:upper:]') || vname_uc="$vname"
    case "$vname_uc" in
      *KEY* | *TOKEN* | *SECRET* | *PASSWORD* | *PASSWD* | *CREDENTIAL* | *AUTH*)
        vval="<redacted>" ;;
    esac
    RESOLVED="${RESOLVED}${vname}=${vval}
"
  done <<< "$VAR_NAMES"
  [ -n "$RESOLVED" ] && KNOWN_VARS_ARG="Known variables (resolved from the live environment):
${RESOLVED}"
fi

# Identify this traffic as a hook's in scout's call log (docs/dashboard.md §3
# `via`): `scout run` is reached both from a shell and from here, and only the
# caller knows which. Exported rather than prefixed onto the command, because
# _timeout is a shell function and an assignment prefix on one does not
# reliably scope to it.
export SCOUT_VIA=hook

LLM_OUTPUT=$(_timeout "$SCOUT_BIN" run \
  --preset shell_safety \
  --arg "command=$COMMAND" \
  --arg "cwd=$CWD" \
  --arg "known_vars=$KNOWN_VARS_ARG" \
  --arg "dir_listing=$DIR_LISTING_ARG" \
  --arg "git_status=$GIT_STATUS_ARG" \
  2>/dev/null) || LLM_OUTPUT=""

VERDICT=$(printf '%s' "$LLM_OUTPUT" | jq -r '.verdict // empty' 2>/dev/null) || VERDICT=""
REASON=$(printf '%s' "$LLM_OUTPUT" | jq -r '.reason // ""' 2>/dev/null) || REASON=""

# Retry through _unwrap_json when the reply did not parse as bare JSON. Logged
# as `unwrapped` so the audit trail distinguishes a clean reply from a rescued
# one — if that field is never true, the model has stopped fencing and this
# fallback can go.
UNWRAPPED=false
if [ -z "$VERDICT" ] && [ -n "$LLM_OUTPUT" ]; then
  UNWRAPPED_JSON=$(_unwrap_json "$LLM_OUTPUT")
  if [ -n "$UNWRAPPED_JSON" ]; then
    VERDICT=$(printf '%s' "$UNWRAPPED_JSON" | jq -r '.verdict // empty' 2>/dev/null) || VERDICT=""
    REASON=$(printf '%s' "$UNWRAPPED_JSON" | jq -r '.reason // ""' 2>/dev/null) || REASON=""
    [ -n "$VERDICT" ] && UNWRAPPED=true
  fi
fi

case "$VERDICT" in
  allow)
    # Shadow mode (see header): classify and log as usual, withhold the allow.
    if [ -n "${SCOUT_SHELL_SAFETY_SHADOW:-}" ] ||
       [ -f "${HOME}/.claude/scout-shell-safety.shadow" ]; then
      _log "allow-shadow" reason "$REASON" unwrapped "$UNWRAPPED"
      exit 0  # emit nothing → harness decides on its own
    fi
    _log "allow" reason "$REASON" unwrapped "$UNWRAPPED"
    jq -n '{
      hookSpecificOutput: {
        hookEventName: "PreToolUse",
        permissionDecision: "allow"
      }
    }'
    ;;
  ask|deny)
    _log "$VERDICT" reason "$REASON" unwrapped "$UNWRAPPED"
    exit 0  # emit nothing → normal prompt
    ;;
  *)
    # Empty, malformed JSON, or unexpected value → fail-to-ask
    _log "parse-failure" reason "${LLM_OUTPUT:0:120}"
    exit 0
    ;;
esac

exit 0
