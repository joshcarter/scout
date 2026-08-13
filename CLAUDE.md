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

## What a session actually picks up after you edit

Installed from a **directory** marketplace (`extraKnownMarketplaces` → `source:
directory`, path = this repo), `CLAUDE_PLUGIN_ROOT` resolves to the working tree
itself. So hooks, `scripts/`, and `presets/` are read live from the checkout —
edit, restart the session, done. There is a snapshot under
`~/.claude/plugins/cache/<marketplace>/<plugin>/<version>/`, but it is not what
runs; `installed_plugins.json`'s `installPath` points there and is misleading.

The **binary** is the exception: it is a copy in `$CLAUDE_PLUGIN_DATA/bin`, so
`cargo build --release` has to be followed by a restart for
`scripts/ensure-binary.sh` to re-copy it. That refresh is mtime-driven — see the
comment there for why a version compare alone silently keeps a stale binary.

To confirm which copy ran, `claude --debug-file <path> -p hi` then grep for
`ensure-binary`; the char count of the reported `additionalContext` distinguishes
repo from snapshot when they differ.

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
