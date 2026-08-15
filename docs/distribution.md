# Distributing scout

scout has two installable surfaces and they do not install each other.
The **CLI** is a Rust binary (`scout`). The **plugin** is a directory of
manifests, hooks, a skill, and an MCP command that points at a binary.
crates.io ships the first. A git marketplace ships the second. Official
catalogs are optional discovery on top of the git marketplace, not a
third product.

How each harness *loads* the plugin — which manifest it reads, which
variables expand, why hooks are Claude-only — is
[`plugin-packaging.md`](plugin-packaging.md). This document is the
release picture: where a stranger gets scout from, what each channel
actually delivers, and which gaps a publish does not close.

**The short version:** a public GitHub repo that already has
`.claude-plugin/marketplace.json` is a marketplace. Nobody has to
register scout for `/plugin marketplace add joshcarter/scout` to
resolve. crates.io (`scout-llm`) is a separate, source-only channel:
`cargo install scout-llm` puts `scout` on PATH and does not install
hooks, the skill, or a Claude/Grok plugin. A marketplace fetch from
GitHub still arrives with an empty `bin/` because the payload binary is
gitignored. Publishing the crate does not fill that file.

---

## 1. Two products, one codebase

| Surface | What the user gets | How they get it |
|---|---|---|
| `scout` CLI + `scout mcp` | The Rust binary | `cargo install scout-llm`, `make install`, or a GitHub Release |
| Coding-agent plugin | Manifests, hooks, skill, SessionStart script, MCP *config* pointing at a binary | `/plugin marketplace add …` then `/plugin install scout@scout` (or the Grok equivalent) |

They share config (`${XDG_CONFIG_HOME:-~/.config}/scout/`) and they
share code. They do not share an installer.

---

## 2. Plugin channel: marketplaces

### 2.1 A marketplace is a git repo

Neither Claude Code nor Grok Build requires a central app-store account.
A marketplace is a git repository (or a local folder) with a catalog
file:

- Claude Code reads `.claude-plugin/marketplace.json`
- Grok Build reads `.grok-plugin/marketplace.json`, and also accepts
  the Claude path

Users add the catalog, then install a plugin from it. Private repos
work, using each person's git credentials. Public is what a broader
release wants.

Grok will not catalog a plugin that *is* the marketplace root
(`source: "./"`). A subdirectory is required. Claude accepts both.
That is why the payload lives at `plugins/scout/` — see
[`plugin-packaging.md`](plugin-packaging.md) §2.1 and §3.1.

### 2.2 What scout already has

```
.claude-plugin/marketplace.json        # name: "scout", source: "./plugins/scout"
plugins/scout/
  plugin.json                          # Grok
  .claude-plugin/plugin.json           # Claude
  hooks/ …
  skills/scout/SKILL.md
  bin/scout                            # gitignored; `make build` writes it
```

One catalog serves both harnesses. No `.grok-plugin/` directory is
needed.

### 2.3 Install commands

**Claude Code**

```
/plugin marketplace add joshcarter/scout
/plugin install scout@scout
```

**Grok Build**

```
grok plugin marketplace add joshcarter/scout
grok plugin install scout --trust
```

Grok can also skip the marketplace and install a subdirectory of a repo
directly: `grok plugin install joshcarter/scout#plugins/scout --trust`.
Claude does not have an equivalent one-shot for a remote repo; it wants
a marketplace add first (or a local `--plugin-dir` for development).

A directory marketplace pointed at a checkout is the **development**
path, and the one the README currently documents. `CLAUDE_PLUGIN_ROOT`
then *is* the working tree, so `make build` is live on the next
session. Grok copies on install; a rebuild means a reinstall. Measured
detail is in [`plugin-packaging.md`](plugin-packaging.md) §2.2.

### 2.4 Official catalogs — optional, for in-app browse

These are indexes that *point at* the git repo. They do not host the
product. They are how a stranger finds scout without being told
`joshcarter/scout`. None of them is required for installability.

| Catalog | How scout gets in | What it buys |
|---|---|---|
| **Claude official** (`anthropics/claude-plugins-official`) | Anthropic curates this. No application. The submission form does **not** put a plugin here. | Auto-registered the first time someone launches Claude Code interactively. Install is `/plugin install scout@claude-plugins-official`. |
| **Claude community** (`anthropics/claude-plugins-community`) | In-app form: [claude.ai](https://claude.ai/admin-settings/directory/submissions/plugins/new) (Team/Enterprise) or [Console](https://platform.claude.com/plugins/submit). Also [clau.de/plugin-directory-submission](https://clau.de/plugin-directory-submission). Run `claude plugin validate ./plugins/scout` first. | Users add `anthropics/claude-plugins-community`, then install `@claude-community`. Approved entries are SHA-pinned; CI bumps the pin as commits land. Nightly sync, so there is a delay after approval. |
| **Grok official** (`xai-org/plugin-marketplace`) | PR that adds one entry to `.grok-plugin/marketplace.json` pointing at the public repo, pinned to a full 40-char commit SHA. Regenerate `plugin-index.json` and run their validators. Guide: [CONTRIBUTING.md](https://github.com/xai-org/plugin-marketplace/blob/main/CONTRIBUTING.md). | Shows up in Grok's built-in Marketplace tab with no extra `marketplace add`. |

Do not submit to either official catalog until the binary story in §2.5
is honest. A listing that installs a payload whose MCP command points
at a missing file is worse than no listing.

### 2.5 The binary is the real release problem

`plugins/scout/bin/scout` is gitignored. Both harnesses clone the git
tree (Grok then copies the payload). The MCP command is
`${CLAUDE_PLUGIN_ROOT}/bin/scout`. A GitHub-fetched payload has an
empty `bin/`, the server never starts, and the hooks go quiet the same
way.

GitHub Releases do not fix that by themselves. Releases are not what
`marketplace add` fetches. A release asset sitting next to a tag is
invisible unless the binary is in the git tree that gets cloned, or
the plugin finds the binary some other way.

That split is sharper on Grok than on Claude:

- Claude marketplace entries can use extra source types: `archive`
  (HTTPS zip, optionally sha256-pinned — the natural "GitHub Release
  zip containing `bin/scout`" path) and `command` (a local installer
  that prints a plugin directory). Those need a recent Claude Code
  (`archive` ≥ 2.1.224, `command` ≥ 2.1.229).
- Grok's catalog only has **local path in this repo** or **git URL +
  pinned SHA**. No zip source. Install is always a git snapshot.

Grok's own docs say marketplaces deliver skills, hooks, and MCP
*config*, not native programs. xAI's review explicitly rejects
"downloading and running binaries." A first-spawn `curl` of a release
ELF is likely to fail official-marketplace review even if it works
technically.

Claude's plugin `bin/` is documented as being added to the Bash tool
PATH. That does not help MCP spawn: scout's server is still an
explicit command path, and Grok does not put payload `bin/` on the
child's PATH ([`plugin-packaging.md`](plugin-packaging.md) §2.4).

Options, when the time comes, in the order they were weighed:

1. **PATH `scout`.** Change the declared command from
   `${CLAUDE_PLUGIN_ROOT}/bin/scout` to `scout`. The plugin delivers
   hooks, skill, and MCP config; the binary is a normal CLI install.
   Matches Grok's stated model and is the most likely to pass
   `xai-org/plugin-marketplace` review. Cost: a GitHub-only plugin
   install with no CLI on PATH is a dead server — the README has to
   say that.
2. **Committed wrapper, not a committed ELF.** A small script at
   `plugins/scout/bin/scout` that locates `scout` on PATH, or
   downloads the matching release into plugin data, then execs. Works
   for a self-hosted marketplace without committing 12 MB per arch.
   Official Grok review will probably still dislike the download
   branch.
3. **Claude-only archive source.** A marketplace entry pointing at a
   Release zip that already contains `bin/scout`. Fine for Claude;
   does nothing for Grok.

Do not commit platform binaries to `main`. Do not rely on a
SessionStart hook to fetch them — Grok does not run plugin hooks,
which is why config seeding moved into the binary
([`plugin-packaging.md`](plugin-packaging.md) §2.5, §3.4).

### 2.6 Hooks are Claude-only

The build/test redirect and the shell-safety auto-allow ride on
PreToolUse. Grok Build does not execute plugin hooks at all. Under
Grok a user gets the MCP tools and the `scout` skill; the automatic
steering is Claude's. That is a harness gap, not a packaging miss, and
nothing in this document changes it. See
[`plugin-packaging.md`](plugin-packaging.md) §2.5 and §3.6.

---

## 3. CLI channel: crates.io

crates.io is a **source** registry, not a binary host.
`cargo install scout-llm` compiles on the user's machine. That is a
fine CLI path for anyone with Rust. It does not replace GitHub
Releases for people who just want a tarball, and it does not install
the plugin.

Nothing has been published yet. The crate name `scout-llm` is unused.
The bare name `scout` is taken by an unrelated fuzzy finder — that is
why the package is `scout-llm` and the binary is `scout`.

### 3.1 Account and token

1. Sign in at [crates.io](https://crates.io/) with GitHub.
2. Verify email under
   [Account Settings](https://crates.io/settings/profile). Publish is
   blocked until that is done.
3. Create a token at [API tokens](https://crates.io/settings/tokens).
   Scope it to publish. Copy it once.
4. `cargo login` writes `~/.cargo/credentials.toml`. Treat it like a
   password; revoke it on crates.io if it leaks.

The token is required for the **first** publish. Later the GitHub repo
can switch to [trusted publishing](https://crates.io/docs/trusted-publishing)
(OIDC, no long-lived token in Actions).

### 3.2 Manifest prerequisites

`cargo package --list` already works. crates.io will not take the
crate yet: there is no `LICENSE` file and no `license` /
`license-file` in `Cargo.toml`. Cargo warns; the registry rejects.

A license has to be a real decision — yank does not delete the
uploaded source. For a CLI like this, `MIT` or `MIT OR Apache-2.0` is
the usual Rust choice. Once picked, set `license` and add the file.

Recommended metadata (description and repository are already there):

```toml
license = "MIT"                          # or whatever is picked
homepage = "https://github.com/joshcarter/scout"
readme = "README.md"
keywords = ["llm", "cli", "mcp", "coding-agents"]
categories = ["command-line-utilities", "development-tools"]
```

`keywords` maxes out at 5. `categories` must be from
[this list](https://crates.io/category_slugs).

### 3.3 What belongs in the crate

`cargo publish` uploads a source tarball. `cargo install` compiles it
and puts **one file** on PATH: `~/.cargo/bin/scout`. Supporting files
in the crate are used at compile time or sit unused in the registry
cache (`~/.cargo/registry/src/…/scout-llm-<ver>/`). Claude Code and
Grok never read that path.

So treat crates.io as the CLI crate. Keep what the binary needs to
compile:

- `src/`
- `presets/*.toml` (`include_str!`)
- `config.example.toml` (`include_str!`)
- `dashboard.html` (`include_str!`)
- `README.md`, `Cargo.toml`, `Cargo.lock`, the license file

Leave `plugins/` out. The hook scripts, skill, and manifests live in
the git repo that already is the marketplace — that is the channel
they actually use. Putting them in the crate would only make the
tarball fatter and invite the false reading that `cargo install` sets
up Claude Code.

Default packaging follows git, so today the crate would also ship
`docs/`, `CLAUDE.md`, `TODO.md`, and `plugins/`. Trim with `exclude`
(or an explicit `include`). `cargo package --list` is the check.
crates.io's `.crate` limit is 10 MB.

### 3.4 First publish

```sh
cargo test
bash tests/test-prefer-local-llm.sh
bash tests/test-shell-safety.sh
bash tests/test-suggest-scout.sh

cargo publish --dry-run
# inspect target/package/
cargo package --list
```

`--dry-run` packs the crate and compiles it from that tarball. That is
the step that catches a missing `include_str!` file.

When that is clean: `cargo publish`. The upload is permanent. `0.1.0`
cannot be overwritten. `cargo yank --vers 0.1.0` only stops *new*
dependents from picking it; the bits stay.

Do not publish an empty placeholder to squat the name. crates.io
reclaims unused reservations, and a working `0.1.0` is a legitimate
first release — the README already says pre-1.0.

After publish:

```sh
cargo install scout-llm
scout --version          # 0.1.0
```

Config still seeds at `~/.config/scout/config.toml` on first run. The
plugin is a separate install.

Next version: bump `version` in `Cargo.toml`, tag the commit, publish
again.

### 3.5 After the first publish

- On the crate's crates.io Settings → **Trusted Publishing**, add this
  GitHub repo. First publish still needs the API token; after that
  Actions can publish with `rust-lang/crates-io-auth-action` and no
  stored secret.
- Keep a git tag that matches the published version (`v0.1.0`).
- GitHub Release binaries are a different pipeline (`cargo build
  --release` per target, attach assets). crates.io will not produce
  those.

---

## 4. How the channels interact

They barely do. A crates.io publish does not register, install, or
enable the Claude/Grok plugin. A marketplace install does not put
`scout` on PATH and does not publish a crate.

### 4.1 What each install actually does today

**crates.io only** (`cargo install scout-llm`)

The user can run `scout grep`, `scout check`, `scout extract`, and
they can point a generic MCP client at `scout mcp`. In Claude or Grok
they have **no** PreToolUse redirect, **no** shell-safety hook, **no**
SessionStart guidance, **no** `scout` skill. The harness does not know
scout exists unless they also add the plugin, or they hand-wire
`scout mcp` as a project MCP server (tools only, still no hooks).

**Plugin only** (GitHub / directory marketplace)

They get hooks + skill + an MCP command of
`${CLAUDE_PLUGIN_ROOT}/bin/scout`. That path is the **payload copy**
inside the plugin, not `~/.cargo/bin/scout`. Because
`plugins/scout/bin/scout` is gitignored, a marketplace fetch from
GitHub still arrives with an empty `bin/` — crates.io being published
does not fill that file. A user who already did `cargo install
scout-llm` still has a dead plugin MCP server, because the manifest
does not look on PATH.

**Both**

Same as plugin-only for the agent, plus a working CLI in the terminal.
The two binaries are independent copies. Updating one (`cargo install
--force`, or `make build` in a checkout) does not update the other.

### 4.2 A future PATH coupling

If the plugin's MCP command changes from
`${CLAUDE_PLUGIN_ROOT}/bin/scout` to bare `scout`, then
`cargo install scout-llm` would satisfy the **server** half of a
plugin install. The harness would spawn whatever `scout` is on PATH.

That still would not install hooks. Hooks only run because they live
in the plugin payload and Claude loads `hooks/hooks.json` from
`CLAUDE_PLUGIN_ROOT`. crates.io cannot deliver that. Grok still would
not run them even if they were present.

### 4.3 Honest public install story

Two install lines, not one:

```
# terminal / PATH / generic MCP
cargo install scout-llm

# Claude or Grok plugin (hooks + skill + MCP config)
/plugin marketplace add joshcarter/scout
/plugin install scout@scout
```

The second line does not work from GitHub until the payload has a
binary (PATH lookup, a wrapper, or a Release-built `bin/scout`).
Publishing the crate does not close that gap.

---

## 5. Still open

- **License.** Required before `cargo publish`. Not yet chosen.
- **`exclude` / `include` in `Cargo.toml`.** Needed so the crate is
  the CLI crate and not a snapshot of the whole repo. Follows the
  license.
- **How the plugin finds a binary on a GitHub install** (§2.5). PATH,
  wrapper, or Claude-only archive. Not decided. This is what makes
  the plugin installable by someone who is not building from a
  checkout; [`plugin-packaging.md`](plugin-packaging.md) §5 listed
  prebuilt release binaries as the same open item.
- **Official catalog submissions.** After the binary story is honest.
  Claude community via the form once `claude plugin validate
  ./plugins/scout` is clean. `xai-org/plugin-marketplace` only if the
  plugin does not depend on shipping or downloading an ELF. Do not
  expect `claude-plugins-official`.
- **GitHub Release CI.** Independent of crates.io. Needed for users
  without a Rust toolchain, and for any archive-source or wrapper
  that fetches a tarball.
