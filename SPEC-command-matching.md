# SPEC: command matching in `prefer-local-llm.sh`

Status: proposed
Scope: `hooks/prefer-local-llm.sh`, `tests/test-prefer-local-llm.sh`

The redirect hook decides whether a Bash command is a build/test invocation
that should be diverted to scout's `check_output` tool. It currently makes that
decision with a single anchored regex over the whole command string. That is the
wrong shape of test for the question being asked, and it misfires in both
directions: it blocks legitimate commands, and it lets real build commands
through.

## 1. Symptom

Committing this repo's own tool-name fix was denied by the hook. The commit
message body contained the line:

```
cargo test; both shell suites carry a pre-existing missing-binary failure).
```

The message was passed via a heredoc, so that line became part of the command
string, matched the intercept pattern, and the commit was refused. Writing the
message to a file and using `git commit -F <file>` worked around it.

A diagnostic script written to investigate the bug was denied for the same
reason — it contained the pattern as test data.

This is not a corner case. The commits and scripts most likely to mention build
verbs are precisely those concerned with build and test infrastructure, which in
this repository is most of them.

## 2. The current test

`hooks/prefer-local-llm.sh:83`:

```sh
BUILD_RE='^\s*(cargo\s+(build|test|check|clippy)\b|go\s+(build|test|vet)\b|npx\s+tsc\b|tsc\s+--|npm(\s+run)?\s+(build|test)\b|python\s+-m\s+pytest\b|pytest\b)'
```

Applied at line 118:

```sh
if ! printf '%s' "$COMMAND" | grep -qE "$BUILD_RE" 2>/dev/null; then
```

The comment above it (line 80) states:

> Anchored with `^\s*` to avoid matching these verbs inside echo/printf strings.

**This claim is false**, and its presence is why the defect went unnoticed.
`grep` is line-oriented. `^` anchors to the start of *any line within the command
string*, not to the start of the command. It therefore offers no protection
whatsoever against verbs appearing inside quoted strings or heredoc bodies. What
it does instead — as an accident rather than a design — is fail to match verbs
that follow a shell control operator on the same line, which is the opposite of
the desired behavior.

## 3. Observed behavior

Measured directly against `BUILD_RE`:

| Command | Current | Desired |
|---|---|---|
| `cargo test` | match | deny ✓ |
| `cd foo` ⏎ `cargo test` | match | deny ✓ |
| heredoc body containing `cargo test now passes` | **match** | allow ✗ |
| `echo "hello"` ⏎ `cargo build is fast` | **match** | allow ✗ |
| `cd foo && cargo test` | **no match** | deny ✗ |
| `cd foo; cargo test` | **no match** | deny ✗ |

Rows 3–4 are false positives: legitimate work is blocked. Rows 5–6 are false
negatives: raw build output floods the conversation context, which is the exact
failure the hook exists to prevent.

Row 2 is important to the design. It is a true positive that must be preserved:
real multi-line scripts do legitimately place build verbs at the start of a
line. Any fix that exempts line-leading verbs wholesale will break it.

## 4. Why this is not a regex adjustment

The two failure directions pull against each other under any anchoring scheme:

- Loosening the anchor to catch `cd foo && cargo test` makes rows 3–4 worse,
  since more positions in string content become matchable.
- Tightening the anchor to exclude heredoc content also excludes row 2, a
  legitimate interception.

No amount of anchoring distinguishes them, because position within the string is
not the property that separates the cases. The property that separates them is
whether the verb appears **in command position** — as the head of a simple
command the shell will actually execute — versus inside data that merely happens
to contain the same characters.

That is a lexical question about shell structure. It requires tracking quote and
heredoc state, which a regex over the raw string cannot do.

## 5. Proposed solution

Replace the single-regex test with a two-stage classifier.

### Stage 1 — cheap rejection

An unanchored substring test for any intercept verb anywhere in the command
string.

- Runs on every Bash tool call, so it must stay to a single `grep`.
- Rejects the large majority of commands, which mention no build verb at all.
- Deliberately over-matches. It has **no false negatives by construction**,
  which is what makes it safe as a pre-filter.

### Stage 2 — command-position segmentation

Reached only on a stage-1 hit.

1. Walk the command string, tracking three pieces of state: inside single
   quotes, inside double quotes, inside a heredoc body.
2. Emit segment boundaries at `;`, `&&`, `||`, `|`, `&`, and newline — but only
   when all three states are inactive.
3. Discard heredoc bodies entirely; they are data, never command position.
4. For each resulting segment, take its leading words and test them against the
   verb list.

This resolves every row of the table simultaneously. Heredoc and quoted content
never reaches the verb test because it is not in command position. Chained
segments each yield their own head, so `cd foo && cargo test` is caught. And a
genuine multi-line script still segments into real commands, preserving row 2.

### Implementation location

Stage 2 belongs in the hook itself, in bash — roughly fifty lines of state
tracking — rather than as a new `scout` subcommand.

The hook is deliberately standalone-installable. That property is the subject of
commit `d73fef4` and the reason for the binary-resolution fallbacks at lines
63–64. Keeping classification free of subprocess dependencies preserves it, and
avoids adding a process spawn to a path that already runs on every Bash call.

If the state machine proves unwieldy in bash, moving stage 2 into the Rust binary
is a clean fallback: the hook already requires a working `scout` before it is
permitted to deny, so this would add no new dependency, and the logic would gain
`cargo test` coverage. The two-stage split is what makes that affordable — the
spawn would occur only after a stage-1 hit, not on every command.

## 6. Related defect: the escape hatch

`ESCAPE_RE` (line 88) is checked against the raw command string at line 125:

```sh
ESCAPE_RE='#[[:space:]]*raw-output'
```

This has the identical flaw. A commit message or script containing the text
`# raw-output` silently disables the hook for that call. Stakes are lower, since
the failure grants an allow rather than blocking work, but it is the same class
of bug and should be fixed in the same pass by testing the escape marker against
segmented text rather than the raw string.

## 7. Test surface

Regression cases, all six rows of §3 plus:

- `bash -c "cargo test"` — indirection through an interpreter. Behavior should
  be chosen deliberately and documented, not left to fall out of the
  implementation.
- `git commit -m "fix cargo build"` — single-line quoted content. Currently
  passes, but by luck rather than by design; it needs a test to stay passing.
- Escaped quotes inside a heredoc body, to confirm the state machine does not
  desynchronize.
- `<<-` indented heredocs and quoted delimiters (`<<'EOF'` versus `<<EOF`).
- Nested and consecutive heredocs in one command.
- Escape-hatch cases from §6: `# raw-output` in command position (honored)
  versus inside a heredoc body (ignored).

## 8. Also in scope

The comment at line 80 must be corrected regardless of which implementation is
chosen. It asserts a protection that does not exist, and it is the reason this
defect persisted through review.

## 9. Effort

One to two hours, the bulk of it in the state machine and its test cases. The
change is self-contained: one hook, one test file, no interface changes and no
effect on the MCP server or the CLI.
