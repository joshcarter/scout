# scout design docs

Design records for the parts of scout whose current shape is not obvious
from the code. Each one is written to explain *why* — what was measured,
what was tried and rejected, and which decisions should not be
re-litigated. Source comments cite these by file and section number.

| Doc | Covers |
|---|---|
| [search-cli.md](search-cli.md) | `scout grep` / `find` / `edit` — terminal output contract, exit codes, search filters, the intent-only search pipeline, editor dispatch |
| [wrap-watch.md](wrap-watch.md) | Spec (not yet implemented): `wrap` and `watch` — run-and-condense for arbitrary verbose commands, delta-summarized long-running processes, and the raw spool that makes filtered output recoverable |
| [dashboard.md](dashboard.md) | `scout dashboard` and the telemetry under it — the call record, the durable-log-plus-live-channel split, token streaming |
| [command-matching.md](command-matching.md) | Why the build/test redirect hook lexes shell commands instead of matching a regex |
| [plugin-packaging.md](plugin-packaging.md) | How Claude Code and Grok Build each load a plugin, measured; why the payload is shaped the way it is |
| [grok-hooks.md](grok-hooks.md) | Why Grok still does not run plugin PreToolUse hooks, and why the answer is one dual-shape script plus a tiny registration, not a second copy |
| [distribution.md](distribution.md) | How a stranger gets scout: git marketplace vs crates.io, official catalogs, why the two channels do not install each other |

Operational notes — how to build, what a session picks up after an edit,
which test suites exist — live in [`../CLAUDE.md`](../CLAUDE.md). Open
work lives in [`../TODO.md`](../TODO.md).
