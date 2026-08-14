//! Tests for `extract`: bypass path, numbering/chunking, materialization and
//! the fail-open contract.
//!
//! Ported from ct's `local_extract/tests.rs`.  The daemon-response tests
//! (`read_response_lines`) became filesystem tests — scout reads files itself,
//! so the equivalent ground truth is a real file in a tempdir.  The fail-open
//! tests call `run` directly instead of going through ct's MCP request loop.

use super::*;
use crate::select::Ctx;

fn lines(n: usize) -> Vec<String> {
    (1..=n).map(|i| format!("line {i}")).collect()
}

/// A context with no LLM behind it: enough for the bypass path and for every
/// failure path that never reaches the model.
fn offline_ctx(project: &str) -> Ctx<'static> {
    Ctx {
        client_error: Some("no config in tests".into()),
        project: project.to_string(),
        // A test must never append to the developer's own call log.
        ledger: crate::stats::Ledger::silent(),
        ..Default::default()
    }
}

// ── Numbering / chunking ─────────────────────────────────────────────

#[test]
fn numbering_matches_the_read_rendering() {
    // Six-wide right-aligned number, arrow, text — the form the model is told
    // to copy line numbers out of.
    let out = render_numbered(&lines(3), 1);
    assert_eq!(out, "     1\u{2192}line 1\n     2\u{2192}line 2\n     3\u{2192}line 3\n");
}

#[test]
fn numbering_honours_the_first_line_offset() {
    let out = render_numbered(&["fn main() {".to_string()], 210);
    assert_eq!(out, "   210\u{2192}fn main() {\n");
}

#[test]
fn small_file_yields_one_chunk_with_absolute_numbers() {
    let chunks = chunk_numbered(&lines(5), 393_216);
    assert_eq!(chunks.len(), 1);
    assert!(chunks[0].starts_with("     1\u{2192}line 1\n"));
    assert!(chunks[0].ends_with("     5\u{2192}line 5\n"));
}

#[test]
fn chunking_splits_on_line_boundaries_and_keeps_numbers_absolute() {
    // Tiny chunk_bytes forces a split; every rendered line is ~14 bytes.
    let chunks = chunk_numbered(&lines(10), 40);
    assert!(chunks.len() > 1, "expected a split, got {} chunk(s)", chunks.len());
    let rejoined: String = chunks.concat();
    assert_eq!(rejoined, render_numbered(&lines(10), 1), "chunking must be lossless");
    for c in &chunks {
        assert!(c.ends_with('\n'), "chunks must end on a line boundary: {c:?}");
    }
    // Line numbers stay absolute so chunk results merge by concatenation.
    assert!(chunks.last().unwrap().contains("    10\u{2192}line 10"));
}

#[test]
fn empty_file_yields_one_empty_chunk() {
    let chunks = chunk_numbered(&[], 1024);
    assert_eq!(chunks, vec![String::new()]);
}

// ── Bypass path ──────────────────────────────────────────────────────

#[test]
fn bypass_returns_the_whole_file_in_full_mode() {
    let payload = bypass_payload("src/lib.rs", &lines(12));
    assert_eq!(payload["mode"], "full");
    assert_eq!(payload["file"], "src/lib.rs");
    assert_eq!(payload["file_lines"], 12);
    assert_eq!(payload["coverage"]["returned_lines"], 12);
    assert_eq!(payload["not_found"], false);
    let content = payload["content"].as_str().unwrap();
    // Every source line is present, numbered.
    assert_eq!(content, render_numbered(&lines(12), 1));
    assert_eq!(content.lines().count(), 12);
    // No LLM ran, so the hint says so rather than pointing at a fallback.
    assert!(payload["hint"].as_str().unwrap().contains("no filtering"));
}

#[test]
fn bypass_payload_handles_an_empty_file() {
    let payload = bypass_payload("empty.rs", &[]);
    assert_eq!(payload["file_lines"], 0);
    assert_eq!(payload["content"], "");
}

#[test]
fn a_small_real_file_bypasses_the_llm_entirely() {
    // The end-to-end bypass: no config, no endpoint, still a full answer.
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("small.rs"), "fn a() {}\nfn b() {}\n").unwrap();
    let ctx = offline_ctx(&dir.path().to_string_lossy());
    let payload = run(&ctx, &serde_json::json!({"file": "small.rs", "question": "what is here?"}))
        .expect("small files must not need the model");
    assert_eq!(payload["mode"], "full");
    assert_eq!(payload["file"], "small.rs");
    assert_eq!(payload["file_lines"], 2);
}

// ── Fail-open contract ───────────────────────────────────────────────

/// Every failure must name the Read tool.
fn assert_fails_open(err: &crate::select::ToolError) -> String {
    let text = err.text();
    assert!(text.contains("Read tool"), "fallback tool must be named, got: {text}");
    text
}

#[test]
fn missing_file_arg_fails_open() {
    let ctx = offline_ctx(".");
    let err = run(&ctx, &serde_json::json!({"question": "where is the retry loop?"})).unwrap_err();
    let text = assert_fails_open(&err);
    assert!(text.contains("'file'"), "text: {text}");
}

#[test]
fn missing_question_arg_fails_open() {
    let ctx = offline_ctx(".");
    let err = run(&ctx, &serde_json::json!({"file": "src/lib.rs", "question": "  "})).unwrap_err();
    let text = assert_fails_open(&err);
    assert!(text.contains("'question'"), "text: {text}");
}

#[test]
fn unreadable_file_fails_open_naming_the_read_tool() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = offline_ctx(&dir.path().to_string_lossy());
    let err =
        run(&ctx, &serde_json::json!({"file": "nope.rs", "question": "anything?"})).unwrap_err();
    let text = assert_fails_open(&err);
    assert!(text.contains("could not read"), "text: {text}");
}

#[test]
fn a_large_file_without_a_configured_llm_fails_open() {
    // Past the bypass threshold the model is required — and its absence must
    // read as a configuration problem, not a mystery.
    let dir = tempfile::tempdir().unwrap();
    let body: String = (1..=500).map(|i| format!("line {i}\n")).collect();
    std::fs::write(dir.path().join("big.rs"), body).unwrap();
    let ctx = offline_ctx(&dir.path().to_string_lossy());
    let err = run(&ctx, &serde_json::json!({"file": "big.rs", "question": "where?"})).unwrap_err();
    let text = assert_fails_open(&err);
    assert!(text.contains("not configured"), "text: {text}");
}

#[test]
fn garbage_llm_output_produces_no_selection() {
    // The handler treats an unparseable reply as an LLM failure; this asserts
    // the predicate it keys on, without needing a live endpoint.
    assert!(crate::select::parse_selector_json("sorry, I can't do that").is_none());
}

// ── Materialization clamping ─────────────────────────────────────────

fn sel_range(start: usize, end: usize, score: i64) -> SelectedRange {
    SelectedRange { start, end, why: format!("{start}-{end}"), score }
}

#[test]
fn materialize_clamps_out_of_range_ends_to_the_file() {
    let ls = lines(10);
    let v = materialize(&ls, &sel_range(8, 99, 2));
    let text = v["text"].as_str().unwrap();
    assert!(text.contains("     8\u{2192}line 8"), "text: {text}");
    assert!(text.ends_with("    10\u{2192}line 10\n"), "clamped at EOF: {text}");
    assert!(!text.contains("line 11"));
}

#[test]
fn materialize_of_a_range_past_eof_is_empty_not_a_panic() {
    let v = materialize(&lines(10), &sel_range(50, 60, 1));
    assert_eq!(v["text"], "");
}

#[test]
fn materialize_handles_a_single_line_range() {
    let v = materialize(&lines(10), &sel_range(3, 3, 0));
    assert_eq!(v["text"].as_str().unwrap(), "     3\u{2192}line 3\n");
    assert_eq!(v["lines"], "3-3");
}

// ── Cross-chunk merging ──────────────────────────────────────────────

fn chunk_sel(
    ranges: Vec<SelectedRange>,
    answer: Option<&str>,
    not_found: bool,
    dropped_invalid: usize,
) -> RangeSelection {
    RangeSelection { ranges, answer: answer.map(str::to_string), not_found, dropped_invalid }
}

#[test]
fn merge_selections_single_chunk_passes_through() {
    let merged = merge_selections(vec![chunk_sel(vec![sel_range(5, 9, 1)], Some("a"), false, 4)], 100);
    assert_eq!(merged.ranges.len(), 1);
    assert_eq!(merged.dropped_invalid, 4);
    assert_eq!(merged.answer.as_deref(), Some("a"));
}

#[test]
fn merge_selections_joins_answers_and_merges_cross_chunk_neighbours() {
    // Chunk 1 ends at line 100, chunk 2 starts at 101: the ranges sit within
    // MERGE_GAP of each other and must merge across the chunk boundary.
    let a = chunk_sel(vec![sel_range(90, 100, 2)], Some("first"), false, 1);
    let b = chunk_sel(vec![sel_range(101, 110, 3)], Some("second"), false, 2);
    let merged = merge_selections(vec![a, b], 500);
    assert_eq!(merged.ranges.len(), 1, "cross-chunk neighbours must merge");
    assert_eq!((merged.ranges[0].start, merged.ranges[0].end), (90, 110));
    assert_eq!(merged.answer.as_deref(), Some("first second"));
    assert!(!merged.not_found);
    assert_eq!(merged.dropped_invalid, 3, "per-chunk drop counts accumulate");
}

#[test]
fn merge_selections_not_found_is_an_and_across_chunks() {
    // One chunk finding the target beats another chunk's not_found...
    let found = chunk_sel(vec![sel_range(10, 12, 3)], Some("here"), false, 0);
    let empty = chunk_sel(vec![], None, true, 0);
    let merged = merge_selections(vec![empty, found], 100);
    assert!(!merged.not_found);
    assert_eq!(merged.answer.as_deref(), Some("here"));
    // ...and only unanimous not_found survives the merge.
    let all_empty = merge_selections(
        vec![chunk_sel(vec![], None, true, 0), chunk_sel(vec![], None, true, 0)],
        100,
    );
    assert!(all_empty.not_found);
    assert_eq!(all_empty.answer, None);
}

// ── not_found payload ────────────────────────────────────────────────

#[test]
fn not_found_payload_is_a_verdict_not_an_empty_file() {
    let p = not_found_payload("src/lib.rs", 900, "where is the retry loop?", Some("nothing here"));
    assert_eq!(p["not_found"], true);
    assert_eq!(p["file_lines"], 900);
    assert!(p["snippets"].as_array().unwrap().is_empty());
    let hint = p["hint"].as_str().unwrap();
    assert!(hint.contains("filter verdict"), "hint: {hint}");
}
