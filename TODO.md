# Claude guidance to use appropriate tools

Make sure Claude gets what it needs to know about scout's features,
and guidance to favor using them. Builds, unit test runs, etc. should
be going through scout with some consistency.

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
`SPEC-dashboard.md` §3 already records a `model` field per call — it
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

Docs to sweep: `README.md`, `CLAUDE.md`, `SPEC-grok-plugin.md`,
`SPEC-dashboard.md`, `PLAN.md`, and the contrast comments in
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
