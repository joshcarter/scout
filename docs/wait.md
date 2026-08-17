# `wait` — blocking on jobs that finish

Status: **spec — not yet implemented.** Written from a measured session
(§1) rather than from a guess about what the model does, because the
first version of this idea was filed as part of `watch` and the
measurement is what separated them.

## §1 What waiting actually costs

### §1.1 The workload

`~/Projects/vail_ec_sim` is a Go discrete-event simulator driven from
Jupyter notebooks. Its verification gate, per that repo's
`docs/fanout-worklist.md` ground rule 2, is to re-execute the affected
notebooks and diff their cell outputs:

    .venv/bin/python -m jupyter nbconvert --to notebook --execute \
        --inplace notebooks/<nb>.ipynb

Each run takes 5–60 minutes — the `--ExecutePreprocessor.timeout` values
in the transcripts range from 400 to 3600 seconds — and a re-baseline
runs twenty-odd of them, several in flight at once. The model starts
them redirected to a log, then waits.

**Waiting is where the money goes.** Not the notebook output, which is
small; not the execution, which is the machine's problem. The waiting.

### §1.2 The measurement

Two concurrent sessions in that project, 2026-08-17 17:00–18:41 UTC
(~100 minutes), from `~/.claude/scout-shell-safety.jsonl` and
`~/.local/state/scout/calls.jsonl`:

| | |
|---|---|
| PreToolUse hook invocations | 363 |
| of those, `shell_safety` LLM classifications | 286 (351 s of local-model time, 260k local input tokens) |
| commands whose entire purpose was waiting | 32 |
| `sleep` literals in those, summed | 5,822 s — an undercount, since 13 are `until … do sleep 20; done` loops whose body repeats |
| decisions that reached the user as a prompt (`ask`/`deny`) | 15 |

And the cloud side, from the two session transcripts (743 assistant
turns, input including cache reads):

| | |
|---|---|
| median input tokens per turn | **168,822** |
| p90 | 281,950 |
| total across both sessions | 127.8M |

So each of those 32 waiting turns cost roughly 169k input tokens — call
it **~5.4M input tokens spent to learn "not done yet."** The payload
returned was `running`, or `waited`, or `tick`.

That is the whole finding. A wait turn is not cheap because its output
is short; a turn costs what the conversation costs to re-read, and this
conversation costs 169k tokens. The only way to make waiting cheaper is
to **do it fewer times**, which means one turn per *completion* instead
of one turn per *interval*.

### §1.3 The idiom is re-invented every time, and it flaps

There is no canonical way to wait, so the model improvises a new one
every turn. Verbatim, from the window above:

    until [ -s $S/br.txt ]; do sleep 20; done; echo ready
    until [ -s $S/br.txt ]; do sleep 30; done; echo READY
    sleep 120; echo waited
    sleep 300; echo tick
    sleep 600; S=…; ls -la $S/br.txt 2>/dev/null || echo running
    while pgrep -f "nbconvert.*scheduler_tuning" >/dev/null; do sleep 15; done
    date; ps -eo pid,lstart,etimes,comm | grep -E "jupyter|python3" | head -5

Each variant is a fresh string, so each pays a fresh classification
(`shell_safety` median 1,273 ms, p90 3,329 ms over its 1,476 lifetime
calls) on the interactive path. Worse, the classifier is not stable
across the variants — the *same* wait loop, three times inside 32
seconds:

    18:32:29  allow-shadow   until [ -s …/br.txt ]; do sleep 20; done; echo ready
    18:32:43  deny           until [ -s …/br.txt ]; do sleep 20; done; S=…
    18:33:01  allow-shadow   until [ -s …/br.txt ]; do sleep 30; done; echo ready

A `deny` here means "not confidently safe, fall through" — the hook
never blocks — so what the user sees is a permission prompt appearing at
random, mid-sweep, on a command that was auto-approved twenty seconds
earlier. Fifteen of those in a hundred minutes, on an otherwise
unattended job. That is the symptom that started this doc; the token
burn in §1.2 is the larger problem underneath it.

## §2 Why this is not `watch`

`docs/wrap-watch.md` §4 specs `watch` for dev servers and `--watch` test
runners. It is a good fit for that and a bad fit for this, and the
distinction is worth stating once so the two do not get merged again:

| | `watch` (wrap-watch.md §4) | `wait` (this doc) |
|---|---|---|
| Subject | an unbounded stream | a job that ends |
| Completion | there isn't one | the entire point |
| Question asked | "anything new since last time?" | "is it done, and did it pass?" |
| Interaction | poll, get a delta | block, get a verdict |
| Cost driver | scrollback volume | **turn count** |
| Healthy state | quiet, no output | still running |

Applying `watch`'s `check_watch` to a notebook sweep makes each poll
cheap (zero new output → no model call) without making polls **rarer**,
and §1.2 says rarity is the only lever that matters. A free poll that
still costs a 169k-token turn has not solved anything.

The two share plumbing — server-owned child processes, the `setsid`
process-group discipline, the raw spool of `docs/wrap-watch.md` §2 — and
should share code. They should not share a tool.

## §3 The design

### §3.1 `wait` is `wrap` deferred

The cleanest framing, and the one that keeps the new surface small:
a job is a `wrap` call whose payload is delivered later. Same capture,
same spool, same preset, same schema — only the delivery is detached.
That gives `wait` a payload contract that is already specified and
already argued (`docs/wrap-watch.md` §3) rather than inventing a third
one.

### §3.2 Tools

    wrap(command, question?, detach: true)
      → {job_id, raw_path, started_at}          — returns immediately

    wait(job_ids?, until?, timeout_s?, question?)
      → {done:     [{job_id, label, …wrap payload}],
         pending:  [{job_id, label, elapsed_s, tail_line}],
         timed_out: bool}

    jobs()            → the same {done, pending} shape, non-blocking
    cancel(job_id)    → kill the group; not preset-backed, registered like `ping`

`job_ids` omitted means **all jobs**, which is the case that matters:
one call drains every completion from a twenty-notebook sweep. `until`
is `"any"` (default — return as soon as one job finishes, so a failure
surfaces early) or `"all"` (return when the batch is done, the
minimum-turn choice for a homogeneous sweep).

`label` defaults to a short derivation of the command (the notebook
name, here) so a ten-job payload is readable without echoing ten command
lines.

### §3.3 The blocking contract

`wait` blocks server-side until its condition is met or `timeout_s`
elapses. Three rules make that safe:

1. **A timeout return is honest and free.** `{done: [], pending: [...],
   timed_out: true}` — no model call, no summary, just bookkeeping. The
   same principle as `check_watch`'s zero-new-output rule.
2. **`timeout_s` is capped** by `[wait] max_block_seconds`, because a
   blocked MCP call is bounded by the *harness's* tool timeout and
   overrunning it turns a clean wait into a spurious error. Default
   300 s, which is conservative.
3. **A blocked call is an unsteerable session.** While `wait` is
   blocking, the user cannot redirect the model. This is the real cost
   of the approach and the reason the default cap is modest: it should
   be raised deliberately, by someone who knows the sweep is
   unattended, not inherited from a default.

Turn arithmetic for a 20-notebook sweep, 20 min each, 4 concurrent
(~100 min wall clock):

| Approach | Wait turns | Input tokens (at 169k/turn) |
|---|---|---|
| today's `sleep`/`until` improvisation | ~32 | ~5.4M |
| `wait(until:"any")`, 300 s cap | ~20 | ~3.4M |
| `wait(until:"all")`, 300 s cap | ~20 | ~3.4M |
| `wait(until:"all")`, cap raised to 1800 s | **~4** | **~0.7M** |

The cap, not the tool, is what buys the last order of magnitude — so
the doc that ships with this must say so, and `[wait] max_block_seconds`
must be raisable in `config.toml` alongside whatever the harness needs
(`MCP_TOOL_TIMEOUT` in Claude Code's `settings.json`; verify the actual
default before publishing a recommended value — see §8).

### §3.4 The payload is a verdict, not a log

Each completed job costs exactly one local-model call regardless of how
large its log is, and returns `wrap`'s schema: `{exit_code, summary,
answer, notable, lines_total, lines_dropped, raw_path}`. The raw log
stays on disk and recoverable per `docs/wrap-watch.md` §2.4.

This is the part the harness cannot do for you (§6.3), and on this
workload it is worth as much as the turn savings: the model currently
writes a fresh `repin.py` / `asserts.py` / `mdedit.py` into its
scratchpad every session to do by hand what a preset should do once.

### §3.5 Fail-open

Identical to every other scout surface. If the local model is down, a
completed job returns `{exit_code, raw_path, degraded: "<reason>"}` with
the summary elided. A broken summarizer must never cost the caller the
job's result — and here it must also never cost the caller the
*knowledge that the job finished*, which is strictly more important.

### §3.6 Config

    [wait]
    max_jobs = 16              # not watch's 4 — a sweep runs twenty
    max_block_seconds = 300
    idle_timeout_seconds = 0   # none: a quiet notebook is normal
    wall_timeout_seconds = 0   # none: the caller sets the deadline

`watch`'s `max_watches = 4` is right for dev servers and wrong here; the
separate section is one more reason these are separate features.

## §4 The gate preset, where the domain lives

For this workload the generic `wrap` summary is not quite the verdict.
The notebook gate has rules a preset can carry and the cloud model
currently re-derives every session:

- **Exit code is not the test.** `nbconvert` does not write the file
  when a cell assert fails, so a run can report success while the
  in-tree outputs are stale. The gate must check that the `.ipynb`
  was actually rewritten.
- **Which cells raised**, with the assert message, not the traceback.
- **Diff `.cells[].outputs[].text` against HEAD**, ignoring ipykernel
  PIDs → `bit-identical`, or the list of numbers that moved.

That last line is the repo's STOP condition: if numbers move where the
physics says they should not, stop and report — do not update the
asserts. It caught three real bugs on 2026-07-15. It is also exactly the
kind of diff-and-condense a local model is good at.

So: `presets/job_notebook.toml` (or a `question:` passed through to
`wrap`'s preset — the cheaper first cut), returning
`{ok, executed, cells_failed, outputs_changed, numbers_moved, summary}`.
Worth prototyping as a `question` string before it earns a preset file.

## §5 Cheap wins available today, without building anything

These are worth doing regardless of whether `wait` ships, and two of
them are the difference between "annoying" and "fine" this week:

1. **`~/.claude/scout-shell-safety.shadow` is on** (since 2026-08-15).
   Every one of the 286 classifications in §1.2 cost ~1.3 s on the
   interactive path and had its verdict discarded — the harness decided.
   That is the experiment in `TODO.md` ("Decide whether the shell-safety
   hook still earns its place") running as designed, and it wants a full
   week, so this is a note rather than a recommendation to turn it off.
   But the latency being felt right now is partly instrumentation.
2. **A wait-shape fast-path in `shell-safety.sh`.** A command whose
   entire body is `sleep` / `until` / `while` over `[ -s file ]`,
   `pgrep`, or `test`, plus `echo` / `cat` / `ls`, is structurally safe
   and should never reach step 3. It sits next to the existing
   trusted-plugin-script fast-path, costs ~20 lines, kills the flapping
   in §1.3, and stays useful even after `wait` ships (the model will
   still improvise occasionally).
3. **A project rule in `vail_ec_sim/CLAUDE.md`:** start notebook runs
   with `run_in_background` and wait for the completion notification;
   never hand-roll a `sleep` loop. The session is *already* writing into
   the harness task directory (`…/tasks/<id>.output`) and then polling
   it by hand with `sleep 200; cat` — that is a usage problem, and
   fixing it is free.

## §6 Rejected

### §6.1 Making `check_watch` cheaper

Covered in §2: a free poll still costs a turn, and the turn is the cost.

### §6.2 Teaching the model to wait better

`docs/wrap-watch.md` §1 already argued this: persuasion loses,
interception wins. Guidance that says "sleep less often" competes with a
trained prior at exactly the moment the model is improvising, and §1.3
shows what improvisation looks like — seven distinct idioms in a hundred
minutes. §5.3 is the exception that proves it: a project rule pointing
at a *tool the harness already provides* is worth trying, because it
redirects rather than exhorts.

### §6.3 Leaning entirely on the harness's `run_in_background`

Claude Code re-invokes the model when a background task exits, which is
a genuinely free wait — better than blocking, since the turn happens
only at completion and the session stays steerable. Three things it does
not do:

- It wakes the model **per job**, not per batch. Twenty notebooks is
  twenty wakeups; `wait(until:"all")` is one.
- It delivers a **log path, not a verdict**. Everything in §4 still has
  to happen in the cloud model's context, by hand, every time.
- It is **Claude Code only**. scout targets Grok Build as well
  (`docs/grok-hooks.md`), and a harness-specific wait is not portable.

The honest conclusion is that these compose rather than compete: use the
harness's notification where it exists, and `wait` for batch drain and
verdict rendering. If the eventual measurement shows `until:"all"` is
what the model actually reaches for, the blocking half of §3.3 is the
part that earned its keep; if the model prefers to be woken, the
`jobs()` + verdict half is. Ship both, measure, cut the loser.

### §6.4 Folding this into `watch`

The original filing. §2 is the argument. Different subject, different
question, different cost driver, different config defaults; shared
plumbing is not shared semantics.

## §7 Rollout order

1. §5.2 (wait-shape fast-path) and §5.3 (project rule) — today, no
   dependency on anything below.
2. `wrap` + the raw spool (`docs/wrap-watch.md` §2, §3). `wait` is
   `wrap` deferred, so it cannot precede it.
3. `wait`: detached `wrap`, the four tools, `[wait]` config. Reuse the
   `watch` process-ownership code if `watch` landed first; write it here
   if it did not.
4. Guidance: `scripts/session-context.sh` and `skills/scout/SKILL.md`
   learn "long job → detach + `wait`", with the batch-drain framing
   stated explicitly, since that is the non-obvious part.
5. §4's gate — first as a `question` string, promoted to a preset only
   if it proves out.

## §8 Open questions

- **What is Claude Code's actual MCP tool-call timeout, and is it
  raisable per project?** §3.3's whole payoff depends on the answer.
  Measure it; do not trust a remembered default.
- **Does a blocked MCP call remain interruptible?** If Ctrl-C during a
  `wait` leaves an orphaned notebook process, §3.1's `setsid` discipline
  needs a signal-handling story before the cap can safely be raised.
- **Should `wait` return partial output for pending jobs?** `tail_line`
  is specced as a hedge against "is it wedged or just slow." It may be
  either insufficient (the model asks anyway, costing a turn) or
  unnecessary (elapsed time answers it). Cheap to add later, so ship
  without and see.
- **Does the model reach for `until:"all"` unprompted?** If it defaults
  to `"any"` out of caution, the turn savings in §3.3 do not
  materialize and the guidance in §7.4 has to be more directive.
