//! `wrap` — run any verbose command and return its output condensed.
//!
//! The structural sibling of `check_output`, and deliberately shaped like it:
//! same argument validation, same capture, same "a killed command is answered
//! by scout, not by the model", same tolerance for a reply that arrives fenced
//! or in prose.  What differs is the job.  `check_output` renders a **verdict**
//! over a genre with pass/fail semantics; `wrap` does **retrieval** over
//! arbitrary output, where the exit code passes through uninterpreted and the
//! model is forbidden to advise (docs/wrap-watch.md §3.1).
//!
//! Two invariants carry the design:
//!
//! * **Everything the caller could check is checked by Rust.**  `exit_code`,
//!   `filtered`, `lines_total`, `lines_dropped`, `bytes_total` and `raw_path`
//!   are stamped here from the capture and the spool write.  The model
//!   contributes `summary`, `answer` and `notable` and nothing else — a reply
//!   claiming a different exit code is simply not read.
//! * **Filtering is recoverable.**  A filtered payload names the spool blob
//!   holding the full capture (§2.4), so a summary that dropped the one line
//!   that mattered costs a `Read raw_path`, not a re-run of a command that may
//!   be slow or non-idempotent.  Which is also why the spool write happens
//!   before the model is asked anything, and why its failure degrades the
//!   payload rather than the result.

use serde_json::Value;
use std::path::PathBuf;
use std::time::Duration;

use crate::select::{
    call_preset_recorded, non_empty_arg, parse_selector_json, Ctx, ToolError, ToolResult,
};
use crate::stats::Outcome;
use crate::{config, spool, verify};

/// Raw tool to name whenever this filter cannot deliver.
const FALLBACK: &str = "running the command yourself with the Bash tool";

/// Hard cap on the wall clock — same ceiling as `[check_output]`, and the
/// MCP dispatch backstop sits above it.
const MAX_TIMEOUT_SECS: u64 = 3600;

/// How much of a command's output scout keeps at all.
///
/// Far above `[wrap] model_input_bytes` on purpose: the spool is the ground
/// truth the escalation path reads (§2.4), so a capture bound equal to what the
/// model saw would leave `raw_path` holding nothing the payload did not already
/// carry.  Bounded all the same — `verify::BoundedBuffer` holds head+tail per
/// stream, so this is the peak resident cost of a command that prints forever,
/// and §3.4 chooses bounded honesty over unbounded memory.
const CAPTURE_MAX_BYTES: usize = 4 * 1024 * 1024;

/// The `[wrap]` tunables (docs/wrap-watch.md §3.2, §3.4).
///
/// Parsed strictly by `config::load_wrap_config`; the defaults are what a
/// caller with no config file gets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WrapConfig {
    /// Output at or under this many lines is returned whole, unfiltered.
    pub passthrough_max_lines: u64,
    /// ...and under this many bytes: a 200-line file of 4 KB lines is not
    /// short, however few lines it has.
    pub passthrough_max_bytes: u64,
    /// How much of the capture the model is shown, head+tail elided.
    pub model_input_bytes: u64,
    /// How long the command may print nothing before it is treated as stuck.
    pub idle_timeout_seconds: u64,
    /// Wall-clock ceiling when the caller omits `timeout_seconds`.
    pub default_timeout_seconds: u64,
}

impl Default for WrapConfig {
    fn default() -> Self {
        WrapConfig {
            passthrough_max_lines: 200,
            passthrough_max_bytes: 16 * 1024,
            model_input_bytes: 16 * 1024,
            idle_timeout_seconds: 120,
            default_timeout_seconds: 900,
        }
    }
}

impl WrapConfig {
    /// `model_input_bytes` as an elision limit: `truncate_diagnostic` asserts a
    /// positive limit, and a configured 0 is a bound, not a licence to panic.
    fn elision_limit(self) -> usize {
        usize::try_from(self.model_input_bytes).unwrap_or(usize::MAX).max(1)
    }
}

/// Everything about the result that is scout's to state rather than the
/// model's (docs/wrap-watch.md §3.3).
struct Ground {
    /// The child's status, uninterpreted; `null` when it never reported one.
    exit_code: Value,
    lines_total: usize,
    bytes_total: usize,
    /// The spool blob, or `null` when the write failed.
    raw_path: Value,
    /// `Some` when the spool write failed: the payload still comes back, minus
    /// its escalation path, and has to say so (§3.5).
    spool_note: Option<&'static str>,
}

/// Run `command` and return its output condensed by the local model.
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

    // A `[wrap]` scout cannot parse is reported by no one and costs nothing:
    // the same rule the spool bounds take on the write path (§3.5), because a
    // mistyped tunable must not be why a command's result is lost.
    let cfg = config::load_wrap_config(&config::config_path()).unwrap_or_default();

    let timeout_secs = args
        .get("timeout_seconds")
        .and_then(Value::as_u64)
        .unwrap_or(cfg.default_timeout_seconds)
        .clamp(1, MAX_TIMEOUT_SECS);

    let capture = verify::capture_with_deadlines(
        &command,
        &cwd,
        Duration::from_secs(timeout_secs),
        Duration::from_secs(cfg.idle_timeout_seconds),
        CAPTURE_MAX_BYTES,
    );

    // What would otherwise have landed in the caller's context.
    ctx.ledger.raw_bytes(capture.output.len() as u64);

    let log_args = serde_json::json!({
        "command": command,
        "question": args.get("question").cloned().unwrap_or(Value::Null),
    });

    // A killed command short-circuits: no model, no round-trip, and a row that
    // says what actually happened.
    if let Some(kind) = capture.timed_out {
        return Ok(timeout_payload(ctx, &log_args, &capture, kind, timeout_secs, cfg));
    }

    let lines_total = capture.output.lines().count();
    let bytes_total = capture.output.len();
    let exit_code = capture.exit_code.map_or(Value::Null, Value::from);

    // §3.2: short output is returned whole — no model, no spool, nothing to
    // recover from.  This is what makes guessing wrong about "this will be
    // verbose" cost only the exec.
    if lines_total as u64 <= cfg.passthrough_max_lines
        && bytes_total as u64 <= cfg.passthrough_max_bytes
    {
        ctx.ledger.record(
            ctx.record("wrap", &log_args)
                .outcome(Outcome::Bypassed)
                .summary(format!("{lines_total} line(s) returned verbatim, unfiltered"))
                .ms(ctx.ledger.elapsed_ms()),
        );
        return Ok(serde_json::json!({
            "exit_code": exit_code,
            "filtered": false,
            "output": capture.output,
        }));
    }

    // §2.2: only a filtered result spools, and it spools the *full* capture
    // even though the model is about to see an elided copy.
    let spool_cfg = config::load_spool_config(&config::config_path()).unwrap_or_default();
    let mut rec = ctx.record("wrap", &log_args);
    let spooled = spool::write("wrap", &rec.id, &capture.output, &spool_cfg);
    if let Some(path) = &spooled {
        rec = rec.raw_path(path);
    }
    let ground = Ground {
        exit_code,
        lines_total,
        bytes_total,
        raw_path: spooled.as_ref().map_or(Value::Null, |p| Value::String(p.display().to_string())),
        spool_note: spooled.is_none().then_some("spool-unavailable"),
    };

    let mut model_input = capture.output.clone();
    verify::truncate_diagnostic(&mut model_input, cfg.elision_limit());

    let mut call_args = args.clone();
    call_args["command"] = Value::String(command.clone());
    call_args["output"] = Value::String(model_input);

    let (rec, reply) = call_preset_recorded(ctx, "wrap", &call_args, rec);
    if let Some(payload) = reply.as_ref().ok().and_then(|text| condensed_payload(&ground, text)) {
        ctx.ledger.record(rec);
        return Ok(payload);
    }

    // Everything else is the fail-open path (§3.5): the model was unreachable,
    // returned nothing, or answered in prose.  The caller still gets the
    // command's output, and the row still says which of the three it was — a
    // round-trip that never happened came back with the record untouched, so
    // this is the only place that knows.
    let (reason, outcome) = match &reply {
        Err(e) => (e.clone(), Outcome::EndpointUnreachable),
        Ok(_) if rec.outcome == Outcome::EmptyResponse => {
            ("the local model returned nothing".to_string(), Outcome::EmptyResponse)
        }
        Ok(_) => ("the local model did not return JSON".to_string(), Outcome::ParseFailure),
    };
    let rec = if rec.outcome.is_ok() { rec.outcome(outcome).summary(&reason) } else { rec };
    ctx.ledger.record(rec);
    Ok(degraded_payload(&ground, &reason, &capture.output, cfg))
}

/// The payload for a command scout killed, written by scout rather than by the
/// model — and logged as `subprocess_timeout` so a wedged command is visible in
/// `scout stats` as itself.
///
/// It takes the degraded shape (§3.5) rather than the filtered one: nothing was
/// condensed, so there are no counts to report and no summary to attribute.
fn timeout_payload(
    ctx: &Ctx,
    log_args: &Value,
    capture: &verify::Capture,
    kind: verify::TimeoutKind,
    wall_secs: u64,
    cfg: WrapConfig,
) -> Value {
    let ran = capture.elapsed.as_secs_f64();
    // The distinction is the actionable part: a wall-clock stop means "still
    // working, give it longer", an idle stop means "it stopped working".
    let what = match kind {
        verify::TimeoutKind::Idle => format!(
            "scout killed the command: it printed nothing for {}s (after running {ran:.0}s), \
             so it is wedged rather than slow",
            cfg.idle_timeout_seconds
        ),
        verify::TimeoutKind::WallClock => format!(
            "scout killed the command: it hit its {wall_secs}s wall-clock timeout \
             (ran {ran:.0}s) while still producing output"
        ),
    };

    ctx.ledger.record(
        ctx.record("wrap", log_args)
            .outcome(Outcome::SubprocessTimeout)
            .summary(&what)
            .ms(ctx.ledger.elapsed_ms()),
    );

    let mut output = capture.output.clone();
    verify::truncate_diagnostic(&mut output, cfg.elision_limit());
    serde_json::json!({
        "exit_code": Value::Null,
        "filtered": false,
        "degraded": format!("timed_out ({}): {what}", kind.as_str()),
        "output": output,
    })
}

/// The filtered payload (§3.3): the model's three fields, everything else
/// scout's.
///
/// `None` when the reply is not a usable condensation — prose, an empty
/// string, or an object with no summary in it — which the caller degrades.
fn condensed_payload(ground: &Ground, reply: &str) -> Option<Value> {
    let parsed = parse_selector_json(reply)?;
    let text = |key: &str| {
        parsed.get(key).and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty())
    };
    let summary = text("summary")?;
    // Verbatim, and only strings: `notable` is quoted output, so anything the
    // model wrapped in structure it invented is dropped rather than rendered.
    let notable: Vec<Value> = parsed
        .get("notable")
        .and_then(Value::as_array)
        .map(|items| items.iter().filter(|v| v.is_string()).cloned().collect())
        .unwrap_or_default();

    let mut payload = serde_json::json!({
        "exit_code": ground.exit_code,
        "filtered": true,
        "summary": summary,
        "answer": text("answer").map_or(Value::Null, |a| Value::String(a.to_string())),
        "notable": notable.clone(),
        "lines_total": ground.lines_total,
        // A model that quoted more lines than the capture holds has dropped
        // none of them; the floor keeps the count honest either way.
        "lines_dropped": ground.lines_total.saturating_sub(notable.len()),
        "bytes_total": ground.bytes_total,
        "raw_path": ground.raw_path,
    });
    if let Some(note) = ground.spool_note {
        payload["degraded"] = Value::String(note.to_string());
    }
    Some(payload)
}

/// Fail open with the command's own output (§3.5).
///
/// A broken local model must never cost the caller the command's result, so
/// this is not an error: it is the head+tail of the raw output, the reason the
/// filter did not run, and the spool path if there is one.
fn degraded_payload(ground: &Ground, reason: &str, output: &str, cfg: WrapConfig) -> Value {
    let mut elided = output.to_string();
    verify::truncate_diagnostic(&mut elided, cfg.elision_limit());
    let reason = match ground.spool_note {
        Some(note) => format!("{note}; {reason}"),
        None => reason.to_string(),
    };
    serde_json::json!({
        "exit_code": ground.exit_code,
        "filtered": false,
        "degraded": reason,
        "output": elided,
        "raw_path": ground.raw_path,
    })
}

/// Fail open, naming this tool's fallback (running the command directly).
fn fail(reason: &str) -> ToolError {
    ToolError::new(format!("scout wrap: {reason}"), FALLBACK)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// A context with no model configured, rooted at `project`.
    fn offline_ctx(project: &str) -> Ctx<'static> {
        Ctx {
            client_error: Some("no config in tests".into()),
            project: project.to_string(),
            tool: "wrap".to_string(),
            // A test must never append to the developer's own call log.
            ledger: crate::stats::Ledger::silent(),
            ..Default::default()
        }
    }

    // Nothing here takes the filtered path, and that is deliberate: it is the
    // one path that writes to `$XDG_CACHE_HOME`, and an in-process test could
    // only redirect that by setting a process-global env var that `spool`'s own
    // tests are already reading.  The spool, the elision the model actually
    // sees, and the degraded payload are pinned from outside instead, against
    // the real binary in a sandbox — see `tests/wrap_cli.rs`.

    fn ground_for(lines_total: usize) -> Ground {
        Ground {
            exit_code: Value::from(0),
            lines_total,
            bytes_total: lines_total * 8,
            raw_path: Value::String("/cache/scout/raw/2026-08-15/143212-wrap-a3f9.log".into()),
            spool_note: None,
        }
    }

    // ── Argument handling ─────────────────────────────────────────────

    #[test]
    fn missing_or_blank_command_fails_open_naming_the_bash_tool() {
        let ctx = offline_ctx(".");
        for args in [serde_json::json!({}), serde_json::json!({"command": "   "})] {
            let text = run(&ctx, &args).unwrap_err().text();
            assert!(text.contains("'command'"), "text: {text}");
            assert!(text.contains("Bash tool"), "fallback must be named: {text}");
        }
    }

    #[test]
    fn a_nonexistent_cwd_fails_open_before_running_anything() {
        let ctx = offline_ctx(".");
        let dir = TempDir::new().unwrap();
        let marker = dir.path().join("ran.txt");
        let err = run(
            &ctx,
            &serde_json::json!({
                "command": format!("touch {}", marker.display()),
                "cwd": "/nope/nowhere",
            }),
        )
        .unwrap_err();
        assert!(!marker.exists(), "the command must not run when the cwd is bad");
        assert!(err.text().contains("not a directory"), "text: {}", err.text());
    }

    // ── Pass-through (§3.2) ───────────────────────────────────────────

    #[test]
    fn small_output_comes_back_verbatim_with_no_model_and_nothing_spooled() {
        // `offline_ctx` has no client, so an `Ok` here *is* the proof that no
        // round-trip was attempted; the absent `raw_path` is the proof that
        // nothing was spooled, since §2.2 gives a pass-through nothing to keep.
        let dir = TempDir::new().unwrap();
        let ctx = offline_ctx(&dir.path().to_string_lossy());
        let p = run(&ctx, &serde_json::json!({"command": "echo one; echo two"})).unwrap();

        assert_eq!(p["exit_code"], 0);
        assert_eq!(p["filtered"], false);
        assert_eq!(p["output"], "one\ntwo");
        assert!(p.get("summary").is_none(), "a pass-through has nothing to summarize: {p}");
        assert!(p.get("raw_path").is_none(), "nothing lossy happened, so nothing was kept: {p}");
        assert_eq!(ctx.ledger.pending_outcome(), Some(Outcome::Bypassed));
    }

    #[test]
    fn a_nonzero_exit_passes_through_uninterpreted() {
        // §3.1: `grep` exiting 1 is not a failure, and wrap does not editorialize.
        let dir = TempDir::new().unwrap();
        let ctx = offline_ctx(&dir.path().to_string_lossy());
        let p = run(&ctx, &serde_json::json!({"command": "echo nope; exit 3"})).unwrap();
        assert_eq!(p["exit_code"], 3);
        assert_eq!(p["filtered"], false);
        assert!(p.get("ok").is_none(), "wrap renders no verdict: {p}");
    }

    // ── Subprocess timeouts ───────────────────────────────────────────

    #[test]
    fn a_timed_out_command_is_answered_without_ever_calling_the_model() {
        let dir = TempDir::new().unwrap();
        let ctx = offline_ctx(&dir.path().to_string_lossy());
        let p = run(
            &ctx,
            &serde_json::json!({"command": "echo starting; sleep 30", "timeout_seconds": 1}),
        )
        .expect("a subprocess timeout must not fail open — there is a real answer to give");

        assert_eq!(p["exit_code"], Value::Null, "a killed command reported no status");
        assert_eq!(p["filtered"], false);
        let degraded = p["degraded"].as_str().unwrap();
        assert!(degraded.contains("wall_clock"), "which deadline fired: {degraded}");
        assert!(p["output"].as_str().unwrap().contains("starting"), "{p}");
        assert_eq!(
            ctx.ledger.pending_outcome(),
            Some(Outcome::SubprocessTimeout),
            "a wedged command must be visible in the log as itself"
        );
    }

    #[test]
    fn the_idle_verdict_names_the_configured_deadline_not_the_compiled_one() {
        let ctx = offline_ctx(".");
        let capture = verify::Capture {
            exit_ok: false,
            exit_code: None,
            output: String::new(),
            stdout: String::new(),
            stderr: String::new(),
            timed_out: Some(verify::TimeoutKind::Idle),
            elapsed: Duration::from_secs(300),
        };
        let cfg = WrapConfig { idle_timeout_seconds: 240, ..WrapConfig::default() };
        let p = timeout_payload(
            &ctx,
            &serde_json::json!({"command": "make"}),
            &capture,
            verify::TimeoutKind::Idle,
            900,
            cfg,
        );
        let degraded = p["degraded"].as_str().unwrap();
        assert!(degraded.contains("240s"), "degraded: {degraded}");
        assert!(!degraded.contains("120s"), "the compiled default must not leak: {degraded}");
    }

    // ── The filtered path ─────────────────────────────────────────────

    #[test]
    fn a_spool_that_cannot_be_written_still_returns_the_result_and_says_so() {
        // The degraded payload, with the escalation path missing rather than
        // the result (§3.5).  Reached here without touching the real cache:
        // `Ground` is what carries the spool's verdict into the payload.
        let ground = Ground {
            exit_code: Value::from(0),
            lines_total: 400,
            bytes_total: 3200,
            raw_path: Value::Null,
            spool_note: Some("spool-unavailable"),
        };
        let p = degraded_payload(
            &ground,
            "local LLM is not configured: no config",
            "line-1\nline-2\n",
            WrapConfig::default(),
        );
        assert_eq!(p["filtered"], false);
        assert_eq!(p["raw_path"], Value::Null);
        let degraded = p["degraded"].as_str().unwrap();
        assert!(degraded.starts_with("spool-unavailable; "), "{degraded}");
        assert!(degraded.contains("not configured"), "both reasons survive: {degraded}");
        assert!(p["output"].as_str().unwrap().contains("line-1"), "{p}");
    }

    // ── Payload shaping ───────────────────────────────────────────────

    #[test]
    fn a_strict_json_reply_becomes_the_filtered_payload_with_ground_truth_stamped() {
        let reply = r#"{"summary": "The log listed 12 commits.",
                        "answer": "a3f9c21 changed the retry default",
                        "notable": ["a3f9c21 raise retry default to 5", "src/client.rs:88"]}"#;
        let p = condensed_payload(&ground_for(3412), reply).expect("a usable condensation");

        assert_eq!(p["filtered"], true);
        assert_eq!(p["exit_code"], 0);
        assert_eq!(p["summary"], "The log listed 12 commits.");
        assert_eq!(p["answer"], "a3f9c21 changed the retry default");
        assert_eq!(p["notable"][1], "src/client.rs:88", "notable lines are verbatim");
        assert_eq!(p["lines_total"], 3412);
        assert_eq!(p["lines_dropped"], 3410);
        assert_eq!(p["bytes_total"], 3412 * 8);
        assert!(p["raw_path"].as_str().unwrap().ends_with("143212-wrap-a3f9.log"));
        assert!(p.get("degraded").is_none(), "nothing degraded here: {p}");
    }

    #[test]
    fn a_fenced_reply_is_still_parsed_and_a_missing_answer_is_null() {
        let reply = "```json\n{\"summary\": \"Ten files changed.\", \"notable\": []}\n```";
        let p = condensed_payload(&ground_for(40), reply).expect("fences are tolerated");
        assert_eq!(p["summary"], "Ten files changed.");
        assert_eq!(p["answer"], Value::Null, "no question, no answer");
        assert_eq!(p["notable"], serde_json::json!([]));
        assert_eq!(p["lines_dropped"], 40);
    }

    #[test]
    fn a_prose_reply_is_not_a_condensation() {
        // Nothing to stamp ground truth onto: the caller degrades to the raw
        // output rather than passing a sentence off as a summary.
        assert!(condensed_payload(&ground_for(40), "Looks like a lot of git output!").is_none());
        assert!(condensed_payload(&ground_for(40), "").is_none());
        assert!(
            condensed_payload(&ground_for(40), r#"{"notable": ["x"]}"#).is_none(),
            "an object with no summary condenses nothing"
        );
    }

    #[test]
    fn the_model_cannot_overwrite_the_facts_scout_measured() {
        // The whole reason `condensed_payload` rebuilds the object rather than
        // passing the reply through with fields added.
        let reply = r#"{"summary": "s", "notable": [], "exit_code": 0, "filtered": false,
                        "lines_total": 3, "lines_dropped": 0, "bytes_total": 1,
                        "raw_path": "/tmp/attacker.log"}"#;
        let mut ground = ground_for(900);
        ground.exit_code = Value::from(2);
        let p = condensed_payload(&ground, reply).unwrap();
        assert_eq!(p["exit_code"], 2, "the shell's status, not the model's claim");
        assert_eq!(p["filtered"], true);
        assert_eq!(p["lines_total"], 900);
        assert_eq!(p["lines_dropped"], 900);
        assert_eq!(p["bytes_total"], 7200);
        assert_eq!(p["raw_path"], ground.raw_path, "the spool path is scout's to state");
    }

    #[test]
    fn lines_dropped_never_goes_negative_and_notable_takes_only_strings() {
        let reply = r#"{"summary": "s", "notable": ["a", "b", "c", 4, {"line": "d"}]}"#;
        let p = condensed_payload(&ground_for(2), reply).unwrap();
        assert_eq!(
            p["notable"],
            serde_json::json!(["a", "b", "c"]),
            "invented structure is dropped"
        );
        assert_eq!(p["lines_dropped"], 0, "a floor, not a wrap-around");
    }

    // ── Configuration ─────────────────────────────────────────────────

    #[test]
    fn the_documented_defaults_are_the_defaults() {
        let cfg = WrapConfig::default();
        assert_eq!(cfg.passthrough_max_lines, 200, "docs/wrap-watch.md §3.2");
        // A 20-line blob of 4 KB lines is not short, however few lines it has,
        // which is why the byte cap sits alongside the line count.
        assert_eq!(cfg.passthrough_max_bytes, 16 * 1024);
        assert_eq!(cfg.model_input_bytes, 16 * 1024);
        assert_eq!(cfg.idle_timeout_seconds, 120);
        assert_eq!(cfg.default_timeout_seconds, 900);
        assert!(
            CAPTURE_MAX_BYTES as u64 > cfg.model_input_bytes,
            "a capture bound at the prompt budget would leave raw_path holding \
             nothing the payload did not already carry (§2.4)"
        );
    }

    #[test]
    fn a_zero_model_input_bytes_bound_elides_rather_than_panicking() {
        let cfg = WrapConfig { model_input_bytes: 0, ..WrapConfig::default() };
        assert_eq!(cfg.elision_limit(), 1);
        let p = degraded_payload(&ground_for(10), "endpoint down", "some output", cfg);
        assert_eq!(p["filtered"], false);
        assert!(p["output"].as_str().unwrap().contains("elided"), "{p}");
    }
}
