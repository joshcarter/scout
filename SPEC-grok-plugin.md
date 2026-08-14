# Spec: Grok Build plugin packaging

**Status:** measured against Grok Build 1.0.3 on 2026-08-14, first by hand in
this checkout and then with a purpose-built probe plugin (`grok-probe`, §1.2).
Nothing here is implemented. Claude Code remains the supported install; this is
what has to change if scout should also install and boot cleanly under Grok.

**Goal:** a Grok user can add the marketplace, install `scout` by name, get a
working MCP server on the next session, and have as much of the Claude
steering (binary bootstrap, build/test redirect, usage guidance) as Grok is
capable of delivering — without breaking the Claude plugin.

**Non-goals:** changing scout's MCP tool schemas; making Grok's native `grep` /
`read_file` go away; a Grok-only fork of the binary.

**The short version:** the packaging problem is solved and the fix is smaller
than expected — one MCP command spelling works in both harnesses, and Grok's
copy-on-install will carry the binary. The steering problem is half solved:
Grok does not run plugin hooks at all and no packaging change reaches them, so
the build/test redirect and shell-safety screening are permanently Claude-only.
Guidance does have a route — a `skills/` entry in the payload, which loads and
which the model quotes back.

---

## 1. How we measured

### 1.1 By hand

| Step | Result |
|---|---|
| Open a Grok session in this repo | `/mcp` listed a **scout** plugin. Expanding it showed **ct** tools, not scout's. |
| `grok mcp doctor` | Two servers attributed to `plugin: scout`: `scout` (spawn failed) and `ct` (46 tools, healthy). |
| `grok plugin marketplace add $HOME/Projects/scout` | Succeeded. Wrote `[[marketplace.sources]] name = "scout"` / `path = …/Projects/scout` into `~/.grok/config.toml`. |
| `grok plugin install scout --trust` | `Error: no marketplace plugin named "scout" in any registered marketplace.` |
| `grok plugin install scout@scout --trust` | `Error: No marketplace plugin named "scout" in "scout".` |
| `grok plugin install $HOME/Projects/scout --trust` | Installed. Copied the **entire** working tree (including `target/`, ~3.9G) to `~/.grok/installed-plugins/scout-<id>`. |
| Copy `target/release/scout` into the path `grok mcp doctor` printed | `scout` MCP came up: handshake OK, 4 tools. New session required. |

### 1.2 With the probe

`~/Projects/grok-probe` is a throwaway plugin shaped exactly like the payload
proposed in §3: plugin in a `plugins/<name>/` subdirectory, a real 12M
executable in the payload's `bin/`, MCP command pointed at a plugin-root
variable. It declares **five MCP servers running one identical script**,
differing only in how the command path is spelled, and registers each hook
twice, once per root variable. Every spawn appends its variant, cwd, and full
plugin-variable environment to `~/grok-probe.log` before attempting the
handshake, so a server that fails to start still reports what it saw.

That turns "which substitutions does Grok honor" into a single reading of
`grok mcp doctor`, and it distinguishes *hook did not fire* from *hook fired
but the path did not expand* — a distinction §2 previously could not make.

---

## 2. Findings

### 2.1 Marketplace `source: "./"` is dropped — CONFIRMED, and the fix works

`.claude-plugin/marketplace.json` is the Claude-shaped single-repo index:

```json
{ "name": "scout", "source": "./", "description": "…" }
```

Claude Code accepts a plugin that *is* the marketplace root. Grok registers the
marketplace and then publishes **zero** plugins from it, which is why
name-based install failed even though the source was in `config.toml`.

| `source` value | Cataloged? |
|---|---|
| `"./plugins/demo"` | yes |
| `{ "type": "local", "path": "./plugins/demo" }` | yes |
| same object form inside `.claude-plugin/marketplace.json` | yes |
| `"./"`, `"."`, `""` | no |
| `{ "type": "local", "path": "./" }` or `"path": "."` | no |

`.grok-plugin/marketplace.json` vs `.claude-plugin/marketplace.json` did not
matter. The rejected shape is **plugin at marketplace root**.

The probe confirmed the working end of this end to end: with
`source: "./plugins/probe"` in `.claude-plugin/marketplace.json`,
`grok plugin install probe --trust` resolved by **plugin** name, and
`registry.json` recorded the subdirectory explicitly:

```json
"marketplace": { "source_display_name": "grok-probe", "plugin_subdir": "plugins/probe" }
```

### 2.2 Path install snapshots the whole repo — CONFIRMED, and it becomes an asset

`grok plugin install /abs/path --trust` does not behave like Claude's directory
marketplace. Claude leaves `CLAUDE_PLUGIN_ROOT` on the working tree. Grok
copies into `~/.grok/installed-plugins/<name>-<id>`. There is no exclude list,
which is how this checkout's `target/` came along at ~3.9G.

With a slim payload that copy stops being a liability and becomes the delivery
mechanism. The probe shipped a real 12M ELF (a copy of `scout`) at
`plugins/probe/bin/fake-binary`. After install:

```
payload binary     = present, 11688064 bytes, exec=yes
payload --version  = scout 0.1.0
```

Byte-identical, exec bit preserved, and it **ran** from the installed copy.
Total install size 12M. This is the single result the whole plan rests on.

The copy is still a snapshot: it is not updated when you rebuild in the tree.
See §3.2 for what that costs.

### 2.3 Plugin-data is empty at spawn — CONFIRMED, and the cause is now known

`.claude-plugin/plugin.json` declares `"command": "${CLAUDE_PLUGIN_DATA}/bin/scout"`.
Claude expands that to `~/.claude/plugins/data/scout-scout`, which SessionStart
(`scripts/ensure-binary.sh`) populates. Grok expands the same token to
`~/.grok/plugin-data/user/<id>/scout/bin/scout` and **never creates it**:

```
✗ command not found (~/.grok/plugin-data/user/<id>/probe/bin/probe-mcp)
```

The earlier session could not tell whether SessionStart ran late, ran with the
wrong fallback, or never ran. The probe answers it: **it never ran** (§2.5).
Ordering is moot. Any design that requires a hook to populate a directory
before MCP spawn is dead under Grok.

The `<id>` is per install identity and not stable across install methods
(Claude-compat `9e028837`, path install `e3540977`, probe `a8876d9c`).

### 2.4 Plugin-root variables DO expand in the MCP command — NEW, decisive

Both root spellings expand, to the same place, and both handshake cleanly:

| Server | Command as declared | Result |
|---|---|---|
| `probe-claude-root` | `${CLAUDE_PLUGIN_ROOT}/bin/probe-mcp` | ✓ resolved to `~/.grok/installed-plugins/probe-0c80af74/bin/probe-mcp`, handshake OK |
| `probe-grok-root` | `${GROK_PLUGIN_ROOT}/bin/probe-mcp` | ✓ same path, handshake OK |
| `probe-claude-data` | `${CLAUDE_PLUGIN_DATA}/bin/probe-mcp` | ✗ command not found |
| `probe-grok-data` | `${GROK_PLUGIN_DATA}/bin/probe-mcp` | ✗ command not found |
| `probe-path` | bare `probe-mcp` | ✗ command not found — payload `bin/` is not on the spawn PATH |

Grok honors Claude's root alias. **One manifest command string serves both
harnesses**, with no conditional logic and no wrapper.

The asymmetry that makes this work: `PLUGIN_ROOT` is whatever the harness
installed, so it exists by definition before anything spawns. `PLUGIN_DATA` is
an empty directory some hook is supposed to fill later.

### 2.5 Grok does not run plugin hooks — NEW, and worse than assumed

The probe registered a SessionStart recorder and a no-op PreToolUse dumper,
each twice (once per root variable), with the union matcher
`Bash|run_terminal_command`. After a full session with shell commands run and
`permission_mode = "always-approve"`:

**Neither event fired. Zero hook blocks in the log.**

This is not a substitution failure — Grok expanded the *same*
`${CLAUDE_PLUGIN_ROOT}` token correctly in the MCP command field (§2.4). Grok
parses `hooks/hooks.json` (`grok inspect` reports the plugin as providing a
file hook) but does not execute it.

A second probe (`hookprobe`) then tried every plausible declaration site at
once, each pointing at one recorder with a distinct site name:

| Site | Fired? |
|---|---|
| `.grok-plugin/plugin.json`, inline `hooks` object | no |
| payload-root `plugin.json`, `"hooks": "./hooks/alt-hooks.json"` path string | no |
| `.claude-plugin/plugin.json`, inline `hooks` object | no |
| `.grok-plugin/hooks.json`, PascalCase events | no |
| `.grok-plugin/hooks.json`, `session_start` / `pre_tool_use` | no |
| payload-root `hooks.json` | no |
| `hooks/hooks.json` (what scout ships) | no |

Six locations, both event-name casings, both the inline-object and the
path-string manifest forms. The plugin installed cleanly and its MCP server
spawned five times in the same session, so the payload was live throughout.
**Nothing fired.**

This closes the question: it is not a shape problem. Grok Build 1.0.3 does not
execute plugin hooks, and no packaging change reaches them. That xAI's own
marketplace `CONTRIBUTING.md` lists hooks as a reviewable component and warns
against "lifecycle hooks that run shell on `Bash`/`Write` with no matcher"
suggests the feature is intended and not yet wired up, so this is worth
retesting on a later Grok release — but nothing in scout can move it.

Previous speculation about matcher names (`Bash` vs `run_terminal_command`) is
untestable until hooks run at all. The union matcher is still the right shape
and costs nothing, but do not expect it to do anything on Grok today.

Consequences, in order of severity:

1. **Config seeding does not happen.** `ensure-binary.sh` seeds
   `~/.config/scout/config.toml` on first run. Under Grok a user gets a healthy
   scout MCP server with no config, so every call needing the local model
   fails. This is the only one that breaks functionality.
2. **Guidance injection does not happen** — confirms §2.7 with a cause.
3. **The build/test redirect** and **shell-safety auto-allow** are unavailable.
   Both fail open, so nothing breaks; Grok simply gets no steering.

### 2.6 Plugin variables are NOT exported to the spawned process — NEW

Every spawn logged all four as unset:

```
CLAUDE_PLUGIN_ROOT = <unset>    GROK_PLUGIN_ROOT   = <unset>
CLAUDE_PLUGIN_DATA = <unset>    GROK_PLUGIN_DATA   = <unset>
```

Grok substitutes into the command *string* at spawn time; it does not put the
variables in the child's environment. So nothing can resolve its own location
at runtime — no wrapper script that tries `$GROK_PLUGIN_DATA` then
`$CLAUDE_PLUGIN_DATA` then PATH can work, because it will see none of them.
Manifest-time substitution is the only mechanism available, and per §2.4 it is
sufficient.

### 2.7 Grok reads which manifest? The payload root — NEW

The probe shipped `plugin.json` in both candidate locations with deliberately
different versions. `registry.json` recorded:

```json
"plugins": { "probe": { "version": "0.0.2" } }
```

`0.0.2` is the payload-root `plugins/probe/plugin.json`. Grok did **not** read
`plugins/probe/.claude-plugin/plugin.json` (`0.0.1`), which is the only
location Claude accepts. Two manifest copies are required. See §3.3.

`hookprobe` extended this to three candidates, each declaring a differently
named MCP server so the winner is visible in `grok mcp doctor`. Payload-root
won again — registry version `0.0.2`, and `hp-root` was the **only** server
registered (`plugin: hookprobe — 1 server`). Neither `.grok-plugin/plugin.json`
(`0.0.3`, `hp-grokdir`) nor `.claude-plugin/plugin.json` (`0.0.1`,
`hp-claudedir`) contributed anything.

So Grok reads exactly one manifest and payload-root outranks
`.grok-plugin/`, even though `.grok-plugin/plugin.json` is what the one
official Grok-native plugin in the xAI marketplace (`neon`) uses — presumably
it works when it is the only manifest present. scout does not need a
`.grok-plugin/` directory: payload-root for Grok, `.claude-plugin/` for Claude.

### 2.8 Project `.mcp.json` is attributed to the plugin — CONFIRMED

Repo-root `.mcp.json` holds a `ct` entry for working *on* this checkout.
Claude treats it as project MCP. Grok's plugin loader treats a plugin folder's
`.mcp.json` as **plugin** MCP — and because the plugin *is* the repo, `/mcp`
under scout listed the healthy `ct` server next to the failing `scout` one.

Moving the payload into `plugins/scout/` fixes this structurally: repo-root
`.mcp.json` is no longer inside the plugin directory, so there is nothing to
attribute. No exclude list needed.

### 2.9 Grok loads Claude's plugin config wholesale — NEW, affects testing

`grok plugin marketplace list` reports `claude-plugins-official`, which was
never added to Grok. Grok reads `~/.claude/settings.json` directly:
`enabledPlugins["scout@scout"]` plus `extraKnownMarketplaces.scout` is enough
to load scout as a Grok plugin with two servers, **independently of Grok's own
registry** — `grok plugin list` and `registry.json` both show it absent while
`grok mcp doctor` shows it healthy.

Practical effect: uninstalling the Grok plugin does not remove it, and neither
does `grok plugin marketplace remove scout`. Use `grok plugin disable scout` on
the Grok side, or clear `scout@scout` from Claude's settings (which disables it
in Claude too). Disable it before testing the new payload, or you will be
looking at two overlapping installs.

### 2.10 Usage guidance never enters a Grok session — CONFIRMED

Claude gets the delegation table from SessionStart `additionalContext` in
`ensure-binary.sh`. Grok did not receive it, because the hook never ran (§2.5).
Asked directly, the Grok session reported the sentinel string was absent from
its system prompt and any injected guidance, and that it could only find the
string by searching the filesystem.

What Grok *did* load: `CLAUDE.md`. That is not a search-routing table. Grok's
native `grep` / `read_file` / `list_dir` are always in the tool list with full
schemas, while MCP tools are a second hop (`search_tool` then `use_tool`).
Combined with a dead or undiscovered scout server, the model uses built-in
search. Packaging plus guidance, not a model-preference mystery.

### 2.11 Plugin skills DO load — NEW, and this is the way in

`hookprobe` shipped `skills/hookprobe/SKILL.md` carrying its own sentinel. The
Grok session reported the skill **listed in the available-skills inventory it
was given**, and quoted the sentinel back. The SKILL.md itself insists on that
distinction, because an agent that greps the filesystem for a sentinel proves
nothing.

So a Grok plugin can deliver instructions to the model. Not through
`additionalContext` (§2.10) and not through hooks (§2.5) — through `skills/`.
That is also what `neon`, the one official Grok-native plugin in the xAI
marketplace, ships instead of hooks.

This is the channel scout's delegation table has to travel down. See §3.6.

---

## 3. Resolution

One payload, shared by both harnesses. Not a Grok-shaped copy alongside the
Claude one — that is two of everything and the drift is silent.

### 3.1 Move the plugin into `plugins/scout/`

```
.claude-plugin/marketplace.json        # source: "./plugins/scout"
plugins/scout/
  plugin.json                          # Grok reads this (§2.7)
  .claude-plugin/plugin.json           # Claude reads this (§2.7)
  hooks/hooks.json                     # union matcher; Claude-only in practice
  hooks/shell-safety.sh
  hooks/prefer-local-llm.sh
  scripts/ensure-binary.sh             # shrinks — see §3.5
  skills/scout/SKILL.md                # the only guidance channel Grok has (§3.6)
  bin/scout                            # the real binary, put there by `make`
  bin/.gitkeep
```

No `.grok-plugin/` directory: Grok reads the payload-root `plugin.json` and
only ever reads one manifest (§2.7).

Claude accepts a subdirectory source — that is the ordinary multi-plugin
marketplace shape; `source: "./"` is the special case and the one Grok drops.
So a single `marketplace.json` serves both, and `/plugin install scout@scout`
is unchanged for Claude.

This one move retires §2.1, §2.2 (payload is 12M, not 3.9G), and §2.8.

Cost: `CLAUDE_PLUGIN_ROOT` moves two levels down, so `ensure-binary.sh` loses
`$PLUGIN_ROOT/target/release/scout`, `$PLUGIN_ROOT/.claude-plugin/plugin.json`,
and `$PLUGIN_ROOT/config.example.toml`. Resolve those against a repo root found
by walking up for `Cargo.toml`, and let the local-build branch not fire when
there is no checkout above — which is the Grok case regardless, since Grok
copies.

### 3.2 Ship the binary in the payload, via `make`

A make target installs `target/release/scout` to `plugins/scout/bin/scout`.
`.gitignore` gets that path; `bin/.gitkeep` keeps the directory tracked.

Fold it into the existing `build` target rather than adding a separate one, so
`make build` cannot leave the plugin stale. `install-bin` (CLI → `$PREFIX/bin`)
stays as it is: two destinations, one command, nothing to remember.

What this buys, per harness:

- **Claude**, directory marketplace: `PLUGIN_ROOT` *is* the checkout, so `make`
  lands on the next session start. The mtime-versus-version logic in
  `ensure-binary.sh` — and its long justifying comment — is no longer needed.
- **Grok**: the payload copy carries the binary at install time (§2.2), so the
  server spawns on the **first** session with no bootstrap at all.

**Copy with rename, never in place.** Under Claude's directory marketplace the
MCP server executes the binary *at the path `make` wants to write*, and Linux
refuses to overwrite a running executable:

```
cp: cannot create regular file '.../bin/scout': Text file busy
```

This is not hypothetical — it is happening today with `ensure-binary.sh`. Its
`cp "$LOCAL_BUILD" "$BIN"` fails with `ETXTBSY` whenever a scout MCP server is
live, which after the first session is always. The failure is self-perpetuating
and silent apart from a status string: the SessionStart line reads
`Binary status: error: copy from local build failed (rebuilt since last
install)` and the session goes on using a binary that can never be updated.

Every writer of the binary — the make target, `install-bin`, whatever remains
of `ensure-binary.sh` — must write a sibling temp file and `mv` it into place.
Rename is atomic and succeeds against a busy target; the running process keeps
its old inode until it exits. `cp` and `install` both truncate in place and
both fail.

Two limits, both acceptable:

- A clean `git clone` has no binary, so a marketplace install straight from
  GitHub yields a payload that cannot spawn. This makes the *local* story
  airtight and does nothing for a stranger; prebuilt release binaries are what
  closes that, and they would populate this same `bin/scout` path.
- Grok copied at install time, so after a rebuild a Grok user must reinstall.
  Claude does not. That is the live-vs-snapshot split, now confined to "the
  binary changed" instead of "anything changed."

### 3.3 One MCP command spelling, two manifest copies

Both `plugins/scout/plugin.json` and `plugins/scout/.claude-plugin/plugin.json`
declare exactly:

```json
"mcpServers": {
  "scout": { "command": "${CLAUDE_PLUGIN_ROOT}/bin/scout", "args": ["mcp"] }
}
```

Per §2.4 this resolves under both harnesses. No `PLUGIN_DATA`, no PATH
dependency, no wrapper, no SessionStart ordering dependency. It also removes
the first-session spawn failure Claude users currently hit (README:107).

The two files are ~20 duplicated lines because Grok and Claude read different
paths (§2.7). Keep them honest with a make target that copies one to the other,
or a test that diffs them. Worth checking during the Claude smoke test whether
Claude also accepts the payload-root `plugin.json` — if it does, delete the
`.claude-plugin/` copy and the problem disappears.

### 3.4 Seed config from the binary, not from a hook

`ensure-binary.sh` currently seeds `~/.config/scout/config.toml`. Under Grok
that never runs (§2.5), leaving a healthy server that fails every real call.

Move first-run config seeding into the binary: on startup, if no config exists
at the resolved path, write the default and note it. This is the right fix
independent of Grok — it makes `scout` on PATH robust for anyone who installed
by `make install`, `cargo install`, or a future release tarball, none of which
run the hook either.

Resolution order must stay in step with `src/config.rs`, `hooks/shell-safety.sh`
and the Makefile: `$SCOUT_CONFIG`, else `${XDG_CONFIG_HOME:-~/.config}/scout/`.

### 3.5 What `ensure-binary.sh` becomes

With §3.2 and §3.4 done, its install half and its config half are both gone.
What remains is SessionStart guidance injection for Claude — a different script
wearing the same name. Rename it, or fold it into a small
`scripts/session-context.sh`, and delete the version compare, the mtime check,
and the `cargo install scout-llm` fallback.

### 3.6 Guidance: ship a `SKILL.md` in the payload

`additionalContext` reaches Claude and nothing else (§2.10); hooks reach
nothing at all under Grok (§2.5). Skills load (§2.11), so the payload gets
`plugins/scout/skills/scout/SKILL.md` carrying the delegation table.

A skill is strictly better than the `AGENTS.md` alternative for this: it
travels with the plugin, so it works in any Grok session rather than only in
sessions whose cwd is this repo. Add `AGENTS.md` too if you want the table
present when working *on* scout — it is cheap — but the skill is the one that
matters.

Content is the table that already exists in `ensure-binary.sh`: prefer
`check_output` / `extract` / `grep(pattern, intent)` for token-heavy work; the
`# raw-output` bypass; `ToolSearch` / `search_tool` to resolve deferred MCP
names. Per `CLAUDE.md`, do not bake a fully-qualified MCP tool name into it.
Do not tell the model to use scout for every identifier search — the Claude
guidance does not say that either.

The skill's `description` frontmatter is what decides whether it ever gets
invoked, so it should name the triggering situations (large file, noisy grep,
build or test output) rather than describe scout.

Two copies of this text now exist — `ensure-binary.sh` for Claude, `SKILL.md`
for Grok. Generate one from the other, or accept the duplication and note it in
both.

Worth noting the layer that does *not* transfer: under Claude, scout's steering
is three layers (guidance, MCP tool descriptions, PreToolUse hooks) and the
hooks are the only one that fires without the model choosing to cooperate.
Grok gets the two passive layers. Expect weaker routing there, and do not read
a Grok session's preference for native `grep` as a scout bug.

### 3.7 Hooks: Claude-only, settled

Keep `hooks/hooks.json` exactly where it is, with the union matcher
`Bash|run_terminal_command`. It is correct for Claude, costs nothing, and is
inert on Grok — and per §2.5 no other location or spelling changes that.

The build/test redirect and shell-safety auto-allow are therefore **Claude-only
features**. Say so plainly in the README rather than letting a Grok user infer
that scout is misbehaving. Retest on a future Grok release; the feature looks
intended, not rejected.

### 3.8 Local workaround until the above ships

```bash
grok mcp add scout -- scout mcp
```

Registers `~/.local/bin/scout` in `~/.grok/config.toml`; no plugin-data, no
SessionStart. Disable the Claude-compat load first (§2.9) or you will have two
scout servers. After a rebuild, `make install` is what keeps this current.

---

## 4. Verification

Once §3.1–3.3 are in:

1. `grok plugin disable scout` (kill the Claude-compat load, §2.9).
2. `grok plugin marketplace add $HOME/Projects/scout` (or `update`).
3. `grok plugin list --json --available` includes
   `{ "name": "scout", "marketplace": "scout" }`.
4. `grok plugin install scout --trust` succeeds;
   `~/.grok/installed-plugins/scout-<id>` is ~12M and contains no `target/`.
5. `grok mcp doctor scout`: command found under
   `~/.grok/installed-plugins/`, handshake OK, 4 tools — on the **first**
   session, no manual copy. `ct` is **not** listed as `plugin: scout`.
6. New Grok session: `/mcp` → scout expands to `check_output` / `extract` /
   `grep` / `ping`, not ct's 46 tools.
7. A scout MCP call succeeds on a machine with no `~/.config/scout/config.toml`
   (§3.4).
8. The Grok session lists a `scout` skill in its available-skills inventory
   without being asked to search for it (§3.6), and routes a large-file
   question to `extract` rather than `read_file`.

Claude Code smoke:

9. `/plugin install scout@scout` from the directory marketplace still resolves
   against the new `source: "./plugins/scout"`.
10. MCP spawns on the **first** session (no plugin-data bootstrap).
11. `make build` leaves `plugins/scout/bin/scout` current; a rebuild is picked
    up after a restart.
12. Both PreToolUse hooks still fire; `ct` still comes from project `.mcp.json`
    when the cwd is this repo.
13. Does Claude accept the payload-root `plugin.json`? If yes, drop
    `.claude-plugin/plugin.json` from the payload (§3.3).

---

## 5. Decision log

Closed by measurement:

- **PATH vs plugin-data?** Neither. Plugin-root, one spelling, both harnesses
  (§2.4). Plugin-data is unreachable under Grok and unnecessary under Claude.
- **A wrapper that resolves the binary at runtime?** Impossible — the child
  process sees none of the plugin variables (§2.6).
- **One hooks.json or two?** One, union matcher. Two files would be two copies
  of a thing that only runs in one harness anyway (§2.5).
- **Should Grok run SessionStart before MCP spawn?** Moot. It does not run
  plugin hooks at all. The design does not depend on it.
- **Will Grok carry a 12M binary in the payload?** Yes, exec bit intact, and it
  runs (§2.2).

- **Are Grok's plugin hooks recoverable?** No. Six declaration sites, two event
  vocabularies, two manifest forms — all silent (§2.5). Not a packaging
  problem, so not scout's to fix.
- **How does guidance reach a Grok session?** A `skills/` entry in the payload.
  Proven loaded and quoted back (§2.11).
- **Does scout need a `.grok-plugin/` directory?** No. Payload-root
  `plugin.json` outranks it, and only one manifest is ever read (§2.7).

Still open:

- **Does Claude accept a payload-root `plugin.json`?** If so, one manifest
  instead of two (§3.3).
- **Prebuilt release binaries.** Deferred, but it is what makes scout
  installable by someone who is not building from this checkout. The payload
  `bin/scout` path in §3.2 is where a release tarball would land, so the two
  designs agree.
- **Live checkout vs snapshot for Grok dev.** Grok copies; reinstall after a
  rebuild is the honest cost. A `.grok/plugins/` symlink would paper over it on
  one machine and mislead about what users experience. Not recommended.
