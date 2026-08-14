//! Unit tests for selector validation against fixture LLM outputs.
//!
//! These are the tests that keep hallucinated selectors out of the caller's
//! context: every case here is a reply a local model has plausibly produced.
//!
//! Ported from ct's `local_select/tests.rs`.  The two envelope tests changed
//! shape: ct asserted on a JSON-RPC result object, scout asserts on
//! `ToolError` (the MCP envelope now belongs to rmcp).

use super::*;
use serde_json::json;

// ── parse_selector_json ──────────────────────────────────────────────

#[test]
fn parses_bare_json() {
    let v = parse_selector_json(r#"{"ranges": [], "not_found": true}"#).expect("should parse");
    assert_eq!(v["not_found"], json!(true));
}

#[test]
fn parses_fenced_json() {
    let text = "```json\n{\"ranges\": [{\"start\": 1, \"end\": 2}], \"not_found\": false}\n```";
    let v = parse_selector_json(text).expect("fenced JSON should parse");
    assert_eq!(v["ranges"][0]["start"], json!(1));
}

#[test]
fn parses_unlabelled_fence() {
    let v = parse_selector_json("```\n{\"keep\": []}\n```").expect("bare fence should parse");
    assert!(v["keep"].as_array().unwrap().is_empty());
}

#[test]
fn parses_prose_wrapped_json() {
    let text = "Sure! Here is the result you asked for:\n\
                {\"ranges\": [{\"start\": 10, \"end\": 20, \"why\": \"retry loop\", \"score\": 3}], \
                 \"answer\": \"Backoff is exponential.\", \"not_found\": false}\n\
                Let me know if you need more.";
    let v = parse_selector_json(text).expect("prose-wrapped JSON should parse");
    assert_eq!(v["ranges"][0]["why"], json!("retry loop"));
    assert_eq!(v["answer"], json!("Backoff is exponential."));
}

#[test]
fn parses_json_after_think_block() {
    let text = "<think>The user wants { braces } and mismatched } here</think>\n{\"keep\": [{\"id\": 2}]}";
    let v = parse_selector_json(text).expect("think block should be stripped");
    assert_eq!(v["keep"][0]["id"], json!(2));
}

#[test]
fn braces_inside_strings_do_not_unbalance_the_scan() {
    let text = r#"noise {"answer": "uses format!(\"{}\", x) here", "ranges": []} trailing"#;
    let v = parse_selector_json(text).expect("string braces should be skipped");
    assert_eq!(v["answer"], json!("uses format!(\"{}\", x) here"));
}

#[test]
fn garbage_returns_none() {
    assert!(parse_selector_json("I could not complete that request.").is_none());
    assert!(parse_selector_json("").is_none());
    assert!(parse_selector_json("{ unterminated").is_none());
    // A JSON array is not a selector object.
    assert!(parse_selector_json("[1, 2, 3]").is_none());
}

// ── validate_ranges ──────────────────────────────────────────────────

#[test]
fn valid_ranges_survive_with_answer() {
    let v = json!({
        "ranges": [{"start": 210, "end": 238, "why": "backoff computation", "score": 3}],
        "answer": "Exponential base-2 capped at 30s.",
        "not_found": false
    });
    let sel = validate_ranges(&v, 2140);
    assert_eq!(sel.dropped_invalid, 0);
    assert_eq!(sel.ranges.len(), 1);
    assert_eq!(sel.ranges[0].start, 210);
    assert_eq!(sel.ranges[0].end, 238);
    assert_eq!(sel.ranges[0].score, 3);
    assert_eq!(sel.answer.as_deref(), Some("Exponential base-2 capped at 30s."));
    assert!(!sel.not_found);
}

#[test]
fn out_of_bounds_ranges_are_dropped_or_clamped() {
    let v = json!({"ranges": [
        {"start": 5000, "end": 5010, "score": 3},   // wholly past EOF — dropped
        {"start": 90,   "end": 500,  "score": 2},   // end past EOF — clamped
        {"start": 0,    "end": 3,    "score": 1}    // start below 1 — clamped
    ]});
    let sel = validate_ranges(&v, 100);
    assert_eq!(sel.dropped_invalid, 1, "the past-EOF range must be dropped, not clamped");
    let mut spans: Vec<(usize, usize)> = sel.ranges.iter().map(|r| (r.start, r.end)).collect();
    spans.sort_unstable();
    assert_eq!(spans, vec![(1, 3), (90, 100)]);
}

#[test]
fn inverted_ranges_are_dropped() {
    let v = json!({"ranges": [
        {"start": 80, "end": 20, "score": 3},
        {"start": 10, "end": 12, "score": 1}
    ]});
    let sel = validate_ranges(&v, 100);
    assert_eq!(sel.dropped_invalid, 1);
    assert_eq!(sel.ranges.len(), 1);
    assert_eq!((sel.ranges[0].start, sel.ranges[0].end), (10, 12));
}

#[test]
fn malformed_range_entries_are_dropped() {
    let v = json!({"ranges": [
        {"start": 10},                       // no end
        {"end": 20},                         // no start
        "not an object",
        {"start": "ten", "end": "twenty"},   // wrong types
        {"start": 30, "end": 32}             // the only good one
    ]});
    let sel = validate_ranges(&v, 100);
    assert_eq!(sel.dropped_invalid, 4);
    assert_eq!(sel.ranges.len(), 1);
}

#[test]
fn overlapping_and_adjacent_ranges_merge() {
    let v = json!({"ranges": [
        {"start": 10, "end": 20, "why": "a", "score": 1},
        {"start": 15, "end": 25, "why": "b", "score": 3},   // overlaps  -> merges
        {"start": 28, "end": 30, "why": "c", "score": 2},   // gap of 2  -> merges
        {"start": 40, "end": 45, "why": "d", "score": 2}    // gap of 9  -> separate
    ]});
    let sel = validate_ranges(&v, 100);
    assert_eq!(sel.ranges.len(), 2, "got {:?}", sel.ranges);
    // Highest score first: the merged 10-30 block inherits score 3 and label "b".
    assert_eq!((sel.ranges[0].start, sel.ranges[0].end), (10, 30));
    assert_eq!(sel.ranges[0].score, 3);
    assert_eq!(sel.ranges[0].why, "b");
    assert_eq!((sel.ranges[1].start, sel.ranges[1].end), (40, 45));
}

#[test]
fn ranges_exactly_merge_gap_apart_merge_but_one_more_does_not() {
    // [10,20] then [24,26]: gap = 3 (lines 21,22,23) -> merge.
    let merged =
        validate_ranges(&json!({"ranges": [{"start": 10, "end": 20}, {"start": 24, "end": 26}]}), 100);
    assert_eq!(merged.ranges.len(), 1);
    // [10,20] then [25,27]: gap = 4 -> separate.
    let split =
        validate_ranges(&json!({"ranges": [{"start": 10, "end": 20}, {"start": 25, "end": 27}]}), 100);
    assert_eq!(split.ranges.len(), 2);
}

#[test]
fn ranges_sort_by_score_then_position() {
    let v = json!({"ranges": [
        {"start": 80, "end": 82, "score": 1},
        {"start": 60, "end": 62, "score": 3},
        {"start": 20, "end": 22, "score": 3}
    ]});
    let sel = validate_ranges(&v, 100);
    let order: Vec<usize> = sel.ranges.iter().map(|r| r.start).collect();
    assert_eq!(order, vec![20, 60, 80]);
}

#[test]
fn not_found_with_empty_ranges_is_preserved() {
    let sel = validate_ranges(&json!({"ranges": [], "not_found": true, "answer": null}), 500);
    assert!(sel.not_found);
    assert!(sel.ranges.is_empty());
    assert_eq!(sel.answer, None);
    assert_eq!(sel.dropped_invalid, 0);
}

#[test]
fn empty_file_drops_everything() {
    let sel = validate_ranges(&json!({"ranges": [{"start": 1, "end": 1}]}), 0);
    assert!(sel.ranges.is_empty());
    assert_eq!(sel.dropped_invalid, 1);
}

#[test]
fn scores_are_clamped_and_why_is_capped() {
    let long_why = "x".repeat(500);
    let v = json!({"ranges": [{"start": 1, "end": 2, "why": long_why, "score": 99}]});
    let sel = validate_ranges(&v, 10);
    assert_eq!(sel.ranges[0].score, 3);
    assert_eq!(sel.ranges[0].why.chars().count(), MAX_WHY_LEN);
}

// ── apply_line_budget ────────────────────────────────────────────────

fn range(start: usize, end: usize, score: i64) -> SelectedRange {
    SelectedRange { start, end, why: String::new(), score }
}

#[test]
fn budget_drops_lowest_score_ranges_first() {
    let ranges = vec![range(10, 39, 3), range(100, 129, 2), range(200, 229, 1)];
    let (kept, dropped) = apply_line_budget(ranges, 60);
    assert_eq!(dropped, 1);
    assert_eq!(kept.len(), 2);
    // Output is reading order, not score order.
    assert_eq!(kept[0].start, 10);
    assert_eq!(kept[1].start, 100);
}

#[test]
fn budget_truncates_rather_than_returning_nothing() {
    let (kept, dropped) = apply_line_budget(vec![range(10, 400, 3)], 50);
    assert_eq!(dropped, 0);
    assert_eq!(kept.len(), 1);
    assert_eq!((kept[0].start, kept[0].end), (10, 59));
    assert_eq!(kept[0].len(), 50);
}

#[test]
fn budget_keeps_everything_when_it_fits() {
    let (kept, dropped) = apply_line_budget(vec![range(1, 5, 3), range(20, 25, 2)], 1000);
    assert_eq!(dropped, 0);
    assert_eq!(kept.len(), 2);
}

// ── validate_keeps ───────────────────────────────────────────────────

#[test]
fn valid_keeps_survive() {
    let v = json!({"keep": [
        {"id": 17, "why": "return value discarded", "score": 3},
        {"id": 4,  "why": "same",                   "score": 3},
        {"id": 9,  "why": "weaker",                 "score": 1}
    ], "none_relevant": false});
    let sel = validate_keeps(&v, 1..=57);
    assert_eq!(sel.dropped_invalid, 0);
    let ids: Vec<usize> = sel.keeps.iter().map(|k| k.id).collect();
    assert_eq!(ids, vec![4, 17, 9], "score desc, then id asc");
    assert!(!sel.none_relevant);
}

#[test]
fn nonexistent_ids_are_dropped() {
    let v = json!({"keep": [{"id": 999}, {"id": 0}, {"id": 3}, {"id": "x"}]});
    let sel = validate_keeps(&v, 1..=10);
    assert_eq!(sel.dropped_invalid, 3, "999 out of range, 0 is not 1-based, 'x' malformed");
    assert_eq!(sel.keeps.len(), 1);
    assert_eq!(sel.keeps[0].id, 3);
}

#[test]
fn duplicate_ids_collapse_to_highest_score() {
    let v = json!({"keep": [
        {"id": 5, "why": "first",  "score": 1},
        {"id": 5, "why": "better", "score": 3}
    ]});
    let sel = validate_keeps(&v, 1..=10);
    assert_eq!(sel.keeps.len(), 1);
    assert_eq!(sel.keeps[0].score, 3);
    assert_eq!(sel.keeps[0].why, "better");
    assert_eq!(sel.dropped_invalid, 1, "the duplicate is counted as a drop");
}

#[test]
fn empty_keep_with_none_relevant_is_preserved() {
    let sel = validate_keeps(&json!({"keep": [], "none_relevant": true}), 1..=57);
    assert!(sel.keeps.is_empty());
    assert!(sel.none_relevant);
    assert_eq!(sel.dropped_invalid, 0);
}

#[test]
fn missing_keep_array_is_not_a_panic() {
    let sel = validate_keeps(&json!({"none_relevant": false}), 1..=57);
    assert!(sel.keeps.is_empty());
    assert!(!sel.none_relevant);
}

#[test]
fn ids_outside_the_batch_window_are_dropped() {
    // Batch 2 covers ids 251..=500.  Id 17 names a real hit — but one from
    // batch 1, whose content this reply never saw.  Hallucination: dropped.
    let v = json!({"keep": [
        {"id": 17,  "why": "not mine", "score": 3},
        {"id": 260, "why": "mine",     "score": 2}
    ]});
    let sel = validate_keeps(&v, 251..=500);
    assert_eq!(sel.dropped_invalid, 1, "cross-batch id must not pass validation");
    assert_eq!(sel.keeps.len(), 1);
    assert_eq!(sel.keeps[0].id, 260);
}

// ── Fail-open error ──────────────────────────────────────────────────

#[test]
fn tool_error_names_the_fallback_tool() {
    let e = ToolError::new("scout grep: local LLM exploded", "the Grep tool for the unfiltered hit list");
    let text = e.text();
    assert!(text.contains("Grep tool"), "fallback must be named: {text}");
    assert!(text.contains("fall back to"), "text: {text}");
}

// ── Ctx ──────────────────────────────────────────────────────────────

#[test]
fn require_client_explains_a_missing_config() {
    let ctx = Ctx {
        client_error: Some("cannot read config \"/nope/config.toml\"".into()),
        project: ".".into(),
        ledger: crate::stats::Ledger::silent(),
        ..Default::default()
    };
    let e = ctx.require_client().err().expect("no client configured");
    assert!(e.contains("not configured"), "{e}");
    assert!(e.contains("/nope/config.toml"), "the underlying reason must survive: {e}");
}

#[test]
fn unknown_preset_is_an_error_not_a_panic() {
    let ctx =
        Ctx { project: ".".into(), ledger: crate::stats::Ledger::silent(), ..Default::default() };
    assert!(ctx.preset("extract").unwrap_err().contains("not found"));
}
