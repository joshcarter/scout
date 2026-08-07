//! Tests for the command-position classifier.
//!
//! The table in SPEC-command-matching.md §3 is the spine of this file: every
//! row appears below, plus the lexer edge cases (§7) that the old raw-string
//! regex could not express.

use super::*;

fn hit(cmd: &str) {
    assert!(
        classify(cmd).intercept,
        "expected intercept for {cmd:?}, got none"
    );
}

fn miss(cmd: &str) {
    assert!(
        !classify(cmd).intercept,
        "expected no intercept for {cmd:?}, got one"
    );
}

// ── SPEC §3: the six observed rows ────────────────────────────────────────────

#[test]
fn row1_plain_build_command_intercepts() {
    hit("cargo test");
}

#[test]
fn row2_line_leading_verb_in_a_multi_line_script_intercepts() {
    // A true positive that must survive: real scripts put build verbs at the
    // start of a line.
    hit("cd foo\ncargo test");
}

#[test]
fn row3_heredoc_body_mentioning_a_verb_does_not_intercept() {
    // The original symptom: a commit message passed by heredoc.
    miss("git commit -F - <<EOF\nfix: tool names\n\ncargo test now passes.\nEOF");
}

#[test]
fn row4_verb_inside_a_multi_line_quoted_string_does_not_intercept() {
    // §3 row 4. Read as quoted data — the only reading under which the desired
    // verdict (allow) is achievable, since a bare second line really is a
    // command and would have to behave like row 2.
    miss("echo \"hello\ncargo build is fast\"");
}

#[test]
fn row4_variant_bare_second_line_is_a_real_command() {
    // The other reading of the same row, stated explicitly so the choice is
    // visible: an unquoted line-leading verb IS command position, identically
    // to row 2, and intercepts.
    hit("echo \"hello\"\ncargo build is fast");
}

#[test]
fn row5_and_operator_chain_intercepts() {
    hit("cd foo && cargo test");
}

#[test]
fn row6_semicolon_chain_intercepts() {
    hit("cd foo; cargo test");
}

// ── Quoted data must never reach the verb table ───────────────────────────────

#[test]
fn quoted_commit_message_does_not_intercept() {
    miss("git commit -m \"fix cargo build\"");
    miss("git commit -m 'fix cargo build'");
}

#[test]
fn echo_of_a_verb_does_not_intercept() {
    miss("echo cargo test");
    miss("echo 'cargo test'");
}

#[test]
fn assignment_of_a_verb_string_does_not_intercept() {
    miss("MSG=\"run cargo test first\"");
}

#[test]
fn bash_dash_c_payload_is_a_miss_by_design() {
    // Decided: indirection through an interpreter is NOT chased. The payload
    // is a single data word to the outer shell, and following it would mean
    // interpreting arbitrary nested languages.
    miss("bash -c \"cargo test\"");
    miss("sh -c 'cargo build'");
}

// ── Chaining, grouping, wrappers ──────────────────────────────────────────────

#[test]
fn pipeline_and_or_chains_intercept() {
    hit("cargo test | tail -5");
    hit("make || cargo build");
    hit("cargo build &");
}

#[test]
fn subshell_and_group_intercept() {
    hit("(cd foo && cargo test)");
    hit("{ cargo test; }");
    hit("! cargo test");
}

#[test]
fn env_prefix_assignments_intercept() {
    hit("RUST_BACKTRACE=1 cargo test");
    hit("RUST_LOG=debug CARGO_TERM_COLOR=never cargo test");
}

#[test]
fn transparent_wrappers_intercept() {
    hit("timeout 60 cargo test");
    hit("env RUST_LOG=debug cargo test");
    hit("env -i cargo test");
    hit("time cargo test");
    hit("nice -n 10 cargo test");
}

#[test]
fn leading_redirections_intercept() {
    hit(">out.log cargo test");
    hit("2>/dev/null cargo build");
    hit("cargo test >out.log 2>&1");
}

#[test]
fn redirect_ampersand_is_not_a_separator() {
    // `2>&1` must not split into a segment whose head is `1`.
    miss("echo hi 2>&1");
    hit("cargo test 2>&1");
}

// ── Command substitution executes ─────────────────────────────────────────────

#[test]
fn command_substitution_is_command_position() {
    hit("echo \"$(cargo test)\"");
    hit("echo \"$(cargo test 2>&1 | tail -1)\"");
    hit("X=$(cargo build)");
}

#[test]
fn backticks_are_command_position() {
    hit("echo \"`cargo test`\"");
    hit("echo `cargo test 2>&1`");
}

#[test]
fn substitution_inside_single_quotes_is_data() {
    miss("echo '$(cargo test)'");
}

#[test]
fn text_after_a_substitution_stays_quoted() {
    // The `)` returns to the enclosing double-quoted word; the trailing text
    // must not be re-read as a fresh command.
    miss("echo \"$(date) cargo test\"");
}

// ── Heredocs ──────────────────────────────────────────────────────────────────

#[test]
fn quoted_and_unquoted_heredoc_delimiters_both_hide_the_body() {
    miss("cat <<EOF\ncargo test\nEOF");
    miss("cat <<'EOF'\ncargo test\nEOF");
    miss("cat <<\"EOF\"\ncargo test\nEOF");
}

#[test]
fn dash_heredoc_accepts_a_tab_indented_delimiter() {
    miss("cat <<-EOF\n\tcargo test\n\tEOF");
    // …and the body ends there, so a following real command still counts.
    hit("cat <<-EOF\n\tsome text\n\tEOF\ncargo build");
}

#[test]
fn two_consecutive_heredocs_queue_in_order() {
    let cmd = "cat <<A <<B\ncargo test\nA\ncargo build\nB";
    miss(cmd);
    // Only after both bodies are consumed does lexing resume.
    hit("cat <<A <<B\nnoise\nA\nnoise\nB\ncargo test");
}

#[test]
fn heredoc_body_cannot_desync_the_lexer() {
    // An unbalanced quote in the body would swallow everything after it if the
    // body were lexed; the trailing command proves it is not.
    hit("git commit -F - <<EOF\nit's a \"quoted cargo test\nEOF\ncargo test");
    miss("git commit -F - <<EOF\nit's a \"quoted cargo test\nEOF\ngit push");
}

#[test]
fn here_string_is_not_a_heredoc() {
    // `<<<` is a word, not a body-consuming redirect: the chained command
    // after it must still be seen.
    hit("grep foo <<<\"$DATA\" && cargo test");
}

#[test]
fn here_string_followed_by_a_newline_does_not_swallow_the_next_command() {
    // Regression: the second `<` of `<<<` was misread as a heredoc opener, and
    // the phantom body ate everything after the newline. Only a newline
    // exposes it — pending heredocs drain at newlines, so the single-line
    // test above passed even while this one failed.
    hit("cat <<< \"note\"\ncargo test");
    hit("cat <<<word\ncargo test");
    miss("cat <<< \"cargo test\"");
    // A here-string is also strippable as a leading redirect.
    hit("<<<data cargo test");
}

#[test]
fn unterminated_heredoc_swallows_the_rest() {
    miss("cat <<EOF\ncargo test\n");
}

// ── Comments ──────────────────────────────────────────────────────────────────

#[test]
fn a_commented_out_verb_does_not_intercept() {
    miss("# cargo test");
    miss("ls -la # cargo test");
}

#[test]
fn hash_mid_word_is_not_a_comment() {
    miss("echo a#b");
    hit("cargo test --features a#b");
}

// ── Escape hatch ──────────────────────────────────────────────────────────────

#[test]
fn marker_in_a_real_comment_escapes() {
    let v = classify("cargo test # raw-output");
    assert!(v.intercept, "verb still recognized");
    assert!(v.escape, "marker honored");
}

#[test]
fn marker_forms_accepted() {
    assert!(classify("cargo test #raw-output").escape);
    assert!(classify("cargo test ## raw-output").escape);
    assert!(classify("cargo test\t#  raw-output please").escape);
}

#[test]
fn marker_in_a_quoted_string_does_not_escape() {
    assert!(!classify("echo '# raw-output'").escape);
    assert!(!classify("echo \"# raw-output\"").escape);
    assert!(!classify("git commit -m \"see # raw-output\"").escape);
}

#[test]
fn marker_in_a_heredoc_body_does_not_escape() {
    assert!(!classify("git commit -F - <<EOF\n# raw-output\nEOF").escape);
}

#[test]
fn unrelated_comment_does_not_escape() {
    assert!(!classify("cargo test # run the suite").escape);
}

// ── Verb table: the intercepted set ───────────────────────────────────────────

#[test]
fn every_intercepted_verb() {
    for cmd in [
        "cargo build",
        "cargo build --release",
        "cargo test",
        "cargo test --quiet",
        "cargo check",
        "cargo clippy",
        "go build ./...",
        "go test ./...",
        "go vet ./...",
        "npx tsc",
        "npx tsc --noEmit",
        "tsc --noEmit",
        "tsc --watch",
        "npm test",
        "npm run test",
        "npm build",
        "npm run build",
        "python -m pytest",
        "python -m pytest tests/",
        "pytest",
        "pytest -k foo",
        "pytest -v tests/",
    ] {
        hit(cmd);
    }
}

// ── Verb table: verb-level anchoring is preserved ─────────────────────────────

#[test]
fn neighbouring_subcommands_are_not_intercepted() {
    for cmd in [
        "cargo add serde",
        "cargo fmt",
        "cargo clean",
        "cargo run",
        "go fmt ./...",
        "go mod tidy",
        "go run .",
        "npm install",
        "npm ci",
        "npm run lint",
        "npx prettier",
        "python -m venv .venv",
        "python script.py",
        "tsc",
        "ls -la",
        "git status",
        "",
        "   ",
    ] {
        miss(cmd);
    }
}

// ── Head-normalization helpers ────────────────────────────────────────────────

#[test]
fn assignment_recognition() {
    assert!(is_assignment("FOO=bar"));
    assert!(is_assignment("_x="));
    assert!(is_assignment("arr+=(1)"));
    assert!(!is_assignment("=bar"));
    assert!(!is_assignment("2=x"));
    assert!(!is_assignment("cargo"));
    assert!(!is_assignment("--flag=1"));
}

#[test]
fn redirect_recognition() {
    assert_eq!(redirect_span("2>&1"), Some(1));
    assert_eq!(redirect_span("</dev/null"), Some(1));
    assert_eq!(redirect_span(">"), Some(2));
    assert_eq!(redirect_span("2>"), Some(2));
    assert_eq!(redirect_span("&>log"), Some(1));
    assert_eq!(redirect_span("cargo"), None);
}

#[test]
fn head_word_extraction() {
    let words = |cmd: &str| -> Vec<String> {
        let (segments, _) = Lexer::new(cmd).run();
        segments.into_iter().next().unwrap_or_default()
    };
    assert_eq!(head_words(&words("A=1 timeout 30s cargo test")), ["cargo", "test"]);
    assert_eq!(head_words(&words("( cargo test")), ["cargo", "test"]);
    assert_eq!(head_words(&words("2>&1 >log cargo build")), ["cargo", "build"]);
}
