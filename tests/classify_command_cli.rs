// End-to-end tests for `scout classify-command`'s stdout — contract #3.
//
// This subcommand exists for exactly one consumer:
// `plugins/scout/hooks/prefer-local-llm.sh`, which pipes the Bash command in on
// stdin and reads the answer back with:
//
//   jq -r 'if (.intercept | type) == "boolean" then .intercept else empty end'
//   jq -r 'if (.escape    | type) == "boolean" then .escape    else empty end'
//
// So the contract is not just "some JSON": it is two keys named `intercept` and
// `escape`, both JSON booleans, on stdout, with a zero exit. The hook treats
// anything else as a classify failure and fails open — which is safe, but means
// a rename or a type change turns the whole redirect off *silently*. Nothing in
// the conversation says the hook stopped working; build output simply starts
// flooding context again.
//
// The existing unit tests cover `classify()`, the function. They say nothing
// about `run_subcommand()`, the serialization — so renaming a key, or emitting
// the booleans as strings, passes `cargo test` and breaks the hook. That gap is
// what these tests close, and the only way to close it is to run the binary and
// read its actual bytes.
//
// Pure local lexing: no LLM, no network, no config. Fast and deterministic.

mod support;

use std::io::Write;
use std::process::{Output, Stdio};

use serde_json::Value;

use support::Sandbox;

/// Run `scout classify-command` with `command` on stdin, exactly as the hook does.
///
/// stdin rather than argv is itself part of the contract: a Bash command can
/// contain quotes, newlines and heredoc bodies, and the hook pipes it in to
/// sidestep every quoting hazard argv would introduce.
fn classify(sandbox: &Sandbox, command: &str) -> (Output, Value) {
    let mut child = sandbox
        .scout()
        .arg("classify-command")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn scout classify-command");
    child.stdin.take().expect("piped stdin").write_all(command.as_bytes()).expect("write command");
    let out = child.wait_with_output().expect("wait for scout classify-command");

    assert_eq!(
        out.status.code(),
        Some(0),
        "classify-command must exit 0 — a non-zero exit makes the hook fail open.\nstderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = String::from_utf8(out.stdout.clone()).expect("stdout must be UTF-8");
    let value: Value = serde_json::from_str(&text)
        .unwrap_or_else(|e| panic!("stdout is not the JSON object the hook parses: {e}: {text:?}"));
    (out, value)
}

/// Assert the wire shape the hook's two `jq` filters depend on.
fn assert_hook_shape(value: &Value) {
    let obj = value.as_object().unwrap_or_else(|| panic!("not a JSON object: {value}"));
    for key in ["intercept", "escape"] {
        let v = obj
            .get(key)
            .unwrap_or_else(|| panic!("the hook reads `.{key}` and it is absent: {value}"));
        assert!(
            v.is_boolean(),
            "`.{key}` must be a JSON boolean — the hook's `jq` filter drops anything else and \
             fails open: {value}"
        );
    }
    // Extra keys would not break the hook today, but a silently growing payload
    // is how a second consumer starts depending on something undocumented.
    assert_eq!(obj.len(), 2, "unexpected extra fields: {value}");
}

#[test]
fn a_build_command_is_intercepted() {
    let sandbox = Sandbox::new();
    let (out, value) = classify(&sandbox, "cargo test");
    assert_hook_shape(&value);
    assert_eq!(value["intercept"], Value::Bool(true), "cargo test must be intercepted: {value}");
    assert_eq!(value["escape"], Value::Bool(false), "no escape marker present: {value}");
    assert!(
        String::from_utf8_lossy(&out.stdout).ends_with('\n'),
        "the hook reads a line; the payload must be newline-terminated"
    );
}

#[test]
fn an_ordinary_command_is_not_intercepted() {
    let sandbox = Sandbox::new();
    let (_, value) = classify(&sandbox, "echo hi");
    assert_hook_shape(&value);
    assert_eq!(value["intercept"], Value::Bool(false), "echo must run normally: {value}");
    assert_eq!(value["escape"], Value::Bool(false));
}

#[test]
fn the_raw_output_marker_sets_escape() {
    // The escape hatch: same command, one comment appended. The hook reads
    // `escape` separately from `intercept` — the command is still a build
    // command, it is just allowed through — so both fields matter here.
    let sandbox = Sandbox::new();
    let (_, value) = classify(&sandbox, "cargo test # raw-output");
    assert_hook_shape(&value);
    assert_eq!(value["intercept"], Value::Bool(true), "still a build command: {value}");
    assert_eq!(value["escape"], Value::Bool(true), "the marker must be honored: {value}");
}

#[test]
fn empty_stdin_is_still_valid_json() {
    // The hook pipes whatever the payload contained; an empty command must
    // produce a parseable answer rather than an empty stdout, which the hook
    // would read as a classify failure.
    let sandbox = Sandbox::new();
    let (_, value) = classify(&sandbox, "");
    assert_hook_shape(&value);
    assert_eq!(value["intercept"], Value::Bool(false));
    assert_eq!(value["escape"], Value::Bool(false));
}

#[test]
fn command_position_is_what_decides_not_position_in_the_string() {
    // The regression this subcommand was written for. The hook used to use one
    // anchored grep, which blocked commit messages that merely *mentioned*
    // `cargo test` while letting `cd foo && cargo test` run raw — `^` anchors to
    // the start of any line, including a heredoc body. Both directions are
    // pinned here, at the boundary the hook actually reads.
    let sandbox = Sandbox::new();

    for (command, want) in [
        // A verb the shell will really run, not at the head of the string.
        ("cd foo && cargo test", true),
        // Mentioned inside a quoted string.
        (r#"git commit -m "run cargo test""#, false),
        // Mentioned inside a heredoc body.
        ("git commit -F - <<EOF\ncargo test\nEOF\n", false),
        // A verb whose subcommand is not in the table.
        ("cargo add serde", false),
        // Other entries in the table, to catch the list itself shrinking.
        ("npm run build", true),
        ("pytest -q", true),
    ] {
        let (_, value) = classify(&sandbox, command);
        assert_hook_shape(&value);
        assert_eq!(
            value["intercept"],
            Value::Bool(want),
            "intercept({command:?}) should be {want}: {value}"
        );
    }
}

#[test]
fn a_multiline_command_survives_the_stdin_round_trip() {
    // Newlines in argv would be a quoting minefield; on stdin they are just
    // bytes. The hook depends on that, so it is worth proving the binary reads
    // the whole stream rather than the first line.
    let sandbox = Sandbox::new();
    let (_, value) = classify(&sandbox, "echo one\necho two\ncargo build\n");
    assert_hook_shape(&value);
    assert_eq!(value["intercept"], Value::Bool(true), "the third line is a build verb: {value}");
}
