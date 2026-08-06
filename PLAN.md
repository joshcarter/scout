# Plan: extract the local-LLM feature into **scout**, a standalone CC plugin

**Status:** third pass — all decisions settled; ready to execute.
Name: **scout** (binary, plugin id, MCP server, config dir);
crates.io package: **scout-llm** (bare `scout` is squatted by a fuzzy
finder). Settled earlier: scope cut, no ct fixup (switching to upstream
ct afterward), no ct integration, clean break on config/compat, full
Claude Code plugin packaging, bootstrap-on-SessionStart installer.
**Builds on:** `LOCAL_LLM_STANDALONE_SURVEY.md`.

---

## 1. Scope

Discarded presets: `commit_message`, `pr_description`, `write_tests`,
`refactor` — the local LLM isn't good at composing net-new code, and
Claude's models never used those tools anyway.

Kept — the things the local model is actually good at:

| Role | Preset | Exposure |
|---|---|---|
| Checking build/test output | `check_output` | MCP tool + hook redirect |
| Checking safety of shell commands | `shell_safety` | CLI (PreToolUse hook) |
| Targeted file investigation | `extract` | MCP tool |
| Intent-filtered grep | `grep` | MCP tool |
| Review-horde persona routing | `quality_review`, `test_review` | CLI (`run --preset`) |
| Ad-hoc escape hatch | (none) `task` verb | CLI |

`quality_review`/`test_review` are free to keep: ct-free providers
(`file_read`, `git_diff_range`), consumed only through the one-shot `run`
CLI by claude-review's `lib-launch.sh:945-962`. They move as-is.

### What the scope cut deletes (vs. the survey's move list)

- `locate_symbol` — the survey's "genuinely lossy piece" — is gone; only
  `write_tests`/`refactor` needed it. No symbol location, no tree-sitter.
- `placement.rs` (952 LOC): deleted, served only test placement/splicing.
- The place→build→restore→retry repair loop in `mcp.rs`: deleted.
- `verify.rs` slims from 792 LOC to a fraction: keep `run_command_capture`,
  `truncate_diagnostic`, `detect_language` (for `check_output`); drop
  `run_build_check`, `has_meaningful_assertions`, `repair_context_message`,
  snapshot/restore.

Revised size: **~4,000–4,500 LOC**, mostly moved verbatim.

## 2. No ct anywhere

Two consequences of "switch back to upstream ct afterward":

1. **No ct-side cleanup phase.** The extraction is a pure copy-out into a
   fresh repo. This fork is abandoned when the new tool works; nothing here
   needs to stay green during the transition beyond not breaking the
   current setup until cutover day.
2. **No `CodeSource` trait, no `CtSource`.** The new tool reads files with
   `std::fs` and greps with ripgrep's library crates (`ignore` +
   `grep-searcher`): gitignore-aware walking, binary skipping, ±N context,
   zero runtime dependency on installed `rg`/`grep`, identical on macOS
   and Linux. `local_grep.rs::parse_hits` swaps `ct::Response` for plain
   structs. `extract`'s ct call (`local_extract.rs:76`) was only fetching
   file content — `fs::read_to_string` + numbering is lossless.

Grep behavior defaults (was open issue 9 — proposing, not asking):
respect `.gitignore` + global ignores, skip hidden dirs and binaries,
context ±2, per-file size cap, hit caps carried over from
`local_filter_config.rs`. All already tunable via that config.

## 3. What the new project is

A **Claude Code plugin** whose payload is one Rust binary. The plugin
bundling is the point: the houtini-lm lesson is that an MCP server alone
sits unused — CC must be *told* the local tools exist, what they're for,
and be actively steered toward them. Three steering layers, all of which
exist today and move over:

1. **MCP tool descriptions** — passive discovery (`tools/list`).
2. **PreToolUse hooks** — active steering:
   - `prefer-local-llm.sh`: denies bare build/test Bash commands and
     redirects to `check_output` via `permissionDecisionReason`, with the
     `# raw-output` escape hatch (commit cb7491c's design).
   - `shell-safety.sh`: screens Bash commands through the
     `shell_safety` preset.
3. **Injected guidance** — the CLAUDE.md delegation-table content, moved
   into whatever mechanism the plugin system supports (skill,
   SessionStart context, or plugin memory — pending §6 research).

Both hooks belong to the new plugin, not claude-review. claude-review
keeps only its horde routing (`lib-launch.sh`), which just execs the
binary from PATH.

### Repo layout (sketch — §6 research may adjust)

```
scout/
  .claude-plugin/plugin.json      # manifest
  .mcp.json                       # launches the binary in MCP mode
  hooks/hooks.json                # PreToolUse: shell-safety, prefer-local-llm
  hooks/*.sh
  src/                            # single crate
    main.rs                       # subcommands: mcp (stdio server), run, stats
    client.rs                     # OpenAI-compatible HTTP client (moves as-is)
    run_cmd.rs                    # one-shot CLI (moves as-is)
    presets/                      # loader/template/providers (git_* + file_read only)
    extract.rs / grep.rs / select.rs / filter_config.rs   # from cmd/ct/src/local_*
    verify.rs                     # slimmed: run_command_capture + helpers
    check_output.rs               # from mcp.rs::forward_check_output
    source.rs                     # fs read + ignore/grep-searcher walk
    stats.rs
  presets/*.toml                  # 6 presets, embedded via include_str!
```

MCP tools exposed: `check_output`, `extract`, `grep` (short names —
the server name provides the namespace: `mcp__scout__check_output`).

### CLI is a first-class surface, not just hook plumbing

`run --preset` (unchanged) stays for hooks and scripts, but scout is
also a human tool. The presets already take named args, so ergonomic
subcommands are thin wrappers:

```
scout grep <pattern> "<what I'm actually looking for>"   # intent-filtered grep
scout extract <file> "<question>"                        # targeted file Q&A
scout check "cargo test --quiet"                         # run + classify output
scout task "<ad-hoc prompt>"                             # escape hatch
scout run --preset <p> --arg k=v                         # raw preset invocation
scout stats                                              # call log report
```

Same code path as the MCP tools (preset + providers + filter), different
argument parsing. This costs one clap layer and makes the tool useful
outside Claude Code entirely.

## 4. Transport

Replace the bespoke ct plugin protocol with an MCP stdio server via the
`rmcp` crate. Deleted rather than ported: `plugin.rs` (539 LOC),
`plugin_config.rs` (422), the `ready`/`initialize`/`shutdown` handshake
half of `main.rs`, `LOCAL_LLM_PLUGIN_PROTOCOL.md`. One process, one
protocol, one hop:

```
Claude Code --MCP stdio--> scout --HTTP--> local LLM
hooks (*.sh) --exec------> scout run --preset ...
```

## 5. Config — clean break

Single loader, single file: `~/.config/scout/config.toml`, `[llm]`
section. No fallback to `~/.claude/ct/config.toml`, no `$CT_LLM_CONFIG`
(a fresh `$SCOUT_CONFIG` override), stats log moves to
`~/.local/state/scout/calls.jsonl`. This also collapses the survey §5b
duplication (two parsers with different clamping) to one.

Built-in presets embedded via `include_str!` (fixes survey §5a — the
binary-only install today loads zero presets); the user preset directory
becomes overrides only.

## 6. Installer — resolved (researched against current CC plugin docs)

Requirement: a proper installer for a well-behaved CC plugin; **no
symlinking sources into `~/.claude`**. Facts that shape the design
(docs: code.claude.com/docs/en/plugins-reference.md, hooks.md, mcp.md):

- Plugins install via marketplace/git/local path; there are **no
  install/postinstall lifecycle scripts**.
- `${CLAUDE_PLUGIN_ROOT}` works in hook commands and `.mcp.json`
  `command`/`args` — but it is **ephemeral across plugin updates**;
  persistent state belongs in `${CLAUDE_PLUGIN_DATA}`
  (`~/.claude/plugins/data/<id>/`, survives updates).
- A plugin `bin/` directory is added to the Bash tool's PATH.
- PreToolUse deny + `permissionDecisionReason` works identically in
  plugins — the prefer-local-llm redirect ports as-is.

**Recommended pattern — bootstrap-on-SessionStart:**

1. CI builds per-platform release binaries (macOS arm64, Linux x86_64,
   Linux arm64) attached to GitHub Releases; version tags match the
   plugin's `plugin.json` version.
2. The plugin ships a `scripts/ensure-binary.sh` SessionStart hook:
   if `${CLAUDE_PLUGIN_DATA}/bin/scout` is missing or its `--version`
   mismatches the plugin version, download the matching release asset
   (fallback: `cargo install scout-llm` if no asset for the platform,
   with a clear error otherwise — the bare `scout` crate name is taken
   by an unrelated fuzzy finder). Idempotent, silent when already
   current.
3. `.mcp.json` and `hooks/hooks.json` reference the binary by absolute
   path: `${CLAUDE_PLUGIN_DATA}/bin/scout`.
4. A one-line shim in the plugin's `bin/` (`exec
   "${CLAUDE_PLUGIN_DATA}/bin/scout" "$@"`) puts the binary on the Bash
   tool's PATH — this is what lets claude-review's horde keep calling
   `scout run --preset quality_review` without knowing where the plugin
   lives.

So "the installer" is just `claude plugin install` (or `--plugin-dir`
during development) — the SessionStart bootstrap does the rest. No
symlinks, no `make install`, repo never touches `~/.claude` directly.

### Guidance injection — resolved

Plugins have no CLAUDE.md equivalent, so the delegation-table content
moves to a **SessionStart hook emitting `additionalContext`** (a compact
version of today's CLAUDE.md table: which tools exist, when to prefer
them, the `# raw-output` escape hatch). The same ensure-binary hook can
emit it. Layered with MCP tool descriptions (automatic) and the
PreToolUse redirect (enforcement), this is the houtini-lm fix: passive
discovery, standing guidance, and active steering all at once. A skill
(`SKILL.md` with a strong trigger description) is available as a fourth
layer if the standing context proves too weak in practice.

## 7. Sequencing

1. **Scaffold** the new repo: crate + plugin skeleton, `rmcp` stdio
   server with a stub tool, installable end-to-end before any logic moves.
2. **Move the ct-free core**: `client.rs`, presets subsystem, `run_cmd.rs`,
   `stats.rs`, the 6 preset TOMLs (embedded). At this point
   `scout run --preset shell_safety` works — CLI parity.
3. **Move + rewire extract/grep/select/filter_config** onto the fs/ignore
   backend; move slimmed `verify.rs` + `check_output`. MCP parity.
4. **Port the hooks** (`shell-safety.sh`, `prefer-local-llm.sh`) into the
   plugin; update the redirect string to `mcp__scout__check_output`;
   write the guidance/skill content.
5. **Cutover day**: install the plugin; update claude-review
   (`lib-launch.sh` binary name, CLAUDE.md delegation table,
   `install-global-config.sh`); remove the old `.mcp.json` ct-local
   entries; switch ct to upstream.
6. **Release**: README, license header, tag 0.1.

Steps 1–4 run entirely in the new repo with the old setup still live;
step 5 is the only switch-flip.

## 8. Decision log (all issues resolved)

- **Name: `scout`** — binary, plugin id, MCP prefix (`mcp__scout__*`),
  config dir. Chosen over `sidecar` (name exhausted by multiple AI-agent
  companion tools, including an existing MCP server) and `recon`
  (owned by pentest/OSINT tooling; wrong connotation in a tool list).
  Docker Scout is namespaced under `docker`, so no PATH conflict; the
  crates.io squat only affects the package name.
- **Crate name: `scout-llm`** (available; so are scout-ai/scout-mcp —
  `-llm` says what it is, not how it ships).
- **Installer**: bootstrap-on-SessionStart (§6). **Guidance injection**:
  SessionStart `additionalContext` + MCP descriptions + PreToolUse
  redirect (§6).
- **License check**: clean — `git log --follow` confirms `verify.rs`,
  `local_*.rs`, and `crates/ct-local-llm/` all originate in post-fork
  TD-3xx commits (May 2026+); the repo has no upstream LICENSE file.
  Caveat: `cmd/ct/src/mcp.rs` may carry upstream ancestry — copy only
  the `forward_check_output` logic out of it, never the file wholesale.
- **Review presets kept** (`quality_review`, `test_review`); generic
  `task` verb kept on the CLI. **CtSource dropped**; plain fs +
  `ignore`/`grep-searcher`. **Backcompat**: clean break.
