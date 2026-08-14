# Spec: `scout dashboard` — a local web view of local-LLM traffic

**Status:** settled — no open questions remain; §7 records the
decisions. Streaming behavior in §5.5 is measured against the
configured endpoint, not assumed.

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

## 2. What the log records today, and why it is not enough

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
a 3-year rotation window to a ~2-day one, in exchange for detail almost
nobody reads retroactively. Token streams and find's per-round internals
are worse: high-rate, only meaningful in motion, and pure landfill once
the call is over.

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
daemon's SSE fan-out cheap.

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
  "id": "01JQ8F...-3",          // sortable per-process id; monotonic within a run
  "run": "01JQ8F...",           // one process; groups find's internal rounds
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

- **`run` + `id`** are the fix for find's fan-out. `run` is minted once
  per process (`std::process::id()` plus start time is sufficient — no
  uuid dep); `id` appends a per-process counter. The dashboard groups
  rows sharing a `run` under the originating `tool`, so a `find` reads
  as one expandable row, not four.
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
  ratio is the number that justifies the tool's existence, and it is
  cheap to capture at exactly the points that already have both values
  in hand.
- **`ts` becomes a float.** `parse_log` reads it with `as_u64()` today
  and would silently get 0 for every new row; it must switch to
  `as_f64()` — this is the one non-additive change and the reason for
  the `v` field.

### Prompt and response bodies

The most valuable pane is "show me the exact resolved prompt" — presets
run through `presets::resolve` template substitution, and the final text
is currently unobservable anywhere. But bodies are large (a
`check_output` prompt carries a whole build log) and carry file contents
from every repo you work in.

**Bodies go over the live channel (§2.5), not to disk.** That is the
default and it resolves what was this spec's main open question: nothing
sensitive is written to `~/.local/state` as a side effect of a dashboard
nobody opened, and the durable log keeps its multi-year rotation window.

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
keeps a hook-only install bounded. Two writers racing on the rename is
benign (`fs::rename` is atomic; the loser rotates an already-fresh file
and loses nothing), but the reader must handle the file shrinking under
it — see §5.

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
glyph and summary · elapsed · tokens in→out. Rows sharing a `run`
collapse into one with a `⋮n` marker that expands to the constituent
preset calls.

**Scope is global — every project, always.** scout is not
project-scoped (it's a CLI that runs in any subdirectory), so the
dashboard never narrows to `$PWD`. `project` is a column, shown when the
visible rows span more than one; filters across the top are by tool, by
`via`, by project, and failures-only.
`shell_safety` will dominate by volume — default the `via:hook` filter
to *off* so the view opens on what Claude deliberately did, with a
toggle to bring hook traffic back in.

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
turns P5 (§6) from a feature into a one-line config flip.

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
`{pid, port, started}`. Liveness is decided by **probing
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

| Route | Returns |
|---|---|
| `GET /` | the embedded HTML |
| `GET /api/status` | model, endpoint, reachability, version, aggregates |
| `GET /api/history?since=<id>&limit=&tool=&via=&failed=` | history rows, no bodies |
| `GET /api/call/<id>` | one call including prompt and response bodies |
| `GET /api/stats` | the `scout stats` table as JSON |
| `GET /api/stream` | **SSE** — live events from the channel (§2.5) |

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

**P1 — enrich the record.** `stats.rs`: `log_call` grows into a
`CallRecord` struct with a builder, `parse_log` learns `v:2` and float
`ts`, rotation lands. Thread `via`/`tool`/`project`/`input`/`outcome`
through the three call sites plus the bypass paths in `extract.rs` and
`grep.rs`. `raw_bytes`/`returned_bytes` captured where both are already
in hand. **This is the bulk of the work, and it stands on its own** —
`scout stats` gets meaningfully better output from it with no dashboard
at all, which makes it a safe place to stop if the rest stalls.
~400 lines touched, mostly mechanical, plus tests in `stats.rs`'s
existing style.

**P2 — the daemon and the server.** `src/dashboard.rs` (lifecycle +
HTTP + tailing reader) and `dashboard.html`, over the log alone — no
channel yet. Sequence it as `--foreground` first; a plain blocking
server is far easier to debug than a detached one, then add
`setsid`/pidfile/`--stop`/`--status` once the routes are right. ~350 +
~700 lines, plus the `libc` dep. **At the end of P2 the dashboard is
already useful** — every pane in §4 works except prompt bodies and the
live views.

**P3 — the live channel.** `src/live.rs`: the datagram sender
(`Option<UnixDatagram>` resolved once per process, drop on any error),
the daemon's receiver thread, the id-keyed upsert store, and
`/api/stream` over SSE. Emit `call.start` / `call.end` with bodies
first — that lights up the detail pane and P4's in-flight timer in one
step, since on the channel they are the same mechanism. ~300 lines.

**P4 — `find` refinement events.** `find.patterns` / `.hits` /
`.rerank` / `.reflect`, and the round tab strip in the detail pane. The
data the log serves worst, and the best argument for the channel
existing. ~150 lines, mostly instrumentation at points `find.rs`
already computes the values.

**P5 — token streaming.** `client.rs` gains a streaming read loop
(`stream_options: {include_usage: true}`, usage from the final chunk,
empty-`choices` guard) and `[llm] stream`; `call.token` events coalesce
on a 50 ms timer. Cleared by the §5.5 measurements — no latency cost, no
usage loss, error taxonomy unchanged — but still sequenced last because
it is the only phase touching a path all the tools depend on, and it
needs P3's channel to have somewhere to send. ~200 lines.

**P6 — bodies sidecar.** `persist_bodies`, the size and age sweep.
Genuinely optional now that the channel carries bodies live — build it
if retroactive detail turns out to be missed in practice, not before.
~150 lines.

**P7 — auto-start.** With an idempotent `scout dashboard` (§5) this is
no longer a feature, just a call to the same start path from the
SessionStart hook. Gated on `[dashboard] autostart`, default **off**: a
plugin that opens a listening port without being asked is rude, and this
one serves your source code.

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
  fidelities.** The log keeps `run`-based grouping for retroactive
  history; the channel carries the per-round internals that make it
  interesting, live only.
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

No open questions remain. Ready to execute.
