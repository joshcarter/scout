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
  `scout grep <pattern> "<intent>"`, `scout extract <file>
  "<question>"`, `scout check "<build cmd>"`, `scout task "<prompt>"`,
  plus `scout run --preset ...` for hooks and `scout stats`.

Search is self-contained: gitignore-aware walking and matching come
from ripgrep's libraries, so there is no dependency on an installed
`rg`/`grep`. Small inputs (a file under ~200 lines, a hit list of 8 or
fewer) skip the local model entirely and are returned whole, so those
paths work before any `~/.config/scout/config.toml` exists. Filter
tunables live in that same file under `[extract]` and `[grep]`.

## Development

```sh
cargo build --release
claude --plugin-dir ~/Projects/scout    # try the plugin in a session
```

The SessionStart hook (`scripts/ensure-binary.sh`) installs the binary
into `${CLAUDE_PLUGIN_DATA}/bin/scout`, preferring a local
`target/release/scout` in a dev checkout, falling back to
`cargo install scout-llm`. `.mcp.json` and the `bin/scout` PATH shim
both point at that installed copy. Note: in a brand-new install the MCP
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
