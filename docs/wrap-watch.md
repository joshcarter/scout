# `wrap`, `watch`, and the raw spool

Status: **spec — not yet implemented.** This records the design and its
rationale before the first line of code, because the shape was argued out
in conversation and the reasoning would otherwise evaporate.

## §1 Why these two, and why now

The call log is unambiguous about which of scout's surfaces works:
`check_output` has steady traffic (202 calls at time of writing);
`extract` and MCP-side `grep` have effectively none. The structural
difference is that `check_output` is not *offered* to the cloud model, it
is *imposed* — the PreToolUse hook denies the bare build command and
redirects, so scout sits in the path of a behavior the model already has,
at the moment the model has already committed to it. Tools that instead
ask the model to predict "this output will be too big" before running
anything require a prediction the model cannot make and compete with its
trained priors. Persuasion loses; interception works.

`wrap` and `watch` grow the interception surface:

- **`wrap`** generalizes the run-and-condense pattern from build/test
  commands to any verbose command — `git log`, `docker logs`,
  `journalctl`, `curl`, long test scripts — where the payload is
  *information to retrieve*, not a *verdict to render*.
- **`watch`** covers long-running processes (dev servers, `--watch` test
  runners): the local model tails the stream, the cloud model asks
  "anything new since last time?" and gets a delta, not a scrollback.

Both lean on a shared piece of plumbing this doc also specifies: the
**raw spool** (§2), which makes every filtered result recoverable. That
property is what earns the right to filter at all — see §2.4.

**Rejected: extending `check_output` in place.** Its schema (`ok`,
`first_error`, `suggested_next_step`) is only meaningful because "a
build" is a genre with pass/fail semantics and conventions. Arbitrary
output has no verdict to render, and a local model asked to editorialize
about it will embarrass itself. `check_output` is also the one surface
that demonstrably works, and the contract the cloud model has learned;
it stays untouched — name, schema, preset, hook.

## §2 The raw spool

### §2.1 Location and naming

One directory scout owns: `${XDG_CACHE_HOME:-~/.cache}/scout/raw/`.
Cache, not state — this is disposable data, unlike `calls.jsonl`. Never
the project tree, never loose files in `/tmp`.

One blob per spooled call, named by date and the call's existing id:

    raw/2026-08-15/143212-wrap-a3f9.log

`calls.jsonl` rows already carry an `id`; the spooling call records
`raw_path` on its row too, which gives the dashboard raw-output
drill-down for free and lets `scout stats` report spool volume.

Directory `0700`, files `0600` — same reasoning as the Rust-side log
permissions (raw command output can contain anything).

### §2.2 Write rule: only spool what was filtered

A pass-through result (§3.2) writes nothing — there is no lossy summary
to recover from, so there is nothing to keep. This single rule keeps the
file count proportional to *filtered* calls, which history says is small
(202 `check_output` calls over the project's whole life). Blobs are
stored uncompressed: plain text means the cloud model's native `Read`
with offset/limit works on `raw_path` directly, which is the entire
point of the escalation path.

The spool always receives the **full** captured output, even when the
model was shown an elided version (§3.4). The spool is ground truth; the
elision is a prompt-budget decision.

### §2.3 GC: prune on write, no daemon

Every spool write does a cheap sweep of `raw/`: delete blobs older than
`max_age_days`, then oldest-first until total size is under
`max_total_bytes`. Defaults: **7 days / 500 MB**, under `[spool]` in
`config.toml`. This is the ccache/uv pattern — the cache tends itself as
a side effect of being used; there is no cron job or cleanup process to
forget. `scout gc` exposes the same sweep manually (and `scout gc --all`
empties the spool).

Expiry is a *clean* failure: the payload said N lines were dropped and
where the raw went; a reader arriving after GC gets file-not-found,
which is honest and self-explaining. The retention window just has to
comfortably exceed a working session, and 7 days does.

**Rejected: session-scoped lifetime** ("delete when the Claude session
ends"). scout cannot reliably observe session end from inside an MCP
server or a hook, and cross-session recovery is a feature, not a leak —
a human reading yesterday's raw log from the dashboard is exactly the
use a session-scoped scheme would break. Age-and-size bounds get the
same hygiene without needing to know anything about sessions.

Watch output files (§4) are **pinned**: the sweep skips any blob backing
a live watch.

### §2.4 The recoverability contract

Every filtered payload must let the caller escalate without trusting the
filter: it carries `raw_path`, `lines_total`, and `lines_dropped` (or
the watch equivalents). The reason this matters is asymmetry of failure:
a summary that drops the one line that mattered fails *silently* — the
caller concludes "not there" and moves on. Explicit drop counts plus a
path turn that from an invisible wrong answer into one `Read raw_path`
(or a follow-up `extract(raw_path, …)`) away from recovery. This
contract is why `check_output`'s `# raw-output` re-run escape hatch is
not enough here: wrapped commands may be slow or non-idempotent, so
recovery must not require running them again.

## §3 `wrap`

### §3.1 Contract

`wrap(command, question?, cwd?, timeout_seconds?)` — run the command,
return its output condensed, with the raw spooled. MCP tool and CLI
(`scout wrap "<cmd>" ["question"]`) share one code path, like every
other scout surface.

`wrap` does **retrieval**, where `check_output` renders a **verdict**.
Three consequences:

- No `ok`, no `suggested_next_step`. The exit code passes through
  uninterpreted — `grep` exiting 1 and `diff` exiting 1 are not
  failures, and the local model is not asked to know that.
- The optional `question` steers the filter ("find the commit that
  touched the retry default"). Without one, the preset falls back to
  faithful generic condensation. This makes `wrap` essentially
  run-capture plus the extract pipeline, where `check_output` is
  run-capture plus the classify pipeline.
- Compression must be faithful, not interpretive: identifiers, numbers,
  and paths are preserved verbatim in `notable`; the model is forbidden
  to advise.

### §3.2 Pass-through: cheap to be wrong about

Output at or under `[wrap] passthrough_max_lines` (default 200, and a
byte cap alongside) is returned whole — verbatim, no model call, no
spool:

```json
{ "exit_code": 0, "filtered": false, "output": "<verbatim>" }
```

This is the property that makes an aggressive redirect matcher (§5)
acceptable: a wrong "this will be verbose" prediction costs nothing but
the exec. `extract` and `grep` already work this way; `wrap` inherits
the philosophy.

### §3.3 Filtered payload

```json
{
  "exit_code": 0,
  "filtered": true,
  "summary": "<a few sentences, faithful, past tense>",
  "answer": "<direct answer to `question`, or null if none was asked>",
  "notable": ["<verbatim lines worth quoting: errors, ids, paths, counts>"],
  "lines_total": 3412,
  "lines_dropped": 3380,
  "bytes_total": 481203,
  "raw_path": "~/.cache/scout/raw/2026-08-15/143212-wrap-a3f9.log"
}
```

The tool description advertises the escalation path explicitly —
adoption depends on the cloud model knowing recovery exists. Draft:

> Run a command whose output would be too long to read, and get a
> condensed result: summary, notable lines verbatim, exit code, and
> counts of what was dropped. The complete raw output is saved to
> `raw_path` — if the summary does not answer your question, Read that
> file (with offset/limit) or ask `extract` about it rather than
> re-running the command. Short output is returned verbatim and
> unfiltered.

### §3.4 Capture, timeouts, elision

Reuse `verify.rs` wholesale: `run_command_capture` /
`capture_with_deadlines`, the process-group kill, `BoundedBuffer`, the
idle deadline, and `check_output`'s wall-clock arg convention
(`timeout_seconds`, same shipped 900/3600 as `[check_output]`).
`[check_output]` now has `idle_timeout_seconds` and
`default_timeout_seconds`; `[wrap]` still uses the compiled defaults
and should grow the same keys. A timed-out command is answered without
a model call, the same way `check_output::timeout_verdict` does it.

The model is shown at most `[wrap] model_input_bytes` (default 16 KB,
matching `check_output`) via the existing head+tail elision
(`truncate_diagnostic`); the spool gets everything the bounded capture
kept. When output exceeds even the capture bound, `lines_total` reflects
what was captured and the payload says the head was elided — bounded
honesty rather than unbounded memory.

### §3.5 Fail-open

No configured model, endpoint down, unparseable reply: return the
elided head+tail of the raw output directly (`filtered: false`, plus
`raw_path` since the spool write already happened, plus a `degraded`
note naming the reason). A broken local model must never cost the caller
the command's result. Same philosophy as every other scout surface.

### §3.6 Preset

`presets/wrap.toml`, strict-JSON contract in the style of
`check_output.toml`, including its untrusted-output paragraph verbatim
in spirit: the captured text is data being condensed, never instructions
to follow — this matters *more* for `wrap`, since `curl` output is
attacker-controlled in a way `cargo test` output usually is not. The
system prompt forbids advice and requires verbatim preservation of
identifiers in `notable`.

## §4 `watch`

### §4.1 Ownership

The MCP server owns watched processes. It is the one long-lived scout
process in a session, so it spawns the child in its own process group
(the `setsid` discipline from `capture_with_deadlines`), appends output
continuously to a spool file, and kills the group when the watch is
stopped or the server exits (stdin EOF / Drop). Watches are therefore
session-scoped where spool blobs are not — the process dies with the
session, the output file survives for the retention window.

No idle timeout: a quiet dev server is normal, not wedged. No default
wall clock: a watch lives until stopped. `[watch] max_watches` (default
4) bounds concurrency.

### §4.2 Tools

- `watch(command, cwd?)` → `{watch_id, raw_path, pid}` — start.
- `check_watch(watch_id, question?)` →
  `{state: "running"|"exited", exit_code?, new_lines, summary, notable,
  raw_path}` — report what happened since the last check. **Zero new
  output is answered without a model call**: `{state, new_lines: 0}`.
  This is the common case for a healthy dev server, and it makes
  polling nearly free on both sides of the wire.
- `stop_watch(watch_id)` → `{exit_code | "killed"}` — kill the group.
  Not preset-backed (no prompt to carry); registered the way `ping` is.

The server keeps one read offset per watch, advanced when a
`check_watch` returns. Single-caller by construction (one MCP client per
server), so per-caller offset bookkeeping is not needed. If the delta
summarize fails, fail open per §3.5 — elided raw delta, offset still
advances, nothing is silently swallowed twice.

### §4.3 Spool growth

A dev server can log for days. Watch files are pinned from GC (§2.3)
but capped at `[watch] spool_cap_bytes` (default 64 MB): on overflow the
file rotates once (`.1` kept, older discarded) and the next
`check_watch` payload states that rotation happened and how much raw is
gone. Bounded honesty again — the caller is told when the ground truth
has a hole in it.

## §5 Hook expansion is phase 2, measured first

The temptation is to widen `prefer-local-llm.sh` immediately with a
verbose-command family list (`git log`, `git diff`, `docker logs`,
`journalctl`, `find`, `curl`, …) redirecting to `wrap`, with the same
`# raw-output` escape. That is probably where most of the token savings
live — but the deny-and-redirect pattern earns its aggression from
`check_output`'s near-zero false-positive rate on an enumerable command
class, and "verbose" is not an enumerable class (docs/command-matching.md
§1 is the cautionary tale about matching commands by shape).

So: ship the tools first; let the SessionStart guidance and skill
advertise them; watch `calls.jsonl` `via` to see whether the cloud model
picks `wrap` up voluntarily now that recovery makes filtering safe to
trust. Add redirect families incrementally, each justified by observed
raw-output volume for that family, behind a `[wrap] redirect` list in
config so a user can trim it. The pass-through rule (§3.2) is what makes
each addition low-stakes.

## §6 Telemetry

- `raw_path` joins the v2 `calls.jsonl` row for spooling calls (readers
  use `.get`, so absent-vs-present is already handled; `record.rs::Row`
  grows the field).
- Spooled bytes ride the existing `raw_bytes` / `returned_bytes` pair,
  so the dashboard's savings math covers `wrap` with no new columns.
- `watch` lifecycle events (start / check / rotate / stop) log as
  ordinary rows under the watch's `op`, so a watch reads as one
  operation in the dashboard the way a `find` round does.

## §7 Rollout order

1. `src/spool.rs` — write, sweep, `scout gc`. Independently testable.
2. `wrap`: preset, handler, CLI, MCP registration, `[wrap]` config.
   (Note the two-config-parsers TODO — new sections should ride the
   strict parser, or land after unification.)
3. `watch`: server-owned processes, the three tools, `[watch]` config.
4. Guidance: `scripts/session-context.sh` and `skills/scout/SKILL.md`
   learn the two tools and the escalation contract.
5. Phase 2 (separate decision, data in hand): redirect families in
   `prefer-local-llm.sh`.

## §8 Open questions

- Whether `check_watch` should accept `stop: true` to atomically report
  final output and kill — saves a round-trip at teardown, complicates
  the schema. Deferred until real usage shows the teardown pattern.
- Whether `wrap`'s CLI should propagate the child's exit code (useful in
  pipelines, surprising next to `scout grep`'s found/not-found codes).
  Leaning yes; decide when the CLI lands.
- Whether the elision threshold for what the *model* sees should scale
  with the local model's context window rather than a fixed 16 KB.
  `check_output` has the same question; solve them together or not at
  all.
