# The dashboard, and the telemetry underneath it

`scout dashboard` serves a read-only web view of local-LLM traffic on
`http://localhost:13001/`. It answers the two questions nothing else
could: *what is the agent actually sending to the local model*, and
*what is coming back*.

This document is the design record — why the architecture is what it is,
what the call record carries, and which decisions were settled by
measurement rather than argument. It is not a user guide; `scout
dashboard --help` and `config.example.toml` cover operation.

---

## 1. Why the dashboard is a log reader

The obvious design is the one used by any daemon-backed tool: hold
history in process memory and serialize it on request. scout cannot do
that, because **scout has no long-lived process**. Every entry point is a
short-lived invocation:

| Entry point | Process | Lifetime |
|---|---|---|
| `check_output` / `extract` / `grep` MCP tools | `scout mcp` | one agent session |
| `scout grep` / `find` / `edit` / `task` | `scout` | one command |
| `hooks/shell-safety.sh` → `scout run --preset shell_safety` | `scout` | one Bash tool call |
| `hooks/prefer-local-llm.sh` | (no LLM call) | — |
| external callers → `scout run --preset quality_review` | `scout` | one review |

There is no shared memory to serve. The one thing every path already
touches is the call log, `$XDG_STATE_HOME/scout/calls.jsonl`
(`stats.rs::log_path`), written from three call sites —
`select::call_preset` (all MCP tools plus `find`'s internal rounds),
`run_cmd` (hook and review traffic), and `main::run_task`.

**So the log is the API, and the dashboard is a reader.** `scout
dashboard` binds 13001, tails the JSONL, and serves it. That has a
pleasant consequence a daemon's in-memory history does not: the
dashboard works retroactively over calls made before it started, and it
survives its own restart.

The cost is that the record has to carry everything the view needs —
which is most of the work, and §3 below.

---

## 2. Two tiers: the log, and the live channel

The log cannot be the only path, but not for the reason you would guess.

Measured: `calls.jsonl` at 55 KB over 614 records spanning 7.8 days —
**~80 calls/day**. Appending costs ~4 syscalls (~5–20 µs) inside a
process that takes **1.58 ms merely to start** and then blocks 1–4
*seconds* on the LLM. Reading is one `fstat` per poll, unchanged on 539
of 540 polls. Both sides sit far below the noise floor, and the page
cache is exactly why. **Efficiency is not the reason for a second
channel** — and a shared-memory ring would be actively worse for memory
pressure, since tmpfs pages are unreclaimable and a file's are not.

The reason is **bandwidth of what we are willing to capture**. Some data
is worth showing but not worth persisting:

| | Prompt + response bodies | Token stream | `find` refinement |
|---|---|---|---|
| Volume | ~50 KB/call | ~200 events/call | ~10 events/round |
| Worth keeping on disk? | debatable | no | no |
| Worth watching live? | **yes** | **yes** | **yes** |

At 80 calls/day, bodies alone are ~4 MB/day: the durable log's rotation
window collapses from ~7 months to ~2 days in exchange for detail almost
nobody reads retroactively. Token streams and `find`'s per-round
internals are worse — high-rate, meaningful only in motion, pure
landfill once the call is over.

So: **the log stays exactly as durable as it was, and everything
expensive moves to a channel that only exists while someone is
watching.**

```
short-lived scout procs ──append──> calls.jsonl ──tail──> daemon ──poll──> browser
         │                        (durable, thin)          │
         └──sendto (if listening)─> unix datagram ──────────┘──SSE──> browser
                                   (ephemeral, fat)
```

### Transport

A **non-blocking `SOCK_DGRAM` unix socket** at
`$XDG_RUNTIME_DIR/scout/live.sock` (falling back to the state dir),
created by the daemon on start and unlinked on clean exit.

Datagram, not stream, and non-blocking, for one reason: **it is
structurally incapable of slowing scout down or failing it.** No connect
handshake, no framing, message boundaries preserved for free. Nobody
listening is an instant `ENOENT`; a stale socket after a crash is an
instant `ECONNREFUSED`; a daemon too slow to drain is `EAGAIN`. All three
are handled identically — **drop the event and move on**. Dropping
telemetry is always the correct answer here.

That matters most for the highest-frequency writer, `shell_safety`,
which sits in the critical path of every Bash tool call the agent makes.
A connection-oriented channel would put a connect timeout there in
exchange for nothing.

The socket resolves **once per process** into an `Option<UnixDatagram>`,
not per event, so a long-lived MCP server does not retry a missing
socket hundreds of times.

### Events

`{v, id, run, seq, kind, ...}` — `id` and `run` are the same ones the
log carries, which is what makes reconciliation possible.

| `kind` | Carries |
|---|---|
| `call.start` | tool, preset, via, project, model, resolved system + user prompt |
| `call.token` | text delta (coalesced — below) |
| `call.end` | full response, usage, ms, outcome |
| `find.patterns` | round, guessed patterns |
| `find.hits` | per-pattern hit counts, which were dropped as degenerate and why |
| `find.rerank` | keeps with scores and `why` |
| `find.reflect` | verdict, and the refined patterns it named |
| `bypass` | tool, reason (`bypass_max_lines` / `bypass_max_hits`), size |

`find.*` is the set the log serves worst — its rounds appear there as
unrelated `find_patterns` / `grep` / `find_reflect` rows. Watching a
search converge live is also the only practical way to tune
`max_patterns`, `degenerate_hit_cap`, and `reflect` against real
questions.

**`call.token` is coalesced** on a ~50 ms timer rather than one datagram
per token. The browser cannot render faster than a frame anyway, and it
keeps the SSE fan-out cheap. Measured: a real `extract` sent 187
completion tokens as 62 datagrams — the local 35B is slower than the
window, so the timer mostly bundles two or three tokens and the saving is
a third rather than the 5× a faster host would see.

### Two consequences worth naming

**In-flight calls stopped being a separate feature.** An earlier design
deferred them: start/end records would double log volume and force
`parse_log` to filter or double-count. On the channel,
`call.start`/`call.end` cost the log nothing and are the same mechanism
that delivers bodies — so "is the model thinking or is it wedged", the
question a local 27B running for 40 seconds provokes most, is answered
as a side effect.

**Reconciliation is the real complexity.** A call arrives twice — live
over the socket, then again when its log line lands — and the daemon
must merge, not duplicate. The store keys on `id` and upserts; the log is
authoritative for summary fields, live events are enrichment.

Best-effort everywhere, and **no replay buffer**: a dashboard started
mid-call sees `call.end` with no `call.start`, and one started mid-`find`
sees round 3 but not rounds 1–2. Both render as ordinary rows — the log
still supplies the summary, only the live detail is missing. Live
capture begins with the next command; scout does not buffer events per
process against the chance that someone starts watching.

Live-captured detail is held in memory, LRU-capped at ~500 calls. A
daemon restart loses it. The opt-in bodies sidecar
(`[dashboard] persist_bodies`) exists for anyone who wants bodies
retroactively, and is off by default.

---

## 3. The record

One line per LLM round-trip, `$XDG_STATE_HOME/scout/calls.jsonl`.
Additive only — every field beyond the original six is optional, and
`stats.rs::parse_log` still reads v1 lines.

```json
{
  "v": 2,
  "id": "01JQ8F...-3",          // this row; monotonic within a run
  "run": "01JQ8F...",           // one process (a whole MCP session)
  "op":  "01JQ8F...-1",         // one user-facing operation — the grouping key
  "ts": 1770000000.482,         // float seconds — sub-second ordering
  "via": "mcp",                 // mcp | cli | hook | run
  "tool": "find",               // user-facing operation
  "preset": "find_patterns",    // template actually sent
  "attempt": 2,                 // find's round counter; 1 elsewhere
  "project": "/home/josh/Projects/scout",
  "model": "qwen3:27b",
  "endpoint": "http://localhost:11434/v1",
  "input": {"question": "where does the dashboard bind its port"},
  "outcome": {"kind": "ok", "summary": "8 patterns, 3 non-degenerate"},
  "raw_bytes": 184320,          // what scout consumed on the caller's behalf
  "returned_bytes": 1180,       // what scout handed back
  "tokens_in": 1840,
  "tokens_out": 210,
  "ms": 3100,
  "ok": true
}
```

The six fields this replaced — `ts`, `preset`, `tokens_in`,
`tokens_out`, `ms`, `ok` — are exactly `scout stats` rendered as a
table, and they answer none of the questions worth opening a browser
for: which rows were one user action, what the request was, what came
back, why it failed (four distinct failure modes collapsed to
`ok:false`, endpoint-down among them), what scout saved, and what scout
served without the model at all.

### Field notes

**`run` + `id` + `op`.** `run` is minted once per process
(`std::process::id()` plus start time — no uuid dependency); `id`
appends a per-process counter; **`op` identifies one user-facing
operation** and is what the dashboard groups on, so a `find` reads as one
expandable row rather than four.

Grouping on `run` was the original design and it was wrong: `scout mcp`
is one process for an *entire* agent session, so `run` would collapse
every MCP tool call of a session into a single row and destroy the pane
the dashboard mostly exists for. `op` is free — `Ledger` is constructed
once per dispatch and so already delimits exactly the right span; the
boundary was known precisely, it just was not recorded.

**Grouping must key on `op`, never on adjacency.** `mcp_server.rs`
dispatches through `tokio::task::spawn_blocking`, so parallel tool calls
run concurrently and **interleave their rows in the log** — and agents
batch independent tool calls as a matter of course, so this is the
ordinary case, not an edge one. A "consecutive rows of one `op`" rule
splits any two concurrent multi-row operations, and also breaks on an
operation that straddles a log rotation. The live reader inherits the
same constraint: events from concurrent operations interleave over the
channel exactly as their rows do in the log.

**`via`** answers "did the model choose this tool, or did a hook force
it?" It is set at the entry point, not at `log_call`, so it cannot
drift.

**`tool` vs `preset`** — both are kept. `preset` is what `scout stats`
aggregates on; `tool` is what a human thinks in.

**`input`** is a small structured object per tool, not a blob:
`check_output` → `{command}`; `extract` → `{file, question, lines}`;
`grep` → `{pattern, intent, hits_scanned}`; `find` → `{question}`;
`task` → `{prompt}`; `shell_safety` → `{command}`. Each string truncates
at 300 chars with a `…` marker.

**`outcome.kind`** replaces the boolean's ambiguity: `ok` | `bypassed` |
`none_relevant` | `empty_response` | `parse_failure` |
`endpoint_unreachable` | `timeout` | `http_error`. Note `bypassed` —
this required logging the fast paths in `extract.rs` and `grep.rs` that
previously returned before any LLM call and so recorded nothing at all.
Those rows carry a real `ms` and zero tokens.

**`raw_bytes` / `returned_bytes`** are the context-saved metric.
`raw_bytes` is the input scout digested — captured build output, file
bytes read, total bytes of the pre-rerank hit list. `returned_bytes` is
the serialized payload handed to the caller. The ratio is the number
that justifies the tool's existence.

There is **no single point where both values are in hand.** The log's
unit is a round-trip, but `raw_bytes` is known before an operation's
*first* call and `returned_bytes` only after its *last* — and the naive
fix of stamping raw onto every row makes a three-chunk `extract` count
its file three times and inflates the headline threefold. The `Ledger` on
`Ctx` (~60 lines) solves it: it parks the newest record, lets the first
row claim the raw deposit, and writes on `finish`, `fail`, or `Drop`.

The consequence for any reader: **raw and returned land on different
rows of the same operation, so sum per `op`, never per row.** A per-row
ratio is meaningless. A real `find` measures 1099 + 42215 raw across two
rows against 1606 returned on a third — per row that reads as `0 → 1606`
and `42215 → 0`; per `op` it is 43314 → 1606.

Two outcomes turned out not to be round-trip properties either, and the
same mechanism absorbed both: `none_relevant` is the rerank's verdict
once every batch is in, and `parse_failure` (an unparseable *selector*)
is discovered by the filter after `call_preset` has already returned
successfully.

**A thin context-saved headline is usually just v1 rows.** The figure
first read as ~44 KB → ~4 KB, which looked like poor deposit coverage. It
was not: the log held 750 v1 rows against 5 v2 rows, and v1 rows predate
byte accounting entirely so they can never contribute. Exercising the
paths directly confirms the mechanism — a `via:hook` `shell_safety` row
records `raw=2419 ret=156`, a `check_output` of `echo hello` records
`raw=5 ret=199`. The number needs v2 traffic to accumulate, and the
plugin's binary only refreshes on a session restart, so hook and MCP
rows lag a rebuild. Do not go hunting for missing deposits on the
strength of an early reading.

### Rotation

`calls.jsonl` rotates at ~8 MB to `calls.jsonl.1` (one generation kept).
At ~500 bytes per v2 record that is ~17k rows, about seven months.

Overdue independent of the dashboard: `shell-safety.sh` fires on *every*
Bash tool call, so `calls.jsonl` grows faster than anything else scout
writes, and `print_report` reads the whole file every time.

The check belongs in the **writer**, not the daemon: every entry point
writes, most with no dashboard running, so rotation cannot depend on one.
`write_record` already opens the file — `stat` it on open and rotate when
over the cap, one syscall per call, and a hook-only install stays
bounded. The reader has to handle the file shrinking underneath it (§5).

**One race is left open deliberately.** Two writers racing on the rename
is not always benign: if the loser's rename lands between the winner's
rename and its reopen, the result is `ENOENT` and nothing is lost; if it
lands after, it renames the winner's fresh file over the generation just
rotated and takes 8 MB of history with it. Closing it needs an `O_EXCL`
lockfile. Against ~80 calls/day, a window of a few microseconds, both
writers having to arrive exactly at the cap, and a cost of one
generation of a diagnostic log, that is more machinery than it earns.
The code comment says so too.

---

## 4. What the dashboard shows

Five panes: dark, monospace, three columns, poll-based.

**Header.** Model and endpoint with a live reachability dot
(`client.check_endpoint`, polled every 15 s — the one thing the log
cannot tell you, and "is the model host even up" is the question asked
most). Binary version. Connection status.

**Overview** (left column):

| Row | Why it's here |
|---|---|
| Calls — 1h / 24h / total | volume at a glance |
| Success rate | with failures broken out by `outcome.kind` below |
| **Context saved** | `Σ raw_bytes → Σ returned_bytes`, as `18.4 MB → 214 KB (86×)` plus an estimated token count. The headline. |
| Tokens in / out | what the local model actually chewed |
| Latency p50 / p95 | p95 matters more than the mean — one 40 s stall is what you notice, and the mean hides it |
| Bypassed | calls scout served without the model |
| Log size | with a rotation hint when close to the cap |

**Failures** (left column, below). Recent errors grouped by
`outcome.kind` with counts. Empty and collapsed on a healthy day; the
first place to look on a bad one.

**Command history** (center). One row per user-facing operation, newest
first:

```
14:22:07  mcp   check_output   cargo test --all           ✗ 2 failures    4.1s   1.8k→210
14:21:44  hook  shell_safety   rm -rf target/             ✓ allow        0.9s    340→12
14:20:12  cli   find     ⋮3    "where does the port bind" ✓ 3 files      11.2s   6.2k→890
14:19:58  mcp   grep           TcpListener ~ "bind sites" ⊘ bypassed        —       —
```

Columns: time · `via` badge · tool · one-line input summary · outcome
glyph and summary · elapsed · tokens in→out. Rows sharing an `op`
collapse into one with a `⋮n` marker that expands.

**Scope is global — every project, always.** scout is not
project-scoped; it is a CLI that runs in any subdirectory, so the
dashboard never narrows to `$PWD`. `project` is a column, shown when the
visible rows span more than one. Filters across the top are by tool, by
`via`, by project, and failures-only.

`shell_safety` dominates by volume, and the first cut defaulted the
`via:hook` filter to *off* so the view opened on what the agent
deliberately did. **That was reverted:** hiding the highest-volume
source by default made the history quietly incomplete — a call you just
watched happen was simply absent with nothing on screen saying so, and
the toggle only offered hook-only or hook-excluded rather than
everything. The view opens on all traffic; the `via` select narrows on
demand.

**Detail** (right column). The fully resolved system and user prompt,
the raw model response, then usage, timings, model, project, and the
parsed outcome. For a grouped `find`, a tab strip across the rounds —
seeing round 1's patterns next to round 2's after a reflect-driven retry
is the best debugging view scout has. Copy-to-clipboard on each block:
pasting a failed prompt into a chat is the main reason you want it.

### Live vs. pinned

The detail pane has two states, and the fix for "how do I get back to
most recent" is to make the state visible rather than implicit:

- **Live** (default, and the state on load): the pane tracks the newest
  call; new calls replace its contents as they land.
- **Pinned**: clicking a history row pins that call. New calls keep
  arriving in the list, but the pane holds still.

A chip in the pane header shows which, and is the way back:

```
● Live                     ⏸ Pinned · 14:20:12 find      ✕
                              ↑ click, or Esc, to resume
```

While pinned the chip counts what you are missing — `⏸ Pinned · 3 new` —
so the pane advertises its own staleness. Three ways out, deliberately
redundant: click the chip (discoverable, always visible); `Esc` (pairs
with `j`/`k` and arrows to walk history without the mouse — `k` past the
newest row also resumes live); browser Back (pinning pushes `#call/<id>`
onto the URL hash, live is no hash — nearly free, and it makes a pinned
call reloadable and pasteable to yourself).

Two details decide whether this feels right:

- **Only a history-row click pins.** Not scrolling, not hovering, not
  clicking inside the detail pane. Implicit pinning is the kind of
  cleverness that leaves you unable to explain why the pane stopped
  updating.
- **Do not re-render when the newest call's `id` is unchanged.** In live
  mode a poll returning nothing new must not reset scroll position or
  drop a text selection mid-read. Diff on `id`, not on payload.

One case stays awkward: in live mode a long prompt you have started
reading can be yanked away by an unrelated call completing. Pinning
covers it. If it grates in practice, the fallback is to advance live mode
only when the pane has not been scrolled.

---

## 5. Serving it

### Lifecycle

`scout dashboard` starts a **detached background daemon**, prints the
URL, and returns.

| Command | Behavior |
|---|---|
| `scout dashboard` | start detached; print URL; **idempotent** — if one is up, print its URL and exit 0 |
| `scout dashboard --stop` | SIGTERM the recorded pid, wait up to 2 s, remove pidfile |
| `scout dashboard --status` | running/not, pid, port, uptime, log path; exit 0/1 |
| `scout dashboard --restart` | stop then start |
| `scout dashboard --foreground` | run in this process, log to stderr — for debugging and for the detach trampoline |
| `--port N` | override `[dashboard] port` |
| `--open` | also launch a browser (`xdg-open`/`open`) |

Idempotent start matters more than it looks: it makes `scout dashboard`
safe in a shell profile or a SessionStart hook, which turns auto-start
from a feature into a one-line config flip.

**Detaching**, on Linux and macOS alike: re-exec self with
`--foreground`, stdin from `/dev/null`, stdout/stderr to the daemon log,
and `pre_exec(|| { libc::setsid(); Ok(()) })` so the daemon leaves the
terminal's process group. Without `setsid` a later `^C` in the launching
shell kills the dashboard and closing the terminal SIGHUPs it — both
would look like random crashes. This is why `libc` is a dependency.

**Pidfile** at `$XDG_STATE_HOME/scout/dashboard.pid` holding
`{pid, port, started}` — but **one fixed path is unsafe once `--port`
exists.** Two live bugs came from assuming otherwise: `--port N`
alongside a default daemon reported "stale pidfile — cleared" and
trampled it, and `--stop` preferred the pidfile's pid over the probe's
and could SIGTERM the wrong process. The SIGTERM handler must be
async-signal-safe, so it `unlink`s its path unconditionally and cannot
read the file back to check whose it is. **The configured port keeps
`dashboard.pid`; any other port gets `dashboard-<port>.pid`.**

Liveness is decided by **probing `GET /api/status` on the recorded port
and checking for a scout marker field**, not by the pid. A recycled pid
is rare, but a pidfile surviving a `kill -9` is not, and pid-liveness is
awkward to check portably. The pidfile's job is to hold the pid for
`--stop`; the HTTP probe decides whether anything is there. The daemon
removes it on clean exit, and any command that probes a dead port
removes it as stale.

**Daemon log** at `$XDG_STATE_HOME/scout/dashboard.log` — bind errors,
panics, rotation notices. Truncated at start if over ~1 MB; diagnostics,
not history.

Each failure mode gets its own message: pidfile present and port
answering (print URL, exit 0); pidfile present and port dead (clear
stale pidfile, start); no pidfile and port busy (error naming the port,
since something that is not scout has it).

### The server

`include_str!` an embedded `dashboard.html`, one `TcpListener`, a thread
per connection, a hand-rolled request line + `Content-Length` parse,
`HTTP/1.0 Connection: close`. ~250 lines over `serde_json` and threads.

Two things that are not optional:

- **The `SO_REUSEADDR` dance, with a 3-attempt retry loop.** A
  foreground-only command could skip it; a restartable daemon on a
  well-known port cannot — accepted connections linger in `TIME_WAIT`
  bound to 13001 and block the rebind, and `--restart` hits precisely
  that window.
- **Bind 127.0.0.1 unconditionally, with no env override.** scout's
  payloads include arbitrary file contents from every repo you work in.
  There is no bind address but loopback worth supporting.

Shutdown on SIGTERM: stop accepting, remove the pidfile, exit. Nothing
to flush — the daemon only reads.

| Route | Returns |
|---|---|
| `GET /` | the embedded HTML |
| `GET /api/status` | model, endpoint, reachability, version, aggregates |
| `GET /api/history?since=<id>&limit=&tool=&via=&project=&failed=` | operations with their rows inlined |
| `GET /api/call/<id>` | one operation, resolved from *any* of its row ids |
| `GET /api/stats` | the `scout stats` table as JSON |
| `GET /api/stream` | SSE — live events from the channel |

Everything is `GET`; there is no state to mutate.

Two choices the routes encode:

- **`since` is an opaque row id, not an ordinal**, because scout's ids
  are not ordered. An id the server cannot find — a tab that slept
  through a rotation — returns a full page with `"resynced": true`
  rather than an error. That is the only behavior that lets a stale tab
  recover itself.
- **History inlines each operation's rows.** Rows are small (`input`
  caps at 300 chars), so the detail pane needs no second fetch;
  `/api/call/<id>` returns the identical object purely so `#call/<id>`
  deep links and `curl` work.

`/api/stream` is `text/event-stream`: hold the connection, write
`data: {...}\n\n` per event, flush. Polling cannot deliver a token
stream and SSE is the cheapest thing that can — ~30 lines over the
hand-rolled HTTP, no new dependency, and the browser's `EventSource`
reconnects on its own. Websockets would buy nothing; the channel is
one-directional by construction.

This makes connection lifetime unbounded, which the `HTTP/1.0
Connection: close` shape does not anticipate: the SSE route needs its own
path through the responder, and its handler thread lives as long as the
browser tab. Concurrent stream connections are capped (8) so a
tab-hoarding session cannot accumulate threads without bound.

The reader keeps an in-memory index — file offset plus inode and length —
and a poll re-reads only the tail. Because the daemon is long-lived and
writers rotate underneath it, it must **detect** rotation rather than
assume it: a changed inode, or a length below the last offset, forces a
reload of `calls.jsonl.1` + `calls.jsonl`. Getting this wrong is the most
likely source of a dashboard that silently stops updating after a few
days, so it has a test that rotates mid-read.

Polling: history every 2 s, status every 15 s.

`find_rounds` rides on `/api/call/<id>` only, never on `/api/history`. A
300-operation page carrying every round's pattern and keep list would
dwarf the summary rows it exists to deliver, and the pane fetches the one
operation it is painting anyway.

---

## 6. Token streaming — measured, then built

Streaming is the only part of this that reaches into scout's existing
behavior rather than sitting beside it, so it was tested before being
designed around. It is safe and it is free.

`client.rs::complete` originally sent `"stream": false`, then
`into_string()` → parse one JSON object → pull
`choices[0].message.content` and `usage`. Streaming replaces that with a
read loop over `data:` chunks, accumulating deltas.

Callers see no difference: `complete` still returns `(String, Value)`
after accumulating the full text. The stream is observed on the way past
and pushed to the channel. **Streaming is observability-only** — no
behavioral change for `check_output`, `grep`, `extract`, or `find`.

### Findings — LM Studio, `qwen/qwen3.6-35b-a3b`

| Question | Answer |
|---|---|
| Does `usage` survive streaming? | **Yes**, in the final chunk |
| Is `stream_options: {include_usage: true}` required? | **Yes** — omit it and usage is absent entirely |
| Do the numbers match non-streaming? | **Exactly** — 15/3/18 and 27/87/114, verified both ways |
| Latency cost? | **None** — 23.2 ms/token non-streaming vs 22.9 ms/token streaming |
| How do HTTP errors arrive? | As a normal 4xx + JSON body **before** any stream begins |

The error finding is the important one. A malformed request returns
`400 application/json`, so ureq's `Error::Status(code, resp)` arm fires
exactly as it does without streaming. **The whole `LlmError` taxonomy —
`EndpointUnavailable`, `Timeout`, `RequestFailed` — survives
unchanged.** Streaming introduces no new class of mid-stream failure
that the existing classification would mishandle. Re-verified after
implementation: a malformed request with `"stream": true` still answers
`400 Bad Request` with `Content-Type: application/json` and no event
stream at all.

Two implementation details the test surfaced:

- **The usage chunk carries `"choices": []`** — an empty array, not a
  populated one. A reader doing `chunk["choices"][0]["delta"]` on every
  chunk breaks on the last one. Match on the usage key first, then
  deltas.
- **~1 content delta per token** (86 deltas / 87 completion tokens),
  which is what makes coalescing worth it.

### The delta sink

The read loop hands deltas to a sink, not to a threaded `CallRecord`:

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
   caller installs no sink and `client.rs` never learns the concept, so
   a unit test cannot fire tokens into a developer's dashboard by
   forgetting a flag in a second place.
3. **The coalescer is testable without a socket.** The 50 ms buffering
   lives in `live.rs`, driven in tests by a collecting closure;
   `client.rs` only yields deltas as they arrive.
4. **`usage` never touches the sink.** It arrives in the final chunk and
   comes back in the return value, so `call.end` remains its only path
   to the dashboard and there is no second usage path to keep in
   agreement with the first.

The sink is **best-effort and must not block** — it runs inside the HTTP
read loop, where a slow consumer stalls the model call itself. The one
implementation scout ships is a buffer append plus an occasional
non-blocking `sendto`.

### Verdict, and the escape hatch

**Stream unconditionally.** No Heisenberg property, no divergence
between observed and unobserved runs. The extra work when nothing is
listening is 89 small JSON parses instead of one medium one — ~100 µs
against a 2000 ms call, 0.005%.

`[llm] stream = true` stays as a config escape hatch, defaulting on,
because this is verified on **LM Studio only**. ollama remains untested;
nothing here found a reason to doubt it and nothing here confirms it. If
it drops `include_usage` under streaming, `stream = false` is the
supported answer and `scout stats` keeps its numbers. Keep `false` a
fully supported path — this is a diagnostic feature, not a load-bearing
one.

Four things that landed differently from the plan:

- **`[DONE]` is not the only terminator worth honoring.** A stream is
  complete when it says `data: [DONE]` *or* when a chunk carries a
  non-null `finish_reason`; LM Studio sends both, in that order.
  Requiring only the former would make scout's correctness depend on a
  frame that is conventional rather than specified. EOF with neither is
  an error — a reply cut off mid-flight reads exactly like a short one
  to every caller above `client.rs`, which is the failure mode worth
  being loud about.
- **The truncation ladder was the wrong place to bound a token event.** A
  token payload is one unstructured string, so there is nothing for
  `fit_event` to shrink intelligently. The fix is to bound the
  *producer*: flush the buffer at 8 KiB whatever the timer says, and
  leave the ladder as the backstop it is.
- **The daemon stores no token text.** Fan out to SSE subscribers and
  forget. A partial reply held in the body cache would outlive a failed
  call looking like a whole one, and the token stream is landfill once
  the call is over. The browser keeps the partial instead, where it is
  labelled as one.
- **The pane appends, it does not repaint.** A repaint every 50 ms would
  fight the reader for scroll position and drop any text selected
  mid-read, so the first delta installs the response block and every
  delta after appends a text node to it.

---

## 7. Decisions

Settled, and recorded so they are not re-litigated:

- **Bodies on disk by default?** No. They go over the channel. The
  sidecar survives as opt-in.
- **`find` grouping in the log or the reader?** Both, at different
  fidelities. The log carries `op` so history groups a `find` into one
  row retroactively; the channel carries the per-round internals that
  make it interesting, live only.
- **Shared memory instead of the log?** No — measured, the log costs
  ~5–20 µs to write against a 1.58 ms floor on process spawn, and a
  tmpfs ring would be worse under memory pressure. The channel exists
  for bandwidth, not efficiency.
- **Does `usage` survive streaming?** Yes, at no latency cost. Stream
  unconditionally.
- **Cross-project or per-project?** All activity, always. `project` is a
  display column and an optional filter, never a scope.
- **Replay buffer for a dashboard opened mid-call?** No. It picks up
  from the next command; the partial run shows up from the log with its
  live detail missing.
- **Auto-start from a SessionStart hook?** Available, gated on
  `[dashboard] autostart`, default **off**. A plugin that opens a
  listening port without being asked is rude, and this one serves your
  source code.

### Where the design was wrong

Recorded because each was found by building rather than by reading, and
the pattern is worth trusting: nearly all came from an implementer
pushing back on the brief, and the one that did not (§4 below) is a
hazard that review invented and building never found.

1. **Byte accounting had no single capture point.** "Cheap to capture
   where both values are in hand" — no such point exists. Produced the
   `Ledger`, and the per-`op` summing rule.
2. **Grouping on `run` contradicted the entry-point table.** `scout mcp`
   is one process per session, so `run` would have collapsed a whole
   session into one row. Produced `op`.
3. **Adjacency grouping breaks under concurrency.** `spawn_blocking`
   dispatch interleaves rows from parallel tool calls, which is the
   ordinary case since agents batch independent calls. Produced
   id-keyed grouping.
4. **A `ts`/`as_u64()` hazard that never existed** — invented in review,
   not observed. Worth remembering as the counterexample: not every
   plausible-sounding hazard is real, and asserting one costs
   implementation time.
5. **The event envelope assumed every event describes a round-trip.** A
   `find.*` event describes an *operation* and spans zero, one, or
   several round-trips, so it has no row `id` to carry. The operation's
   own id goes in the `id` slot: `Ledger`'s `op` and every row `id` come
   from the same `stats::next_id` counter so they cannot collide, and
   the store keys on `op` regardless. Nothing downstream noticed,
   because the dashboard already grouped on `op`.
6. **The truncation ladder only understood strings.** Bodies are
   strings; `find`'s payloads are arrays, and the ladder had nothing to
   trim — a wide search would have hit the 64 KiB cap and lost the event
   entirely. It now halves the longest array repeatedly and sets
   `truncated: true`, so a shortened list is distinguishable from a
   complete one. The general lesson: a fail-open contract has to be
   re-checked against each new *shape* of payload, not just each new
   payload.
7. **"No config branch to reason about" and `[llm] stream` were the same
   paragraph.** Both are right, and the resolution is that the branch is
   allowed to exist in exactly one place: which *reader* consumes the
   response. Everything above `client.rs` — callers, the error taxonomy,
   the log, the `call.end` payload — has no idea which way the wire
   went, and the return value is the same pair either way. A flag that
   reached further than that would have been the thing to refuse.
8. **The streaming investigation cleared the phase without saying how a
   delta reaches the channel.** It measured latency, usage and error
   shape — the things that could have killed it — and left the only real
   design question unstated. The mirror of §4 above: a document can be
   thorough about the hazards it went looking for and silent about the
   decision that actually shapes the code.
9. **A fail-open ladder cannot fix an unbounded producer.** §6 said to
   re-check `fit_event` against each new payload shape, which was done —
   and the answer that time was that the ladder is the wrong layer.
   Refining §6: check each new shape against the ladder, and be willing
   to conclude the ladder is not where that shape belongs.
10. **"Mostly instrumentation at points that already compute the values"
    was half right** — the same shape of claim as §1, and wrong the same
    way. Two of `find`'s four streamed values live one function away
    from where the brief pointed: the rerank's keeps, scores and `why`
    belong to `grep::rerank`, and the reflect verdict was discarded
    inside `find::reflect` the moment `next_patterns` decided not to act
    on it. Cheap to fix; worth distrusting the phrasing next time it
    appears.

### One payoff worth recording

The first real `find` streamed through the round tab strip —
`find "where does the dashboard bind its port"`, six `find.*` events
across two rounds — showed that its seeded `(?i)port` pattern matched
**301** hits against a `degenerate_hit_cap` of **300** and was discarded
whole. Nothing in the log says so, and nothing in the result hints at
it. One glance at the pane does. That is the argument for the live
channel existing, in a single observation.

A related edge, observed on that same run: **a round can legitimately
produce no `find.rerank`.** When a refined round's hits are all already
in the union, `find` returns the prior payload rather than spending a
call to reproduce it — so round 2 has patterns and hits and nothing
else. It renders through the same partial-view path as a dashboard that
joined mid-`find`, which is the right answer: "absent" and "not yet"
look identical and both are ordinary.

---

## Appendix: phase shorthand

Source comments and `dashboard.html` refer to build phases by number.
They are historical labels, not a roadmap, but they are load-bearing in
comments like "a pre-P3 payload" — meaning a record written before that
phase existed.

| | Delivered |
|---|---|
| **P1** | the enriched record — `op`/`run`/`id`, `via`, `tool`, `input`, `outcome`, the `Ledger` and byte accounting, rotation |
| **P2** | the detached daemon and the HTTP server, over the log alone |
| **P3** | the live channel — unix datagram, SSE, prompt/response bodies, in-flight calls |
| **P4** | `find` refinement events and the round tab strip |
| **P5** | token streaming — `call.token` deltas coalesced at 50 ms, `[llm] stream` |
| **P6** | bodies sidecar (`persist_bodies`) — optional, unbuilt |
| **P7** | auto-start from SessionStart — optional, unbuilt |
