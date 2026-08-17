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
| Running any other command whose output is too long to read | `wrap(command, question?)` → `{exit_code, summary, answer, notable, lines_dropped, raw_path}` | `scout wrap "<cmd>" ["<question>"]` |
| A job that will run for minutes and then finish | `wrap(command, detach: true)` then `wait(until: "all")` → `{done: [wrap payloads], pending, timed_out}` | — |
| One specific question about a large file | `extract(file, question)` | `scout extract <file> "<question>"` |
| A pattern that will match far more than you want | `grep(pattern, intent)` | `scout grep <pattern> "<intent>"` |
| Anything else you'd rather not spend context on | — | `scout task "<prompt>"` |

`check_output` and `wrap` split by what the output is for. `check_output`
renders a **verdict** on a build or test run — did it pass, what broke first,
what to do next — and a hook redirects bare build/test commands to it anyway.
`wrap` does **retrieval** from everything else verbose: `git log`, a `git diff`
over a big change, `docker logs`, `journalctl`, `curl`, a `find` across a large
tree, a long script. It renders no verdict and passes the exit code through
uninterpreted, because a `diff` that exits 1 has not failed — it found
differences. An optional `question` steers the filter; without one you get
faithful generic condensation.

These are MCP tools. The names above are unqualified; the full names carry a
plugin-derived prefix. If a scout tool is not already in your loaded toolset,
look it up by its unqualified name (`check_output`, `wrap`, `wait`, `jobs`,
`cancel`, `extract`, `grep`) to resolve the full name and load its schema.

## Long jobs that finish

A command that will run for minutes and then **end** — a notebook, a fat test
matrix, a long script — is `wrap(command, detach: true)`, then
`wait(until: "all")`. That is one turn per *completion* (or per batch), not
one turn per interval.

- Launch the batch in one turn (up to 16 live jobs), then **one**
  `wait(until: "all")`. `until: "any"` is for fail-fast, not for a
  homogeneous sweep.
- Do not `sleep`. Do not `until [ -s file ]`. Do not `pgrep` loops. Those
  cost a full conversation-sized turn to learn "not done yet."
- `{timed_out: true}` is bookkeeping, not an error. Call `wait` again.
- Stopping a wait leaves the jobs running. `cancel(job_id)` kills one group.
- If you launched with the harness's own background command, use the
  harness wait. If you launched with `wrap(detach)`, drain with scout
  `wait` — that is where the wrap verdict is.
- Unbounded streams (dev servers, `--watch` runners) are not this tool.

## What filtering costs you, and how to get it back

`wrap` never leaves you stranded behind a summary:

- **Short output is not filtered at all.** At or under ~200 lines the command's
  output comes back verbatim, with `filtered: false` and no model call. Guessing
  wrong about whether something will be long costs only the exec, so use `wrap`
  speculatively — that is what it is for.
- **Filtered results say what they dropped.** The payload carries
  `lines_total`, `lines_dropped`, `bytes_total`, and `raw_path`.
- **`raw_path` is the complete raw output**, a plain file kept for about a week.
  If the summary does not answer your question, `Read` it with `offset`/`limit`,
  or call `extract(raw_path, question)` on it.

Escalate that way rather than re-running the command. Wrapped commands are
often slow, and some are not safe to repeat at all; the raw file is one `Read`
away and always faithful.

## What scout is actually best at

`extract` and `grep` earn their round-trip on text that no code index covers,
and on match sets too large to skim:

- **Logs and run output** — CI logs, crash dumps, `.jsonl` event streams,
  profiler output. Nothing indexes these, they are enormous, and you almost
  always want one answer rather than the file.
- **Generated and vendored trees** — lockfiles, `node_modules`, `target/`,
  protobuf/OpenAPI output, migrations. Real text, deliberately unindexed,
  frequently huge.
- **Long prose and config** — changelogs, specs, ADRs, large YAML/TOML. The
  question ("when did the retry default change?") is semantic, so a pattern
  match is the wrong shape for it.
- **Semantic filtering of a big match set** — when the pattern is right but
  most hits are not, `intent` throws away the noise locally. This is the one
  case that applies to indexed source too.

`wrap` covers the same shape of problem one step earlier, where the text does
not exist yet because the command has not run:

- **Logs from a running system** — `docker logs`, `journalctl`, `kubectl logs`,
  tailing a service's output. Thousands of lines, one question.
- **Git history and large diffs** — `git log` over a release range, `git diff`
  on a sweeping refactor, `git blame` on a long file. Real answers ("which
  commit changed the retry default?") buried in bulk.
- **HTTP and API responses** — `curl` against an endpoint that returns a wall
  of JSON or HTML, when you want two fields out of it.
- **Long scripts and wide traversals** — deploy and migration scripts,
  `find`/`du`/`ls -R` across a big tree, anything chatty by design.

## When NOT to use it

Small files and narrow searches are cheaper read directly — a 40-line config,
a symbol you expect three hits for, a directory listing. scout adds a local
model round-trip; below a few hundred lines that is a loss, and its own filters
short-circuit on small inputs anyway. That short-circuit is why `wrap` is the
exception: you rarely know how much a command will print before running it, and
a short result comes back whole regardless, so guessing "verbose" is free.

Do not route every identifier search through it. Native search is the right
tool for "where is this defined."

**If a structural code-intelligence tool is available** — an LSP, a
tree-sitter-backed indexer, anything that resolves symbols — prefer it for
indexed source. Outlines, call graphs, "find every caller", and "show me this
function" are questions with exact answers, served from an index, with no model
in the loop. scout answers a different kind of question, on text that tool
cannot see. Reaching for scout there is slower and less accurate; the list
above is the boundary.

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
