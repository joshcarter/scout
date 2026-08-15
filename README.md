# scout

A local-LLM scout for coding agents. The cloud model does the heavy
lifting; scout hands the small problems to a local model (any
OpenAI-compatible endpoint): classifying build/test output, screening
shell commands before they run, and answering targeted questions about
files and search results without dumping them into the big model's
context.

**Status: working, pre-1.0.** The MCP server exposes `check_output`,
`extract` and `grep` (plus `ping` for wiring checks); the PreToolUse
hooks that steer Claude Code toward them are in place; the CLI covers
`grep` / `find` / `edit` / `extract` / `check` / `task`; and `scout
dashboard` serves a local web view of everything the local model has
been asked. Design records for each of those live in [`docs/`](docs/).

## Shape

- **Rust binary** `scout` (crates.io package: `scout-llm`; the bare
  `scout` crate name is an unrelated fuzzy finder).
- **Claude Code plugin**: `.claude-plugin/plugin.json`, an MCP stdio
  server (`scout mcp`), SessionStart bootstrap + guidance injection,
  and PreToolUse hooks that steer the agent toward the local tools.
  This is the only supported way into Claude Code.
- **CLI**: first-class human surface, same code path as the MCP tools:
  `scout grep <pattern> "<intent>"`, `scout find "<question>"`,
  `scout edit "<question>"`, `scout extract <file> "<question>"`,
  `scout check "<build cmd>"`, `scout task "<prompt>"`, plus
  `scout run --preset ...` for hooks and `scout stats`.

`find` is the intent-only end of the search spectrum: state what you
want and the local model guesses the patterns, scout runs every guess
itself and reranks the union against your question. Your question's own
distinctive words are searched too, alongside the guesses — the word you
typed is evidence, a synonym is a hypothesis. Then it checks its work:
one more small call asks whether the kept hits actually answer the
question, and when they don't, it re-searches for the identifiers
visible in them (a comment naming `draw_waterslide` is a pointer to the
answer, not the answer). Rounds are capped by `[find] max_attempts`
(3 by default, `--attempts` overrides). It is CLI-only for now and needs
a configured model; `grep` covers pattern-only and pattern-plus-intent,
and degrades to a plain search with no model at all.

`edit` fronts both pipelines and ends in `$EDITOR`, choosing which one
by how many arguments you give it: `scout edit "<question>"` runs
`find`, `scout edit <pattern> "<intent>"` runs the reranked `grep`, and
`scout edit -p <pattern>` runs a plain search. A single hit opens
straight away, positioned at the match; several are listed with numbers
and a `[1-n, a=all, q=quit]` prompt. `a` opens every file at once — or,
for the vi family, a real quickfix list (`vim -q`) so `:cn` walks the
hits. Positioning follows the editor: vi/vim/nvim, emacs/emacsclient,
helix, VS Code and its forks, and zed are known by name; anything else
is opened plainly with the line number printed first.

Search is self-contained: gitignore-aware walking and matching come
from ripgrep's libraries, so there is no dependency on an installed
`rg`/`grep`. Small inputs (a file under ~200 lines, a hit list of 8 or
fewer) skip the local model entirely and are returned whole, so those
paths work before any `~/.config/scout/config.toml` exists. Filter
tunables live in that same file under `[extract]`, `[grep]`, `[cli]`
and `[find]`.

## Install

The plugin payload lives in `plugins/scout/` and carries the binary at
`plugins/scout/bin/scout`. Build it first — the binary is gitignored, so
a fresh clone has an empty `bin/`:

```sh
make build
```

**Claude Code:**

```
/plugin marketplace add <path-to-checkout>
/plugin install scout@scout
```

**Grok Build:**

```sh
grok plugin marketplace add <path-to-checkout>
grok plugin install scout --trust
```

Both harnesses install from the local checkout — **not** from GitHub.
`/plugin marketplace add joshcarter/scout` looks like it should work and
does not: the payload's binary is gitignored, so a marketplace fetched
from GitHub arrives with an empty `bin/`, and the MCP server declared as
`${CLAUDE_PLUGIN_ROOT}/bin/scout` then points at a file that is not
there. The hooks resolve the same path and go quiet the same way.
Installing from a directory also makes `CLAUDE_PLUGIN_ROOT` resolve into
this working tree, so edits to `hooks/`, `scripts/` and `skills/` take
effect on the next session with no reinstall.

The MCP server is declared as `${CLAUDE_PLUGIN_ROOT}/bin/scout`, which
both harnesses expand to the installed payload, so it comes up on the
first session with nothing to bootstrap. On first run scout writes a
default `${XDG_CONFIG_HOME:-~/.config}/scout/config.toml` — edit
`[llm].endpoint` and `[llm].model` to match your local LLM host.

One difference worth knowing: **the hooks are Claude-only.** The
build/test redirect and the shell-safety auto-allow both ride on
PreToolUse, and Grok Build 1.0.3 does not execute plugin hooks at all
(see `docs/plugin-packaging.md` §2.5). Under Grok you get the MCP tools and
the `scout` skill; the automatic steering is Claude's.

**The CLI** (use scout from a terminal, like a smarter `ack`):

```sh
make install    # binary to ~/.local/bin
```

Plugin and CLI are independent — install either, or both. They share
everything that matters: config and preset overrides live in
`${XDG_CONFIG_HOME:-~/.config}/scout/` (`config.toml`, `presets/`), and
both surfaces run the same code path. Only the binary is duplicated:
the plugin keeps its own copy in the payload so it stays current
without touching your PATH.

**Other MCP clients**: the binary is a stdio MCP server — run
`scout mcp` and point your client at it. There's no install wrapper for
this; in a coding agent, use the plugin.

## Development

```sh
make build                              # binary + refresh the plugin payload
claude --plugin-dir ~/Projects/scout    # try the plugin in a session
```

`make build` compiles to `target/release/scout` and copies it to
`plugins/scout/bin/scout`, which is what both manifests point at. Use
`make build`, not a bare `cargo build --release` — the latter leaves the
payload stale. The copy goes through a temp file and `mv` on purpose:
overwriting in place fails with `ETXTBSY` whenever an MCP server is
running from the destination, which under a directory marketplace is the
normal case.

Two manifests, identical content: Grok reads
`plugins/scout/plugin.json`, Claude reads
`plugins/scout/.claude-plugin/plugin.json`. Keep them in step. Both are
inside `plugins/scout/`, which is also why the payload lives there rather
than at the repo root: a `.mcp.json` sitting inside a plugin folder gets
attributed to the plugin, so any project-level MCP config you keep for
working on this checkout belongs above it. See
[`docs/plugin-packaging.md`](docs/plugin-packaging.md) for the full
picture of how each harness loads this.

Verify the MCP handshake without Claude:

```sh
printf '%s\n' \
  '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"test","version":"0"}}}' \
  '{"jsonrpc":"2.0","method":"notifications/initialized"}' \
  '{"jsonrpc":"2.0","id":2,"method":"tools/list"}' \
  | ./target/release/scout mcp
```
