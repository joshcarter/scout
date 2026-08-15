# The search CLI: `grep`, `find`, `edit`

scout's MCP tools exist for an agent. The CLI exists for a human, and it
is a first-class surface rather than hook plumbing — same code path,
different argument parsing.

The design goal for `scout grep` was: read like `ack`. Per-file hits with
line numbers and a little context, colored, safe against minified-JSON
bombs — plus scout's own addition, a one-line "why this hit" note from
the local model. `scout find` removes the need to guess a pattern at all,
and `scout edit` gets the results into `$EDITOR` with minimal
keystrokes.

**The MCP tool surface is deliberately frozen against all of this.** None
of the terminal rendering, none of the `[cli]` table, and none of `find`
reaches the JSON payload shapes an agent sees. Where a search feature is
genuinely useful to both (type and glob filters), it is added to the MCP
schema as new optional fields, never as a change to existing ones.

---

## 1. Output modes

| Condition | Format |
|---|---|
| stdout is a TTY | human text, colored |
| stdout is not a TTY (pipe/redirect) | human text, no color |
| `--format json` | the JSON payload, pretty-printed |
| `--format vimgrep` | `file:line:col: text` per hit, quickfix-compatible |

TTY detection is `std::io::IsTerminal` — stdlib, no dependency. Color is
`--color auto|always|never` (default `auto`) and honors `NO_COLOR`.

**Piped output stays human text, not JSON.** This matches rg, ack, and
grep: `scout grep foo | head` should not explode into JSON. Scripts that
want structure opt in with `--format json`.

Exit codes follow the grep convention:

- `0` — at least one hit returned
- `1` — no hits, *including* `none_relevant` (the model judged every hit
  irrelevant); the stderr message distinguishes the two
- `2` — error (bad pattern, LLM failure with no bypass)

Two paths this list did not mention, and now does because `tests/grep_cli.rs`
pins them:

- A **usage error** — no pattern, or conflicting flags such as `--intent`
  with `--no-filter` — also exits `2`. That is clap's default rather than
  anything scout chose, but it agrees with "2 means error", so it is worth
  stating rather than leaving to coincidence.
- **`--type-list`** exits `0`. It prints and stops; it is not a search, so
  "no hits" would be the wrong answer.

The code is the same in every `--format`: the format decides what stdout
looks like, never whether the search succeeded.

## 2. Human output

Rerank mode — the model was involved, so each hit has a `why`:

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

Header line is `path:line` — path magenta, line green, ack's scheme —
then the model's `why` in default text. **The 1–5 score is not shown.**
It still exists internally, orders the hits, and stays in the JSON
payload; it is just noise for a human. If marginal hits ever leak
through, a `[grep] min_score` floor is the cheap knob to add.

The context block is gutter-numbered, the matched line marked with `▶`
and bolded, and the matched pattern within it highlighted. One header per
hit even when the same file appears twice — simpler than ack's file
grouping. Bypass mode (`mode: "full"`, few enough hits to skip the model)
uses the same layout without the `why`.

**Status lines go to stderr, never stdout**, so humans get the metadata
the JSON payload carries without scripts having to parse it out of the
results:

- Before the LLM call: `filtering 214 hits with <model>…` — the rerank
  takes seconds and silence looks like a hang.
- After: `12 of 214 hits kept · 202 filtered by intent`, plus
  `search truncated at 2000 hits` or `capped at top 10 (--max-hits)`
  when applicable.
- On `none_relevant`: `local model judged none of the 214 hits relevant
  — rerun with --no-filter to see all of them` (exit 1).

## 3. Search filters

All implemented in `source.rs` over the `ignore` crate, and **exposed
through the MCP grep schema too** — an agent benefits from type and
directory filtering exactly as much as a human does.

| Flag | Meaning |
|---|---|
| `-t, --type TYPE` (repeatable) | only these file types — ripgrep's built-in type list, via `TypesBuilder::add_defaults()` + `select()` |
| `-T, --type-not TYPE` (repeatable) | exclude these types (`negate()`) |
| `--type-list` | print known types and exit |
| `-g, --glob GLOB` (repeatable) | include/exclude by glob, `!` negates (rg-compatible), via `OverrideBuilder` |
| `--dir PATH` / `--exclude-dir PATH` | friendlier spelling for directory scoping; sugar over `-g 'PATH/**'` / `-g '!PATH/**'` |
| `-C, --context N` | context lines override |
| `-n, --max-hits N` | cap results — a **ceiling, not a quota** |
| `--no-filter` | skip the LLM rerank entirely; pure structured search |

`.gitignore` and hidden-file filtering already handle most build
products; `--exclude-dir` is for what they do not, such as checked-in
fixtures.

The flag dialect is rg-compatible on purpose (`-t`, `-g`, `-M`); rg's
type names ship inside the `ignore` crate anyway.

**`intent` is optional on `scout grep`.** `scout grep serialize` works
with no intent at all (ack parity). An absent — or empty — intent is an
implicit `--no-filter`: structured search only, the local model is never
called, so it works with no LLM configured. Results come back in
`mode: "full"` with `intent: null`, truncated at `--max-hits`, and the
hint names both the truncation and the real total. The MCP schema is
unchanged; `intent` stays required there.

With `find` covering the intent-only end, the spectrum is complete:
pattern-only ⇒ pattern + intent ⇒ intent-only.

## 4. The minified-JSON problem

A single 5 MB minified line will wreck any grep renderer. ripgrep is the
tool that solved this: `rg -M 200` (`--max-columns`) replaces over-long
matched lines with `[Omitted long matching line]`, and
`--max-columns-preview` shows the first N columns plus
`[... N more matches]`. ack and ag have no equivalent — a known ack pain
point, and rg's approach is the one to copy. (Same idea outside grep:
`jless` and `fx` exist because even *viewing* minified JSON needs
structure-aware truncation.)

scout handles it in two layers:

1. **Search layer.** `context_max_bytes` (default 2000) caps a rendered
   context block, so a minified 5 MB line contributes at most ~2 KB to
   any payload. MCP context pollution is bounded before rendering is
   involved at all.
2. **Render layer.** `--max-columns` (default 150, `[cli] max_columns`).
   A line over the cap renders as a *window around the match* with `…`
   on the truncated side(s):

   ```
   ▶ 1 │ …"retry":3,"endpoint":"http://localhost:11434/v1","timeout"… [line is 48,213 columns]
   ```

   Context lines — non-matching neighbors — simply cut at the cap; no
   window is needed.

Windowing around the match, rather than rg's show-the-start preview,
requires the match's **column offset**, which the UTF8 sink originally
discarded. `SearchHit` carries `col` (byte offset of the first match in
the line, from `Matcher::find`). That one capture also unlocks in-line
match highlighting and `--format vimgrep`'s column field.

The cap is a terminal-rendering concern only: `--format json` returns the
payload as-is, already bounded by `context_max_bytes`.

## 5. `scout find` — intent-only search

`scout grep` requires the caller to guess a pattern first. `find` removes
that step: state what you want, and the *local model* guesses the
patterns.

```
scout find "<question>" [same filter flags as grep]
```

e.g. `scout find "where are the config file options parsed?"`

### Why not a text index

Index-based conceptual search answers this class of query with high
recall and terrible precision — pages of candidates, nearly all
irrelevant, because a text index can *retrieve* but cannot *judge*.
scout's bet is the inverse: retrieval stays dumb (grep), and the LLM does
the one thing an index cannot — read each hit in context and decide
whether it actually serves the question.

`find` is therefore not a shallow substitute for indexed search; on this
query shape it is the higher-precision tool. It also needs no index, no
daemon, and no enrollment — it works in any directory.

### The pipeline

The LLM never searches. It has exactly three jobs, all token-bounded:
guess patterns, judge hits, and say whether the judged hits answered the
question.

1. **Pattern synthesis** — one small call (`find_patterns.toml`). Input
   is the question plus a cheap structural sketch of the project: the
   file tree, **paths only**, truncated to `tree_max_bytes`. Symbol names
   would reintroduce the parsing dependency scout deliberately does not
   have. Output is up to `max_patterns` candidates, optionally tagged
   with type or glob hints.

   The question's own distinctive words are searched too, alongside the
   guesses — the word you typed is evidence, a synonym is a hypothesis.

2. **Mechanical search — no LLM.** Every candidate goes through
   `source::search`; results are unioned and deduped by `(file, line)`.
   Then the **degenerate-pattern guard**: a candidate with 0 hits is
   dropped silently, and a candidate with more than `degenerate_hit_cap`
   hits (default 300) is a bad discriminator — the moral equivalent of
   low IDF, `parse` in a parser — so *all* of its hits are dropped
   before the model sees them. A pattern that matches everything
   distinguishes nothing. Survivors flow into the existing
   `max_considered` / `batch_size` caps, so the rerank's token budget is
   untouched.

3. **Rerank** — the existing `grep` stage, unchanged. The union list is
   noisier than a single-pattern list, and filtering noise against an
   intent is exactly what that stage is for.

4. **Reflect** (`[find] reflect`, default on) — one more small call asks
   whether the kept hits actually answer the question. When they do not,
   it re-searches for the identifiers visible in them: a comment naming
   `draw_waterslide` is a pointer to the answer, not the answer.

5. **Retry, bounded.** A round is retried when *every* candidate whiffed
   (0 usable hits after the guard), and when reflect judges the results
   off-target and names better patterns. Both share one budget,
   `[find] max_attempts` (default 3; 1 disables both), overridable with
   `--attempts`. Local-model latency makes more than a few rounds
   unpleasant at a terminal anyway.

   **Partial whiffs do not retry** — only a total whiff does. A
   thin-but-nonzero result is answerable by the rerank stage.

### UX

Output, exit codes, and filter flags are identical to `scout grep` — same
renderer, same payload shape. stderr shows the guesses, for trust and for
manual fallback: `trying: config, toml, load_config, from_str · 2
whiffed`, then the normal rerank status line. If every attempt whiffs:
`no pattern guess produced hits after N attempts — try scout grep with
an explicit pattern` (exit 1).

`find` requires a configured LLM, unlike `grep`, which degrades to pure
search. A missing or broken config fails with the same message shape as
the rerank path, naming `scout grep <pattern>` as the fallback.

**`find` is CLI-only.** Exposing it over MCP is deferred until the
pattern-synthesis preset has proven itself in daily use.

## 6. `scout edit` — results into `$EDITOR`

A separate verb, not a flag, because the contract differs: it must *end*
in an editor, and interactivity is acceptable. It fronts both pipelines,
disambiguated by **arity** — no sentence-detection heuristics:

```
scout edit <question>              # one positional → find pipeline
scout edit <pattern> <intent>      # two positionals → grep pipeline
scout edit -p <pattern>            # pattern-only, no rerank
```

Flow: run the matching pipeline; **one hit** execs `$EDITOR` positioned
at it; **multiple hits** print the normal human output *numbered* and
prompt `edit which? [1-12, a=all, q=quit]` on the TTY, where a number
opens that hit and `a` opens every hit file; **zero hits** gives grep's
message and exit 1.

When the editor exits, scout exits. No re-prompt loop — rerunning is
cheap.

Positioning is per-editor, detected by `$EDITOR` basename:

| Editor | Invocation |
|---|---|
| vi/vim/nvim | `$EDITOR +<line> <file>`; for `a`, a temp file of `file:line:col: text` and `$EDITOR -q <tmpfile>` (quickfix — precedent: `git jump`, `rg --vimgrep \| vim -q -`) |
| emacs/emacsclient | `$EDITOR +<line>:<col> <file>` |
| hx (helix) | `hx <file>:<line>:<col>` |
| code/codium/cursor | `$EDITOR -g <file>:<line>:<col>` |
| zed | `zed <file>:<line>:<col>` |
| unknown | `$EDITOR <file>`, with `at line <N>` printed beforehand |

For plain-shell composition without `scout edit`, `--format vimgrep`
already enables `vim -q <(scout grep --format vimgrep …)`. That is also
why the vimgrep formatter shipped first — `scout edit`'s `vim -q` path
needs exactly the same one.

## 7. Config

`[cli]` and `[find]` in `config.toml`, both parsed leniently like
`[grep]` and `[extract]` — an unusable value keeps the default rather
than erroring. See `config.example.toml` for the annotated versions.

```toml
[cli]
color       = "auto"   # auto | always | never
context     = 2        # default -C (falls back to [grep] context_lines)
max_hits    = 20       # ceiling for terminal invocations, not a quota
max_columns = 150      # per-line render cap; 0 = unlimited

[find]
max_attempts       = 3     # search rounds; shared by whiff-retry and reflect-retry
max_patterns       = 8     # candidates per round — a latency knob as much as a recall knob
degenerate_hit_cap = 300   # a pattern matching more lines than this is discarded whole
tree_max_bytes     = 8192  # cap on the paths-only file-tree sketch
reflect            = true  # the did-this-answer-the-question stage
```

`[cli]` is read only by the CLI; the MCP server never consults it, so
nothing there changes what an agent sees. `$EDITOR` is respected as-is;
there is no editor config key.

The terminal `max_hits` default (20) is higher than the wire default
because the cap is a **ceiling, not a quota** — the rerank returns only
hits the model affirmatively kept, and never pads to the cap.

## 8. Implementation shape

- `src/render.rs` — payload → styled text. A pure function from the JSON
  payload (`mode`, `hits[]`, counters) plus a `RenderOpts` (width cap,
  color, TTY) to a `String`, so it is unit-testable against fixed
  payloads.
- `src/find.rs` — pattern synthesis, multi-pattern union and dedupe, the
  degenerate guard, reflect, and the retry loop; hands the merged hit
  list to the existing rerank and renderer. Preset:
  `presets/find_patterns.toml`.
- `src/edit.rs` — arity dispatch, the numbered picker, the editor
  dispatch table.
- `src/source.rs` — `SearchOptions` carries `types` and `overrides`
  threaded into `WalkBuilder`; `SearchHit` carries `col`.
- `src/mcp_server.rs` — optional `types`, `types_not`, `globs` on the
  grep tool, plumbed through the unchanged pipeline.

## 9. Decisions

Settled, and recorded so they are not re-litigated. Source comments
throughout `render.rs`, `find.rs`, `edit.rs` and `filter_config.rs` cite
this section by number.

- **Flag dialect: rg-compatible** (`-t`, `-g`, `-M`). rg's type names
  ship inside the `ignore` crate anyway.
- **`max_columns` default: 150.**
- **Same-file hit grouping: none.** One header per hit, even when a file
  appears twice. Simpler than ack's grouping; revisit if it reads noisy.
- **`scout edit` after the editor exits: scout exits.** No re-prompt
  loop — rerunning is cheap.
- **Terminal `--max-hits` default: 20.** Safe because the cap is a
  **ceiling, not a quota** — the rerank returns only hits the model
  affirmatively kept and never pads to the cap. If marginal keeps ever
  read as cruft, add a `[grep] min_score` floor.
- **Score display: dropped** from human output. Scores remain internal
  (they order the hits) and stay in the JSON payload.
- **`--format vimgrep`: kept**, and shipped early — it is the same
  `file:line:col:` formatter `scout edit`'s `vim -q` path needs anyway.
- **`find` over MCP: deferred** until the pattern-synthesis preset proves
  itself on the CLI. `[cli]` and `[find]` are therefore CLI-only tables,
  and `find_patterns` / `find_reflect` are presets no MCP tool can reach.
- **File-tree sketch: paths only.** Symbol names would reintroduce the
  parsing dependency scout deliberately does not have.
- **Partial-whiff retry: no.** Retry only when *every* pattern whiffs; a
  thin-but-nonzero result is answerable by the rerank stage.
- **`intent` optional on `scout grep`: yes.** An absent or empty intent
  is an implicit `--no-filter` — structured search only, the local model
  never called, so it works with no LLM configured at all. The MCP tool
  schema is unchanged; `intent` stays required there, per the
  frozen-contract rule at the top of this document.

---

## Appendix: phase shorthand

Source comments refer to build phases by number. They are historical
labels, not a roadmap, but they are load-bearing in comments like "a
pre-P3 payload" — meaning a hit recorded before match columns existed.

| | Delivered |
|---|---|
| **P1** | the renderer, TTY detection, grep exit codes, `--format` |
| **P2** | type and glob filters (CLI and MCP), `--no-filter` |
| **P3** | match columns, `--max-columns` windowing, in-line highlighting |
| **P4** | `scout find` — pattern synthesis, degenerate guard, retry cycle |
| **P5** | `scout edit` — arity dispatch, picker, editor table |
