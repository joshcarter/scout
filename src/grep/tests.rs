//! Tests for `grep`: hit parsing, bypass path, rerank payload shape, and the
//! fail-open contract.
//!
//! Ported from ct's `local_grep/tests.rs`.  `grep_response` (a `ct::Response`
//! fixture) became a `source::SearchResults` fixture, and the fail-open tests
//! call `run` directly instead of going through ct's MCP request loop.

use super::*;
use crate::select::Ctx;
use crate::source::SearchHit;

const CTX: usize = 2;

fn hit(file: &str, line: usize, ctx: &str) -> RawHit {
    RawHit {
        file: file.to_string(),
        line,
        text: matched_line(ctx, line, CTX).map(str::to_string),
        context: ctx.to_string(),
    }
}

fn results(hits: Vec<(&str, usize, &str)>) -> SearchResults {
    SearchResults {
        hits: hits
            .into_iter()
            .map(|(file, line, text)| SearchHit {
                file: file.to_string(),
                line,
                text: text.to_string(),
            })
            .collect(),
        truncated: false,
    }
}

fn offline_ctx(project: &str) -> Ctx<'static> {
    Ctx {
        client: None,
        client_error: Some("no config in tests".into()),
        presets: &[],
        project: project.to_string(),
        progress: None,
    }
}

// ── Hit parsing ──────────────────────────────────────────────────────

#[test]
fn matched_line_is_recovered_from_the_context_block() {
    // The renderer emits lines[i-2 ..= i+2], so the match is at index 2.
    let ctx = "a\nb\nMATCH\nd\ne";
    assert_eq!(matched_line(ctx, 412, CTX), Some("MATCH"));
}

#[test]
fn matched_line_handles_the_top_of_file() {
    // Line 1 has no lines above it: the block starts at the match.
    assert_eq!(matched_line("MATCH\nb\nc", 1, CTX), Some("MATCH"));
    // Line 2 has exactly one line above it.
    assert_eq!(matched_line("a\nMATCH\nc\nd", 2, CTX), Some("MATCH"));
}

#[test]
fn matched_line_is_none_when_truncation_cut_the_block() {
    // The renderer's byte budget can cut the joined block before the matched
    // line is reached.  Refusing to answer beats mislabeling a surviving
    // neighbour's text as the match.
    assert_eq!(matched_line("only-one-line", 400, CTX), None);
    assert_eq!(matched_line("", 400, CTX), None);
    assert_eq!(matched_line("a very long line\n... (truncated)", 400, CTX), None);
}

#[test]
fn parse_hits_extracts_file_line_text_and_context() {
    let r = results(vec![
        ("internal/ec/builder.go", 412, "a\nb\nWritePack(&w)\nd\ne"),
        ("internal/ec/other.go", 1, "WritePack(x)\nnext"),
    ]);
    let hits = parse_hits(&r, CTX);
    assert_eq!(hits.len(), 2);
    assert_eq!(hits[0].file, "internal/ec/builder.go");
    assert_eq!(hits[0].line, 412);
    assert_eq!(hits[0].text.as_deref(), Some("WritePack(&w)"));
    assert_eq!(hits[0].context, "a\nb\nWritePack(&w)\nd\ne");
    assert_eq!(hits[1].text.as_deref(), Some("WritePack(x)"));
}

#[test]
fn parse_hits_keeps_a_truncated_hit_with_null_text() {
    // A deep-in-file hit whose context block was cut before the matched line:
    // the hit survives (file/line/context are real) but text is null, never
    // another line's content.
    let hits = parse_hits(&results(vec![("gen.go", 900, "one-surviving-fragment")]), CTX);
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].text, None);
    assert_eq!(hits[0].context, "one-surviving-fragment");
}

#[test]
fn parse_hits_on_an_empty_result_is_empty() {
    assert!(parse_hits(&results(vec![]), CTX).is_empty());
    assert!(parse_hits(&SearchResults::default(), CTX).is_empty());
}

// ── Hit-list rendering ───────────────────────────────────────────────

#[test]
fn hit_list_ids_are_global_across_batches() {
    let hits = vec![hit("a.rs", 10, "x\ny\nA\nz\nw"), hit("b.rs", 20, "x\ny\nB\nz\nw")];
    let first_batch = render_hit_list(&hits, 1);
    assert!(first_batch.starts_with("[1] a.rs:10\n"));
    assert!(first_batch.contains("[2] b.rs:20\n"));
    // A second batch continues the numbering rather than restarting.
    let second_batch = render_hit_list(&hits, 251);
    assert!(second_batch.starts_with("[251] a.rs:10\n"));
    assert!(second_batch.contains("[252] b.rs:20\n"));
}

// ── Bypass path ──────────────────────────────────────────────────────

#[test]
fn bypass_returns_every_hit_in_full_mode() {
    let hits = vec![hit("a.rs", 10, "x\ny\nA\nz\nw"), hit("b.rs", 20, "x\ny\nB\nz\nw")];
    let payload = bypass_payload("WritePack", "call sites that ignore the error", &hits, false);
    assert_eq!(payload["mode"], "full");
    assert_eq!(payload["hits_total"], 2);
    assert_eq!(payload["hits_considered"], 2);
    assert_eq!(payload["returned"], 2);
    assert_eq!(payload["dropped"], 0);
    assert_eq!(payload["none_relevant"], false);
    let out = payload["hits"].as_array().unwrap();
    assert_eq!(out.len(), 2, "bypass returns everything");
    assert_eq!(out[0]["file"], "a.rs");
    assert_eq!(out[0]["line"], 10);
    assert_eq!(out[0]["text"], "A");
    assert_eq!(out[0]["context"], "x\ny\nA\nz\nw");
    assert!(payload["hint"].as_str().unwrap().contains("no filtering"));
}

#[test]
fn bypass_on_zero_hits_is_an_empty_full_result_not_an_error() {
    let payload = bypass_payload("nothing", "anything", &[], false);
    assert_eq!(payload["mode"], "full");
    assert_eq!(payload["hits_total"], 0);
    assert_eq!(payload["none_relevant"], false);
}

#[test]
fn a_short_hit_list_from_a_real_tree_bypasses_the_llm() {
    // End-to-end through the filesystem search: no config, no endpoint, and
    // still a correct answer for a quiet pattern.
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.rs"), "fn one() {}\nfn needle() {}\nfn two() {}\n").unwrap();
    std::fs::write(dir.path().join("b.rs"), "nothing here\n").unwrap();
    let ctx = offline_ctx(&dir.path().to_string_lossy());
    let payload = run(&ctx, &serde_json::json!({"pattern": "needle", "intent": "the definition"}))
        .expect("short hit lists must not need the model");
    assert_eq!(payload["mode"], "full");
    assert_eq!(payload["hits_total"], 1);
    assert_eq!(payload["hits"][0]["file"], "a.rs");
    assert_eq!(payload["hits"][0]["line"], 2);
    assert_eq!(payload["hits"][0]["text"], "fn needle() {}");
}

// ── No-intent path (implicit --no-filter) ────────────────────────────

#[test]
fn no_intent_returns_every_hit_without_touching_the_model() {
    // Well past `bypass_max_hits`, with no client configured: the absence of
    // an intent must short-circuit before the model is ever needed.
    let dir = tempfile::tempdir().unwrap();
    let body: String = (0..40).map(|i| format!("let needle_{i} = 1;\n")).collect();
    std::fs::write(dir.path().join("a.rs"), body).unwrap();
    let ctx = offline_ctx(&dir.path().to_string_lossy());
    let payload = run(&ctx, &serde_json::json!({"pattern": "needle", "max_hits": 100}))
        .expect("no intent must never require the model");
    assert_eq!(payload["mode"], "full");
    assert_eq!(payload["intent"], serde_json::Value::Null);
    assert_eq!(payload["hits_total"], 40);
    assert_eq!(payload["returned"], 40);
    assert_eq!(payload["hits"].as_array().unwrap().len(), 40);
    assert!(payload["hint"].as_str().unwrap().contains("no intent given"));
}

#[test]
fn an_empty_intent_is_the_same_as_no_intent() {
    let dir = tempfile::tempdir().unwrap();
    let body: String = (0..40).map(|i| format!("let needle_{i} = 1;\n")).collect();
    std::fs::write(dir.path().join("a.rs"), body).unwrap();
    let ctx = offline_ctx(&dir.path().to_string_lossy());
    for intent in [serde_json::Value::String(String::new()), serde_json::Value::Null] {
        let payload =
            run(&ctx, &serde_json::json!({"pattern": "needle", "intent": intent, "max_hits": 100}))
                .expect("an empty intent must not demand the model");
        assert_eq!(payload["mode"], "full");
        assert_eq!(payload["intent"], serde_json::Value::Null);
        assert_eq!(payload["returned"], 40);
    }
}

#[test]
fn no_intent_truncates_at_max_hits_and_says_so() {
    let hits: Vec<RawHit> = (0..12).map(|i| hit("a.rs", i + 1, "x\ny\nA\nz\nw")).collect();
    let p = unfiltered_payload("needle", &hits, 5, false);
    assert_eq!(p["mode"], "full");
    assert_eq!(p["intent"], serde_json::Value::Null);
    assert_eq!(p["hits_total"], 12, "the full count stays visible");
    assert_eq!(p["returned"], 5);
    assert_eq!(p["hits"].as_array().unwrap().len(), 5);
    let hint = p["hint"].as_str().unwrap();
    assert!(hint.contains("first 5 of 12"), "hint must name the truncation: {hint}");
    assert!(hint.contains("--max-hits"), "hint: {hint}");
}

#[test]
fn no_intent_under_the_cap_reports_no_truncation() {
    let hits = vec![hit("a.rs", 10, "x\ny\nA\nz\nw")];
    let p = unfiltered_payload("needle", &hits, 10, true);
    assert_eq!(p["hits_total"], 1);
    assert_eq!(p["returned"], 1);
    assert_eq!(p["search_truncated"], true, "a capped scan stays visible");
    let hint = p["hint"].as_str().unwrap();
    assert_eq!(hint, "no intent given — unfiltered search, no filtering applied");
}

// ── Rerank payload ───────────────────────────────────────────────────

#[test]
fn rerank_payload_exposes_its_own_lossiness() {
    let returned = vec![serde_json::json!({"file": "a.rs", "line": 412, "text": "WritePack(&w)"})];
    let p = rerank_payload("WritePack", "ignored errors", 57, 57, returned, 2, false, false);
    assert_eq!(p["mode"], "rerank");
    assert_eq!(p["hits_total"], 57);
    assert_eq!(p["returned"], 1);
    assert_eq!(p["dropped"], 56);
    assert_eq!(p["dropped_invalid"], 2);
    assert_eq!(p["truncated_before_rerank"], false);
    let hint = p["hint"].as_str().unwrap();
    assert!(hint.contains("grep 'WritePack'"), "hint: {hint}");
    assert!(hint.contains("57"), "hint must name the unfiltered total: {hint}");
}

#[test]
fn none_relevant_is_an_explicit_verdict_not_an_empty_match_set() {
    let p = rerank_payload("WritePack", "ignored errors", 57, 57, vec![], 0, true, false);
    assert_eq!(p["returned"], 0);
    assert_eq!(p["none_relevant"], true);
    let hint = p["hint"].as_str().unwrap();
    assert!(hint.contains("NOT an empty match set"), "hint: {hint}");
    assert!(hint.contains("57"), "hint: {hint}");
}

#[test]
fn over_cap_runs_report_the_truncation() {
    let p = rerank_payload("x", "y", 900, 500, vec![], 0, true, true);
    assert_eq!(p["hits_total"], 900);
    assert_eq!(p["hits_considered"], 500);
    assert_eq!(p["truncated_before_rerank"], true);
    assert_eq!(p["search_truncated"], true, "a capped scan must be visible too");
}

// ── Fail-open contract ───────────────────────────────────────────────

/// Every failure must name the raw Grep tool.
fn assert_fails_open(err: &crate::select::ToolError) -> String {
    let text = err.text();
    assert!(text.contains("Grep tool"), "fallback tool must be named, got: {text}");
    text
}

#[test]
fn missing_pattern_arg_fails_open() {
    let ctx = offline_ctx(".");
    let err = run(&ctx, &serde_json::json!({"intent": "ignored errors"})).unwrap_err();
    let text = assert_fails_open(&err);
    assert!(text.contains("'pattern'"), "text: {text}");
}

#[test]
fn a_broken_pattern_fails_open_naming_the_grep_tool() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = offline_ctx(&dir.path().to_string_lossy());
    let err = run(
        &ctx,
        &serde_json::json!({"pattern": "(unclosed", "intent": "anything", "regex": true}),
    )
    .unwrap_err();
    let text = assert_fails_open(&err);
    assert!(text.contains("grep failed"), "text: {text}");
}

#[test]
fn a_noisy_hit_list_without_a_configured_llm_fails_open() {
    // Past the bypass threshold the model is required — and its absence must
    // read as a configuration problem, not a mystery.
    let dir = tempfile::tempdir().unwrap();
    let body: String = (0..40).map(|i| format!("let needle_{i} = 1;\n")).collect();
    std::fs::write(dir.path().join("a.rs"), body).unwrap();
    let ctx = offline_ctx(&dir.path().to_string_lossy());
    let err =
        run(&ctx, &serde_json::json!({"pattern": "needle", "intent": "the real one"})).unwrap_err();
    let text = assert_fails_open(&err);
    assert!(text.contains("not configured"), "text: {text}");
}
