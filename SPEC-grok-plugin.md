# Spec: Grok Build plugin packaging

**Status:** findings from a Grok Build 1.0.3 session in this checkout
(2026-08-14). Nothing in this file is implemented. Claude Code remains
the supported install; this is what has to change if scout should also
install and boot cleanly under Grok.

**Goal:** a Grok user can add the marketplace, install `scout` by name,
get a working MCP server on the next session, and have the same
steering (binary bootstrap, build/test redirect, usage guidance) Claude
Code already gets — without breaking the Claude plugin.

**Non-goals:** changing scout's MCP tool schemas; making Grok's native
`grep` / `read_file` go away; a Grok-only fork of the binary.

---

## 1. What we tried

| Step | Result |
|---|---|
| Open a Grok session in this repo | `/mcp` listed a **scout** plugin. Expanding it showed **ct** tools, not scout's. |
| `grok mcp doctor` | Two servers attributed to `plugin: scout`: `scout` (spawn failed) and `ct` (46 tools, healthy). |
| `grok plugin marketplace add $HOME/Projects/scout` | Succeeded. Wrote `[[marketplace.sources]] name = "scout"` / `path = …/Projects/scout` into `~/.grok/config.toml`. |
| `grok plugin install scout --trust` | `Error: no marketplace plugin named "scout" in any registered marketplace.` |
| `grok plugin install scout@scout --trust` | `Error: No marketplace plugin named "scout" in "scout".` |
| `grok plugin install $HOME/Projects/scout --trust` | Installed. Copied the **entire** working tree (including `target/`, ~3.9G) to `~/.grok/installed-plugins/scout-<id>`. |
| Copy `target/release/scout` into the path `grok mcp doctor` printed | `scout` MCP came up: handshake OK, 4 tools. New session required. |

Grok had also already picked scout up via Claude compatibility
(`~/.claude/settings.json` `enabledPlugins.scout@scout` +
`extraKnownMarketplaces` pointing at this directory). That path loaded
the **Claude plugin cache snapshot**
(`~/.claude/plugins/cache/scout/scout/0.1.0`), not the live checkout.
`grok plugin list` was empty in that state; `grok inspect` still showed
`scout (user, enabled)`.

---

## 2. Findings

### 2.1 Marketplace `source: "./"` is dropped

`.claude-plugin/marketplace.json` is the Claude-shaped single-repo
index:

```json
{ "name": "scout", "source": "./", "description": "…" }
```

Claude Code accepts a plugin that *is* the marketplace root. Grok
registers the marketplace and then publishes **zero** plugins from it.
`grok plugin list --json --available` never included a `scout`
marketplace entry, which is why name-based install failed even though
the source was in `config.toml`.

Probed against Grok 1.0.3 (local temp marketplaces, add → list →
remove):

| `source` value | Cataloged? |
|---|---|
| `"./plugins/demo"` | yes |
| `{ "type": "local", "path": "./plugins/demo" }` | yes |
| same object form inside `.claude-plugin/marketplace.json` | yes |
| `"./"`, `"."`, `""` | no |
| `{ "type": "local", "path": "./" }` or `"path": "."` | no |

`.grok-plugin/marketplace.json` vs `.claude-plugin/marketplace.json`
did not matter. The rejected shape is **plugin at marketplace root**.
Grok's own docs assume a `plugins/<name>/` subdirectory.

`grok plugin install <name>` looks up the **plugin** name, not the
marketplace name. Even with a working index the command is
`install scout`, not `install <marketplace>`.

### 2.2 Path install snapshots the whole repo

`grok plugin install /abs/path --trust` does not behave like Claude's
directory marketplace. Claude leaves `CLAUDE_PLUGIN_ROOT` on the
working tree. Grok copies into `~/.grok/installed-plugins/scout-<id>`.

There is no exclude list. This checkout's `target/` came along
(~3.9G). The copy is not updated when you rebuild in the tree; it is a
snapshot of whatever was on disk at install time (hooks, `plugin.json`,
and a stale `target/release/scout` if one happened to be there).

### 2.3 MCP command points at an empty Grok plugin-data dir

`.claude-plugin/plugin.json` declares:

```json
"mcpServers": {
  "scout": {
    "command": "${CLAUDE_PLUGIN_DATA}/bin/scout",
    "args": ["mcp"]
  }
}
```

Claude expands `CLAUDE_PLUGIN_DATA` to
`~/.claude/plugins/data/scout-scout`. SessionStart
(`scripts/ensure-binary.sh`) copies `target/release/scout` there
before the session is useful. That is why Claude works.

Grok expands the same substitution to a **different** path:

```
~/.grok/plugin-data/user/<id>/scout/bin/scout
```

The `<id>` is per plugin-install identity, not stable across
install methods. Claude-compat used `9e028837`; the path install used
`e3540977`. The directory did not exist until we created it by hand.
`grok mcp doctor` then reported:

```
✗ command not found (~/.grok/plugin-data/user/<id>/scout/bin/scout)
```

A working `scout` was already on `PATH` (`~/.local/bin/scout`). Grok
never used it, because the plugin command is a hard-coded plugin-data
path.

Grok documents `GROK_PLUGIN_DATA` / `GROK_PLUGIN_ROOT` for hooks and
says it also sets the `CLAUDE_PLUGIN_*` aliases. Observed behavior:
plugin-data was never populated, so either SessionStart did not run,
or it ran too late (after MCP spawn), or it ran with the script's
fallback (`$HOME/.claude/plugins/data/scout-scout`) instead of Grok's
data dir. We did not distinguish those three; the spawn failure is the
same.

Grok's plugin docs also say plugins deliver files, not native
binaries. A SessionStart-copied Rust binary is a Claude-shaped
bootstrap Grok does not currently honor on the MCP path.

### 2.4 Project `.mcp.json` is attributed to the plugin

Repo-root `.mcp.json` is a **project** server for working *on* this
checkout:

```json
{ "mcpServers": { "ct": { "command": "ct", "args": ["mcp-serve"] } } }
```

`CLAUDE.md` already says this is a ct entry, nothing of scout's.
Claude Code treats it as project MCP.

Grok's plugin loader treats a plugin folder's `.mcp.json` as **plugin**
MCP. Because the plugin *is* the repo (and the path-install copy
includes that file), `/mcp` under the scout plugin listed the healthy
`ct` server (46 tools) next to the failing `scout` server. That is
the "I expanded scout and saw ct tools" symptom.

`grok inspect` reported the plugin as providing `1` MCP (the
`plugin.json` `scout` server) while `grok mcp doctor` reported `2`
servers from `plugin: scout`. Both views are consistent with "load
`plugin.json` *and* `.mcp.json`."

### 2.5 Hook matchers name Claude tools

`hooks/hooks.json` matches `Bash` for `shell-safety.sh` and
`prefer-local-llm.sh`. Grok's shell tool is `run_terminal_command`.
Grok matchers test the real tool name. Those PreToolUse hooks will not
fire on Grok unless the matcher includes that name.

SessionStart uses `${CLAUDE_PLUGIN_ROOT}/scripts/ensure-binary.sh`.
Grok inspect listed the plugin hook as a `file` hook pointing at
`hooks/hooks.json`; we did not see a populated Grok plugin-data dir
after session start, so bootstrap + guidance injection did not take
effect.

`bin/scout` (the PATH shim) only execs
`$CLAUDE_PLUGIN_DATA/bin/scout` or
`~/.claude/plugins/data/scout-scout/bin/scout`. It does not look at
`$GROK_PLUGIN_DATA` or a real binary on `PATH`.

### 2.6 Usage guidance never enters a Grok session

Claude gets the "prefer scout over raw Bash/Read/Grep for token-heavy
work" table from SessionStart `additionalContext` in
`ensure-binary.sh`. Grok did not receive that block.

What Grok *did* load: `CLAUDE.md` (commit cadence, don't hardcode MCP
names, prefer `check_output` over bare `cargo test`). That is not a
search-routing table, and scout's own table only recommends
`grep(pattern, intent)` when a raw pattern would be too noisy anyway.

Grok's native `grep` / `read_file` / `list_dir` are always in the tool
list with full schemas. MCP tools are a second hop (`search_tool` then
`use_tool`). Combined with a dead or undiscovered scout server, the
model uses built-in search. That is a packaging + guidance problem,
not a model-preference mystery.

---

## 3. Suggested resolution

Keep the Claude plugin at repo root as it is. Add a Grok-shaped
payload next to it. Do not make Grok install the Rust crate.

### 3.1 Slim plugin directory + Grok marketplace index

Add a subdirectory Grok will catalog, and an index that points at it:

```
.grok-plugin/marketplace.json          # source: "./plugins/scout"
plugins/scout/
  plugin.json                          # metadata; MCP via .mcp.json
  .mcp.json                            # scout only (see §3.2)
  hooks/hooks.json                     # Grok-safe matchers (§3.4)
  scripts/ensure-binary.sh             # optional; must not be the only boot path
  bin/scout                            # shim that knows GROK_PLUGIN_DATA
```

`.grok-plugin/marketplace.json`:

```json
{
  "name": "scout",
  "owner": { "name": "Josh Carter", "url": "https://github.com/joshcarter" },
  "plugins": [
    {
      "name": "scout",
      "description": "Local-LLM scout: classifies build/test output, screens shell commands, and answers targeted code questions with a local model so the cloud model doesn't have to.",
      "source": { "type": "local", "path": "./plugins/scout" }
    }
  ]
}
```

Then `grok plugin marketplace add $HOME/Projects/scout` followed by
`grok plugin install scout --trust` should resolve. The installed copy
is hooks + manifests, not `target/`.

Claude's `.claude-plugin/marketplace.json` can stay on `source: "./"`
so `/plugin install scout@scout` is unchanged.

### 3.2 Split project MCP from plugin MCP

| File | Audience | Servers |
|---|---|---|
| repo-root `.mcp.json` | anyone working *on* scout | `ct` only |
| `plugins/scout/.mcp.json` | Grok plugin payload | `scout` only |
| `.claude-plugin/plugin.json` `mcpServers` | Claude plugin payload | `scout` only (already) |

Do not copy repo-root `.mcp.json` into `plugins/scout/`. Once Grok
installs the slim dir, `/mcp` under scout should list scout's tools,
and `ct` should show up as project MCP when the cwd is this repo.

### 3.3 MCP command that does not depend on SessionStart

Grok will not reliably have `$CLAUDE_PLUGIN_DATA/bin/scout` at spawn.
The command that already works on this machine is `scout` on `PATH`
(`~/.local/bin/scout`, same binary `cargo build --release` produces).

Recommended `plugins/scout/.mcp.json`:

```json
{
  "mcpServers": {
    "scout": {
      "command": "scout",
      "args": ["mcp"]
    }
  }
}
```

Claude can keep `${CLAUDE_PLUGIN_DATA}/bin/scout` in
`.claude-plugin/plugin.json`. Two declarations, two boot stories:

- **Claude:** SessionStart copies into plugin-data; MCP uses that copy.
- **Grok:** MCP uses `PATH`. `cargo install --path .` / the existing
  `~/.local/bin/scout` is the binary. Optional SessionStart can still
  refresh `$GROK_PLUGIN_DATA/bin/scout` for the shim, but it must not
  be the only way the server starts.

If a single `plugin.json` must serve both harnesses, prefer `scout` on
`PATH` and make `ensure-binary.sh` install *onto* `PATH` (or document
`cargo install --path .`) rather than only into plugin-data. A missing
plugin-data path is a hard spawn failure; a PATH lookup at least
fails with a diagnosable `command not found` that `grok mcp doctor`
already explains.

`scripts/ensure-binary.sh` should, if it runs under Grok:

1. Treat `GROK_PLUGIN_DATA` as a first-class dest (not only
   `CLAUDE_PLUGIN_DATA`).
2. Keep writing the Claude dest so a dual-harness machine stays in
   sync.
3. Not use `~/.claude/plugins/data/scout-scout` as the Grok fallback.

`bin/scout` should exec, in order: `$GROK_PLUGIN_DATA/bin/scout`,
`$CLAUDE_PLUGIN_DATA/bin/scout`, the Claude dest, then a real binary
found via `command -v` that is **not** the shim (avoid recursion).

### 3.4 Hook matchers and guidance Grok will actually load

In the Grok plugin's `hooks/hooks.json` (or the shared file if one
file must serve both):

- PreToolUse matcher: `Bash|run_terminal_command` (Grok's shell tool
  name). If search is ever redirected, Grok's names are `grep`,
  `read_file`, `list_dir` — not `Grep` / `Read` / `Glob`.
- SessionStart may still run `ensure-binary.sh`, but guidance cannot
  live only in `additionalContext`. Grok did not inject that block.

Put the delegation table where Grok auto-loads project instructions:
an `AGENTS.md` (or a short addition to `CLAUDE.md`, which Grok already
reads). A `plugins/scout` `SKILL.md` with a strong trigger is a second
copy for sessions outside this repo.

The table that already exists in `ensure-binary.sh` is the right text:
prefer `check_output` / `extract` / `grep(pattern, intent)` for
token-heavy work; `# raw-output` bypass; ToolSearch / `search_tool`
for deferred MCP names. Do not tell the model to use scout for every
identifier search — that is not what the Claude guidance says either.

### 3.5 Local workaround until the above ships

No repo change required:

```bash
grok mcp add scout -- scout mcp
```

That registers `~/.local/bin/scout` in `~/.grok/config.toml` and does
not depend on plugin-data or SessionStart. Do not add it while a
plugin-declared `scout` server is also configured unless you have
checked that user config **replaces** the plugin server (Grok's merge
order documents config.toml over Claude/Cursor/`.mcp.json`; plugin
servers are a separate source).

Uninstall the accidental 3.9G snapshot if it is still present:

```bash
grok plugin uninstall scout --confirm
```

The `[[marketplace.sources]] name = "scout"` entry is harmless but
useless until §3.1 exists.

After a rebuild, `cp target/release/scout ~/.local/bin/scout` (or
`cargo install --path . --force`) is what keeps the PATH server
current. Grok's plugin-data copy, if you still use it, is a second
manual `cp` to whatever `grok mcp doctor scout` prints — the hash
changes when the install identity changes.

---

## 4. Verification

Once §3.1–3.4 are in:

1. `grok plugin marketplace add $HOME/Projects/scout` (or refresh).
2. `grok plugin list --json --available` includes
   `{ "name": "scout", "marketplace": "scout" }`.
3. `grok plugin install scout --trust` succeeds and
   `~/.grok/installed-plugins/` does **not** contain `target/`.
4. `grok mcp doctor scout` : command found, handshake OK, 4 tools.
   `ct` is **not** listed as `plugin: scout`.
5. New Grok session: `/mcp` → scout expands to `check_output` /
   `extract` / `grep` / `ping` (or whatever the server currently
   exports), not ct's 46 tools.
6. `grok inspect` shows the slim path, not
   `~/.claude/plugins/cache/scout/…`.
7. A `cargo test` via Grok's shell tool is redirected or at least
   nudged toward `check_output` if the matcher in §3.4 is in place.

Claude Code smoke: `/plugin install scout@scout` from the directory
marketplace still resolves; SessionStart still copies into
`~/.claude/plugins/data/scout-scout`; `ct` still comes from project
`.mcp.json` when the cwd is this repo.

---

## 5. Decision log

Nothing here is decided except the diagnosis. Open product choices:

- **One hooks.json or two?** Shared file with a union matcher
  (`Bash|run_terminal_command`) is less drift. Two files is less
  chance of a Claude matcher surprise.
- **PATH vs plugin-data for Claude?** Leave Claude on plugin-data.
  Switching Claude to `PATH` would drop the "plugin owns the binary"
  story `ensure-binary.sh` exists for.
- **Should Grok run SessionStart before MCP spawn?** That is a Grok
  bug/limitation, not something scout can fix. Design as if it does
  not.
- **Live checkout vs snapshot.** Grok copies. The slim dir is the
  workaround. A `[plugins].paths` / `.grok/plugins/` symlink at the
  slim dir would be a dev-only alternative if we want edits to hooks
  without reinstall; not required for a first Grok-ready install.
