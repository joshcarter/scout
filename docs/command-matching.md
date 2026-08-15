# Command matching in the build/test redirect hook

`hooks/prefer-local-llm.sh` decides whether a Bash command is a
build/test invocation that should be diverted to scout's `check_output`
tool. That decision looks like a job for a regex. It is not, and this
document records why — because the wrong answer shipped first, survived
review, and broke real work in both directions.

---

## 1. The symptom

Committing a fix to this repo was denied by the hook. The commit message
body contained the line:

```
cargo test; both shell suites carry a pre-existing missing-binary failure).
```

The message was passed via a heredoc, so that line became part of the
command string, matched the intercept pattern, and the commit was
refused. Writing the message to a file and using `git commit -F <file>`
worked around it. A diagnostic script written to investigate the bug was
denied for the same reason — it contained the pattern as test data.

This is not a corner case. The commits and scripts most likely to mention
build verbs are precisely the ones concerned with build and test
infrastructure, which in this repository is most of them.

## 2. The test that was wrong

```sh
BUILD_RE='^\s*(cargo\s+(build|test|check|clippy)\b|go\s+(build|test|vet)\b|npx\s+tsc\b|tsc\s+--|npm(\s+run)?\s+(build|test)\b|python\s+-m\s+pytest\b|pytest\b)'
```

with a comment above it claiming:

> Anchored with `^\s*` to avoid matching these verbs inside echo/printf
> strings.

**That claim was false, and its presence is why the defect went
unnoticed.** `grep` is line-oriented. `^` anchors to the start of *any
line within the command string*, not to the start of the command. It
offered no protection whatsoever against verbs appearing inside quoted
strings or heredoc bodies. What it did instead — by accident, not by
design — was fail to match verbs that follow a shell control operator on
the same line, which is the opposite of the desired behavior.

### Measured behavior

| Command | Old regex | Desired |
|---|---|---|
| `cargo test` | match | deny ✓ |
| `cd foo` ⏎ `cargo test` | match | deny ✓ |
| heredoc body containing `cargo test now passes` | **match** | allow ✗ |
| `echo "hello"` ⏎ `cargo build is fast` | **match** | allow ✗ |
| `cd foo && cargo test` | **no match** | deny ✗ |
| `cd foo; cargo test` | **no match** | deny ✗ |

Rows 3–4 are false positives: legitimate work blocked. Rows 5–6 are
false negatives: raw build output floods the conversation context, which
is the exact failure the hook exists to prevent.

Row 2 matters to the design. It is a true positive that must be
preserved — real multi-line scripts do legitimately place build verbs at
the start of a line — so any fix that exempts line-leading verbs
wholesale breaks it.

## 3. Why no regex adjustment works

The two failure directions pull against each other under any anchoring
scheme:

- Loosening the anchor to catch `cd foo && cargo test` makes rows 3–4
  worse, since more positions in string content become matchable.
- Tightening the anchor to exclude heredoc content also excludes row 2,
  a legitimate interception.

No amount of anchoring distinguishes them, because **position within the
string is not the property that separates the cases.** The property that
does is whether the verb appears in **command position** — as the head of
a simple command the shell will actually execute — versus inside data
that merely happens to contain the same characters.

That is a lexical question about shell structure. It needs quote,
heredoc, comment, and command-substitution state, none of which a regex
over the raw string can track.

## 4. The two-stage classifier

### Stage 1 — cheap rejection, in the hook

An unanchored `grep` for the leading word of every intercept verb:

```sh
PREFILTER_RE='(^|[^[:alnum:]_])(cargo|go|tsc|npx|npm|pytest|python)([^[:alnum:]_]|$)'
```

It runs on every Bash tool call, so it stays to a single `grep`. It
rejects the large majority of commands, which mention no build verb at
all — no hit means log and exit with zero further subprocesses.

It **over-matches deliberately**, and has no false negatives by
construction, which is what makes it safe as a pre-filter. The one hard
requirement is that it must not under-match relative to the verb table,
so every leading word in that table appears here. Word boundaries are
spelled with POSIX bracket expressions rather than `\b`, which BSD
`grep -E` does not support.

### Stage 2 — command-position segmentation, in the binary

Reached only on a stage-1 hit: `scout classify-command`, which lexes the
command and reports two booleans.

```
printf '%s' "$COMMAND" | scout classify-command
→ {"intercept":true,"escape":false}
```

The lexer walks the command string tracking single quotes, double quotes,
heredoc bodies, comments, and command substitution. It emits segment
boundaries at `;`, `&&`, `||`, `|`, `&`, and newline — but **only when
every state is inactive**. Heredoc bodies are discarded entirely; they
are data, never command position. Each resulting segment's leading words
are then tested against the verb list.

This resolves every row of §2's table at once. Heredoc and quoted content
never reaches the verb test because it is not in command position.
Chained segments each yield their own head, so `cd foo && cargo test` is
caught. And a genuine multi-line script still segments into real
commands, preserving row 2.

**The command arrives on stdin, never argv.** Commands contain quotes,
newlines and heredoc bodies; stdin sidesteps every quoting hazard.
Output is one JSON object on stdout with exit 0. Any other exit code, or
output the hook cannot parse, means "I don't know" and the hook fails
open with reason `classify-failure`.

### Why stage 2 is Rust and not more bash

The original proposal put the state machine in the hook, in bash, to
preserve standalone installability. It shipped in the binary instead, for
three reasons that were only clear once the test matrix was written:

- The hook already refuses to deny without a working `scout` (see
  §6), so calling the binary adds no new dependency.
- The lexer's test matrix belongs under `cargo test`, where it can be
  exhaustive.
- Version skew — an older installed binary with no `classify-command`
  subcommand — is just another fail-open path, not a new failure class.

The two-stage split is what makes the spawn affordable: it happens only
after a stage-1 hit, not on every Bash call.

`classify-command` is deliberately **not** an MCP tool. It is not a
question the model should be asking, and adding it to the tool surface
would only cost context.

## 5. The escape hatch had the identical flaw

```sh
ESCAPE_RE='#[[:space:]]*raw-output'
```

tested against the raw command string. A commit message or script
containing the text `# raw-output` silently disabled the hook for that
call. The stakes were lower — the failure grants an allow rather than
blocking work — but it is the same class of bug, and it was fixed in the
same pass. The marker now counts only when it appears in a **real shell
comment**, which the same lexer already tracks.

## 6. Fail-open invariants

Two, and they are the reason this hook is allowed to deny at all:

1. **Fail open on the hook's own errors.** Any error or parse failure →
   exit 0 → Bash runs normally.
2. **Fail open when the redirect target is unreachable.** This hook
   emits deny — unlike `shell-safety.sh`, which only ever adds an allow —
   so it carries extra responsibility. A deny into a dead end bricks the
   user's Bash tool for every build and test command. Before emitting
   deny, the hook verifies that the scout binary exists **and** that its
   local-LLM endpoint answers a quick ping. Missing binary or unreachable
   endpoint → log the reason, exit 0, let the raw command run.

Ordering against `shell-safety.sh` does not matter: Claude Code applies
deny > ask > allow, and the two hooks emit opposite decisions.

## 7. Deliberate non-goals

- **Heredoc bodies are pure data**, even with an unquoted delimiter where
  the shell would expand `$(...)` inside them. The symptom that motivated
  this work was a commit message passed by heredoc; treating bodies as
  data is the whole point.
- **Indirection through an interpreter** — `bash -c "cargo test"` — is a
  MISS. The payload is a single data word to the outer shell, and chasing
  it would mean interpreting arbitrary nested languages.

## 8. Test surface

`tests/test-prefer-local-llm.sh` and `src/classify_command/tests.rs`
between them cover all six rows of §2, plus:

- `bash -c "cargo test"` — the interpreter-indirection non-goal, tested
  so the choice stays deliberate rather than emergent.
- `git commit -m "fix cargo build"` — single-line quoted content, which
  passed under the old regex by luck rather than design.
- Escaped quotes inside a heredoc body, confirming the state machine does
  not desynchronize.
- `<<-` indented heredocs, and quoted vs unquoted delimiters (`<<'EOF'`
  vs `<<EOF`).
- Nested and consecutive heredocs in one command.
- The escape marker in command position (honored) versus inside a heredoc
  body (ignored).

## 9. The lesson worth keeping

A comment asserting a protection that does not exist is worse than no
comment: it is what let this defect survive review. When a matcher claims
to exclude a case, the test suite should contain that case — and if it
cannot be written as a test, the claim probably is not true.
