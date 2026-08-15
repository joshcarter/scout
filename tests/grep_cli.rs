// End-to-end tests for `scout grep`'s exit codes — contract #2.
//
// `docs/search-cli.md` §1 specifies the grep convention:
//
//   0 — at least one hit returned
//   1 — no hits, *including* `none_relevant`; the stderr message distinguishes
//       the two
//   2 — error (bad pattern, LLM failure with no bypass)
//
// This is the contract every script wrapping `scout grep` depends on, and it is
// unreachable from a unit test by construction: the dispatcher ends in
// `std::process::exit` in about a dozen places, which aborts the test harness
// rather than returning a value anyone can assert on. Only a subprocess sees it.
//
// Every case here is served without the local model. An absent `--intent` is an
// implicit `--no-filter` (docs/search-cli.md §3): scout runs its own search
// engine and returns `mode: "full"` with no LLM call at all, so these tests need
// nothing installed and are deterministic on any machine. The `none_relevant`
// path — the other route to exit 1 — does need a model and is marked `#[ignore]`
// below rather than left to flake.

mod support;

use std::process::Output;

use support::Sandbox;

/// A project with hits that are easy to reason about, and one file the search
/// must not wander into by accident.
fn fixture() -> Sandbox {
    let sandbox = Sandbox::new();
    sandbox.write(
        "src/alpha.rs",
        "fn needle_one() {}\nfn unrelated() {}\nfn needle_two() {}\n",
    );
    sandbox.write("src/beta.rs", "// nothing of interest here\n");
    sandbox
}

fn code(out: &Output) -> i32 {
    out.status.code().expect("scout was killed by a signal instead of exiting")
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[test]
fn a_match_exits_zero_and_prints_the_hit_to_stdout() {
    let sandbox = fixture();
    let out = sandbox.scout().args(["grep", "needle_one"]).output().expect("run scout grep");

    assert_eq!(code(&out), 0, "a hit must exit 0\nstderr: {}", stderr(&out));
    assert!(stdout(&out).contains("alpha.rs"), "hit not on stdout: {:?}", stdout(&out));
    // Results on stdout, metadata on stderr — always, so a script never has to
    // strip status chatter out of what it is parsing (docs/search-cli.md §2).
    assert!(!stderr(&out).contains("needle_one() {}"), "result text leaked to stderr");
}

#[test]
fn no_match_exits_one_with_an_empty_stdout() {
    let sandbox = fixture();
    let out =
        sandbox.scout().args(["grep", "zzz_definitely_absent"]).output().expect("run scout grep");

    assert_eq!(code(&out), 1, "no hits must exit 1\nstderr: {}", stderr(&out));
    assert!(stdout(&out).trim().is_empty(), "stdout should be empty: {:?}", stdout(&out));
    // The doc requires the stderr message to distinguish "found nothing" from
    // "the model rejected everything"; this is the former.
    assert!(
        stderr(&out).contains("no matches"),
        "stderr does not say there were no matches: {:?}",
        stderr(&out)
    );
}

#[test]
fn a_bad_regex_exits_two() {
    let sandbox = fixture();
    // Unclosed character class: rejected by the match engine, not by clap, so
    // this is the doc's "bad pattern" case rather than a usage error.
    let out = sandbox.scout().args(["grep", "--regex", "["]).output().expect("run scout grep");

    assert_eq!(code(&out), 2, "a bad pattern must exit 2\nstderr: {}", stderr(&out));
    assert!(!stderr(&out).is_empty(), "an error exit with nothing on stderr says nothing");
}

#[test]
fn a_usage_error_exits_two() {
    let sandbox = fixture();
    // No pattern and no `--type-list`: clap's `required_unless_present` fires.
    // clap's own default for a usage error is 2, which happens to agree with
    // the doc's "2 = error" — pinned here so a future `exit_code` override or a
    // hand-rolled arg check cannot quietly move it to 1 and collide with the
    // "no hits" signal.
    let out = sandbox.scout().args(["grep"]).output().expect("run scout grep");

    assert_eq!(code(&out), 2, "a usage error must exit 2\nstderr: {}", stderr(&out));
    assert!(stdout(&out).trim().is_empty(), "usage errors belong on stderr");
}

#[test]
fn conflicting_flags_exit_two() {
    let sandbox = fixture();
    // `--no-filter` with an `--intent` is a contradiction clap rejects, rather
    // than a silent no-op (docs/search-cli.md §3). It must not be mistaken for
    // "no hits".
    let out = sandbox
        .scout()
        .args(["grep", "needle_one", "--intent", "anything", "--no-filter"])
        .output()
        .expect("run scout grep");

    assert_eq!(code(&out), 2, "conflicting flags must exit 2\nstderr: {}", stderr(&out));
}

#[test]
fn type_list_is_informational_and_exits_zero() {
    let sandbox = fixture();
    // `--type-list` answers "what can I pass to -t?", so it has to work before
    // the caller has a pattern — i.e. it must win over the pattern requirement
    // instead of tripping the usage error above.
    let out = sandbox.scout().args(["grep", "--type-list"]).output().expect("run scout grep");

    assert_eq!(code(&out), 0, "--type-list must exit 0\nstderr: {}", stderr(&out));
    assert!(stdout(&out).contains("rust:"), "no type list on stdout: {:?}", stdout(&out));
}

#[test]
fn the_exit_code_is_the_same_in_every_output_format() {
    // The exit code reports the search result, not the renderer. A script that
    // switched to `--format json` for parseability must not also have to change
    // how it reads `$?`.
    let sandbox = fixture();
    for format in ["human", "json", "vimgrep"] {
        let hit = sandbox
            .scout()
            .args(["grep", "needle_one", "--format", format])
            .output()
            .expect("run scout grep");
        assert_eq!(code(&hit), 0, "--format {format} with a hit\nstderr: {}", stderr(&hit));

        let miss = sandbox
            .scout()
            .args(["grep", "zzz_definitely_absent", "--format", format])
            .output()
            .expect("run scout grep");
        assert_eq!(code(&miss), 1, "--format {format} with no hits\nstderr: {}", stderr(&miss));
    }
}

#[test]
fn json_format_emits_the_payload_on_stdout_and_status_on_stderr() {
    let sandbox = fixture();
    let out = sandbox
        .scout()
        .args(["grep", "needle", "--format", "json"])
        .output()
        .expect("run scout grep");

    assert_eq!(code(&out), 0, "stderr: {}", stderr(&out));
    let payload: serde_json::Value =
        serde_json::from_str(&stdout(&out)).expect("--format json must put parseable JSON on stdout");
    // Bypassed mode: no intent, so the model was never asked and every hit came
    // back. This is the shape docs/search-cli.md §3 promises for an absent intent.
    assert_eq!(payload["mode"], "full", "expected the bypassed mode: {payload}");
    assert_eq!(payload["intent"], serde_json::Value::Null, "intent should be null: {payload}");
    assert_eq!(payload["hits"].as_array().map(Vec::len), Some(2), "payload: {payload}");
}

#[test]
#[ignore = "needs a reachable local LLM: none_relevant is the model rejecting every hit"]
fn none_relevant_also_exits_one() {
    // The second route to exit 1, and the reason the doc insists the stderr
    // message distinguishes the two. It cannot be reached without a model
    // (an absent intent bypasses the rerank entirely), so it is not in the
    // default run. To exercise it, point config.toml at a live endpoint and
    // run with `--ignored`.
    let sandbox = fixture();
    let out = sandbox
        .scout()
        .args(["grep", "needle", "--intent", "code that talks to a payment gateway"])
        .output()
        .expect("run scout grep");

    assert_eq!(code(&out), 1);
    assert!(stderr(&out).contains("none of"), "stderr: {}", stderr(&out));
}
