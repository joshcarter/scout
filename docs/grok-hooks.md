# Grok hooks: one script, two envelopes

scout's PreToolUse hooks — build/test redirect, shell-safety auto-allow,
extract/grep nudge — are the only steering layer that fires without the
model choosing to cooperate. They work under Claude Code. Under Grok
Build they do not, and no packaging change inside `plugins/scout/` will
make them. This document records what was measured, why a second copy of
the scripts is the wrong response, and the thin envelope that lets one
script serve both harnesses once Grok is given a registration it will
actually run.

It is the follow-on to [`plugin-packaging.md`](plugin-packaging.md) §2.5
and §3.6. That document closed "can the plugin hook file be made to
fire?" (no). This one answers "so do we fork the scripts?" (also no).

The findings below were measured against **Grok Build 1.0.4**
(`d846eb93d9`) on 2026-08-15, in a live session that already had scout's
MCP server and skill loaded. 1.0.3 had the same result; 1.0.4's changelog
added `updatedInput` and `StopCancelled` and did not start executing
plugin hooks. Retest before trusting this against a later release.

**The short version:** keep one copy of each hook script. The
complicated middle (classify-command, deny floor, LLM call, unwrap,
throttle) is harness-agnostic. What differs is a ~15-line parse/emit
envelope plus a tiny Grok-side *registration* that points at those same
scripts, because Grok still does not execute `hooks/hooks.json`. Do not
add a `hooks-grok/` tree.

---

## 1. What a Grok session actually loads

`grok inspect` on this checkout, 1.0.4, with scout enabled:

| Surface | Present? | Form |
|---|---|---|
| MCP server (`check_output` / `extract` / `grep` / `ping`) | yes | spawned from the live checkout `plugins/scout/bin/scout` |
| `scout` skill | yes | listed in the session's skill inventory |
| `hooks/hooks.json` | discovered | `hookType: "file"`, `event: "(plugin)"`, `matcher: null` |
| PreToolUse command handlers from that file | no | never unfolded |

A working hook from `~/.claude/settings.json` (`prefer-ct.sh`) shows up
in the same inspect dump as `hookType: "command"`, `event: "pre_tool_use"`,
with its matcher intact. That contrast is the tell: Grok's inventory
knows the plugin file exists. The runner never turns it into handlers.

Scout is also **not** in Grok's own plugin registry. `grok plugin list`
showed only the probe plugins; `grok plugin details scout` was empty.
The session loaded scout through Claude compatibility
(`~/.claude/settings.json` `enabledPlugins["scout@scout"]`) plus
`[plugins].enabled = ["scout"]` in `~/.grok/config.toml`. That path
difference does not matter for this bug. The two plugins that *are*
natively installed with `--trust` (`probe`, `hookprobe`, both under
`~/.grok/installed-plugins/`) have the same dead hooks.

---

## 2. Plugin hooks do not execute

Three recorders were live in the 1.0.4 session. Each logs *before* it
looks at the payload, so a fire-and-fail is distinguishable from
never-ran:

- `~/grok-probe.log` — MCP servers spawned at session start. Zero
  `SESSIONSTART HOOK` / `PRETOOLUSE HOOK` lines. That file has never
  recorded a hook fire, on 1.0.3 or 1.0.4.
- `~/grok-hookprobe.log` — same pattern. MCP spawn only. hookprobe's
  `plugin.json` already declares `"hooks": "./hooks/alt-hooks.json"`;
  Grok found that path and still did not run it.
- The session event stream — 25 `tool_started` / `permission_requested`
  pairs, no hook events of any kind. `unified.jsonl` for the process
  had no hook lines either.

This is the same result as plugin-packaging.md §2.5, retested. Six
declaration sites, both event-name casings, inline-object and path-string
manifest forms, native `--trust` install and Claude-compat load: nothing
fires.

Grok's own user guide documents plugin hooks as a working feature
(`GROK_PLUGIN_ROOT`, trust, `/hooks`). The docs are ahead of the binary.
1.0.4 did not close the gap.

### What will not fix it

- Adding `"hooks": "./hooks/hooks.json"` to `plugin.json`
- Another `hooks.json` location or event-name casing
- `grok plugin install scout --trust`
- Trust / folder-trust (this project is trusted; `installed-plugins/`
  is auto-trusted)
- Matcher spelling (`Bash` vs `run_terminal_command`). The union
  matcher in `hooks/hooks.json` is still only forward-looking.

### What does run

Grok's hook runner **does** load non-plugin sources, and inspect expands
those into real `pre_tool_use` command rows:

| Source | Path | Trusted? |
|---|---|---|
| User hooks | `~/.grok/hooks/*.json` | always |
| Config | `[[hooks.PreToolUse]]` in `~/.grok/config.toml` | always |
| Project hooks | `<repo>/.grok/hooks/*.json` | folder-trust (this repo has it) |
| Claude settings | `~/.claude/settings.json` `hooks` | already how `prefer-ct.sh` is listed |

That is the only route that can deny or rewrite a tool call without the
model cooperating. Skills and MCP descriptions cannot.

---

## 3. Do not fork the scripts

The question after §2 is whether Grok needs its own
`prefer-local-llm-grok.sh`. It does not.

Almost all of `prefer-local-llm.sh` and `shell-safety.sh` is
classify-command, the deny floor, the LLM call, JSON unwrapping, and
logging. None of that cares which harness spawned the process.
`suggest-scout.sh` is the same: throttle and size checks are shared;
only the parse and emit are not.

What is Claude-shaped is about fifteen lines per script, in three
places. Dual-shape those. Leave the middle alone.

### 3.1 Input parse

All three scripts read Claude's snake_case and then require Claude's
tool name:

```bash
TOOL_NAME=$(printf '%s' "$INPUT" | jq -r '.tool_name // empty')
[ "$TOOL_NAME" = "Bash" ] || exit 0
COMMAND=$(printf '%s' "$INPUT" | jq -r '.tool_input.command // empty')
```

Grok sends camelCase (`toolName`, `toolInput`, `hookEventName`) and its
own tool names (`run_terminal_command`, `read_file`). Same facts,
different spellings. One `jq` normalize at the top handles both:

```bash
TOOL_NAME=$(printf '%s' "$INPUT" | jq -r '.tool_name // .toolName // empty')
COMMAND=$(printf '%s' "$INPUT" | jq -r \
  '.tool_input.command // .toolInput.command // empty')
FILE=$(printf '%s' "$INPUT" | jq -r \
  '.tool_input.file_path // .toolInput.file_path // .toolInput.target_file // empty')
```

Then treat `Bash` and `run_terminal_command` as the same case, and
`Read` and `read_file` as the same case. `suggest-scout.sh` already
accepts `run_terminal_command` as a name but still only reads
`.tool_name`, so a Grok payload falls through the `*)` and exits 0.

Grok's matcher aliases already map `Bash` → `run_terminal_command` and
`Read` → `read_file`, so a registration that says `matcher = "Bash"`
will fire on Grok's shell tool. The script still has to accept both
names in the payload, because the matcher alias does not rewrite
`toolName`.

### 3.2 Output vocabulary

Claude needs `hookSpecificOutput.permissionDecision`, and the redirect
text must ride in `permissionDecisionReason` — Claude silently drops a
top-level field named `reason`, leaving a bare "denied". That constraint
is load-bearing and stays.

Grok's documented PreToolUse deny is `{"decision":"deny","reason":"..."}`.
Allow is `{"decision":"allow"}`. Rewrite is
`hookSpecificOutput.updatedInput`.

Emit both shapes in one object. Each harness reads the fields it knows
and ignores the rest:

```json
{
  "decision": "deny",
  "reason": "...",
  "hookSpecificOutput": {
    "hookEventName": "PreToolUse",
    "permissionDecision": "deny",
    "permissionDecisionReason": "..."
  }
}
```

That is a change to the three `jq -n` emit sites, not a second script.
This object has not been watched going through Grok's runner — plugin
hooks never ran — but it is the union of what both docs describe. The
first live Grok registration should confirm Grok honours `decision`
when `hookSpecificOutput` is also present, and that Claude still
honours `permissionDecision` when `decision`/`reason` sit alongside it.

`suggest-scout.sh` only ever emits `additionalContext`. It must keep
doing that and nothing else (invariant 1: no `permissionDecision` of
any kind). Whether Grok applies PreToolUse `additionalContext` at all
is unverified; Grok's docs mention `decision` and `updatedInput` for
that event, not advisory context. A silent no-op there is acceptable —
the nudge is the hook that can be dropped without breaking a deny
invariant.

### 3.3 Finding the binary

The lookup today is `$CLAUDE_PLUGIN_ROOT/bin/scout`, then a Claude data
dir nothing populates, then `PATH`. A user-hook or a future Grok plugin
hook will not have `CLAUDE_PLUGIN_ROOT` set (Grok also does not export
plugin variables into spawned processes; see plugin-packaging.md §2.6).

Add `$0`-relative as the first candidate.
`scripts/session-context.sh` already does this:

```sh
PLUGIN_ROOT="${CLAUDE_PLUGIN_ROOT:-$(cd "$(dirname "$0")/.." && pwd)}"
```

A registration that invokes the payload script by absolute path then
finds `../bin/scout` with no env vars at all. `GROK_PLUGIN_ROOT` /
`GROK_PLUGIN_DATA` are extra aliases in the same block, not a reason to
fork. Keep the three copies of the block byte-identical, as the
existing comment requires.

### 3.4 SessionStart stays Claude-only

`scripts/session-context.sh` is not part of this port. Grok ignores
SessionStart stdout even if the hook someday fires ("Passive Hooks:
stdout is ignored"), and the same guidance already ships as
`plugins/scout/skills/scout/SKILL.md`. Two *channels*, already — not
two versions of the same hook. plugin-packaging.md §3.5 is still the
decision.

---

## 4. The one extra file that is not a script

Because Grok still does not execute `hooks/hooks.json`, a Grok
registration has to live outside the plugin and point at the same
scripts. Either `~/.grok/hooks/scout.json` or `[[hooks.PreToolUse]]`
entries in `~/.grok/config.toml`. Sketch:

```json
{
  "hooks": {
    "PreToolUse": [
      {
        "matcher": "Bash",
        "hooks": [
          {
            "type": "command",
            "command": "/abs/path/plugins/scout/hooks/shell-safety.sh",
            "timeout": 30
          },
          {
            "type": "command",
            "command": "/abs/path/plugins/scout/hooks/prefer-local-llm.sh",
            "timeout": 30
          }
        ]
      },
      {
        "matcher": "Bash|Read",
        "hooks": [
          {
            "type": "command",
            "command": "/abs/path/plugins/scout/hooks/suggest-scout.sh",
            "timeout": 10
          }
        ]
      }
    ]
  }
}
```

`Bash` is enough in the matcher: Grok aliases it to
`run_terminal_command`. Timeouts must be explicit — Grok defaults
observe/pre-tool hooks to **5 seconds**, and the LLM hooks regularly
take that long. Claude's default is more forgiving; leaving timeout
unset in `hooks/hooks.json` is fine there and wrong here.

This file is matchers, absolute paths, and timeouts. It is not a second
implementation. When Grok starts running plugin hooks, the registration
goes away. `hooks/hooks.json` already has the union matcher and stays
the Claude (and future-Grok-plugin) declaration.

A project-local `.grok/hooks/` copy would travel with the repo and
would run only in this checkout, under folder-trust. A user-level
`~/.grok/hooks/` copy runs in every Grok session. Pick based on whether
the steering should follow the developer or the repo. Do not commit a
machine-absolute path into the repo.

---

## 5. How the envelope is kept from rotting

Do not add a `hooks-grok/` tree. Keep the existing shell suites
(`tests/test-prefer-local-llm.sh`, `tests/test-shell-safety.sh`,
`tests/test-suggest-scout.sh`) and feed them a second payload:

```bash
# already
{ "tool_name": "Bash", "tool_input": { "command": "cargo test" } }
# also
{ "toolName": "run_terminal_command", "toolInput": { "command": "cargo test" } }
```

Assert the stdout still has Claude's `permissionDecision` *and* Grok's
`decision`. The lexer, deny floor, and LLM path stay tested once.

A Grok-shaped payload that the current scripts receive today exits 0
with empty stdout: `.tool_name` is empty, the `Bash` guard fails, and
the hook is a silent no-op. That is the regression the second payload
is there to catch.

---

## 6. What this does not settle

- Whether Grok honours a combined `decision` + `permissionDecision`
  object. Plausible, not watched.
- Whether Grok applies PreToolUse `additionalContext` (the suggest
  hook). If it does not, drop the Grok registration for that script
  rather than inventing a second nudge channel.
- Whether a later Grok release starts executing plugin `hooks.json`.
  If it does, delete the user/config registration and keep the
  dual-shape scripts — they become the thing that makes
  `hooks/hooks.json` load-bearing on both sides instead of only
  Claude.
- Live checkout vs snapshot for a user-hook path. Grok copies on
  `plugin install`; a registration that points into
  `~/.grok/installed-plugins/scout-<id>/` goes stale on reinstall.
  Pointing at the checkout payload is right for development and wrong
  as a published install story. Same tension as plugin-packaging.md
  §5, last open item.

---

## 7. Decision log

Closed by measurement (1.0.4) or by the shape of the scripts:

- **Are Grok's plugin hooks recoverable by packaging?** No. Restated
  from plugin-packaging.md §2.5; still true on 1.0.4.
- **Fork the scripts?** No. The middle is shared. Dual-shape the
  parse, the emit, and the binary lookup.
- **A `hooks-grok/` directory?** No. That is two copies of something
  whose only real difference is jq paths.
- **Port SessionStart?** No. Skill already carries the guidance;
  Grok ignores SessionStart stdout.
- **How does a Grok session run the hooks today?** A user/config/
  project registration pointing at the same scripts, with explicit
  timeouts. Not `hooks/hooks.json`.
- **One emit object or a harness branch?** One object, both
  vocabularies. Branching on `GROK_HOOK_EVENT` / `toolName` would
  work and would be the first thing to drift.

Still open: the four items in §6.
