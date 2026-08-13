# scout — working notes

## Commit cadence

Commit a feature once there is reasonable confidence it is finished — don't sit
on completed work waiting for certainty. "Reasonable confidence" means the
change does what it set out to do and the relevant tests pass, not that every
possible follow-up has been resolved. Leftover polish, open questions, and
follow-on ideas belong in `TODO.md` or a later commit, not in an
indefinitely-growing working tree.

Corollary: keep unrelated in-progress work out of the commit. Stage the files
that belong to the feature rather than committing everything dirty.

## Never hardcode a fully-qualified MCP tool name

The plugin is the only supported Claude Code install, so the prefix is
`mcp__plugin_<plugin>_<server>__` — but do not bake that literal into a hook, a
preset, or the SessionStart guidance. It is derived from the plugin and server
names rather than declared anywhere we control at read time, and a stale literal
fails silently: the model is told to call a tool that does not exist, and the
redirect dead-ends.

Refer to tools by their unqualified name (`check_output`, `extract`, `grep`) and
point the model at `ToolSearch` to resolve the full name and load the schema.
That lookup is free and cannot go stale.

## Tests

`make test` runs `cargo test` only — the shell suites are not wired into it, so
run them directly. All three matter when touching hooks:

- `cargo test` — Rust unit + integration tests. Prefer the `check_output` MCP
  tool over running it as a bare Bash command; a PreToolUse hook redirects it
  anyway so raw build output stays out of context.
- `bash tests/test-prefer-local-llm.sh` — redirect hook (deny shape, escape
  hatch, redirect text).
- `bash tests/test-shell-safety.sh` — command classification.

Known pre-existing failure, one in each shell suite: the `[missing binary]` case
expects a `missing-binary` log reason but gets `endpoint-unreachable`
(prefer-local-llm) or `parse-failure` (shell-safety). Both hooks fall back to
`command -v scout` when the configured path is absent, so they find a real
binary on `PATH` and never reach the missing-binary branch. Test-isolation gap,
not a hook bug — but it means both suites sit at n-1, so check the count rather
than assuming green.
