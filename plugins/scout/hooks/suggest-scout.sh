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
#   Grep  — the native tool, same rule expressed in its schema: output_mode
#           "content" (the only mode that emits match text) with no head_limit
#           and no single-file path. The default mode returns paths only and is
#           already narrow, so it stays silent.
#   Glob  — only a genuinely unbounded pattern, i.e. one whose basename is a
#           bare wildcard (`**/*`, `src/*`). `**/*.rs` constrains the result set
#           by extension and is usually exactly what was wanted.
#
# THERE IS NO THROTTLE. There was one — a per-kind 600s stamp — and it was
# removed deliberately: it is the wrong lever. The trigger conditions above are
# what keep this quiet, and they are evaluated per call against the actual cost
# of the actual call. A time window instead suppresses the *second* expensive
# search in a burst, which is the one most likely to be the one that floods
# context. Narrow triggers, no clock.
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

# Fail-open on missing HOME (unusual CI environments, test harnesses). The
# binary-resolution fallback below expands $HOME under `set -u`, so this guard
# has to stay even though the throttle that first motivated it is gone.
[ -z "${HOME:-}" ] && exit 0

command -v jq >/dev/null 2>&1 || exit 0

READ_MIN_BYTES="${SCOUT_SUGGEST_MIN_BYTES:-51200}"

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

_emit() {
  jq -n --arg ctx "$1" \
    '{hookSpecificOutput:{hookEventName:"PreToolUse",additionalContext:$ctx}}'
  exit 0
}

# Shared by the Bash and Grep branches — same situation, two spellings of it.
GREP_NUDGE="This is a broad, uncapped search. If the raw match set will be much larger than what you actually want, scout's grep(pattern, intent) runs the search locally and returns only the hits matching your stated intent — the rest never enters this conversation.

  grep(pattern=\"<pattern>\", intent=\"<what you are looking for>\")

Unqualified MCP name; run ToolSearch for \"grep\" if it is not in your loaded toolset. A plain search is still right when you expect few hits, or want every one of them."

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

    _emit "$GREP_NUDGE"
    ;;

  Grep)
    # The native tool is where Claude Code actually searches — it almost never
    # shells out to grep, so the Bash branch above is close to dead weight on
    # this host and load-bearing only on one that has no first-class search
    # tool. Same rule, restated against this schema.

    # output_mode defaults to files_with_matches: paths only, already narrow.
    # "content" is the one mode that streams match text into the transcript.
    MODE=$(printf '%s' "$INPUT" | jq -r '.tool_input.output_mode // "files_with_matches"' 2>/dev/null) || exit 0
    [ "$MODE" = "content" ] || exit 0

    # head_limit is this schema's spelling of `| head` — an explicit cap.
    HEAD_LIMIT=$(printf '%s' "$INPUT" | jq -r '.tool_input.head_limit // empty' 2>/dev/null) || exit 0
    [ -n "$HEAD_LIMIT" ] && exit 0

    # A path naming one regular file is the `grep TODO src/main.rs` case: the
    # search is already bounded by the file, so stay quiet. A directory path
    # bounds nothing in particular — Grep recurses.
    GREP_PATH=$(printf '%s' "$INPUT" | jq -r '.tool_input.path // empty' 2>/dev/null) || exit 0
    [ -n "$GREP_PATH" ] && [ -f "$GREP_PATH" ] && exit 0

    _emit "$GREP_NUDGE"
    ;;

  Glob)
    PATTERN=$(printf '%s' "$INPUT" | jq -r '.tool_input.pattern // empty' 2>/dev/null) || exit 0
    [ -n "$PATTERN" ] || exit 0

    # Glob returns paths, not content, so it is a far weaker case than Grep and
    # the bar is correspondingly higher: fire only when the basename is a bare
    # wildcard and the pattern therefore constrains nothing. `**/*.rs` and
    # `**/test_*.py` have already bounded their own result sets by extension or
    # stem, and enumerating a tree is often exactly the point.
    case "${PATTERN##*/}" in
      '*'|'**'|'*.*') ;;
      *) exit 0 ;;
    esac

    _emit "This glob constrains nothing but the directory — it will return every file beneath it. If you want files by content or by role rather than the whole listing, scout can narrow it locally without the full list entering this conversation:

  grep(pattern=\"<pattern>\", intent=\"<what you are looking for>\")   — content hits, intent-filtered
  wrap(command=\"<find …>\", question=\"<what you want to know>\")     — runs the listing, answers the question

Unqualified MCP names; run ToolSearch for \"grep\" or \"wrap\" if they are not in your loaded toolset. A plain glob is still right when you want the full file list, or are getting oriented in an unfamiliar tree."
    ;;
esac

exit 0
