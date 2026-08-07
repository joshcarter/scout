//! `check_output` — run a build/test command and classify its output locally.
//!
//! The caller supplies only a command string.  scout runs it here, capturing
//! stdout+stderr, and forwards `{command, output, language}` to the
//! `check_output` preset.  The raw output never enters the caller's context;
//! only the compact JSON verdict the model returns does.
//!
//! Written fresh for scout (PLAN §8: ct's `mcp.rs` may carry upstream
//! ancestry, so only the *logic* of its `forward_check_output` was carried
//! over, never the file).  The shape of that logic survives: pluck `command`,
//! resolve `cwd` against the project root, clamp the timeout, capture with
//! `verify::run_command_capture`, inject the output into the preset args, call
//! the model.
//!
//! Invariants:
//!
//! * The command runs exactly once, with its output capped before it is ever
//!   sent anywhere.
//! * The verdict is the model's, but `exit_ok` reports the real exit status
//!   alongside it so a mis-classification is always detectable.
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
const DEFAULT_TIMEOUT_SECS: u64 = 60;

/// Hard cap on the timeout — an unbounded wait would freeze the MCP loop.
const MAX_TIMEOUT_SECS: u64 = 600;

/// Run `command` and return the local model's classification of its output.
pub fn run(ctx: &Ctx, args: &Value) -> ToolResult {
    let command = non_empty_arg(args, "command")
        .ok_or_else(|| fail("'command' argument is required and must be non-empty"))?;

    let cwd: PathBuf = args
        .get("cwd")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(&ctx.project));
    if !cwd.is_dir() {
        return Err(fail(&format!("cwd {} is not a directory", cwd.display())));
    }

    let timeout_secs = args
        .get("timeout_seconds")
        .and_then(Value::as_u64)
        .unwrap_or(DEFAULT_TIMEOUT_SECS)
        .clamp(1, MAX_TIMEOUT_SECS);

    let (exit_ok, output) = verify::run_command_capture(
        &command,
        &cwd,
        Duration::from_secs(timeout_secs),
        verify::MAX_OUTPUT_BYTES,
    );

    // Augment the args with the captured output before calling the model.
    // `language` is injected too so a user-authored preset override can key
    // its prompt on the toolchain (`${args.language}`); the built-in preset
    // ignores it.
    let language = verify::detect_language(&cwd).map(|l| l.as_str());
    let mut call_args = args.clone();
    call_args["command"] = Value::String(command.clone());
    call_args["output"] = Value::String(output);
    call_args["language"] = language.map(Value::from).unwrap_or(Value::Null);

    let text = call_preset(ctx, "check_output", &call_args).map_err(|e| fail(&e))?;

    Ok(classify_payload(&command, exit_ok, language, &text))
}

/// Build the tool payload from the model's reply.
///
/// The preset demands a strict JSON object; a well-behaved model's reply is
/// passed through with the ground truth (`command`, `exit_ok`) added.  A model
/// that answered in prose anyway still produces a usable result rather than a
/// failure — the text is returned under `summary` with `ok` taken from the
/// real exit status, which is the honest fallback verdict.
pub fn classify_payload(command: &str, exit_ok: bool, language: Option<&str>, reply: &str) -> Value {
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
            client: None,
            client_error: Some("no config in tests".into()),
            presets: &[],
            project: project.to_string(),
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
        let err = run(&ctx, &serde_json::json!({"command": "true", "cwd": "/nope/nowhere"}))
            .unwrap_err();
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
        let err = run(
            &ctx,
            &serde_json::json!({"command": format!("touch {}", marker.display())}),
        )
        .unwrap_err();
        assert!(marker.exists(), "the command should have run");
        let text = assert_fails_open(&err);
        assert!(text.contains("not configured"), "text: {text}");
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
        let p = classify_payload("make test", true, None, r#"{"ok": false, "summary": "2 failed"}"#);
        assert_eq!(p["ok"], false, "the model's verdict wins the 'ok' field");
        assert_eq!(p["exit_ok"], true, "the shell's verdict is still reported");
    }
}
