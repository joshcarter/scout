# scout

A local-LLM scout for coding agents. The cloud model does the heavy
lifting; scout hands the small problems to a local model (any
OpenAI-compatible endpoint): classifying build/test output, screening
shell commands before they run, and answering targeted questions about
files and search results without dumping them into the big model's
context.

**Status: in progress.** Extraction from [ct] is underway — see
`PLAN.md`. The MCP server exposes `check_output`, `extract` and `grep`
(plus `ping` for wiring checks); the hooks that steer Claude Code
toward them land next.

## Shape

- **Rust binary** `scout` (crates.io package: `scout-llm`; the bare
  `scout` crate name is an unrelated fuzzy finder).
- **Claude Code plugin**: `.claude-plugin/plugin.json`, an MCP stdio
  server (`scout mcp`), SessionStart bootstrap + guidance injection,
  and (coming) PreToolUse hooks that steer the agent toward the local
  tools.
- **CLI**: first-class human surface, same code path as the MCP tools:
  `scout grep <pattern> "<intent>"`, `scout find "<question>"`,
  `scout extract <file> "<question>"`, `scout check "<build cmd>"`,
  `scout task "<prompt>"`, plus `scout run --preset ...` for hooks and
  `scout stats`.

`find` is the intent-only end of the search spectrum: state what you
want and the local model guesses the patterns, scout runs every guess
itself and reranks the union against your question. It is CLI-only for
now and needs a configured model; `grep` covers pattern-only and
pattern-plus-intent, and degrades to a plain search with no model at
all.

Search is self-contained: gitignore-aware walking and matching come
from ripgrep's libraries, so there is no dependency on an installed
`rg`/`grep`. Small inputs (a file under ~200 lines, a hit list of 8 or
fewer) skip the local model entirely and are returned whole, so those
paths work before any `~/.config/scout/config.toml` exists. Filter
tunables live in that same file under `[extract]`, `[grep]`, `[cli]`
and `[find]`.

## Install

**As a Claude Code plugin** (recommended — hooks, MCP server, and
binary bootstrap in one step):

```
/plugin marketplace add joshcarter/scout
/plugin install scout@scout
```

On the next session start, `scripts/ensure-binary.sh` installs the
binary into `${CLAUDE_PLUGIN_DATA}/bin` and seeds a default config if
you don't have one. Edit `[llm].endpoint` and `[llm].model` to match
your local LLM host.

**The CLI** (use scout from a terminal, like a smarter `ack`):

```sh
make install-cli    # binary to ~/.local/bin, config if missing
```

Plugin and CLI share everything that matters: config and presets live
in `${XDG_CONFIG_HOME:-~/.config}/scout/` (`config.toml`, `presets/`),
and both surfaces run the same code path. Only the binary is
duplicated (the plugin manages its own copy in `CLAUDE_PLUGIN_DATA` so
it can keep it current without touching your PATH).

**Standalone** (no plugin — e.g. for other MCP clients): `make install`
additionally registers the MCP server with Claude Code at user scope.
Don't combine it with the plugin, or the server registers twice.

## Development

```sh
cargo build --release
claude --plugin-dir ~/Projects/scout    # try the plugin in a session
```

The SessionStart hook (`scripts/ensure-binary.sh`) installs the binary
into `${CLAUDE_PLUGIN_DATA}/bin/scout`, preferring a local
`target/release/scout` in a dev checkout, falling back to
`cargo install scout-llm`. The MCP server declaration in
`.claude-plugin/plugin.json` and the `bin/scout` PATH shim both point
at that installed copy. (Repo-root `.mcp.json` is dev tooling for this
checkout, not part of the plugin.) Note: in a brand-new install the MCP
server may fail to start on the very first session (the bootstrap hook
hasn't run yet); it comes up on the next session.

Verify the MCP handshake without Claude:

```sh
printf '%s\n' \
  '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"test","version":"0"}}}' \
  '{"jsonrpc":"2.0","method":"notifications/initialized"}' \
  '{"jsonrpc":"2.0","id":2,"method":"tools/list"}' \
  | ./target/release/scout mcp
```

[ct]: the code-intelligence daemon this feature is being extracted from
