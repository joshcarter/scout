//! `check_output` — run a build/test command and classify its output locally.
//!
//! The caller supplies only a command string.  scout runs it here, capturing
//! stdout+stderr, and forwards `{command, output, language}` to the
//! `check_output` preset.  The raw output never enters the caller's context;
//! only the compact JSON verdict the model returns does.
//!
//! The shape is the obvious one: pluck `command`, resolve `cwd` against the
//! project root, clamp the timeout, capture with `verify::run_command_capture`,
//! inject the output into the preset args, call the model.
//!
//! Written from scratch rather than adapted from any existing file, and
//! deliberately so — this module has no inherited provenance to trace.
//!
//! Invariants:
//!
//! * The command runs exactly once, with its output capped as it arrives —
//!   never buffered whole and truncated afterwards.
//! * The verdict is the model's, but `exit_ok` reports the real exit status
//!   alongside it so a mis-classification is always detectable.
//! * A command killed for exceeding a deadline never reaches the model.  There
//!   is nothing to classify, and the round-trip used to be paid anyway — the
//!   model was handed the sentence "sh: command timed out after 60s" and its
//!   summary of it recorded as an ordinary success.
//! * Every failure path returns a `ToolError` naming the raw fallback — a
//!   broken classifier must never stop the caller from running the command.

use serde_json::Value;
use std::path::PathBuf;
use std::time::Duration;

use crate::select::{call_preset, non_empty_arg, parse_selector_json, Ctx, ToolError, ToolResult};
use crate::verify;

/// Raw tool to name whenever this filter cannot deliver.
const FALLBACK: &str = "running the command yourself with the Bash tool";

/// Default wall-clock deadline for the command.
///
/// Deliberately far above `verify::IDLE_TIMEOUT`, and that ordering is the
/// whole point: the wall clock is a circuit breaker, not the thing that
/// decides whether a command is stuck.  A build that keeps printing is
/// working, however long it takes, and it runs to completion; one that goes
/// quiet for `IDLE_TIMEOUT` is wedged and dies in two minutes regardless of
/// how much of this budget is left.
///
/// It was 60s, below the idle deadline, which made the idle check
/// unreachable — the wall clock always fired first and every long build was
/// killed for being long rather than for being stuck.  If you lower this
/// under `IDLE_TIMEOUT` you turn the liveness check back off.
const DEFAULT_TIMEOUT_SECS: u64 = 900;

/// Hard cap on the timeout — an unbounded wait would freeze the MCP loop.
const MAX_TIMEOUT_SECS: u64 = 3600;

/// Run `command` and return the local model's classification of its output.
pub fn run(ctx: &Ctx, args: &Value) -> ToolResult {
    let command = non_empty_arg(args, "command")
        .ok_or_else(|| fail("'command' argument is required and must be non-empty"))?;

    let cwd: PathBuf = args
        .get("cwd")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .map_or_else(|| PathBuf::from(&ctx.project), PathBuf::from);
    if !cwd.is_dir() {
        return Err(fail(&format!("cwd {} is not a directory", cwd.display())));
    }

    let timeout_secs = args
        .get("timeout_seconds")
        .and_then(Value::as_u64)
        .unwrap_or(DEFAULT_TIMEOUT_SECS)
        .clamp(1, MAX_TIMEOUT_SECS);

    let capture = verify::run_command_capture(
        &command,
        &cwd,
        Duration::from_secs(timeout_secs),
        verify::MAX_OUTPUT_BYTES,
    );

    // The captured output is the whole point of this tool: it is what would
    // otherwise have landed in the caller's context (docs/dashboard.md §3).
    ctx.ledger.raw_bytes(capture.output.len() as u64);

    // `language` is injected into the preset args too so a user-authored
    // override can key its prompt on the toolchain (`${args.language}`); the
    // built-in preset ignores it.
    let language = verify::detect_language(&cwd).map(super::verify::Language::as_str);

    // A killed command short-circuits: no model, no round-trip, and a row that
    // says what actually happened rather than `ok`.
    if let Some(kind) = capture.timed_out {
        return Ok(timeout_verdict(ctx, &command, language, &capture, kind, timeout_secs));
    }

    let mut call_args = args.clone();
    call_args["command"] = Value::String(command.clone());
    call_args["output"] = Value::String(capture.output);
    call_args["language"] = language.map_or(Value::Null, Value::from);

    let text = call_preset(ctx, "check_output", &call_args).map_err(|e| fail(&e))?;

    Ok(classify_payload(&command, capture.exit_ok, language, &text))
}

/// The verdict for a command scout killed, written by scout rather than by the
/// model — and logged as `subprocess_timeout` so a wedged build is visible in
/// `scout stats` instead of hiding inside the `ok` count.
///
/// The payload keeps the shape every other path returns (`ok`, `summary`,
/// `first_error`, `suggested_next_step`, `command`, `exit_ok`) so callers and
/// `docs/` stay accurate, plus `timed_out` naming which deadline fired.
fn timeout_verdict(
    ctx: &Ctx,
    command: &str,
    language: Option<&str>,
    capture: &verify::Capture,
    kind: verify::TimeoutKind,
    wall_secs: u64,
) -> Value {
    let ran = capture.elapsed.as_secs_f64();
    let (what, next) = match kind {
        // Two different problems, and the difference is the actionable part: a
        // wall-clock stop means "still working, give it longer", an idle stop
        // means "it stopped working, a longer timeout will not help".
        verify::TimeoutKind::Idle => (
            format!(
                "scout killed the command: it printed nothing for {}s (after running {ran:.0}s), \
                 so it is wedged rather than slow",
                verify::IDLE_TIMEOUT.as_secs()
            ),
            "a silent process is usually blocked, not busy — look for a held lock, a prompt \
             waiting on stdin, or a stalled network fetch. Raising timeout_seconds will not help."
                .to_string(),
        ),
        verify::TimeoutKind::WallClock => (
            format!(
                "scout killed the command: it hit its {wall_secs}s wall-clock timeout \
                 (ran {ran:.0}s) while still producing output"
            ),
            format!(
                "it was making progress, so give it longer: re-run with timeout_seconds above \
                 {wall_secs} (max {MAX_TIMEOUT_SECS}), or narrow the command."
            ),
        ),
    };
    let summary = match last_output_line(&capture.output) {
        Some(line) => format!("{what}. Last output: {line:?}"),
        None => format!("{what}. It produced no output at all."),
    };

    ctx.ledger.record(
        ctx.record("check_output", &serde_json::json!({ "command": command }))
            .outcome(crate::stats::Outcome::SubprocessTimeout)
            .summary(&summary)
            .ms(ctx.ledger.elapsed_ms()),
    );

    let mut payload = serde_json::json!({
        "ok": false,
        "summary": summary,
        "first_error": Value::Null,
        "suggested_next_step": next,
        "command": command,
        "exit_ok": false,
        "timed_out": kind.as_str(),
    });
    if let Some(l) = language {
        payload["language"] = Value::String(l.to_string());
    }
    payload
}

/// The last non-blank line of the capture, capped — the single most useful
/// fact about a hang is what it managed to say before it stopped saying
/// anything.
fn last_output_line(output: &str) -> Option<String> {
    const MAX: usize = 120;
    let line = output.lines().rev().map(str::trim).find(|l| !l.is_empty())?;
    Some(match line.char_indices().nth(MAX) {
        Some((byte, _)) => format!("{}…", &line[..byte]),
        None => line.to_string(),
    })
}

/// Build the tool payload from the model's reply.
///
/// The preset demands a strict JSON object; a well-behaved model's reply is
/// passed through with the ground truth (`command`, `exit_ok`) added.  A model
/// that answered in prose anyway still produces a usable result rather than a
/// failure — the text is returned under `summary` with `ok` taken from the
/// real exit status, which is the honest fallback verdict.
pub fn classify_payload(
    command: &str,
    exit_ok: bool,
    language: Option<&str>,
    reply: &str,
) -> Value {
    let mut payload = match parse_selector_json(reply) {
        Some(Value::Object(map)) => Value::Object(map),
        _ => serde_json::json!({
            "ok": exit_ok,
            "summary": reply.trim(),
            "first_error": Value::Null,
            "suggested_next_step": "the local model did not return JSON; the summary above is its raw reply",
            "unstructured": true,
        }),
    };
    payload["command"] = Value::String(command.to_string());
    payload["exit_ok"] = Value::Bool(exit_ok);
    if let Some(l) = language {
        payload["language"] = Value::String(l.to_string());
    }
    payload
}

/// Fail open, naming this tool's fallback (running the command directly).
fn fail(reason: &str) -> ToolError {
    ToolError::new(format!("scout check_output: {reason}"), FALLBACK)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::select::Ctx;

    fn offline_ctx(project: &str) -> Ctx<'static> {
        Ctx {
            client_error: Some("no config in tests".into()),
            project: project.to_string(),
            // A test must never append to the developer's own call log.
            ledger: crate::stats::Ledger::silent(),
            ..Default::default()
        }
    }

    fn assert_fails_open(err: &ToolError) -> String {
        let text = err.text();
        assert!(text.contains("Bash tool"), "fallback tool must be named, got: {text}");
        text
    }

    // ── Argument handling ─────────────────────────────────────────────

    #[test]
    fn missing_command_fails_open() {
        let ctx = offline_ctx(".");
        let err = run(&ctx, &serde_json::json!({})).unwrap_err();
        let text = assert_fails_open(&err);
        assert!(text.contains("'command'"), "text: {text}");
    }

    #[test]
    fn blank_command_fails_open() {
        let ctx = offline_ctx(".");
        let err = run(&ctx, &serde_json::json!({"command": "   "})).unwrap_err();
        assert!(assert_fails_open(&err).contains("'command'"));
    }

    #[test]
    fn a_nonexistent_cwd_fails_open_before_running_anything() {
        let ctx = offline_ctx(".");
        let err =
            run(&ctx, &serde_json::json!({"command": "true", "cwd": "/nope/nowhere"})).unwrap_err();
        let text = assert_fails_open(&err);
        assert!(text.contains("not a directory"), "text: {text}");
    }

    #[test]
    fn without_a_configured_llm_it_fails_open_after_running_the_command() {
        // The command still runs (it is the caller's command, and the failure
        // is scout's), but the missing config is named plainly.
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("ran.txt");
        let ctx = offline_ctx(&dir.path().to_string_lossy());
        let err = run(&ctx, &serde_json::json!({"command": format!("touch {}", marker.display())}))
            .unwrap_err();
        assert!(marker.exists(), "the command should have run");
        let text = assert_fails_open(&err);
        assert!(text.contains("not configured"), "text: {text}");
    }

    // ── Subprocess timeouts ───────────────────────────────────────────

    #[test]
    fn a_timed_out_command_is_answered_without_ever_calling_the_model() {
        // `offline_ctx` has no client, so any call_preset would fail open with
        // a ToolError.  An `Ok` here *is* the proof that no round-trip was
        // attempted — and the row records why.
        let dir = tempfile::tempdir().unwrap();
        let ctx = offline_ctx(&dir.path().to_string_lossy());
        let p = run(
            &ctx,
            &serde_json::json!({"command": "echo starting; sleep 30", "timeout_seconds": 1}),
        )
        .expect("a subprocess timeout must not fail open — there is a real verdict to give");

        assert_eq!(p["ok"], false, "a killed command is never ok");
        assert_eq!(p["exit_ok"], false);
        assert_eq!(p["command"], "echo starting; sleep 30");
        assert_eq!(p["timed_out"], "wall_clock", "the outer cap is what fired at 1s");
        assert_eq!(p["first_error"], Value::Null);
        let summary = p["summary"].as_str().unwrap();
        assert!(summary.contains("wall-clock"), "which deadline fired must be stated: {summary}");
        assert!(summary.contains("starting"), "the last output is the useful clue: {summary}");
        let next = p["suggested_next_step"].as_str().unwrap();
        assert!(next.contains("timeout_seconds"), "next step: {next}");

        assert_eq!(
            ctx.ledger.pending_outcome(),
            Some(crate::stats::Outcome::SubprocessTimeout),
            "a wedged build must be visible in the log as itself, not as an ok row"
        );
    }

    #[test]
    fn an_idle_timeout_says_so_and_does_not_suggest_a_longer_deadline() {
        // Constructed directly rather than by waiting out IDLE_TIMEOUT: the
        // branch under test is the wording, and 120s is not a test budget.
        let ctx = offline_ctx(".");
        let capture = verify::Capture {
            exit_ok: false,
            output: "Compiling serde v1.0.197\n".to_string(),
            timed_out: Some(verify::TimeoutKind::Idle),
            elapsed: Duration::from_secs(140),
        };
        let p = timeout_verdict(
            &ctx,
            "cargo build",
            Some("rust"),
            &capture,
            verify::TimeoutKind::Idle,
            600,
        );

        assert_eq!(p["timed_out"], "idle");
        assert_eq!(p["language"], "rust");
        let summary = p["summary"].as_str().unwrap();
        assert!(summary.contains("printed nothing"), "summary: {summary}");
        assert!(summary.contains("Compiling serde"), "summary: {summary}");
        let next = p["suggested_next_step"].as_str().unwrap();
        assert!(
            next.contains("will not help"),
            "silence means stuck; a bigger timeout is the wrong advice: {next}"
        );
    }

    #[test]
    fn last_output_line_takes_the_last_useful_line_and_caps_it() {
        assert_eq!(last_output_line(""), None);
        assert_eq!(last_output_line("   \n\n"), None);
        assert_eq!(last_output_line("first\nlast\n\n").as_deref(), Some("last"));
        let long = last_output_line(&"x".repeat(500)).unwrap();
        assert!(long.ends_with('…'), "truncation must be visible");
        assert!(long.chars().count() <= 121, "capped to {} chars", long.chars().count());
    }

    // ── Payload shaping ───────────────────────────────────────────────

    #[test]
    fn a_strict_json_reply_passes_through_with_ground_truth_added() {
        let reply = r#"{"ok": false, "summary": "3 tests failed",
                        "first_error": {"file": "src/a.rs", "line": 12, "message": "assert failed"},
                        "suggested_next_step": "run cargo test a::b"}"#;
        let p = classify_payload("cargo test", false, Some("rust"), reply);
        assert_eq!(p["ok"], false);
        assert_eq!(p["summary"], "3 tests failed");
        assert_eq!(p["first_error"]["line"], 12);
        assert_eq!(p["command"], "cargo test", "the command is ground truth, not model output");
        assert_eq!(p["exit_ok"], false, "the real exit status is always reported");
        assert_eq!(p["language"], "rust");
    }

    #[test]
    fn a_fenced_reply_is_still_parsed() {
        let reply = "```json\n{\"ok\": true, \"summary\": \"Build succeeded\"}\n```";
        let p = classify_payload("cargo build", true, None, reply);
        assert_eq!(p["ok"], true);
        assert_eq!(p["summary"], "Build succeeded");
        assert!(p.get("language").is_none(), "no manifest, no language field");
    }

    #[test]
    fn a_prose_reply_degrades_to_the_exit_status_verdict() {
        let p = classify_payload("cargo test", true, None, "Everything looks fine to me!");
        assert_eq!(p["ok"], true, "with no JSON, the exit status is the verdict");
        assert_eq!(p["summary"], "Everything looks fine to me!");
        assert_eq!(p["unstructured"], true);
        assert_eq!(p["exit_ok"], true);
    }

    #[test]
    fn a_model_verdict_can_disagree_with_the_exit_status_and_both_survive() {
        // A test runner that exits 0 while printing failures is exactly why
        // the classifier exists — and why the raw status stays visible.
        let p =
            classify_payload("make test", true, None, r#"{"ok": false, "summary": "2 failed"}"#);
        assert_eq!(p["ok"], false, "the model's verdict wins the 'ok' field");
        assert_eq!(p["exit_ok"], true, "the shell's verdict is still reported");
    }
}
