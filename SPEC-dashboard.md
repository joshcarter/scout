# Spec: `scout dashboard` — a local web view of local-LLM traffic

**Status:** **P1–P5 shipped — the dashboard shows the call as it is being
written, not just metadata about it, and a `find` shows its own
reasoning.** Every functional phase is done; P6 and P7 are optional. No
open design questions remain; §7 records the decisions and §6 the
phases. Streaming behavior in §5.5 is measured against the configured
endpoint, not assumed.

| Phase | State | What it delivers |
|---|---|---|
| **P1** — enrich the record | ✅ `a16b7fb` | `op`/`run`/`id`, `via`, `tool`, `input`, `outcome`, byte accounting, rotation |
| **P2** — daemon + server | ✅ `1d99442` | `scout dashboard` on 13001; all §4 panes but bodies and live views |
| — op grouping fix | ✅ `cee6d14` | recorded `op` replaces P2's idle-gap heuristic |
| **P3** — live channel | ✅ | unix datagram socket, SSE, prompt/response bodies, in-flight calls |
| **P4** — `find` refinement events | ✅ | per-round internals: patterns, hits, rerank, reflect |
| **P5** — token streaming | ✅ | `call.token` deltas, coalesced at 50 ms; `[llm] stream`, default on |
| **P6** — bodies sidecar | ⬜ optional | retroactive bodies on disk; build only if missed |
| **P7** — auto-start | ⬜ optional | SessionStart hook calls the idempotent start |

**What works right now:** `scout dashboard` starts a detached daemon on
<http://localhost:13001/>. Command history groups by operation, the
context-saved ratio sums per-operation, failures break out by kind,
p50/p95 latency, the Live/Pinned detail pane with three ways back, and
filters by tool/via/project/failed. The reader survives log rotation.
The live channel carries resolved prompts and responses: in-flight rows
appear in history, `/api/stream` is SSE, and the detail pane shows
bodies while the daemon is up. A `find` streams its own rounds — the
patterns it guessed and which were the question's own words, what each
one matched and which the degenerate guard threw away, the rerank's keeps
with their scores and `why`, and the reflect verdict — behind a round tab
strip in the detail pane. The reply itself arrives as it is generated:
the response block fills token by token while the call is still running,
and the authoritative body replaces it on `call.end`.

**What is missing:** nothing functional. P6 (bodies on disk) and P7
(auto-start) remain optional and unbuilt.

**One operational note:** per `CLAUDE.md`, the plugin's binary is a copy
in `$CLAUDE_PLUGIN_DATA/bin` refreshed on session restart. Until a
restart picks up the P1 build, hook and MCP traffic still writes v1 rows
with none of the new fields, and the dashboard will look sparse.

**Goal:** answer, at a glance, the two questions you cannot answer today:
*what is Claude actually sending to the local model*, and *what is coming
back*. Port 13001, alongside ct's 13000.

**Non-goals:** no mutation API (read-only — ct's `/api/projects/unload`
has no analogue here); no auth (loopback bind only); no charts library,
no build step, no npm.

**Shape:** two tiers. A durable, thin **call log** every scout process
appends to unconditionally, and an ephemeral, fat **live channel** that
carries prompt bodies, token streams, and `find`'s internal refinement —
but only while the dashboard is listening. §2.5 is the argument for the
split; it is the load-bearing decision in this spec.

---

## 1. Why this can't be ct's dashboard with the nouns swapped

ct is a **daemon**. Its dashboard reads `Arc<Mutex<Server>>` — history,
activity, and stats are live in-process, and `web.rs` just serialises
them. `include_str!("../dashboard.html")` plus a hand-rolled
`TcpListener` loop is all the plumbing it needs.

scout has **no long-lived process**. Every entry point is a short-lived
invocation:

| Entry point | Process | Lifetime |
|---|---|---|
| `check_output` / `extract` / `grep` MCP tools | `scout mcp` | one Claude Code session |
| `scout grep` / `find` / `edit` / `task` CLI | `scout` | one command |
| `hooks/shell-safety.sh` → `scout run --preset shell_safety` | `scout` | one Bash tool call |
| `hooks/prefer-local-llm.sh` | (no LLM call) | — |
| claude-review → `scout run --preset quality_review` | `scout` | one review |

There is no shared memory to serve. The one thing every path already
touches is the call log, `$XDG_STATE_HOME/scout/calls.jsonl`
(`stats.rs::log_path`), written by three call sites — `select::call_preset`
(all MCP tools plus `find`'s internal rounds), `run_cmd` (hook and
review traffic), and `main::run_task`.

**So: the log is the API, and the dashboard is a reader.** `scout
dashboard` binds 13001, tails the JSONL, and serves it. That choice has
a pleasant consequence — the dashboard works retroactively over calls
made before it was started, and survives its own restart, which ct's
in-memory history does not.

The cost is that the record has to carry everything the view needs. That
is where most of the work is, and §3 is the real content of this spec.

---

## 2. What the log recorded before P1, and why it was not enough

*Historical — P1 (`a16b7fb`) fixed every gap below. Kept because it is
the argument for the record's shape, and because rows in this form are
still the bulk of an existing `calls.jsonl`.*

```json
{"ts":1770000000,"preset":"grep","tokens_in":1840,"tokens_out":210,"ms":3100,"ok":true}
```

Six fields. Rendered as a table this is exactly `scout stats`, and it
answers none of the questions worth opening a browser for:

- **Which of these was one user action?** A single `scout find` emits
  three or four rows (`find_patterns`, `grep`, `find_reflect`, maybe a
  second `find_patterns` after a retry). They are indistinguishable from
  four unrelated calls.
- **What was the request?** `preset:"grep"` does not say which pattern,
  which intent, or which repo.
- **What came back?** Nothing. `ok:true` means the HTTP call returned
  non-empty text — not that the model answered usefully, and not what it
  said.
- **Why did it fail?** All four failure modes — endpoint down, timeout,
  empty response, unparseable selector JSON — collapse to `ok:false`.
  Endpoint-down is the single most common real failure and it is
  invisible here.
- **What did scout save me?** The tool's entire premise is keeping tokens
  out of Claude's context, and nothing measures it.
- **What did scout handle without the model at all?** `extract`'s
  `bypass_max_lines` and `grep`'s `bypass_max_hits` paths never log,
  so a fast-path hit looks identical to scout not being called.

---

## 2.5 Two tiers: the log, and the live channel

The log cannot be the only path, but not for the reason you'd guess.

Measured on this machine: `calls.jsonl` is 55 KB over 614 records
spanning 7.8 days — **~80 calls/day**. Appending costs ~4 syscalls
(~5–20 µs) inside a process that takes **1.58 ms merely to start** and
then blocks 1–4 *seconds* on the LLM. Reading is one `fstat` per poll,
unchanged 539 polls out of 540. Both sides are far below the noise
floor, and page cache is exactly why. Efficiency is not the reason to
add a second channel, and a shared-memory ring would in fact be *worse*
for memory pressure — tmpfs pages are unreclaimable, a file's are not.

The reason is **bandwidth of what we're willing to capture**. Some data
is worth showing but not worth persisting:

| | Prompt + response bodies | Token stream | `find` refinement |
|---|---|---|---|
| Volume | ~50 KB/call | ~200 events/call | ~10 events/round |
| Worth keeping on disk? | debatable | no | no |
| Worth watching live? | **yes** | **yes** | **yes** |

At 80 calls/day, bodies alone are ~4 MB/day — the durable log goes from
a ~7-month rotation window to a ~2-day one, in exchange for detail almost
nobody reads retroactively. Token streams and find's per-round internals
are worse: high-rate, only meaningful in motion, and pure landfill once
the call is over.

*(P1 measured the v2 record at ~500 bytes against v1's ~90, so the 8 MB
cap is ~17k rows — about seven months, not the multiple years an earlier
draft assumed from v1 sizes. Still ample, but §4's "log size / rotation
hint" row should be sized against seven months.)*

So: **the log stays exactly as durable as it is today, and everything
expensive moves to a channel that only exists when someone is
watching.**

```
short-lived scout procs ──append──> calls.jsonl ──tail──> daemon ──poll──> browser
         │                        (durable, thin)          │
         └──sendto (if listening)─> unix datagram ──────────┘──SSE──> browser
                                   (ephemeral, fat)
```

### Transport

**Non-blocking `SOCK_DGRAM` unix socket** at
`$XDG_RUNTIME_DIR/scout/live.sock` (falling back to the state dir),
created by the daemon on start and unlinked on clean exit.

Datagram, not stream, and non-blocking, for one reason: **it is
structurally incapable of slowing scout down or failing it.** No
connect handshake, no framing, message boundaries preserved for free.
Nobody listening is an instant `ENOENT`; a stale socket after a crash is
an instant `ECONNREFUSED`; a daemon too slow to drain is `EAGAIN`. Every
one of those is handled the same way — **drop the event and move on**.
Dropping telemetry is always the correct answer here.

That matters most for the highest-frequency writer, `shell_safety`,
which sits in the critical path of every Bash tool call Claude makes.
A connection-oriented channel would put a connect timeout there in
exchange for nothing.

Resolve the socket **once per process** into an `Option<UnixDatagram>`,
not per event, so a long-lived MCP server doesn't retry a missing socket
hundreds of times — and so the streaming decision (below) can be made up
front.

### Events

`{v, id, run, seq, kind, ...}` — `id`/`run` are the same ones §3 puts
in the log, which is what makes reconciliation possible.

| `kind` | Carries |
|---|---|
| `call.start` | tool, preset, via, project, model, resolved system + user prompt |
| `call.token` | text delta (coalesced — see below) |
| `call.end` | full response, usage, ms, outcome |
| `find.patterns` | round, guessed patterns |
| `find.hits` | per-pattern hit counts, which were dropped as degenerate and why |
| `find.rerank` | keeps with scores and `why` |
| `find.reflect` | verdict, and the refined patterns it named |
| `bypass` | tool, reason (`bypass_max_lines` / `bypass_max_hits`), size |

`find.*` is the set the log serves worst today — its rounds currently
appear as unrelated `find_patterns`/`grep`/`find_reflect` rows. Watching
the search converge live is also the only practical way to tune
`max_patterns`, `degenerate_hit_cap`, and `reflect` against real
questions.

**Coalesce `call.token`** on a ~50 ms timer rather than one datagram per
token. 210 tokens × ~1 µs is still noise against a 3 s call, but the
browser cannot render faster than a frame anyway, and it keeps the
daemon's SSE fan-out cheap. *(Measured after P5: a real `extract` sent
187 completion tokens as 62 datagrams — the local 35B is slower than the
window, so the timer mostly bundles two or three tokens at a time and the
saving is a third rather than the 5× a faster host would see.)*

### Two consequences worth naming

**In-flight calls stop being a separate feature.** An earlier draft
deferred them: start/end records would double log volume and force
`parse_log` to filter or double-count. On the channel,
`call.start`/`call.end` cost the log nothing and are the same mechanism
that delivers bodies — so "is the model thinking or is it wedged", the
question a 27B running for 40 seconds provokes most, gets answered as a
side effect of §6's P3 rather than a phase of its own.

**Reconciliation is the new complexity.** A call arrives twice — live
over the socket, then again when its log line lands — and the daemon
must merge, not duplicate. Key the in-memory store by `id` and upsert;
treat the log as authoritative for summary fields and live events as
enrichment.

Best-effort everywhere, and **no replay buffer**: a dashboard started
mid-call sees `call.end` with no `call.start`, and one started mid-`find`
sees round 3 but not rounds 1–2. Both must render as ordinary rows — the
log still supplies the summary, only the live detail is missing. Live
capture begins with the next command, and scout does not buffer events
per process against the chance that someone starts watching.

### Retention of live-captured detail

The daemon holds bodies in memory, LRU-capped (~500 calls). A daemon
restart loses them; that's an accepted trade. The §3 sidecar therefore
becomes **opt-in** (`persist_bodies = false` by default) for anyone who
wants bodies retroactively, rather than the default path.

---

## 3. The record, extended

Additive only — every new field is optional, and `stats.rs::parse_log`
keeps working on old lines. One line per LLM round-trip, same file.

```json
{
  "v": 2,
  "id": "01JQ8F...-3",          // this row; monotonic within a run
  "run": "01JQ8F...",           // one process (a whole MCP session)
  "op":  "01JQ8F...-1",         // one user-facing operation — the grouping key
  "ts": 1770000000.482,          // float seconds — sub-second ordering
  "via": "mcp",                  // mcp | cli | hook | run
  "tool": "find",                // user-facing operation
  "preset": "find_patterns",     // template actually sent (unchanged semantics)
  "attempt": 2,                  // find's round counter; 1 elsewhere
  "project": "/home/josh/Projects/scout",
  "model": "qwen3:27b",
  "endpoint": "http://localhost:11434/v1",
  "input": {"question": "where does the dashboard bind its port"},
  "outcome": {"kind": "ok", "summary": "8 patterns, 3 non-degenerate"},
  "raw_bytes": 184320,           // what scout consumed on Claude's behalf
  "returned_bytes": 1180,        // what scout handed back
  "tokens_in": 1840,
  "tokens_out": 210,
  "ms": 3100,
  "ok": true
}
```

Field notes:

- **`run` + `id` + `op`.** `run` is minted once per process
  (`std::process::id()` plus start time is sufficient — no uuid dep);
  `id` appends a per-process counter; **`op` identifies one user-facing
  operation** and is what the dashboard groups on, so a `find` reads as
  one expandable row rather than four.

  *Corrected during P2 — this spec contradicted itself.* An earlier
  draft grouped on `run` alone, while §1's own table records that
  `scout mcp` is **one process for an entire Claude Code session**.
  Grouping on `run` would therefore collapse every MCP tool call of a
  session into a single history row and destroy the pane the dashboard
  mostly exists for. P2 shipped an idle-gap heuristic as a stopgap; it
  was replaced with `op`, because `Ledger` (§3, byte accounting) is
  constructed once per dispatch and so already delimits exactly the
  right span — the boundary was known precisely, it just wasn't
  recorded. `run` stays: it identifies the process, which is genuinely
  useful, and P3 reconciles against both.

  **Grouping must key on `op`, never on adjacency.** `mcp_server.rs`
  dispatches through `tokio::task::spawn_blocking`, so parallel tool
  calls run concurrently and **interleave their rows in the log** — and
  Claude Code batches independent tool calls as a matter of course, so
  this is the ordinary case, not an edge one. A "consecutive rows of one
  `op`" rule splits any two concurrent multi-row operations. It also
  breaks on an operation that straddles a log rotation. P3's reader
  inherits the same constraint: live events from concurrent operations
  will interleave over the channel exactly as their log rows do.
- **`via`** is genuinely informative and free to derive — the MCP server,
  the CLI dispatcher, and `run_cmd` each know which they are. It answers
  "did Claude choose this tool, or did a hook force it?" Set it at the
  entry point, not at `log_call`, so it can't drift.
- **`tool` vs `preset`** — keep both. `preset` stays what `scout stats`
  aggregates on (no behavior change); `tool` is what a human thinks in.
- **`input`** is a small structured object per tool, not a blob:
  `check_output` → `{command}`; `extract` → `{file, question, lines}`;
  `grep` → `{pattern, intent, hits_scanned}`; `find` → `{question}`;
  `task` → `{prompt}` truncated; `shell_safety` → `{command}`.
  Truncate each string at 300 chars with a `…` marker.
- **`outcome.kind`** replaces the boolean's ambiguity:
  `ok` | `bypassed` | `none_relevant` | `empty_response` |
  `parse_failure` | `endpoint_unreachable` | `timeout` | `http_error`.
  Note `bypassed` — this requires logging the fast paths in
  `extract.rs`/`grep.rs` that currently return before any LLM call.
  Those rows carry `ms` (still real work) and zero tokens.
- **`raw_bytes` / `returned_bytes`** are the context-saved metric.
  `raw_bytes` is the input scout digested — captured build output,
  file bytes read, total bytes of the pre-rerank hit list.
  `returned_bytes` is the serialized payload handed to the caller. The
  ratio is the number that justifies the tool's existence.

  **Corrected during P1, and it changes P2.** An earlier draft called
  this "cheap to capture at exactly the points that already have both
  values in hand." No such point exists. The log's unit is a
  *round-trip*, but `raw_bytes` is known before an operation's **first**
  call and `returned_bytes` only after its **last** — and the naive fix
  of stamping raw onto every row makes a three-chunk `extract` count its
  file three times and inflates the headline number threefold.

  P1 solved it with a `Ledger` on `Ctx` (~60 lines, the only
  non-mechanical part of the phase): it parks the newest record, lets
  the first row claim the raw deposit, and writes on `finish`, `fail`,
  or `Drop`. The consequence for the dashboard: **raw and returned land
  on different rows of the same operation, so the reader must sum per
  `op`, never per row.** A per-row ratio is meaningless. A real `find`
  measures 1099 + 42215 raw across two rows against 1606 returned on a
  third — per row that reads as `0 → 1606` and `42215 → 0`; per `op` it
  is 43314 → 1606.

  Two outcomes turned out not to be round-trip properties either, and
  the same mechanism absorbed both: `none_relevant` is the rerank's
  verdict once every batch is in, and `parse_failure` in the §3 sense
  (an unparseable *selector*) is discovered by the filter after
  `call_preset` has already returned successfully.

  **On the headline looking small at first:** P2 observed the
  context-saved figure reading only ~44 KB → ~4 KB and read it as thin
  deposit coverage. It isn't. The log was 750 v1 rows against 5 v2 rows
  — v1 rows predate byte accounting entirely and can never contribute.
  Exercising the paths directly confirms the mechanism is sound: a
  `via:hook` `shell_safety` row records `raw=2419 ret=156`, and a
  `check_output` of `echo hello` records `raw=5 ret=199`. The number
  simply needs v2 traffic to accumulate, and CLAUDE.md notes the
  installed plugin binary only refreshes on a session restart — so
  hook and MCP rows lag a rebuild. **Do not go hunting for missing
  deposits on the strength of an early reading.**
- **`ts` becomes a float.** *(Corrected during P1: an earlier draft said
  `parse_log` read `ts` with `as_u64()` and would silently get 0 for
  every new row. It never read `ts` at all — the hazard was invented.
  It is real for **P2's** reader, though, so P1 gave float `ts` a
  consumer and tests covering both encodings.)*

### Prompt and response bodies

The most valuable pane is "show me the exact resolved prompt" — presets
run through `presets::resolve` template substitution, and the final text
is currently unobservable anywhere. But bodies are large (a
`check_output` prompt carries a whole build log) and carry file contents
from every repo you work in.

**Bodies go over the live channel (§2.5), not to disk.** That is the
default and it resolves what was this spec's main open question: nothing
sensitive is written to `~/.local/state` as a side effect of a dashboard
nobody opened, and the durable log keeps its ~7-month rotation window.

A **sidecar remains available, opt-in**, for retroactive detail — bodies
to `$XDG_STATE_HOME/scout/bodies/<run>.jsonl` as `{id, system, user,
response}`:

```toml
[dashboard]
# port = 13001
# persist_bodies = false   # true → also write bodies to the sidecar
# max_body_bytes = 65536   # per field, head+tail elided in the middle
# retain_days = 7
# max_bodies_bytes = 268435456   # ceiling as well as an age bound
```

Note `max_bodies_bytes` alongside `retain_days`: at ~80 calls/day with
~50 KB bodies the sidecar grows ~4 MB/day, so an age bound alone lets it
reach ~28 MB before the first sweep — fine, but it should be a chosen
number rather than an emergent one.

If `persist_bodies` is ever turned on, call the tradeoff out in the
README: it writes file contents and build output to disk, loopback-only
but not encrypted.

### Rotation

**Decided: yes.** Overdue independent of the dashboard —
`shell-safety.sh` fires on *every* Bash tool call, so `calls.jsonl`
already grows faster than anything else scout writes, and `print_report`
reads the whole file every time.

Rotate `calls.jsonl` at ~8 MB to `calls.jsonl.1` (keep one), and sweep
`bodies/` older than `retain_days` on daemon start and on each rotation.

The check belongs in the writer, not the daemon: every entry point
writes, most of them with no dashboard running, so rotation cannot
depend on one. `write_record` already opens the file — `stat` it on
open and rotate when over the cap, which costs one syscall per call and
keeps a hook-only install bounded. The reader must handle the file
shrinking under it — see §5.

*(P1 correction: two writers racing on the rename is **not** always
benign, as an earlier draft claimed. If the loser's rename lands between
the winner's rename and its reopen, `ENOENT` and nothing is lost; if it
lands after, it renames the winner's fresh file over the generation just
rotated and takes 8 MB of history with it. Left unclosed deliberately —
a few microseconds against ~80 calls/day, both writers must also arrive
exactly at the cap, and the cost is one generation of a diagnostic log.
Closing it needs an `O_EXCL` lockfile, which is more machinery than this
earns. The code comment says so too.)*

---

## 4. What the dashboard shows

Five panes. The layout follows ct's — dark, monospace, three columns,
poll-based — because it works and because the muscle memory transfers.

**Header.** Model and endpoint, with a live reachability dot
(`client.check_endpoint`, polled every 15s — this is the one thing the
log cannot tell you, and "is ollama even up" is the question you'll ask
most). Binary version. Connection status.

**Overview** (left column, stat rows like ct's):

| Row | Why it's here |
|---|---|
| Calls — 1h / 24h / total | volume at a glance |
| Success rate | with failures broken out by `outcome.kind` below |
| **Context saved** | `Σ raw_bytes → Σ returned_bytes`, shown as `18.4 MB → 214 KB (86×)` and an estimated token count. The headline number. |
| Tokens in / out | what the local model actually chewed |
| Latency p50 / p95 | p95 matters more than the mean for a local model — one 40s stall is what you notice, and the mean hides it |
| Bypassed | calls scout served without the model |
| Log size | with a rotation hint when close to the cap |

**Failures** (left column, below overview). Recent errors grouped by
`outcome.kind` with counts. Empty and collapsed on a healthy day; the
first place to look on a bad one.

**Command history** (center — the pane you actually asked for). One row
per user-facing operation, newest first:

```
14:22:07  mcp   check_output   cargo test --all           ✗ 2 failures    4.1s   1.8k→210
14:21:44  hook  shell_safety   rm -rf target/             ✓ allow        0.9s    340→12
14:20:12  cli   find     ⋮3    "where does the port bind" ✓ 3 files      11.2s   6.2k→890
14:19:58  mcp   grep           TcpListener ~ "bind sites" ⊘ bypassed        —       —
```

Columns: time · `via` badge · tool · one-line input summary · outcome
glyph and summary · elapsed · tokens in→out. Rows sharing an **`op`**
collapse into one with a `⋮n` marker that expands to the constituent
preset calls.

**Scope is global — every project, always.** scout is not
project-scoped (it's a CLI that runs in any subdirectory), so the
dashboard never narrows to `$PWD`. `project` is a column, shown when the
visible rows span more than one; filters across the top are by tool, by
`via`, by project, and failures-only.
`shell_safety` dominates by volume, and the first cut of this defaulted
the `via:hook` filter to *off* so the view opened on what Claude
deliberately did, with a toggle to bring hook traffic back in.
**Reverted:** hiding the highest-volume source by default made the
history quietly incomplete — a call you just watched happen was simply
absent, with nothing on screen saying so, and the toggle only offered
hook-only or hook-excluded rather than everything. The view now opens on
all traffic; the `via` select already narrows to one source on demand.

**Detail** (right column). The fully resolved system and user prompt,
the raw model response, then usage, timings, model, project, and the
parsed outcome. For a grouped `find`, a small tab strip across the
rounds — seeing round 1's patterns next to round 2's after a
reflect-driven retry is the best debugging view scout could have.
Copy-to-clipboard on each block (ct's multi-select copy is worth
stealing wholesale — pasting a failed prompt into a chat is the main
reason you'd want it).

### Live vs. pinned

The detail pane has two states, and the fix for "how do I get back to
most recent" is to make the state visible rather than implicit:

- **Live** (the default, and the state on load): the pane tracks the
  newest call. New calls replace its contents as they land.
- **Pinned**: clicking any history row pins that call. New calls keep
  arriving in the history list, but the detail pane holds still.

A chip in the detail pane's header shows which, and is the way back:

```
● Live                     ⏸ Pinned · 14:20:12 find      ✕
                              ↑ click, or Esc, to resume
```

While pinned, the chip counts what you're missing — `⏸ Pinned · 3 new` —
so the pane advertises its own staleness. That's the YouTube-live-chat
/ Slack "jump to present" pattern, and it works because the button
answers both questions at once: *am I looking at the latest?* and *how
do I get back?*

Three ways out, deliberately redundant:

1. **Click the chip** — the discoverable one, always visible.
2. **`Esc`** — clears the selection. Pair with `j`/`k` and arrow keys to
   walk the history without the mouse; pressing `k` past the newest row
   also resumes live.
3. **Browser Back** — pinning pushes `#call/<id>` onto the URL hash,
   live is no hash. Nearly free to implement, gives back/forward for
   free, and makes a pinned call reloadable and pasteable to yourself.

Two details that decide whether this feels right:

- **Only a history-row click pins.** Not scrolling, not hovering, not
  clicking inside the detail pane. Implicit pinning is the kind of
  cleverness that leaves you unable to explain why the pane stopped
  updating.
- **Don't re-render the pane when the newest call's `id` is unchanged.**
  In live mode a poll that returns nothing new must not reset scroll
  position or drop a text selection mid-read. Diff on `id`, not on
  payload.

One case is genuinely awkward and worth naming: in live mode, a long
prompt you have started reading can be yanked away by an unrelated call
completing — a `shell_safety` row landing while you study a
`check_output` failure. The `via:hook` filter defaulting to off covers
most of it (filtered-out calls must not steal the live pane either), and
pinning covers the rest. If it still grates in practice, the fallback is
to make live mode advance only when the pane hasn't been scrolled — but
try the simple version first.

---

## 5. Serving it

### Lifecycle

`scout dashboard` starts a **detached background daemon**, prints the
URL, and returns immediately.

| Command | Behavior |
|---|---|
| `scout dashboard` | start detached; print URL; **idempotent** — if one is already up, print its URL and exit 0 |
| `scout dashboard --stop` | SIGTERM the recorded pid, wait up to 2s, remove pidfile |
| `scout dashboard --status` | running/not, pid, port, uptime, log path; exit 0/1 |
| `scout dashboard --restart` | stop then start |
| `scout dashboard --foreground` | run in this process, log to stderr — for debugging and for the detach trampoline |
| `--port N` | override `[dashboard] port` |
| `--open` | also launch a browser (`xdg-open`/`open`) |

Idempotent start matters more than it looks: it makes `scout dashboard`
safe to put in a shell profile or a SessionStart hook, which is what
turns P7 (§6) from a feature into a one-line config flip.

**Detaching**, on both Linux and macOS: re-exec self with
`--foreground`, `stdin` from `/dev/null`, `stdout`/`stderr` to the daemon
log, and `pre_exec(|| { libc::setsid(); Ok(()) })` so the daemon leaves
the terminal's process group. Without `setsid` a later `^C` in the
launching shell kills the dashboard, and closing the terminal SIGHUPs
it — both would look like random crashes.

That adds **`libc`** as a dependency. It's the one new crate this spec
needs; it is ubiquitous, and ct's `web.rs` already depends on it for the
same neighborhood of problem.

**Pidfile** at `$XDG_STATE_HOME/scout/dashboard.pid`, holding
`{pid, port, started}` — but **one fixed path is unsafe once `--port`
exists** (found in P2, two live bugs: `--port N` alongside a default
daemon reported "stale pidfile — cleared" and trampled it, and `--stop`
preferred the pidfile's pid over the probe's and could SIGTERM the wrong
process). The SIGTERM handler must be async-signal-safe, so it `unlink`s
its path unconditionally and cannot read the file back to check whose it
is. The configured port therefore keeps `dashboard.pid`; any other port
gets `dashboard-<port>.pid`. Liveness is decided by **probing
`GET /api/status` on the recorded port and checking for a scout marker
field**, not by the pid alone — a recycled pid is rare but a pidfile
surviving a `kill -9` is not, and pid-liveness is awkward to check
portably anyway (`/proc` is Linux-only). The pidfile's job is to hold
the pid for `--stop`; the HTTP probe decides whether anything is there.
The daemon removes it on clean exit and any command that probes a dead
port removes it as stale.

**Daemon log** at `$XDG_STATE_HOME/scout/dashboard.log` — bind errors,
panics, and rotation notices. Truncate at start if over ~1 MB; this is
diagnostics, not history.

**Failure modes**, each with its own message: pidfile present and port
answering (→ print URL, exit 0); pidfile present and port dead (→ clear
stale pidfile, start); no pidfile and port busy (→ error naming the
port, since something that isn't scout has it).

### The server

Copy the shape of `ct/cmd/ct/src/web.rs`: `include_str!` an embedded
`dashboard.html`, one `TcpListener`, a thread per connection, a
hand-rolled request line + `Content-Length` parse, `HTTP/1.0
Connection: close`. ~250 lines over `serde_json` and threads, both
already in the tree.

Two notes against ct's version:

- **Do take the `SO_REUSEADDR` dance.** An earlier draft argued a
  foreground command didn't need it; a restartable daemon on a
  well-known port does, exactly as ct's does — accepted connections
  linger in `TIME_WAIT` bound to 13001 and block the rebind. `libc` is
  now in the tree for `setsid`, so this is free. Keep ct's 3-attempt
  retry loop too; `--restart` hits precisely that window.
- **Bind 127.0.0.1, unconditionally, no env override.** ct has
  `CT_WEB_BIND`; scout's payloads include arbitrary file contents from
  every repo you work in, so there is no bind address but loopback worth
  supporting.

Shutdown on SIGTERM: stop accepting, remove the pidfile, exit. No
in-flight state to flush — the daemon only reads.

Endpoints:

| Route | Returns | State |
|---|---|---|
| `GET /` | the embedded HTML | ✅ P2 |
| `GET /api/status` | model, endpoint, reachability, version, aggregates | ✅ P2 |
| `GET /api/history?since=<id>&limit=&tool=&via=&project=&failed=` | operations with their rows inlined | ✅ P2 |
| `GET /api/call/<id>` | one operation, resolved from *any* of its row ids | ✅ P2 |
| `GET /api/stats` | the `scout stats` table as JSON | ✅ P2 |
| `GET /api/stream` | **SSE** — live events from the channel (§2.5) | ✅ P3 |

Two P2 choices that the routes above encode:

- **`since` is an opaque row id, not an ordinal**, because scout's ids
  are not ordered. An id the server cannot find — a tab that slept
  through a rotation — returns a full page with `"resynced": true`
  rather than an error. That is the only behavior that lets a stale tab
  recover itself.
- **History inlines each operation's rows.** Rows are small (`input` is
  capped at 300 chars), so the detail pane needs no second fetch;
  `/api/call/<id>` returns the identical object purely so `#call/<id>`
  deep links and `curl` work.

`/api/stream` is `text/event-stream`: hold the connection open, write
`data: {...}\n\n` per event, flush. Polling cannot deliver a token
stream, and SSE is the cheapest thing that can — ~30 lines over the
hand-rolled HTTP already here, no new dependency, and the browser's
`EventSource` reconnects on its own. Websockets would buy nothing; the
channel is one-directional by construction.

Note this makes connection lifetime unbounded, which the `HTTP/1.0
Connection: close` shape borrowed from ct does not anticipate — the SSE
route needs its own path through the responder, and its handler thread
lives as long as the browser tab. Cap concurrent stream connections
(say 8) so a tab-hoarding session can't accumulate threads without
bound.

The reader keeps an in-memory index — file offset plus inode and length.
A poll re-reads only the tail. Because the daemon is long-lived and the
writers rotate underneath it (§3), it must detect rotation rather than
assume it: a changed inode, or a length below the last offset, forces a
reload of `calls.jsonl.1` + `calls.jsonl`. Getting this wrong is the
most likely source of a dashboard that silently stops updating after a
few days, so it deserves a test that rotates mid-read.

Polling: history every 2s, status every 15s. No websockets.

Everything is `GET`. There is no state to mutate.

---

## 5.5 Streaming — measured, and cleared

Token streaming is the only change here that reaches into scout's
existing behavior rather than sitting beside it, so it was the one thing
worth testing before designing around. **Tested; it is safe, and it is
free.**

`client.rs::complete` today sends `"stream": false`, then
`resp.into_string()` → parse one JSON object → pull
`choices[0].message.content` and `usage` (`src/client.rs:153-164`).
Streaming replaces that with a read loop over `data:` chunks,
accumulating deltas until `data: [DONE]`.

Callers see no difference: `complete` still returns `(String, Value)`
after accumulating the full text. The stream is observed on the way past
and pushed to the channel. Streaming is observability-only, with no
behavioral change for `check_output`, `grep`, `extract`, or `find`.

### Findings

Measured against the configured endpoint — **LM Studio,
`qwen/qwen3.6-35b-a3b`**:

| Question | Answer |
|---|---|
| Does `usage` survive streaming? | **Yes**, in the final chunk |
| Is `stream_options: {include_usage: true}` required? | **Yes** — omit it and usage is absent entirely |
| Do the numbers match non-streaming? | **Exactly** — 15/3/18 and 27/87/114, verified both ways |
| Latency cost? | **None** — 23.2 ms/token non-streaming vs 22.9 ms/token streaming |
| How do HTTP errors arrive? | As a normal 4xx + JSON body **before** any stream begins |

The error finding is the important one: a malformed request returns
`400 application/json`, so ureq's `Error::Status(code, resp)` arm fires
exactly as it does today. **The whole `LlmError` taxonomy in
`client.rs:110-150` — `EndpointUnavailable`, `Timeout`, `RequestFailed`
— survives unchanged.** Streaming does not introduce a new class of
mid-stream failure that the existing classification would mishandle.

Two implementation details the test surfaced:

- **The usage chunk carries `"choices": []`.** An empty array, not a
  populated one. A reader doing `chunk["choices"][0]["delta"]` on every
  chunk breaks on the last one. Match on the usage key first, then
  deltas.
- **~1 content delta per token** (86 deltas / 87 completion tokens),
  which confirms §2.5's coalescing: one datagram per delta is 86
  syscalls where a 50 ms timer gives ~40.

### How a delta reaches the channel — a delta sink

Decided when P5 was built; §5.5 as first written said streaming was safe
without ever saying what the read loop hands the event to, and that gap
is the only design question the phase actually had.

**A sink, not a threaded `CallRecord`:**

```rust
pub fn complete(&self, messages: Vec<Value>, max_tokens: Option<u64>)
    -> Result<(String, Value), LlmError>
{ self.complete_streaming(messages, max_tokens, &mut |_| {}) }

pub fn complete_streaming(&self, messages: Vec<Value>, max_tokens: Option<u64>,
                          on_delta: &mut dyn FnMut(&str))
    -> Result<(String, Value), LlmError> { … }
```

Four properties, each of which the obvious alternative gives up:

1. **`task.rs` has no `CallRecord` to thread.** `task::handle` calls
   `complete`; the record lives above it in `main::run_task`. With the
   sink, `task.rs` needed no edit at all.
2. **`CallRecord::silent` stays enforced where it already is.** A silent
   caller installs no sink, and `client.rs` never learns the concept —
   so a unit test cannot fire tokens into a developer's dashboard by
   forgetting a flag in a second place.
3. **The coalescer is testable without a socket.** The 50 ms buffering
   lives in `live.rs`, driven in tests by a collecting closure;
   `client.rs` only yields deltas as they arrive.
4. **`usage` never touches the sink.** It arrives in the final chunk and
   comes back in the return value; `call.end` remains its only path to
   the dashboard, so there is no second usage path to keep in agreement
   with the first.

The sink is **best-effort and must not block** — it runs inside the HTTP
read loop, where a slow consumer stalls the model call itself. The one
implementation scout ships is a buffer append plus an occasional
non-blocking `sendto`.

### Verdict

**Stream unconditionally.** No Heisenberg property, no divergence
between observed and unobserved runs, no config branch to reason about.
The extra work when no dashboard is listening is 89 small JSON parses
instead of one medium one — call it 100 µs against a 2000 ms call, or
0.005%. That is well inside "low-cost when the daemon isn't running".

`[llm] stream = true` stays as a config escape hatch, defaulting on,
because **this is verified on LM Studio only**. The other host scout's
`config.example.toml` names is ollama on `:11434`, untested here; if it
turns out to drop `include_usage`, the fallback is not a design change,
just `stream = false` for that host. Keep `false` a fully supported
path — this is a diagnostic feature, not a load-bearing one.

*(Unrelated quirk worth recording: LM Studio ignores an unknown `model`
name and serves whatever is loaded, rather than erroring. Pre-existing,
not a streaming behavior, but it means a typo'd model in
`config.toml` fails silently rather than loudly.)*

---

## 6. Phases

### ✅ P1 — enrich the record (`a16b7fb`)

`stats.rs`: `log_call` became a `CallRecord` with a chained builder, an
`Outcome` enum of 8 kinds, `run`/`id` minting, and rotation in the
writer. `via`/`tool`/`project`/`input`/`outcome` threaded through all
three call sites plus the bypass paths in `extract.rs` and `grep.rs`,
which previously logged nothing at all. `scout stats` gained the
context-saved ratio and a failures-by-kind breakdown, so the phase paid
off with no dashboard at all.

Delivered ~1290 lines against an estimated ~400. The overrun was the
`Ledger` and the byte accounting it exists for — see §3.

### ✅ P2 — the daemon and the server (`1d99442`, `cee6d14`)

`src/dashboard.rs` + `dashboard.html`, over the log alone. ~1250 impl +
~500 test lines and ~815 lines of HTML, against an estimated 350 + 700;
the lifecycle's failure modes and the reader's rotation handling were
most of the overrun. `libc` is the only added dependency.

Verified end to end: detached start leaves the terminal's session
(`PGID == SID == PID`, `TT=?`, reparented, so `^C` and hangup cannot
reach it), `--restart` rebinds immediately, a `kill -9` leaves a stale
pidfile that the next command clears, and a non-scout process on 13001
produces a distinct error. The reader survives a live rotation against a
running daemon.

`cee6d14` replaced P2's idle-gap grouping heuristic with the recorded
`op` — see §3.

### ✅ P3 — the live channel

**Why it is next:** the detail pane currently shows metadata about a
call but not the call. "Show me the exact resolved prompt" was the
original goal of this whole document — presets go through
`presets::resolve` template substitution and the final text is
observable nowhere else — and P3 is what delivers it. It also delivers
in-flight calls for free (§2.5), which answers "is the model thinking or
is it wedged" on a local 27B.

**New file `src/live.rs`, three parts:**

1. **Sender.** `Option<UnixDatagram>` resolved **once per process** into
   a `OnceLock`, non-blocking, connected to
   `$XDG_RUNTIME_DIR/scout/live.sock`. Every failure mode — `ENOENT`
   (nobody listening), `ECONNREFUSED` (stale socket), `EAGAIN` (daemon
   not draining) — drops the event silently. This is the same fail-open
   contract `stats.rs::append_line` already honors, and it matters most
   for `shell_safety`, which sits in the critical path of every Bash
   tool call Claude makes.
2. **Receiver.** A daemon thread reading datagrams into the existing
   in-memory store, plus an LRU body cache (~500 operations). Bodies
   live in memory only; a daemon restart loses them, which §2.5 accepts.
3. **`/api/stream`.** Replace P2's `501` with `text/event-stream`: hold
   the connection, `data: {...}\n\n` per event, flush. Needs its own
   path through the responder — the `HTTP/1.0 Connection: close` shape
   borrowed from ct assumes bounded connections, and an SSE handler
   thread lives as long as the browser tab. **Cap concurrent streams**
   (say 8) so tab-hoarding cannot accumulate threads.

**Where to hook the emitters.** `select::call_preset` already holds the
resolved `system`/`user` messages and the reply — that is `call.start`
and `call.end` in one function. `Ctx`'s `Ledger` already mints the `op`,
so events carry it without new plumbing.

**Reconciliation is the real work, not the transport.** Each call
arrives twice — live over the socket, then again when its log line
lands. Key the store by `id`, upsert, and treat the log as authoritative
for summary fields with live events as enrichment. Three constraints the
earlier phases established:

- **Group by `op`, never adjacency.** `spawn_blocking` dispatch means
  concurrent operations interleave over the channel exactly as they do
  in the log (§3).
- **No replay buffer** (§2.5). A dashboard started mid-call sees
  `call.end` with no `call.start`; render it as an ordinary completed
  row, not an orphan.
- **The live pane must not be stolen by a filtered-out call** — the
  `via:hook` default-off filter already implies this in P2's client-side
  filtering, and streamed events must respect it too.

**Testing.** A missing socket must be provably free (assert the sender
resolves to `None` and no syscall repeats per event); a full receive
buffer must drop rather than block; reconciliation must not duplicate a
call that arrives on both paths; SSE must survive a client disconnecting
mid-stream without leaking the handler thread.

~300 lines was the estimate. P2 ran 3× its estimate, so treat that as
optimistic — the reconciliation and the SSE responder path are both
places where the detail is the work.

### ✅ P4 — `find` refinement events

`find.patterns` / `.hits` / `.rerank` / `.reflect`, plus the round tab
strip in the detail pane. The data the log serves worst — its rounds are
three unrelated-looking preset rows even after `op` grouping ties them
together — and the best argument for the channel existing. Watching the
search converge is also the only practical way to tune `max_patterns`,
`degenerate_hit_cap`, and `reflect` against real questions.

Delivered ~850 lines (≈340 Rust impl, ≈320 Rust test, ~185 HTML) against
an estimated ~150. Verified end to end against LM Studio: a real
`find "where does the dashboard bind its port"` streamed six `find.*`
events across two rounds, and the same rounds came back on
`/api/call/<id>` after a reload.

The payoff was immediate and is the reason the phase exists: that run's
seeded `(?i)port` matched **301** hits against a `degenerate_hit_cap` of
**300** and was discarded whole. Nothing in the log says so, and nothing
in the result hints at it — one glance at the pane does.

Where it landed differently from the brief, all four found by building:

- **A `find.*` event has no row `id` to carry.** §2.5's envelope is
  `{v, id, run, seq, kind, …}` and P3 made `id` the row id, which is what
  makes reconciliation possible. But a round is not a round-trip — it
  spans zero, one, or several — so there is no row id to put there. The
  operation's own id goes in the `id` slot instead: `Ledger`'s `op` and
  every row `id` come from the same `stats::next_id` counter, so the two
  can never collide, and the store keys on `op` regardless. Nothing
  downstream noticed, because the dashboard already groups on `op`.
- **Two of the four values are not computed in `find.rs`.** The rerank's
  keeps, scores and `why` are `grep::rerank`'s, and the reflect verdict
  is discarded inside `find::reflect` the moment `next_patterns` decides
  not to act on it. Both are still one-line reads — the keeps come back
  inside the payload `rerank` returns, and the verdict needed only a
  `clone` before `next_patterns` consumes it — but "at points `find.rs`
  already computes the values" was not where to look for either.
- **`fit_event` could only shrink strings.** P3's truncation elides
  `system`/`user`/`response`; every `find.*` payload is *arrays*. A wide
  search would have hit the 64 KiB cap with nothing the shrinker
  recognised and lost the event. It now halves the longest array
  repeatedly and sets `truncated: true`, so a shortened list is
  distinguishable from a complete one.
- **A round can legitimately produce no `find.rerank`.** When a refined
  round's hits are all already in the union (`union.len() ==
  previously`), `find` returns the prior payload rather than spending a
  call to reproduce it — so round 2 has patterns and hits and nothing
  else. Observed on the very first real run. It renders through the same
  partial-view path as a dashboard that joined mid-`find`, which is the
  right answer: "absent" and "not yet" look identical and both are
  ordinary.

One deliberate omission: `find_rounds` rides on `/api/call/<id>` only,
never on `/api/history`. A 300-operation page carrying every round's
pattern and keep list would dwarf the summary rows it exists to deliver,
and the pane fetches the one operation it is painting anyway.

### ✅ P5 — token streaming

`client.rs`'s streaming read loop and `[llm] stream` (default on);
`call.token` coalesced on a 50 ms timer; the detail pane's response
block filling as the reply is written. **Cleared by measurement already**
(§5.5) — no latency cost, no usage loss, `LlmError` taxonomy unchanged —
so this was implementation, not investigation, and both measured gotchas
held: `stream_options: {include_usage: true}` is required, and the final
chunk's `"choices": []` is real.

Delivered ~730 lines (≈290 Rust impl, ≈375 Rust test, ~63 HTML, 8 config
comment) against an estimated ~200. The test-to-impl ratio is the
overrun: the delta sink exists so the coalescer can be driven by a
closure and the SSE reader by a `Cursor`, and having made both testable
without a socket it was worth testing both properly.

**Re-verified against LM Studio, `qwen/qwen3.6-35b-a3b`**, since §5.5's
error claim is what the whole `LlmError` taxonomy rests on: a malformed
request with `"stream": true` still answers `400 Bad Request` with
`Content-Type: application/json` and no event stream at all, so ureq's
`Error::Status` arm fires exactly as before. Same prompt both ways
returns the same usage (28/20/48 streamed and not), and a real `scout
extract` streamed 187 completion tokens as **62 datagrams** over 15.1 s —
the coalescer doing what §2.5 asked, and the concatenated deltas equal
to `call.end`'s authoritative body byte for byte.

**ollama remains untested.** Nothing here found a reason to doubt it, and
nothing here confirms it either; if it drops `include_usage` under
streaming, `stream = false` is the supported answer and `scout stats`
keeps its numbers.

Where it landed differently from the brief:

- **`[DONE]` is not the only terminator worth honoring.** A stream is
  complete when it says `data: [DONE]` *or* when a chunk carries a
  non-null `finish_reason`; LM Studio sends both, in that order, and
  requiring only the former would make scout's correctness depend on a
  frame that is conventional rather than specified. EOF with neither is
  an error — a reply cut off mid-flight reads exactly like a short one to
  every caller above `client.rs`, which is the failure mode worth being
  loud about.
- **The truncation ladder was the wrong place to bound a token event.**
  §7.6's lesson applied again and gave a different answer: a token
  payload is one unstructured string, so there is nothing for
  `fit_event` to shrink *intelligently* — the fix is to bound the
  producer, flushing the buffer at 8 KiB whatever the timer says, and to
  leave the ladder as the backstop it is.
- **The daemon stores no token text.** Fan out to SSE subscribers and
  forget: a partial reply held in the body cache would outlive a failed
  call looking like a whole one, and §2.5 already calls the token stream
  landfill once the call is over. The browser keeps the partial instead,
  where it is labelled as one.
- **The pane appends, it does not repaint.** A repaint every 50 ms would
  fight the reader for scroll position and drop any text selected
  mid-read, so the first delta installs the response block and every
  delta after it appends a text node to it.

### ⬜ P6 — bodies sidecar (optional)

`persist_bodies`, plus the size and age sweep. Genuinely optional now
that the channel carries bodies live — **build it only if retroactive
detail turns out to be missed in practice.** ~150 lines.

### ⬜ P7 — auto-start (optional)

With an idempotent `scout dashboard` (§5) this is no longer a feature,
just a call to the same start path from the SessionStart hook. Gated on
`[dashboard] autostart`, default **off**: a plugin that opens a
listening port without being asked is rude, and this one serves your
source code.

### Not phased, but worth doing

- **`LlmError::RequestFailed` is too coarse.** P1 has to decide
  `http_error` by `msg.starts_with("HTTP ")` — string-matching a value
  the same function formatted moments earlier. Recorded in `TODO.md`;
  worth fixing the next time that enum is touched anyway.
- **A misconfigured `[llm] model` fails silently.** LM Studio serves
  whatever is loaded when handed a name that is not in `/v1/models`, and
  `check_endpoint` cannot catch it. The response reports the model that
  actually ran and scout discards it — comparing the two would catch
  substitution precisely. Recorded in `TODO.md`.

---

## 7. Decisions and open questions

Settled: log rotation (§3); background daemon started by `scout
dashboard` (§5); the five panes of §4; click-to-populate detail with a
Live/Pinned chip as the way back (§4); the two-tier log + live channel
split (§2.5).

Resolved by §2.5 — recorded so they aren't re-litigated:

- *Bodies on disk by default?* **No.** They go over the channel. The
  sidecar survives as opt-in (§3).
- *`find` grouping in the log or the reader?* **Both, at different
  fidelities.** The log carries an `op` id so history groups a `find`
  into one row retroactively; the channel carries the per-round
  internals that make it interesting, live only (P4).
- *Shared memory instead of the log?* **No** — measured, the log costs
  ~5–20 µs to write against a 1.58 ms floor on process spawn, and a
  tmpfs ring would be worse under memory pressure. The channel exists
  for bandwidth, not efficiency.

- *Does `usage` survive streaming?* **Yes**, and at no latency cost —
  measured, §5.5. Stream unconditionally; no observed/unobserved
  divergence.
- *Cross-project or per-project?* **All activity, always.** scout is
  not project-scoped — it's a CLI that runs in any subdirectory — so
  the dashboard shows everything. `project` is a display column and an
  optional filter, never a scope.
- *Replay buffer for a dashboard opened mid-call?* **No.** A dashboard
  started mid-`find` picks up from the next command; the partial run
  shows up from the log with its live detail missing. Not worth the
  per-process buffering to close.

### Where the spec was wrong

Recorded because each was found by building, not by reading, and the
pattern is worth trusting: nearly all of them came from an implementer
pushing back on the brief, and the one that did not (§7.4) is a hazard
that review invented and building never found.

1. **Byte accounting had no single capture point** (P1). "Cheap to
   capture where both values are in hand" — no such point exists.
   Produced the `Ledger`, and the per-`op` summing rule.
2. **Grouping on `run` contradicted §1** (P2). `scout mcp` is one
   process per session, so `run` would have collapsed a whole session
   into one row. Produced the `op` field.
3. **Adjacency grouping breaks under concurrency** (post-P2).
   `spawn_blocking` dispatch interleaves rows from parallel tool calls,
   which is the ordinary case since Claude Code batches independent
   calls. Produced the id-keyed grouping.
4. **A `ts`/`as_u64()` hazard that never existed** (P1). Invented in
   review, not observed. Worth remembering as the counterexample: not
   every plausible-sounding hazard is real, and a spec asserting one
   costs implementation time.
5. **The event envelope assumed every event describes a round-trip**
   (P4). `find.*` describes an operation, and has no row `id` to carry.
   Produced the `id = op` rule, and the observation that the store was
   already keying on `op` anyway.
6. **The truncation ladder only understood strings** (P4). Bodies are
   strings; `find`'s payloads are arrays, and the ladder had nothing to
   trim. Produced array halving and the `truncated` marker. The general
   lesson: a fail-open contract has to be re-checked against each new
   *shape* of payload, not just each new payload.
7. **"No config branch to reason about" and `[llm] stream` are the same
   paragraph** (P5, §5.5). The verdict said "stream unconditionally, no
   config branch", then kept the flag two sentences later because only
   one host had been measured. Both are right, and the resolution is
   that the branch is allowed to exist in exactly one place: which
   *reader* consumes the response. Everything above `client.rs` —
   callers, the error taxonomy, the log, the `call.end` payload — has no
   idea which way the wire went, and the return value is the same pair
   either way. A flag that reaches further than that would have been the
   thing §5.5 was right to refuse.
8. **§5.5 cleared streaming without saying how a delta reaches the
   channel** (P5). It measured latency, usage and error shape — the
   things that could have killed the phase — and left the only real
   design question unstated. Produced the delta sink, now written down
   in §5.5. The pattern is the mirror of §7.4: a spec section can be
   thorough about the hazards it went looking for and silent about the
   decision that actually shapes the code.
9. **A fail-open ladder cannot fix an unbounded producer** (P5). §7.6
   said to re-check `fit_event` against each new payload *shape*, which
   was done — and the answer this time was that the ladder is the wrong
   layer. A coalesced token run is one string with no internal structure
   to trim, so it is bounded where it is produced, at 8 KiB. Refining
   §7.6: check each new shape against the ladder, and be willing to
   conclude the ladder is not where that shape belongs.
10. **"Mostly instrumentation at points that already compute the values"
   was half right** (P4) — the same shape of claim §7.1 records for P1's
   byte accounting, and wrong the same way. Two of the four values live
   one function away from where the brief pointed. Cheap to fix here;
   worth distrusting the phrasing next time it appears.

**No open design questions remain, and no functional phase is
outstanding. What is left (P6, P7) is optional by construction: build the
bodies sidecar only if retroactive detail turns out to be missed, and
auto-start only if opening a port unasked stops feeling rude.**
