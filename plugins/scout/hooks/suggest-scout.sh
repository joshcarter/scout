#!/usr/bin/env bash
# suggest-scout.sh — PreToolUse hook: advisory nudge toward scout's extract/grep
# when a Read or a shell search is about to pull a lot of text into context.
#
# WHY THIS EXISTS, separate from prefer-local-llm.sh:
#
#   check_output was the only scout tool that ever got called, and the reason
#   was structural rather than editorial: it is the only one with a hook. The
#   guidance for extract and grep shipped once, at SessionStart, thousands of
#   tokens before the moment it applied — while any competing code-intelligence
#   plugin nudges at the tool call itself. Just-in-time beats session-start, so
#   extract and grep lost a decision they were never present for.
#
#   This hook puts scout's guidance where that decision is made. It is NOT a
#   redirect: prefer-local-llm.sh denies because raw build output flooding
#   context is a failure mode, whereas reading a large file or running a broad
#   grep is frequently the correct call. So this one only ever adds
#   additionalContext — never a permissionDecision of any kind, allow included.
#   Mixing that into prefer-local-llm.sh would have muddied its invariants,
#   which turn on it being the file that denies.
#
# ANTI-NAG IS THE DESIGN CONSTRAINT. A hook that fires on every Read teaches the
# model to skim past it, and routing every search through a local model is a
# loss — scout's own skill says so. The trigger conditions below are therefore
# deliberately narrow, and each one is a case where scout plausibly wins:
#
#   Read  — file is at least READ_MIN_BYTES (default 50 KB). Below that,
#           reading it whole is cheaper than a model round-trip. Binary
#           extensions are skipped outright: extract cannot help there.
#   Bash  — a *broad* search (`grep -r`, or `rg`, which recurses by default)
#           with no output cap (-l, -c, -m, --files-with-matches, | head,
#           | wc). A capped or single-file search is already narrow, which is
#           exactly when a plain grep is the right tool.
#
#   On top of that, a per-kind throttle (SCOUT_SUGGEST_THROTTLE_SECS, default
#   600) keeps a burst of large reads from producing a burst of identical
#   nudges. Set it to 0 to disable — the shell suite does.
#
# Deliberately NOT checked: whether the local-LLM endpoint responds. That ping
# is an HTTP round-trip, and this hook runs on every Read in the session;
# prefer-local-llm.sh can afford it because it runs only after a build verb
# matched AND it is about to deny into a dead end if the target is down. Here
# the worst case is far cheaper: the model calls extract, gets an error back,
# and falls back to Read. Costing every Read a network ping to avert that is a
# bad trade. The binary's existence IS checked — a stat, not a round-trip.
#
# Hard invariants:
#   1. Never emits a permissionDecision. This hook cannot block, cannot allow,
#      and cannot change what runs. Its entire output surface is
#      additionalContext.
#   2. Fail-open and silent on any error. Malformed payload, missing jq,
#      unreadable file, missing binary → exit 0 with no output.
#   3. No fully-qualified MCP tool name in the nudge text (see CLAUDE.md). The
#      mcp__plugin_<plugin>_<server>__ prefix is derived, not declared, and a
#      stale literal points the model at a tool that does not exist. Name the
#      unqualified tool and point at ToolSearch.

set -euo pipefail

# Fail-open on missing HOME (unusual CI environments, test harnesses).
[ -z "${HOME:-}" ] && exit 0

command -v jq >/dev/null 2>&1 || exit 0

READ_MIN_BYTES="${SCOUT_SUGGEST_MIN_BYTES:-51200}"
THROTTLE_SECS="${SCOUT_SUGGEST_THROTTLE_SECS:-600}"
STAMP_DIR="${SCOUT_SUGGEST_STAMP_DIR:-$HOME/.claude/scout-suggest}"

# ── Resolve the scout binary ─────────────────────────────────────────────────
# Payload first, then the legacy data dir, then PATH. See the fuller comment on
# the identical block in prefer-local-llm.sh for why each entry is there and why
# this is duplicated rather than sourced. Keep the three copies byte-identical.
SCOUT_BIN="${CLAUDE_PLUGIN_ROOT:+${CLAUDE_PLUGIN_ROOT}/bin/scout}"
[ -x "$SCOUT_BIN" ] || SCOUT_BIN="${CLAUDE_PLUGIN_DATA:-$HOME/.claude/plugins/data/scout-scout}/bin/scout"
[ -x "$SCOUT_BIN" ] || SCOUT_BIN="$(command -v scout 2>/dev/null || true)"
[ -x "$SCOUT_BIN" ] || exit 0

# Extensions extract cannot do anything useful with. Text-shaped logs (.log,
# .jsonl, .csv) are deliberately absent — they are among its best targets.
BINARY_RE='\.(png|jpe?g|gif|webp|ico|pdf|zip|gz|bz2|xz|zst|tar|tgz|wasm|bin|so|dylib|dll|exe|o|a|rlib|class|jar|woff2?|ttf|otf|mp[34]|mov|avi|sqlite3?|db)$'

# ── Throttle ──────────────────────────────────────────────────────────────────
# One stamp per nudge kind. Returns 0 (fire) or 1 (suppress). Any failure to
# read or write the stamp fires the nudge — the throttle is a politeness
# optimization, never a correctness gate.
_should_fire() {
  local kind="$1" stamp now last
  [ "$THROTTLE_SECS" -le 0 ] 2>/dev/null && return 0

  stamp="$STAMP_DIR/$kind"
  now=$(date +%s 2>/dev/null) || return 0
  mkdir -p "$STAMP_DIR" 2>/dev/null || return 0

  if [ -f "$stamp" ]; then
    last=$(cat "$stamp" 2>/dev/null) || return 0
    case "$last" in
      ''|*[!0-9]*) last=0 ;;
    esac
    [ $((now - last)) -lt "$THROTTLE_SECS" ] && return 1
  fi

  printf '%s' "$now" > "$stamp" 2>/dev/null || true
  return 0
}

_emit() {
  jq -n --arg ctx "$1" \
    '{hookSpecificOutput:{hookEventName:"PreToolUse",additionalContext:$ctx}}'
  exit 0
}

# ── Parse PreToolUse payload ──────────────────────────────────────────────────
INPUT=$(cat)
TOOL_NAME=$(printf '%s' "$INPUT" | jq -r '.tool_name // empty' 2>/dev/null) || exit 0

case "$TOOL_NAME" in
  Read)
    FILE=$(printf '%s' "$INPUT" | jq -r '.tool_input.file_path // empty' 2>/dev/null) || exit 0
    [ -n "$FILE" ] || exit 0
    [ -f "$FILE" ] || exit 0

    printf '%s' "$FILE" | grep -qiE "$BINARY_RE" && exit 0

    SIZE=$(wc -c < "$FILE" 2>/dev/null | tr -d ' ') || exit 0
    case "$SIZE" in
      ''|*[!0-9]*) exit 0 ;;
    esac
    [ "$SIZE" -ge "$READ_MIN_BYTES" ] || exit 0

    _should_fire read || exit 0

    KB=$((SIZE / 1024))
    _emit "This file is ~${KB}KB. If you need one specific thing from it rather than the whole text, scout's extract(file, question) answers the question against a local model and returns just the answer — the file never enters this conversation.

  extract(file=\"$FILE\", question=\"<your question>\")

Unqualified MCP name; run ToolSearch for \"extract\" if it is not in your loaded toolset. Reading it whole is still right when you need the full text, or need exact line numbers to edit against."
    ;;

  Bash|run_terminal_command)
    COMMAND=$(printf '%s' "$INPUT" | jq -r '.tool_input.command // empty' 2>/dev/null) || exit 0
    [ -n "$COMMAND" ] || exit 0

    # Breadth signal: a recursive grep, or rg/ag, which recurse by default.
    # Advisory-only output means a false positive here costs a few tokens, so
    # a plain regex is proportionate — no lexer, unlike prefer-local-llm.sh.
    #
    # The [[:alnum:]]* on BOTH sides of [rR] is load-bearing: r is rarely the
    # last letter of the cluster in practice (`grep -rn`, `grep -ri`), and an
    # earlier version anchored it at the end and silently missed all of those.
    printf '%s' "$COMMAND" \
      | grep -qE '(^|[;&|(]|&&|\|\|)[[:space:]]*(grep[[:space:]]+(-[[:alnum:]]*[rR][[:alnum:]]*|--recursive)|rg|ag)([[:space:]]|$)' 2>/dev/null || exit 0

    # Already capped → already narrow → plain search is correct, stay quiet.
    printf '%s' "$COMMAND" \
      | grep -qE '(\|[[:space:]]*(head|tail|wc)|[[:space:]]-[[:alnum:]]*[lcm]([[:space:]]|$)|--count|--files-with-matches|--max-count)' 2>/dev/null && exit 0

    _should_fire grep || exit 0

    _emit "This is a broad, uncapped search. If the raw match set will be much larger than what you actually want, scout's grep(pattern, intent) runs the search locally and returns only the hits matching your stated intent — the rest never enters this conversation.

  grep(pattern=\"<pattern>\", intent=\"<what you are looking for>\")

Unqualified MCP name; run ToolSearch for \"grep\" if it is not in your loaded toolset. A plain search is still right when you expect few hits, or want every one of them."
    ;;
esac

exit 0
