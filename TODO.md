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
