---
name: scout
description: Use when a task would pull a lot of text into context — running a build or test command, reading a large file to answer one question about it, or grepping a pattern that will match far more than you need. Routes the work to a local LLM and returns only the summary.
---

# scout

scout runs token-heavy work against a **local** model and returns a short
result, so the raw output never enters this conversation. Reach for it when the
cheap version of a task would cost thousands of tokens of scrollback.

## When to use which

| Situation | Tool | CLI equivalent |
|---|---|---|
| Running a build or test command | `check_output(command)` → `{ok, summary, first_error, suggested_next_step}` | `scout check "<cmd>"` |
| One specific question about a large file | `extract(file, question)` | `scout extract <file> "<question>"` |
| A pattern that will match far more than you want | `grep(pattern, intent)` | `scout grep <pattern> "<intent>"` |
| Anything else you'd rather not spend context on | — | `scout task "<prompt>"` |

These are MCP tools. The names above are unqualified; the full names carry a
plugin-derived prefix. If a scout tool is not already in your loaded toolset,
look it up by its unqualified name (`check_output`, `extract`, `grep`) to
resolve the full name and load its schema.

## When NOT to use it

Small files and narrow searches are cheaper read directly — a 40-line config,
a symbol you expect three hits for, a directory listing. scout adds a local
model round-trip; below a few hundred lines that is a loss, and its own filters
short-circuit on small inputs anyway.

Do not route every identifier search through it. Native search is the right
tool for "where is this defined."

## Build and test output

This is where it pays most. `check_output` classifies the run and returns the
first real error rather than the whole log:

```
check_output(command="cargo test")
→ {ok: false, summary: "…", first_error: {…}, suggested_next_step: "…"}
```

If `ok` is false **and** `first_error` is null, the classifier could not parse
the output — that is the case for re-running the command directly to see the
raw log.

## Setup

scout needs a local LLM endpoint in `~/.config/scout/config.toml`. The binary
writes a default there on first run; edit `[llm].endpoint` and `[llm].model` to
match your host. If a scout call fails with a config or connection error, that
file is the thing to check.
