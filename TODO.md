# Shell Script Madness

There's a bunch of complicated shell scripts with a bunch of regexps.
Is that the better call vs. invoking a scout command to run the logic?
For example `sed` gets invoked all over the place--is that really
better than invoking `scout`?

# Publish to crates.io

See `docs/distribution.md`.

# Grok Build compatibility

See `docs/grok-hooks.md`.

# `LlmError::RequestFailed` is too coarse to classify

`RequestFailed(String)` covers an HTTP status error, a mid-call I/O
failure, and an unreadable or unparseable response body alike. P1 needs
to tell those apart to set `outcome.kind`, and the only handle available
is the message text — so `client.rs` decides `http_error` by
`msg.starts_with("HTTP ")`.

It is contained (the sniff sits next to where the string is minted, in
the same file) and it works, but it is string-matching on a value the
same function formatted moments earlier. The real fix is to split the
variant so the taxonomy is carried in the type. Worth doing the next
time that enum is touched for another reason rather than on its own.

# A misconfigured model name fails silently

A typo in `[llm] model` runs happily against whatever the host has
loaded, and nothing in scout notices. Measured against LM Studio: a
model name that is not in `/v1/models` at all — `no-such-model-xyz`,
`qwen/qwen9.9-nonexistent` — returns HTTP 200, is served by the
currently-loaded model, and reports that substitute in the response's
`model` field. Streaming and non-streaming behave identically.

Note this is specifically the *invalid* name case. A valid-but-unloaded
model is expected to JIT load, which is correct behavior and is not what
this is about (untested here — testing it would have evicted the loaded
model).

`check_endpoint` can't catch it: it does `GET /models`, which succeeds
regardless. So the failure is invisible — results just come from the
wrong model, quietly.

The cheap fix is already in hand: **the response reports the model that
actually ran, and scout throws it away** (`client.rs`, `complete` reads
`choices[0].message.content` and `usage` and ignores `data["model"]`).
Compare it to the configured name and warn once on mismatch. That is
precise rather than heuristic — a JIT-loaded valid model comes back
matching, so the warning fires only on real substitution. Optionally
also validate `[llm] model` against `/v1/models` at config load.

Worth carrying the observed model into the call log too, since
`docs/dashboard.md` §3 already records a `model` field per call — it
should be the model that ran, not the one requested.

# Decide whether the shell-safety hook still earns its place

It predates Claude Code's auto-approve mode and may now be redundant. The
removal is scoped and mechanical; what is not settled is whether auto mode
covers the case the hook exists for — commands whose effect depends on
`$(...)` or `$VAR`, which a static allowlist cannot decide. There is no way to
ask the harness offline, so shadow mode (committed) is the instrument: it
classifies and logs as usual but withholds the allow.

To run the experiment: `touch ~/.claude/scout-shell-safety.shadow`, work
normally for a week, then count `allow-shadow` in
`~/.claude/scout-shell-safety.jsonl`. Each of those is a command the hook would
have approved, so a prompt seen on one is a case auto mode did not cover. Give
it a full week — the log has multi-day gaps, and daily volume swings with what
is being worked on (421 allows over one 8-day window, but 232 of them on a
single day).

The substitution fast-path has since been deleted, which makes this experiment
*better*, not stale: every command carrying a `$()` now reaches step 3 instead
of some of them being auto-approved before the model saw them. The shadow count
therefore measures the hook's whole contribution rather than a subset. It also
means a fresh run is not comparable to counts taken before that deletion — the
old numbers understate step 3's traffic.

Worth knowing before reading a result: the fast path fired 34 times ever, all
of them payloads written while testing it. On the day it shipped, 589 hook
invocations and 274 commands with expansions produced zero fast-path hits. If
the shadow experiment shows a similarly small number, that is the hook's real
value and not a measurement artifact.

If it goes: delete `hooks/shell-safety.sh`, its `hooks.json` block,
`tests/test-shell-safety.sh`, and the auto-allow paragraph in
`scripts/session-context.sh`. Then `presets/shell_safety.toml` has no caller —
drop it, its `include_str!` in `src/presets/mod.rs` (the "8 built-in presets"
comment becomes 7), its two tests in `src/presets/tests.rs`, the
`"check_output" | "shell_safety"` arm in `src/stats.rs`, and the
`[shell_safety] deny` block in `config.example.toml`, which only this hook
reads.

The telemetry coupling is the part worth thinking about rather than deleting on
sight: `shell-safety.sh` is the only writer of `SCOUT_VIA=hook`, so `via:hook`
becomes a category nothing produces (`VIA_HOOK`/`KNOWN_VIA` in `src/stats.rs`,
used nowhere else). The 8 MB rotation cap in `src/stats.rs` is sized on this
hook firing per Bash call, the dashboard's `hook traffic` filter defaults to
off to keep its volume out of the way, and `src/live.rs` cites it as the
latency case to protect. All four rationales expire with the hook.

Docs to sweep: `README.md`, `CLAUDE.md`, `docs/plugin-packaging.md`,
`docs/dashboard.md`, `docs/command-matching.md`, and the contrast comments in
`hooks/prefer-local-llm.sh` explaining why that hook is the one that denies.

# Make the GitHub install path work

`README.md` now tells both harnesses to install from a local checkout,
because `/plugin marketplace add joshcarter/scout` produces a payload that
cannot run: `plugins/scout/bin/scout` is gitignored, so the fetched snapshot
has an empty `bin/`, and both the MCP server and the hooks resolve
`${CLAUDE_PLUGIN_ROOT}/bin/scout` to a file that is not there. The failure is
silent from the user's side — the hooks simply stop firing, with nothing
logged, because the script never runs.

Anyone who finds scout on GitHub will try the marketplace line first, so this
needs a real answer before the repo is useful to someone who is not building
it. Options, roughly in order of appeal:

- Publish a release with per-platform binaries and have the plugin fetch on
  first use. Needs a bootstrap step that survives ETXTBSY (see CLAUDE.md) and
  some notion of verifying what was downloaded.
- Commit binaries. Simple, and wrong for an 11 MB artifact per platform.
- Ship a `SessionStart` bootstrap that builds from source when `bin/` is empty
  and cargo is present. Works for the developer audience scout has today, and
  degrades to a clear error rather than silence for everyone else.

Whichever way it goes, the fix should also make the empty-`bin/` case *loud*:
a hook that cannot find its binary should say so once, not disappear.

# `call.end` blanks a live row's preset until the next log poll

Found while verifying P5's detail pane. `dashboard.html`'s
`liveRowFromEvent` fills `preset`/`tool`/`via`/`input` from the event, and
`emit_end` (`src/live.rs`) carries none of them — so the `Object.assign(row,
fresh)` on `call.end` overwrites the values `call.start` supplied with empty
strings. The visible effect is small and short-lived: the detail pane's
`preset` row and the call tab strip's label go blank for the second or two
until `/api/history` replaces the operation from the log. Pre-dates P5.

Two ways to fix, and the second is probably right: have `call.end` carry the
identifying fields too (more bytes on every event, for data that cannot have
changed), or have the merge treat an absent field as "unchanged" rather than
as "empty" — which is what the rest of the reconciliation already does.

# Hook timeouts: killed calls are still missing from `calls.jsonl`

The knobs exist (env + config.toml). The hook audit logs now record a
killed subprocess as `timeout` (exit 124 from `timeout(1)`), not
`parse-failure` / `classify-failure`, so the hook-side count is no longer
blind.

`calls.jsonl` still has no row: `scout` has no SIGTERM handler, so the
hook's `timeout` wrapper kills the process between `emit_start` and the
`emit_end`/`log()` pair. The dashboard's `abandoned` rows surface it live;
nothing durable on the call log does. A handler that writes the parked
ledger row on SIGTERM is the remaining piece.

Worth designing rather than tuning: the timeout is also the worst-case stall
before the user sees a permission prompt, which is presumably why it is as
tight as it is. Raising it buys auto-approvals and costs latency on the
failure path, and the right tradeoff probably is not one number for all
presets. Note the dashboard's TTL sweep already removes the *accounting*
consequence of a kill, so this is no longer urgent — only wrong.

# `handle_stream` can park a thread on a client that stops reading

`dashboard.rs`'s SSE handler writes with a blocking `write_all`. A browser tab
that stops reading — suspended, throttled, a laptop lid — leaves the handler
blocked in the kernel until the socket buffer drains, holding one of
`MAX_STREAMS = 8` slots.

This is the last way to hold a stream slot for a long time; the `Full` vs
`Disconnected` fix removed the other one. Lower stakes than that was: a
genuinely dead client eventually errors and releases, so this is a stall rather
than a permanent leak. A write timeout on the socket, or a bounded write with
the same deadline shape the header phase now uses, would close it.

# `prefer-local-llm.sh` has no `jq` guard

`suggest-scout.sh` checks `command -v jq` and exits cleanly when it is missing.
`prefer-local-llm.sh` does not: without `jq` it parses the payload as empty,
logs `matched:false`, and exits 0. Fail-open, so nothing breaks — but the hook
becomes a silent no-op on a machine without `jq`, which is the same
disappear-without-a-word failure mode as the `CLAUDE_PLUGIN_ROOT` bug. It
should say so once rather than vanish.

Worth doing at the same time: the hooks depend on `jq`, `sed`, `awk` and
`python3` (the last for `[shell_safety] deny` and both hooks' config.toml
timeout keys), and nothing declares that anywhere a user would read.

# The hook audit logs are still created at the umask

`~/.claude/scout-intercepts.jsonl` and `~/.claude/scout-shell-safety.jsonl` are
written by the bash hooks, so the `0600`/`0700` work on the Rust side did not
reach them. They carry every Bash command the agent ran, with its cwd. Same
reasoning as `calls.jsonl`: low stakes on a single-user box, cheap to fix, and
the kind of thing that reads badly in a public repo.

Neither log rotates, either, while `calls.jsonl` rotates at 8 MB. They get a
row per Bash tool call, so they grow faster than the file that has a cap.

# `grep` should drop content, never existence

The intent filter currently decides which hits the caller learns about
at all. That gives it the wrong kind of authority: its dangerous failure
is the false negative — it drops the one relevant hit, the payload reads
"nothing relevant", and the caller concludes *not there* and moves on.
Nothing prompts an escalation, because absence of evidence looks exactly
like evidence of absence. (This is also why a spooled raw-hit-list
fallback, per docs/wrap-watch.md §2, buys nothing here — a fallback
nobody is prompted to consult isn't recoverability. And unlike `wrap`,
grep doesn't need one: a search is deterministic, fast, and side-effect
free, so the raw is always one `--no-filter` re-run away. The missing
piece is the *trigger*, and it has to ride in the payload itself.)

Three changes, in order of importance:

- **Restrict the filter's authority to quoting, not existence.** It
  selects which hits get snippets and context; every other hit still
  appears as a bare location, grouped compactly — `dropped: 89 in
  tests/, 61 in vendor/, 18 in src/ (src/retry.rs:41,88,
  src/client.rs:203, …)`. Locations are nearly free (a couple hundred
  hits compress to a few hundred tokens without content), and a
  mis-filter becomes *visible*: a retry-related search with
  `src/retry.rs` sitting in the dropped pile is one Read away from
  recovery instead of a silent wrong answer.

- **Zero-kept results get a special contract.** "0 of 214 hits
  relevant" is the maximum-damage payload and precisely the case where
  the filter is likeliest wrong. Never return that verdict bare: when
  kept-count is zero (or tiny relative to total), skip the confident
  summary and return the grouped location digest instead, with an
  explicit line — "the intent filter kept nothing; if you expected a
  match, these are the N locations." An instruction embedded in the
  tool result at the moment of decision is the one nudge that reliably
  lands on the cloud model, unlike the advisory hook.

- **Bias the filter prompt asymmetrically.** Drop only what it is
  *confident* is irrelevant; keep on uncertainty. False positives cost
  tokens; false negatives cost correctness. The current framing ("keep
  what matches the intent") optimizes the wrong side.

Touches `presets/grep.toml`, the hit-assembly side of `src/grep.rs` /
`src/select.rs`, and the terminal contract in docs/search-cli.md (the
CLI presumably wants the dropped-location digest on stderr, not mixed
into the hit list). The MCP tool description should also start
advertising the digest, so the model knows dropped-but-listed is a thing
it can act on.

# A preset override can still declare a *wrong* schema

`inherit_mcp_schema` fixes the case where an override says nothing about
`[preset.input_schema]`. An override that declares one which disagrees with
what the Rust handler reads is still accepted, and still produces a tool the
model calls incorrectly.

There is nothing declarative to validate against: the handlers read arguments
ad hoc (`args["pattern"]`), so the schema and the contract are only related by
convention. The cheap 80% is a startup assertion that each MCP tool's
advertised `required` covers a small hardcoded per-tool list — which is exactly
what `tests/mcp_stdio.rs::each_tools_required_args_match_what_its_handler_reads`
already pins for the built-ins, so promoting it to a runtime check is mostly a
move. The real fix is typed argument structs the schema can be derived from,
which is a bigger change and would also delete the hand-written schemas in the
preset TOMLs.
