# Spec: Terminal UX for `scout grep` (+ `scout find`, `scout edit`)

**Status:** draft — no open questions remain; §9 records the decisions.

**Goal:** when a human runs `scout grep` in a terminal, the output should
read like `ack`: per-file hits with line numbers and a little context,
colored, safe against minified-JSON bombs — plus scout's unique addition,
a one-line "why this hit" note from the local model. A companion verb
gets the results into `$EDITOR` with minimal keystrokes.

**Non-goals:** no change to the MCP tool surface or its JSON payload
shapes (Claude's contract stays frozen); no interactive TUI (no fzf-style
picker beyond a numbered prompt); no pager integration in v1.

---

## 1. Output mode selection

Today `run_filter` in `main.rs` prints pretty JSON unconditionally. New
behavior:

| Condition | Format |
|---|---|
| stdout is a TTY | human text, colored |
| stdout is not a TTY (pipe/redirect) | human text, no color |
| `--format json` | current JSON payload, pretty-printed |
| `--format vimgrep` | `file:line:col: text` per hit (quickfix-compatible; `col` is 1 until P3 lands real match columns) |

- Detection via `std::io::IsTerminal` (stdlib, no new dep).
- Color: `--color auto|always|never` (default `auto`), honor `NO_COLOR`.
- **Piped output stays human text, not JSON** (matches rg/ack/grep:
  `scout grep foo | head` should not explode into JSON). Scripts that
  want structure opt in with `--format json`. The MCP server is
  unaffected — it never goes through the CLI renderer.
- `extract` and `check` get the same `--format` flag later; this spec
  covers `grep` only, but the renderer lives in a shared module
  (`src/render.rs`) so extending is mechanical.

### Exit codes (grep convention)

- `0` — at least one hit returned
- `1` — no hits (including `none_relevant`: the model judged all hits
  irrelevant — the message on stderr distinguishes the two)
- `2` — error (bad pattern, LLM failure with no bypass, etc.)

## 2. Human output format

Rerank mode (LLM involved — hit has `why` and `score`):

```
src/select.rs:212 · validates keep-ids against the batch range
  210 │     let sel = validate_keeps(&v, first_id..=last);
  211 │     dropped_invalid += sel.dropped_invalid;
▶ 212 │     none_relevant &= sel.none_relevant;
  213 │     keeps.extend(sel.keeps);
  214 │ }

src/grep.rs:116 · merges selector output across batches
  ...
```

- Header line: `path:line` (colored: path magenta, line green — ack's
  scheme), then the model's `why` in default text. The 1–5 score is
  **not** shown — it still exists internally (it orders the hits and
  stays in the JSON payload), it's just noise for a human. If marginal
  hits ever leak through, a `[grep] min_score` floor (drop keeps scored
  below N) is the cheap knob to add.
- Context block: the existing ±`context_lines` block, gutter-numbered.
  Matched line marked with `▶` and its line bolded; the matched *pattern*
  within the line highlighted (see §4 on match offsets).
- Blank line between hits. One header per hit even when the same file
  appears twice (simpler than ack's file-grouping; revisit if noisy).
- Bypass mode (`mode: "full"`, ≤ `bypass_max_hits` hits): same layout,
  no `why`/`score` segment.

### Status lines (stderr, never stdout)

Humans need the metadata the JSON payload carries; scripts must not have
to parse it out of stdout:

- Before the LLM call: `filtering 214 hits with <model>…` — the rerank
  takes seconds and silence looks like a hang.
- After: `12 of 214 hits kept · 202 filtered by intent` and, when
  applicable, `search truncated at 2000 hits` / `capped at top 10
  (--max-hits)`.
- `none_relevant`: `local model judged none of the 214 hits relevant —
  rerun with --no-filter to see all of them` (exit 1).

## 3. New search filters

All implemented in `source.rs` via the `ignore` crate we already depend
on, and **exposed through the MCP tool schema too** — Claude benefits
from type/dir filtering exactly as much as a human does. (MCP additions
are new optional fields; existing fields unchanged.)

| Flag | Meaning | Implementation |
|---|---|---|
| `-t, --type TYPE` (repeatable) | only these file types | `ignore::types::TypesBuilder::add_defaults()` — ripgrep's full built-in type list (`rust`, `js`, `json`, …), `select()` |
| `-T, --type-not TYPE` (repeatable) | exclude these types | `negate()` |
| `--type-list` | print known types and exit | walk the defaults |
| `-g, --glob GLOB` (repeatable) | include/exclude by glob, `!` negates (rg-compatible) | `ignore::overrides::OverrideBuilder` |
| `--dir PATH` / `--exclude-dir PATH` | friendlier spelling for directory scoping | sugar over `-g 'PATH/**'` / `-g '!PATH/**'` |
| `-C, --context N` | context lines override | existing `context_lines` |
| `-n, --max-hits N` | cap results (already exists as `--max-hits`; add `-n` short) | existing; CLI default raised to 20. This is a **ceiling, not a quota** — the model returns only hits it judges relevant, so results routinely stop short of the cap |
| `--no-filter` | skip the LLM rerank entirely — pure structured search | bypass path |

Notes:

- `.gitignore`/hidden filtering already handles most build products;
  `--exclude-dir` is for the cases it doesn't (e.g. checked-in fixtures).
- `SearchOptions` grows `types: Option<ignore::types::Types>` and
  `overrides: Option<ignore::overrides::Override>`, threaded into
  `WalkBuilder`.

## 4. The minified-JSON problem

**Precedent:** ripgrep is the tool that solved this. `rg -M 200`
(`--max-columns`) replaces over-long matched lines with
`[Omitted long matching line]`, and `--max-columns-preview` shows the
first N columns plus `[... N more matches]`. ack and ag have no
equivalent — this is a known ack pain point, and rg's approach is the
one to copy. (Non-grep precedent, same idea: `jless`/`fx` exist because
even *viewing* minified JSON needs structure-aware truncation.)

scout's plan, two layers:

1. **Search layer (already exists, tighten the story):**
   `context_max_bytes` (default 2000) already caps a rendered context
   block, so a minified 5 MB line can contribute at most ~2 KB to any
   payload — MCP context pollution is already bounded. Keep it.
2. **Render layer (new): per-line column cap.** Default
   `--max-columns 150` (config: `[cli] max_columns`). A line over the
   cap renders as a *window around the match* with `…` ellipses on the
   truncated side(s):

   ```
   ▶ 1 │ …"retry":3,"endpoint":"http://localhost:11434/v1","timeout"… [line is 48,213 columns]
   ```

   Windowing around the match (rather than rg's show-the-start preview)
   requires knowing the match's **column offset**, which `source.rs`
   currently discards — the UTF8 sink records only line numbers. Capture
   it: `SearchHit` gains `col: usize` (byte offset of first match in the
   line, from `Matcher::find` on the matched line). This also unlocks
   in-line match highlighting (§2) and `--format vimgrep`'s column field.
   Context lines (non-matching neighbors) over the cap simply truncate
   at the cap with `…` — no window needed.

The cap applies to human *and* JSON CLI output? **No** — JSON output
returns the payload as-is (already bounded by `context_max_bytes`);
`max_columns` is a terminal-rendering concern only.

## 5. `scout find` — intent-only search

`scout grep` requires the caller to guess a pattern first. `scout find`
removes that step: the caller states what they want, and the *local
model* guesses the patterns.

```
scout find "<question>" [same filter flags as grep]
```

e.g. `scout find "where are the config file options parsed?"`

### Why not a text index

Index-based conceptual search (e.g. `ct search`) answers this class of
query with high recall and terrible precision — pages of candidates,
nearly all irrelevant, because a text index can retrieve but cannot
*judge*. scout's bet is the inverse: retrieval stays dumb (grep), and
the LLM does the one thing an index can't — read each hit in context
and decide whether it actually serves the question. `find` is therefore
not a shallow substitute for indexed search; on this query shape it is
the higher-precision tool. It also needs no index, daemon, or
enrollment — it works in any directory.

### Pipeline

The LLM never searches. It has exactly two jobs, both token-bounded:
guess patterns, judge hits.

1. **Pattern synthesis (one small LLM call).** New preset
   (`find_patterns.toml`): input is the question plus a cheap structural
   sketch of the project (the file tree, truncated to a byte cap);
   output is 3–8 candidate patterns, each optionally tagged with type
   (`rust`) or glob hints. Prompt is a few KB — negligible.
2. **Mechanical search — no LLM.** Run every candidate through
   `source::search`, union the results, dedupe by `(file, line)`. Then
   the **degenerate-pattern guard**: a candidate with 0 hits is dropped
   silently; a candidate with more than `degenerate_hit_cap` hits
   (default 300) is a bad discriminator — the moral equivalent of low
   IDF, e.g. `parse` in a parser — and its hits are dropped before the
   model ever sees them. Survivors flow into the existing
   `max_considered` / `batch_size` caps, so the rerank stage's token
   budget is untouched.
3. **Rerank — the existing stage, unchanged.** The union list is
   noisier than a single-pattern list; filtering noise against an
   intent is exactly what this stage is for.
4. **Guess-again cycle (bounded).** If *every* candidate whiffed (0
   usable hits after the guard), re-ask the pattern preset, telling it
   which patterns produced nothing. Attempts are capped by
   `[find] max_attempts` (default 2; 1 = no retry), overridable with
   `--attempts N`. Local-model latency makes more than 2–3 rounds
   unpleasant at a terminal anyway.

### UX

- Output, exit codes, and filter flags (`-t`, `-g`, `-C`, `-n`, …) are
  identical to `scout grep` — same renderer, same payload shape.
- stderr shows the guesses, for trust and for manual fallback:
  `trying: config, toml, load_config, from_str · 2 whiffed`, then
  the normal rerank status line. If all attempts whiff:
  `no pattern guess produced hits after 2 attempts — try scout grep
  with an explicit pattern` (exit 1).
- Requires a configured LLM (unlike `grep`, which degrades to pure
  search); a missing/broken config fails with the same message shape
  as the rerank path, naming `scout grep <pattern>` as the fallback.

## 6. `scout edit` — results into `$EDITOR`

A separate verb, not a flag: the contract differs (it must *end* in an
editor, and interactivity is acceptable). It front-ends **both**
pipelines, disambiguated by arity — no sentence-detection heuristics:

```
scout edit <question>              # one positional → find pipeline
scout edit <pattern> <intent>      # two positionals → grep pipeline
scout edit -p <pattern>            # pattern-only grep (no rerank)
```

Flow:

1. Run the matching pipeline (`find` or `grep`).
2. **One hit** → exec `$EDITOR` positioned at it.
3. **Multiple hits** → print the normal human output *numbered*, prompt
   `edit which? [1-12, a=all, q=quit]` on the TTY:
   - a number → open that hit at its line
   - `a` → open all hit files (positioned at first hit; vim-family gets
     a quickfix list, see below)
4. **Zero hits** → same message as grep, exit 1.

When the editor exits, scout exits — no re-prompt loop; rerunning is
cheap.

Editor positioning is per-editor; detect by `$EDITOR` basename:

| Editor | Invocation |
|---|---|
| vi/vim/nvim | `$EDITOR +<line> <file>`; for `a`: write a temp file of `file:line:col: text` lines and run `$EDITOR -q <tmpfile>` (quickfix; precedent: `git jump`, `rg --vimgrep \| vim -q -`) |
| emacs/emacsclient | `$EDITOR +<line>:<col> <file>` |
| hx (helix) | `hx <file>:<line>:<col>` |
| code/codium/cursor | `$EDITOR -g <file>:<line>:<col>` |
| zed | `zed <file>:<line>:<col>` |
| unknown | `$EDITOR <file>` and print `at line <N>` beforehand |

For plain-shell composition without `scout edit`, `--format vimgrep`
already enables `vim -q <(scout grep --format vimgrep …)`.

## 7. Config additions

New `[cli]` and `[find]` tables in `config.toml` (all optional, parsed
leniently like `[grep]`/`[extract]`):

```toml
[cli]
max_columns = 150      # per-line render cap (0 = unlimited)
color       = "auto"   # auto | always | never
context     = 2        # default -C for terminal use (falls back to [grep] context_lines)
max_hits    = 20       # default result cap for terminal invocations (ceiling, not quota)

[find]
max_attempts       = 2    # pattern-guess rounds before giving up (--attempts overrides)
max_patterns       = 8    # candidates requested per round
degenerate_hit_cap = 300  # a pattern matching more lines than this is discarded
tree_max_bytes     = 8192 # cap on the file-tree sketch sent to the pattern preset
```

`$EDITOR` is respected as-is; no editor config key unless someone needs
`$SCOUT_EDITOR`.

## 8. Implementation shape

- `src/render.rs` — new: payload → styled text. Pure function from the
  existing JSON payload (`mode`, `hits[]`, counters) + a `RenderOpts`
  (width cap, color, TTY) → `String`. Unit-testable with fixed payloads;
  color via raw ANSI or the tiny `anstyle`/`owo-colors` (pick one, avoid
  a heavy dep).
- `main.rs::run_filter` — grow a `format` parameter; grep/edit route
  through the renderer, `--format json` keeps today's path.
- `source.rs` — `SearchOptions` grows types/overrides/col capture;
  `SearchHit` grows `col`.
- `src/find.rs` — new: pattern-synthesis call, multi-pattern
  union/dedupe, degenerate-pattern guard, retry loop; hands the merged
  hit list to the existing rerank + renderer. New preset
  `presets/find_patterns.toml`.
- `src/edit.rs` — new: arity dispatch (find vs grep pipeline), picker,
  editor dispatch table.
- MCP schema (`mcp_server.rs`) — add optional `types`, `types_not`,
  `globs` fields to the grep tool; plumb through unchanged pipeline.

Phasing:

1. **P1 — renderer + TTY detection + exit codes + `--format`.** Biggest
   daily-use win; no search-layer changes.
2. **P2 — type/glob filters** (CLI + MCP) and `--no-filter`.
3. **P3 — match columns + max_columns windowing + highlighting.**
4. **P4 — `scout find`** (pattern synthesis + guard + retry cycle).
5. **P5 — `scout edit`** (depends on both pipelines existing).

## 9. Decisions

Resolved 2026-08-07:

- **Flag dialect:** rg-compatible (`-t`, `-g`, `-M`); rg's type names
  ship in the `ignore` crate anyway.
- **`max_columns` default:** 150.
- **Same-file hit grouping:** none — one header per hit.
- **`scout edit` after the editor exits:** scout exits; no re-prompt
  loop.
- **`--max-hits` terminal default:** 20. Safe because the cap is a
  ceiling, not a quota — the rerank returns only hits the model
  affirmatively kept, never padding to the cap. If marginal keeps ever
  read as cruft, add a `[grep] min_score` floor.
- **Score display:** dropped from human output; scores remain internal
  (ordering) and in the JSON payload.
- **`--format vimgrep`:** kept, and in P1 — it is the same
  `file:line:col:` formatter `scout edit`'s `vim -q` path needs anyway;
  emits `col` 1 until P3.
- **`find` over MCP:** deferred until the CLI version proves the
  pattern-synthesis preset out.
- **File-tree sketch:** paths only — symbol names would reintroduce the
  parsing dependency PLAN §1 deliberately cut.
- **Partial-whiff retry:** no — retry only when *all* patterns whiff;
  a thin-but-nonzero result is answerable by the rerank stage.
- **`intent` optional on `scout grep`:** yes. `scout grep serialize`
  works with no intent (ack parity); an absent — or empty — intent is
  an implicit `--no-filter`: structured search only, the local model is
  never called, so it works with no LLM configured at all. Results come
  back in `mode: "full"` with `intent: null`, truncated at `--max-hits`
  (the hint names the truncation and the full total). With `scout find`
  covering the intent-only end, the spectrum is complete: pattern-only
  ⇒ pattern+intent ⇒ intent-only. **Implemented 2026-08-07.** The MCP
  tool schema is unchanged — `intent` stays required there, per the
  frozen-contract non-goal.

## 10. Open questions

None — question 1 (optional `intent`) is resolved in §9.
