# Plugin packaging: one payload, two harnesses

scout ships as a plugin for two agent harnesses — Claude Code and Grok
Build — from a single payload at `plugins/scout/`. This document records
how each harness actually behaves, because almost none of it is
documented and most of it was surprising.

The findings below were **measured against Grok Build 1.0.3**, first by
hand and then with a purpose-built probe plugin. Retest before trusting
them against a later release; where a behavior looks like an unfinished
feature rather than a deliberate choice, that is noted.

**The short version:** the packaging problem is solved and the fix was
smaller than expected — one MCP command spelling works in both
harnesses, and Grok's copy-on-install carries the binary. The steering
problem is half solved. Grok does not run plugin hooks at all and no
packaging change reaches them, so the build/test redirect and
shell-safety screening are permanently Claude-only. Guidance does have a
route: a `skills/` entry in the payload, which loads and which the model
quotes back.

---

## 1. How this was measured

By hand first, which established that something was wrong but could not
say what. The useful instrument was a throwaway probe plugin shaped
exactly like the real payload: plugin in a `plugins/<name>/`
subdirectory, a real 12 MB executable in the payload's `bin/`, MCP
command pointed at a plugin-root variable.

It declared **five MCP servers running one identical script**, differing
only in how the command path was spelled, and registered every hook
twice, once per root variable. Every spawn appended its variant, cwd, and
full plugin-variable environment to a log *before* attempting the
handshake, so a server that failed to start still reported what it saw.

That turns "which substitutions does this harness honor" into a single
reading of `grok mcp doctor`, and — the part that mattered most — it
distinguishes *hook did not fire* from *hook fired but the path did not
expand*. Guessing could not tell those apart.

**The technique generalizes.** When a harness's behavior is undocumented,
a probe that declares every candidate spelling at once and records
before it can fail is worth more than any amount of reading.

---

## 2. Findings

### 2.1 A plugin at the marketplace root is dropped by Grok

`.claude-plugin/marketplace.json` in the Claude-shaped single-repo form:

```json
{ "name": "scout", "source": "./", "description": "…" }
```

Claude Code accepts a plugin that *is* the marketplace root. Grok
registers the marketplace and then publishes **zero** plugins from it,
which is why `grok plugin install scout --trust` failed with "no
marketplace plugin named scout" even though the source was in
`config.toml`.

| `source` value | Cataloged by Grok? |
|---|---|
| `"./plugins/demo"` | yes |
| `{ "type": "local", "path": "./plugins/demo" }` | yes |
| `"./"`, `"."`, `""` | no |
| `{ "type": "local", "path": "./" }` or `"path": "."` | no |

`.grok-plugin/marketplace.json` vs `.claude-plugin/marketplace.json` made
no difference. The rejected shape is specifically **plugin at
marketplace root**.

With `source: "./plugins/probe"`, install resolved by plugin name and
Grok's `registry.json` recorded the subdirectory explicitly:

```json
"marketplace": { "source_display_name": "grok-probe", "plugin_subdir": "plugins/probe" }
```

A subdirectory source is the ordinary multi-plugin marketplace shape and
Claude accepts it too, so **one `marketplace.json` serves both.**

### 2.2 Grok's path install snapshots the whole repo — which becomes an asset

`grok plugin install /abs/path --trust` does not behave like Claude's
directory marketplace. Claude leaves `CLAUDE_PLUGIN_ROOT` pointing at the
working tree. Grok **copies** into `~/.grok/installed-plugins/<name>-<id>`,
with no exclude list — which is how a `target/` directory once came along
at ~3.9 GB.

With a slim payload that copy stops being a liability and becomes the
delivery mechanism. The probe shipped a real 12 MB ELF at
`plugins/probe/bin/fake-binary`. After install:

```
payload binary     = present, 11688064 bytes, exec=yes
payload --version  = scout 0.1.0
```

Byte-identical, exec bit preserved, and it **ran** from the installed
copy. Total install size 12 MB. This single result is what the whole
design rests on.

The copy is still a snapshot: it is not updated when you rebuild in the
tree. Under Grok, a rebuild means a reinstall. Under Claude's directory
marketplace it does not.

### 2.3 Plugin-*data* is empty at spawn, under Grok, permanently

An early design declared `"command": "${CLAUDE_PLUGIN_DATA}/bin/scout"`.
Claude expands that to `~/.claude/plugins/data/scout-scout`, which a
SessionStart hook populates. Grok expands the same token to
`~/.grok/plugin-data/user/<id>/scout/bin/scout` and **never creates it**:

```
✗ command not found (~/.grok/plugin-data/user/<id>/probe/bin/probe-mcp)
```

The cause is §2.5 — the hook never ran. Ordering is moot. **Any design
that requires a hook to populate a directory before MCP spawn is dead
under Grok.**

The `<id>` is per install identity and is not stable across install
methods.

### 2.4 Plugin-*root* variables DO expand in the MCP command — the decisive finding

Both root spellings expand, to the same place, and both handshake
cleanly:

| Command as declared | Result |
|---|---|
| `${CLAUDE_PLUGIN_ROOT}/bin/probe-mcp` | ✓ resolved under `~/.grok/installed-plugins/`, handshake OK |
| `${GROK_PLUGIN_ROOT}/bin/probe-mcp` | ✓ same path, handshake OK |
| `${CLAUDE_PLUGIN_DATA}/bin/probe-mcp` | ✗ command not found |
| `${GROK_PLUGIN_DATA}/bin/probe-mcp` | ✗ command not found |
| bare `probe-mcp` | ✗ command not found — the payload `bin/` is **not** on the spawn PATH |

Grok honors Claude's root alias. **One manifest command string serves
both harnesses**, with no conditional logic and no wrapper.

The asymmetry that makes this work: `PLUGIN_ROOT` is whatever the harness
installed, so it exists by definition before anything spawns.
`PLUGIN_DATA` is an empty directory some hook is supposed to fill later.

### 2.5 Grok does not run plugin hooks at all

The probe registered a SessionStart recorder and a no-op PreToolUse
dumper, each twice, with the union matcher `Bash|run_terminal_command`.
After a full session with shell commands run and
`permission_mode = "always-approve"`: **neither event fired, zero hook
blocks in the log.**

This is not a substitution failure — Grok expanded the *same*
`${CLAUDE_PLUGIN_ROOT}` token correctly in the MCP command field (§2.4).
Grok parses `hooks/hooks.json` (`grok inspect` reports the plugin as
providing a file hook) but does not execute it.

A second probe tried every plausible declaration site at once:

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
path-string manifest forms. The plugin installed cleanly and its MCP
server spawned five times in the same session, so the payload was live
throughout. **Nothing fired.**

It is not a shape problem. That xAI's own marketplace `CONTRIBUTING.md`
lists hooks as a reviewable component and warns against "lifecycle hooks
that run shell on `Bash`/`Write` with no matcher" suggests the feature is
intended and not yet wired up — worth retesting on a later release, but
nothing in scout can move it.

Consequences, in order of severity:

1. **Config seeding does not happen.** A hook that seeds
   `~/.config/scout/config.toml` on first run leaves a Grok user with a
   healthy scout MCP server and no config, so every call needing the
   local model fails. This is the only one that breaks functionality —
   see §3.4.
2. **Guidance injection does not happen** (§2.10).
3. **The build/test redirect and shell-safety auto-allow are
   unavailable.** Both fail open, so nothing breaks; Grok simply gets no
   steering.

### 2.6 Plugin variables are not exported to the spawned process

Every spawn logged all four as unset:

```
CLAUDE_PLUGIN_ROOT = <unset>    GROK_PLUGIN_ROOT   = <unset>
CLAUDE_PLUGIN_DATA = <unset>    GROK_PLUGIN_DATA   = <unset>
```

Grok substitutes into the command *string* at spawn time; it does not put
the variables in the child's environment. **Nothing can resolve its own
location at runtime.** A wrapper script that tries `$GROK_PLUGIN_DATA`
then `$CLAUDE_PLUGIN_DATA` then PATH cannot work, because it will see
none of them. Manifest-time substitution is the only mechanism
available — and per §2.4 it is sufficient.

### 2.7 Grok reads exactly one manifest, at the payload root

The probe shipped `plugin.json` in both candidate locations with
deliberately different versions. `registry.json` recorded `0.0.2` — the
payload-root `plugins/probe/plugin.json`. Grok did **not** read
`plugins/probe/.claude-plugin/plugin.json` (`0.0.1`), which is the only
location Claude accepts.

A follow-up probe extended this to three candidates, each declaring a
differently named MCP server so the winner was visible in
`grok mcp doctor`. Payload-root won again, and its server was the
**only** one registered. Neither `.grok-plugin/plugin.json` nor
`.claude-plugin/plugin.json` contributed anything.

So payload-root outranks `.grok-plugin/`, even though `.grok-plugin/` is
what the one official Grok-native plugin in the xAI marketplace uses —
presumably it works when it is the only manifest present. **scout needs
no `.grok-plugin/` directory: payload-root for Grok, `.claude-plugin/`
for Claude.** Two copies of the same content is the cost.

### 2.8 A `.mcp.json` inside the plugin folder is attributed to the plugin

Claude treats a repo-root `.mcp.json` as *project* MCP. Grok's plugin
loader treats a `.mcp.json` inside a plugin folder as *plugin* MCP — and
when the plugin *is* the repo, unrelated project servers show up under
the plugin's own listing in `/mcp` and `grok mcp doctor`.

Moving the payload into `plugins/scout/` fixes this structurally: a
repo-root `.mcp.json` is no longer inside the plugin directory, so there
is nothing to attribute. No exclude list needed.

### 2.9 Grok loads Claude's plugin config wholesale

`grok plugin marketplace list` reports marketplaces that were never added
to Grok. Grok reads `~/.claude/settings.json` directly:
`enabledPlugins["scout@scout"]` plus `extraKnownMarketplaces.scout` is
enough to load scout as a Grok plugin, **independently of Grok's own
registry** — `grok plugin list` and `registry.json` both show it absent
while `grok mcp doctor` shows it healthy.

Practical effect on testing: uninstalling the Grok plugin does not remove
it, and neither does `grok plugin marketplace remove scout`. Use
`grok plugin disable scout` on the Grok side, or clear `scout@scout` from
Claude's settings (which disables it in Claude too). **Disable it before
testing a new payload, or you will be looking at two overlapping
installs.**

### 2.10 Usage guidance never reaches a Grok session through hooks

Claude gets scout's delegation table from SessionStart
`additionalContext`. Grok did not receive it, because the hook never ran
(§2.5). Asked directly, the Grok session reported the sentinel string was
absent from its system prompt and from any injected guidance, and that it
could only find the string by searching the filesystem.

What Grok *did* load was `CLAUDE.md`, which is not a search-routing
table. Meanwhile Grok's native `grep` / `read_file` / `list_dir` are
always in the tool list with full schemas, while MCP tools are a second
hop (`search_tool` then `use_tool`). Combined with a dead or undiscovered
scout server, the model uses built-in search every time. **Packaging plus
guidance, not a model-preference mystery.**

### 2.11 Plugin skills DO load — and this is the way in

A probe shipped `skills/hookprobe/SKILL.md` carrying its own sentinel.
The Grok session reported the skill **listed in the available-skills
inventory it was given**, and quoted the sentinel back. The SKILL.md
itself insisted on that distinction, because an agent that greps the
filesystem for a sentinel proves nothing.

So a Grok plugin can deliver instructions to the model — not through
`additionalContext` (§2.10) and not through hooks (§2.5), but through
`skills/`. That is also what the one official Grok-native plugin in the
xAI marketplace ships instead of hooks.

---

## 3. The resolution

One payload, shared by both harnesses. Not a Grok-shaped copy alongside
the Claude one — that is two of everything, and the drift is silent.

### 3.1 The payload lives in `plugins/scout/`

```
.claude-plugin/marketplace.json        # source: "./plugins/scout"
plugins/scout/
  plugin.json                          # Grok reads this (§2.7)
  .claude-plugin/plugin.json           # Claude reads this (§2.7)
  hooks/hooks.json                     # union matcher; Claude-only in practice
  hooks/*.sh
  scripts/session-context.sh           # SessionStart guidance, Claude-only
  skills/scout/SKILL.md                # the only guidance channel Grok has
  bin/scout                            # the real binary, put there by `make`
  bin/.gitkeep
```

This one move retires §2.1, §2.2 (the payload is 12 MB, not 3.9 GB), and
§2.8 at once.

The cost: `CLAUDE_PLUGIN_ROOT` sits two levels below the repo root, so
anything in the payload that wants a repo path — `target/release/scout`,
`config.example.toml` — must resolve it by walking up for `Cargo.toml`,
and must let the local-build branch simply not fire when there is no
checkout above. That is the Grok case regardless, since Grok copies.

### 3.2 The binary ships in the payload, via `make`

`make build` compiles `target/release/scout` and installs it to
`plugins/scout/bin/scout`. That path is gitignored; `bin/.gitkeep` keeps
the directory tracked. Folding it into `build` rather than adding a
separate target is deliberate — `make build` cannot leave the plugin
stale.

What it buys per harness:

- **Claude**, directory marketplace: `PLUGIN_ROOT` *is* the checkout, so
  `make build` lands on the next session start with no bootstrap at all.
- **Grok**: the payload copy carries the binary at install time (§2.2),
  so the server spawns on the **first** session, also with no bootstrap.

**Copy with rename, never in place.** Under Claude's directory
marketplace the MCP server is executing the binary *at the path `make`
wants to write*, and Linux refuses to overwrite a running executable:

```
cp: cannot create regular file '.../bin/scout': Text file busy
```

This is not hypothetical. An earlier `ensure-binary.sh` used
`cp "$LOCAL_BUILD" "$BIN"` and failed with `ETXTBSY` whenever a scout MCP
server was live — which, after the first session, is always. The failure
was self-perpetuating and silent apart from a status string: the
SessionStart line read `Binary status: error: copy from local build
failed (rebuilt since last install)` and the session went on using a
binary that could never be updated. It ran that way for weeks.

**Every writer of the binary** — the make target, `install-bin`, any
bootstrap — must write a sibling temp file and `mv` it into place. Rename
is atomic and succeeds against a busy target; the running process keeps
its old inode until it exits. Both `cp` and `install` truncate in place
and both fail.

One limit remains: a clean `git clone` has no binary, so a marketplace
install straight from GitHub yields a payload that cannot spawn. Prebuilt
release binaries are what closes that, and they would populate this same
`bin/scout` path — the two designs agree.

### 3.3 One MCP command spelling, two manifest copies

Both `plugins/scout/plugin.json` and
`plugins/scout/.claude-plugin/plugin.json` declare exactly:

```json
"mcpServers": {
  "scout": { "command": "${CLAUDE_PLUGIN_ROOT}/bin/scout", "args": ["mcp"] }
}
```

Per §2.4 this resolves under both harnesses. No `PLUGIN_DATA`, no PATH
dependency, no wrapper, no SessionStart ordering dependency — and it
removes the first-session spawn failure that the plugin-data design
inflicted on Claude users.

The two files are ~20 duplicated lines, required because the harnesses
read different paths (§2.7). Keep them honest with a make target that
copies one to the other, or a test that diffs them.

*Open:* whether Claude also accepts the payload-root `plugin.json`. If it
does, the `.claude-plugin/` copy can be deleted and the duplication
disappears.

### 3.4 Config is seeded by the binary, not by a hook

A hook cannot seed config under Grok (§2.5), which would leave a healthy
server failing every real call. So **the binary seeds it**: on startup,
if no config exists at the resolved path, write the default and note it.

This is the right fix independent of Grok — it makes `scout` on PATH
robust for anyone who installed by `make install`, `cargo install`, or a
release tarball, none of which run a hook either.

Resolution order has to stay in step across `src/config.rs`,
`hooks/shell-safety.sh`, and the Makefile: `$SCOUT_CONFIG`, else
`${XDG_CONFIG_HOME:-~/.config}/scout/`.

### 3.5 Guidance ships as a `SKILL.md` in the payload

`additionalContext` reaches Claude and nothing else (§2.10); hooks reach
nothing at all under Grok (§2.5). Skills load (§2.11), so
`plugins/scout/skills/scout/SKILL.md` carries the delegation table.

A skill beats an `AGENTS.md` for this: it travels with the plugin, so it
works in any session rather than only in sessions whose cwd is this repo.

The skill's `description` frontmatter is what decides whether it is ever
invoked, so it names the **triggering situations** — large file, noisy
grep, build or test output — rather than describing scout.

Two constraints on the content, both from `CLAUDE.md`: never bake a
fully-qualified MCP tool name into it, and do not tell the model to use
scout for every identifier search.

Two copies of this text exist — the SessionStart script for Claude, the
skill for Grok. Generate one from the other, or accept the duplication
and note it in both.

### 3.6 Hooks are Claude-only, and that is settled

`hooks/hooks.json` stays where it is, with the union matcher
`Bash|run_terminal_command`. It is correct for Claude, costs nothing, and
is inert on Grok — and per §2.5 no other location or spelling changes
that.

The build/test redirect and the shell-safety auto-allow are therefore
**Claude-only features**. Say so plainly in the README rather than
letting a Grok user infer that scout is misbehaving.

Worth naming the layer that does not transfer: under Claude, scout's
steering is three layers — guidance, MCP tool descriptions, and
PreToolUse hooks — and **the hooks are the only one that fires without
the model choosing to cooperate.** Grok gets the two passive layers.
Expect weaker routing there, and do not read a Grok session's preference
for native `grep` as a scout bug.

---

## 4. Verification checklist

Worth re-running after any packaging change.

**Grok:**

1. `grok plugin disable scout` — kill the Claude-compat load (§2.9).
2. `grok plugin marketplace add <checkout>` (or `update`).
3. `grok plugin list --json --available` includes
   `{ "name": "scout", "marketplace": "scout" }`.
4. `grok plugin install scout --trust` succeeds;
   `~/.grok/installed-plugins/scout-<id>` is ~12 MB and contains no
   `target/`.
5. `grok mcp doctor scout`: command found under
   `~/.grok/installed-plugins/`, handshake OK, tools listed — on the
   **first** session, with no manual copy. No unrelated project servers
   attributed to `plugin: scout`.
6. A new Grok session's `/mcp` expands scout to `check_output` /
   `extract` / `grep` / `ping`.
7. A scout MCP call succeeds on a machine with no
   `~/.config/scout/config.toml` (§3.4).
8. The session lists a `scout` skill in its available-skills inventory
   *without being asked to search for it*, and routes a large-file
   question to `extract` rather than `read_file`.

**Claude Code:**

9. `/plugin install scout@scout` resolves against
   `source: "./plugins/scout"`.
10. MCP spawns on the **first** session, with no plugin-data bootstrap.
11. `make build` leaves `plugins/scout/bin/scout` current, and a rebuild
    is picked up after a session restart.
12. Both PreToolUse hooks still fire.
13. Does Claude accept the payload-root `plugin.json`? If yes, drop
    `.claude-plugin/plugin.json` from the payload (§3.3).

### An alternative that skips plugins entirely

```sh
grok mcp add scout -- scout mcp
```

Registers a PATH-resolved `scout` in `~/.grok/config.toml` — no
plugin-data, no SessionStart, no reinstall after a rebuild if
`make install` keeps `~/.local/bin/scout` current. Disable the
Claude-compat load first (§2.9) or you will have two scout servers.

---

## 5. Decision log

Closed by measurement:

- **PATH or plugin-data for the MCP command?** Neither. Plugin-root, one
  spelling, both harnesses (§2.4). Plugin-data is unreachable under Grok
  and unnecessary under Claude.
- **A wrapper that resolves the binary at runtime?** Impossible — the
  child process sees none of the plugin variables (§2.6).
- **One `hooks.json` or two?** One, union matcher. Two files would be two
  copies of something that only runs in one harness anyway (§2.5).
- **Should Grok run SessionStart before MCP spawn?** Moot. It does not
  run plugin hooks at all, and the design no longer depends on it.
- **Will Grok carry a 12 MB binary in the payload?** Yes — exec bit
  intact, and it runs (§2.2).
- **Are Grok's plugin hooks recoverable?** No. Six declaration sites, two
  event vocabularies, two manifest forms, all silent (§2.5). Not a
  packaging problem, so not scout's to fix.
- **How does guidance reach a Grok session?** A `skills/` entry in the
  payload. Proven loaded and quoted back (§2.11).
- **Does scout need a `.grok-plugin/` directory?** No. Payload-root
  `plugin.json` outranks it, and only one manifest is ever read (§2.7).

Still open:

- **Does Claude accept a payload-root `plugin.json`?** If so, one
  manifest instead of two (§3.3).
- **Prebuilt release binaries.** This is what makes scout installable by
  someone who is not building from a checkout. The payload `bin/scout`
  path is where a release tarball would land.
- **Live checkout vs snapshot for Grok development.** Grok copies;
  reinstalling after a rebuild is the honest cost. A symlink would paper
  over it on one machine and mislead about what users experience. Not
  recommended.
