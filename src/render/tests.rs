//! Renderer tests — fixed payloads in, exact text out.
//!
//! The fixtures below are the three payload flavours `grep::run` produces,
//! trimmed to the fields the renderer reads.  They are written out literally
//! rather than generated so a change to the frozen payload contract shows up
//! here as a failing test rather than as a silently different fixture.

use super::*;
use serde_json::json;

fn plain() -> RenderOpts {
    RenderOpts { color: false, context_lines: 2, ..RenderOpts::default() }
}

/// `mode: "rerank"` — the LLM filtered, so each hit carries `why` and `score`.
fn rerank_payload() -> Value {
    json!({
        "mode": "rerank",
        "pattern": "none_relevant",
        "intent": "where is the verdict merged",
        "hits_total": 214,
        "returned": 1,
        "none_relevant": false,
        "hits": [{
            "file": "src/select.rs",
            "line": 212,
            "text": "    none_relevant &= sel.none_relevant;",
            "context": "    let sel = validate_keeps(&v, first_id..=last);\n    dropped_invalid += sel.dropped_invalid;\n    none_relevant &= sel.none_relevant;\n    keeps.extend(sel.keeps);\n}",
            "why": "validates keep-ids against the batch range",
            "score": 3
        }]
    })
}

/// `mode: "full"` with an intent — the short-list bypass.  No `why`.
fn bypass_payload() -> Value {
    json!({
        "mode": "full",
        "pattern": "fn fail",
        "intent": "the fail-open helper",
        "hits_total": 1,
        "returned": 1,
        "none_relevant": false,
        "hits": [{
            "file": "src/grep.rs",
            "line": 365,
            "text": "fn fail(reason: &str) -> ToolError {",
            "context": "/// Fail open, naming this filter's fallback.\n///\nfn fail(reason: &str) -> ToolError {\n    ToolError::new(msg, FALLBACK)\n}"
        }]
    })
}

/// `mode: "full"` with `intent: null` — the unfiltered no-intent path.
fn unfiltered_payload() -> Value {
    json!({
        "mode": "full",
        "pattern": "fn fail",
        "intent": Value::Null,
        "hits_total": 2,
        "returned": 2,
        "none_relevant": false,
        "hits": [
            {
                "file": "src/grep.rs",
                "line": 1,
                "text": "fn fail(a: usize) {",
                "context": "fn fail(a: usize) {\n    todo!()\n}"
            },
            {
                "file": "src/extract.rs",
                "line": 98,
                "text": "fn fail(b: usize) {",
                "context": "// above\n\nfn fail(b: usize) {\n    todo!()\n}"
            }
        ]
    })
}

// ── Human format ─────────────────────────────────────────────────────

#[test]
fn rerank_hit_renders_header_why_and_gutter() {
    let out = render_human(&rerank_payload(), &plain());
    let expected = concat!(
        "src/select.rs:212 · validates keep-ids against the batch range\n",
        "  210 │     let sel = validate_keeps(&v, first_id..=last);\n",
        "  211 │     dropped_invalid += sel.dropped_invalid;\n",
        "▶ 212 │     none_relevant &= sel.none_relevant;\n",
        "  213 │     keeps.extend(sel.keeps);\n",
        "  214 │ }\n",
    );
    assert_eq!(out, expected);
}

#[test]
fn score_is_never_shown() {
    // SPEC §9: the score orders hits and stays in the payload, but a human
    // reading grep output does not want to see it.  The exact-output test
    // above pins the rest; this pins the intent.
    let mut payload = rerank_payload();
    payload["hits"][0]["score"] = json!(7);
    let out = render_human(&payload, &plain());
    assert!(!out.contains("score"), "score label leaked:\n{out}");
    assert!(!out.contains('7'), "score value leaked into human output:\n{out}");
}

#[test]
fn bypass_hit_has_no_why_segment() {
    let out = render_human(&bypass_payload(), &plain());
    assert!(out.starts_with("src/grep.rs:365\n"), "unexpected header:\n{out}");
    assert!(!out.contains('·'), "bypass mode must not render a why segment:\n{out}");
    // Context 2 puts the block's first line two above the match.
    assert!(out.contains("  363 │ /// Fail open"), "{out}");
    assert!(out.contains("▶ 365 │ fn fail"), "{out}");
}

#[test]
fn unfiltered_payload_renders_like_the_others() {
    let out = render_human(&unfiltered_payload(), &plain());
    assert!(!out.contains('·'), "no-intent mode has no why segment:\n{out}");
    // Hit 1 is at the top of the file: no leading context, numbering from 1.
    assert!(out.starts_with("src/grep.rs:1\n▶ 1 │ fn fail(a: usize) {\n"), "{out}");
    assert!(out.contains("src/extract.rs:98\n"), "{out}");
}

#[test]
fn hits_are_separated_by_a_blank_line() {
    let out = render_human(&unfiltered_payload(), &plain());
    assert!(out.contains("\n\nsrc/extract.rs:98\n"), "missing blank separator:\n{out}");
    assert!(!out.starts_with('\n'), "no leading blank line:\n{out}");
    assert!(!out.ends_with("\n\n"), "no trailing blank line:\n{out}");
}

#[test]
fn top_of_file_block_starts_at_line_one() {
    // A match on line 2 has exactly one line above it, even with context 2.
    let payload = json!({
        "mode": "full", "hits": [{
            "file": "a.rs", "line": 2, "text": "b", "context": "a\nb\nc\nd"
        }]
    });
    let out = render_human(&payload, &plain());
    assert_eq!(out, "a.rs:2\n  1 │ a\n▶ 2 │ b\n  3 │ c\n  4 │ d\n");
}

#[test]
fn truncation_marker_gets_no_line_number() {
    // `source::extract_context` appends this itself — it is not file text, so
    // numbering it would claim a line that does not exist.
    let payload = json!({
        "mode": "full", "hits": [{
            "file": "min.json", "line": 9, "text": Value::Null,
            "context": "aaaa\n... (truncated)"
        }]
    });
    let out = render_human(&payload, &plain());
    assert_eq!(out, "min.json:9\n  7 │ aaaa\n    │ ... (truncated)\n");
}

#[test]
fn context_lines_option_shifts_the_gutter_origin() {
    // The payload never says how wide the block is; the caller supplies the
    // `context_lines` the search actually ran with.
    let payload = json!({
        "mode": "full", "hits": [{
            "file": "a.rs", "line": 50, "text": "m", "context": "x\nm\ny"
        }]
    });
    let out = render_human(&payload, &RenderOpts { context_lines: 1, ..plain() });
    assert_eq!(out, "a.rs:50\n  49 │ x\n▶ 50 │ m\n  51 │ y\n");
}

#[test]
fn empty_hit_list_renders_nothing() {
    let payload = json!({"mode": "rerank", "hits": [], "none_relevant": true});
    assert_eq!(render_human(&payload, &plain()), "");
    assert_eq!(render_vimgrep(&payload), "");
    // A payload with no `hits` key at all must not panic either.
    assert_eq!(render_human(&json!({"mode": "rerank"}), &plain()), "");
    assert_eq!(render_vimgrep(&json!({"mode": "rerank"})), "");
}

// ── Colour ───────────────────────────────────────────────────────────

#[test]
fn plain_output_carries_no_escapes() {
    let out = render_human(&rerank_payload(), &plain());
    assert!(!out.contains('\x1b'), "ANSI leaked into uncoloured output:\n{out:?}");
}

#[test]
fn coloured_output_uses_the_ack_palette() {
    let out = render_human(&rerank_payload(), &RenderOpts { color: true, ..plain() });
    // path magenta, line number green (ack's scheme).
    assert!(out.starts_with("\x1b[35msrc/select.rs\x1b[0m:\x1b[32m212\x1b[0m ·"), "{out:?}");
    // The matched line — and only the matched line — is bold.
    assert!(out.contains("\x1b[1m    none_relevant &= sel.none_relevant;\x1b[0m"), "{out:?}");
    assert_eq!(out.matches("\x1b[1m").count(), 1, "only the match is bold:\n{out:?}");
    // Gutter is dim on every line of the block.
    assert_eq!(out.matches("\x1b[2m").count(), 5, "{out:?}");
}

// ── Match highlighting (SPEC-cli §2) ─────────────────────────────────

/// A one-line hit whose match sits at `[col, col_end)`.
fn span_payload(text: &str, col: usize, col_end: usize) -> Value {
    json!({
        "mode": "full", "hits": [{
            "file": "a.rs", "line": 1, "text": text, "context": text,
            "col": col, "col_end": col_end
        }]
    })
}

/// Everything after the gutter on the (single) rendered line.
fn body(out: &str) -> String {
    out.lines().nth(1).expect("a context line").to_string()
}

#[test]
fn the_matched_pattern_is_highlighted_inside_the_matched_line() {
    let out = render_human(&span_payload("abcNEEDLEdef", 3, 9), &RenderOpts { color: true, ..plain() });
    // Bold line, bold-red match, bold resumed after it — so the span never
    // reads as lighter than the line it sits in.
    assert!(
        out.contains("\x1b[1mabc\x1b[1;31mNEEDLE\x1b[0m\x1b[1mdef\x1b[0m"),
        "{out:?}"
    );
}

#[test]
fn highlighting_never_leaks_into_uncoloured_output() {
    let out = render_human(&span_payload("abcNEEDLEdef", 3, 9), &plain());
    assert_eq!(out, "a.rs:1\n▶ 1 │ abcNEEDLEdef\n");
}

#[test]
fn a_hit_with_no_column_still_bolds_its_matched_line() {
    // Pre-P3 payloads, and hits whose matched line the context budget cut,
    // carry no span.  They are still the matched line.
    let payload = json!({
        "mode": "full",
        "hits": [{"file": "a.rs", "line": 1, "text": "abc", "context": "abc", "col": Value::Null}]
    });
    let out = render_human(&payload, &RenderOpts { color: true, ..plain() });
    assert!(out.contains("\x1b[1mabc\x1b[0m"), "{out:?}");
    assert!(!out.contains("\x1b[1;31m"), "nothing to highlight:\n{out:?}");
}

#[test]
fn an_empty_span_is_not_highlighted() {
    // `col_end == col` is what the search layer records when it cannot
    // re-locate the match; painting a zero-width span would emit stray escapes.
    let out = render_human(&span_payload("abc", 0, 0), &RenderOpts { color: true, ..plain() });
    assert!(!out.contains("\x1b[1;31m"), "{out:?}");
    assert!(out.contains("\x1b[1mabc\x1b[0m"), "{out:?}");
}

#[test]
fn a_span_past_the_end_of_the_line_neither_panics_nor_paints() {
    // Routine, not corrupt: the payload's column indexes the file's line, and
    // `text` may be only the prefix of it that fit the context budget.
    let out = render_human(&span_payload("abc", 40, 46), &RenderOpts { color: true, ..plain() });
    assert!(out.contains("\x1b[1mabc\x1b[0m"), "{out:?}");
    assert!(!out.contains("\x1b[1;31m"), "{out:?}");
}

#[test]
fn a_mid_codepoint_span_cannot_provoke_a_panic() {
    // The renderer is a pure function over a payload it does not author, so it
    // floors both ends onto char boundaries before slicing anything.
    for (a, b) in [(1usize, 3usize), (0, 1), (3, 5), (1, 6)] {
        let payload = span_payload("ééé", a, b);
        // Colouring is where the sub-slices happen; a bad boundary panics here.
        let coloured = render_human(&payload, &RenderOpts { color: true, ..plain() });
        assert!(coloured.contains('é'), "({a},{b}) lost the line: {coloured:?}");
        // The text itself is never altered, only wrapped.
        assert_eq!(render_human(&payload, &plain()), "a.rs:1\n▶ 1 │ ééé\n", "at ({a},{b})");
    }
}

#[test]
fn only_the_matched_line_of_a_block_is_highlighted() {
    let payload = json!({
        "mode": "full", "hits": [{
            "file": "a.rs", "line": 3, "text": "b needle b",
            "context": "a needle a\nb needle b\nc needle c",
            "col": 2, "col_end": 8
        }]
    });
    let out = render_human(&payload, &RenderOpts { color: true, context_lines: 1, max_columns: 150 });
    assert_eq!(out.matches("\x1b[1;31m").count(), 1, "neighbours are not matches:\n{out:?}");
}

// ── Column cap: windowing (SPEC-cli §4) ──────────────────────────────

fn capped(n: usize) -> RenderOpts {
    RenderOpts { color: false, context_lines: 2, max_columns: n }
}

#[test]
fn a_short_line_is_never_windowed_or_annotated() {
    let out = render_human(&span_payload("abcNEEDLEdef", 3, 9), &capped(150));
    assert_eq!(out, "a.rs:1\n▶ 1 │ abcNEEDLEdef\n");
}

#[test]
fn an_over_long_matched_line_windows_around_its_match() {
    let text = format!("{}NEEDLE{}", "a".repeat(100), "b".repeat(100));
    let out = render_human(&span_payload(&text, 100, 106), &capped(20));
    // 20 columns centred on the match: 7 a's, the match, 7 b's, ellipsised
    // on both sides, with the real width named at the end.
    assert_eq!(body(&out), "▶ 1 │ …aaaaaaaNEEDLEbbbbbbb… [line is 206 columns]");
}

#[test]
fn a_match_at_the_start_of_a_line_is_ellipsised_on_one_side_only() {
    let text = format!("NEEDLE{}", "b".repeat(200));
    let out = render_human(&span_payload(&text, 0, 6), &capped(20));
    assert_eq!(body(&out), "▶ 1 │ NEEDLEbbbbbbbbbbbbbb… [line is 206 columns]");
}

#[test]
fn a_match_at_the_end_of_a_line_is_ellipsised_on_one_side_only() {
    let text = format!("{}NEEDLE", "a".repeat(200));
    let out = render_human(&span_payload(&text, 200, 206), &capped(20));
    assert_eq!(body(&out), "▶ 1 │ …aaaaaaaaaaaaaaNEEDLE [line is 206 columns]");
}

#[test]
fn a_match_wider_than_the_cap_is_shown_from_its_own_start() {
    // Centring on a match that cannot fit would show its middle, which
    // identifies nothing; the head of a match is the part worth reading.
    let text = format!("{}{}{}", "a".repeat(10), "X".repeat(100), "b".repeat(10));
    let out = render_human(&span_payload(&text, 10, 110), &capped(20));
    assert_eq!(body(&out), format!("▶ 1 │ …{}… [line is 120 columns]", "X".repeat(20)));
}

#[test]
fn a_cap_smaller_than_the_match_still_highlights_what_is_visible() {
    let text = format!("{}{}{}", "a".repeat(10), "X".repeat(100), "b".repeat(10));
    let out = render_human(&span_payload(&text, 10, 110), &RenderOpts { color: true, ..capped(20) });
    // The whole window is match, so the highlight has no unhighlighted
    // neighbours inside it — but it is still painted.
    assert!(out.contains(&format!("\x1b[1;31m{}\x1b[0m", "X".repeat(20))), "{out:?}");
}

#[test]
fn the_highlight_survives_the_window_slice() {
    let text = format!("{}NEEDLE{}", "a".repeat(100), "b".repeat(100));
    let out = render_human(&span_payload(&text, 100, 106), &RenderOpts { color: true, ..capped(20) });
    // Byte offsets 100..106 in the *line* become 7..13 in the window.
    assert!(
        out.contains("\x1b[1maaaaaaa\x1b[1;31mNEEDLE\x1b[0m\x1b[1mbbbbbbb\x1b[0m"),
        "{out:?}"
    );
}

#[test]
fn max_columns_zero_is_unlimited() {
    let text = format!("{}NEEDLE{}", "a".repeat(100), "b".repeat(100));
    let out = render_human(&span_payload(&text, 100, 106), &capped(0));
    assert_eq!(body(&out), format!("▶ 1 │ {text}"));
    assert!(!out.contains('…') && !out.contains("columns]"), "{out}");
}

#[test]
fn an_over_long_context_line_is_cut_at_the_cap_rather_than_windowed() {
    // Neighbours have no match to centre on, so they show their head — the
    // only part whose position a reader can reason about (SPEC §4).
    let payload = json!({
        "mode": "full", "hits": [{
            "file": "a.rs", "line": 2, "text": "short",
            "context": format!("{}\nshort\n{}", "a".repeat(60), "b".repeat(60)),
            "col": 0, "col_end": 5
        }]
    });
    let out = render_human(&payload, &RenderOpts { context_lines: 1, ..capped(20) });
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines[1], format!("  1 │ {}… [line is 60 columns]", "a".repeat(20)));
    assert_eq!(lines[2], "▶ 2 │ short");
    assert_eq!(lines[3], format!("  3 │ {}… [line is 60 columns]", "b".repeat(20)));
}

#[test]
fn an_over_long_matched_line_with_no_span_falls_back_to_the_head() {
    let payload = json!({
        "mode": "full",
        "hits": [{"file": "a.rs", "line": 1, "text": "x", "context": "a".repeat(60)}]
    });
    let out = render_human(&payload, &capped(20));
    assert_eq!(body(&out), format!("▶ 1 │ {}… [line is 60 columns]", "a".repeat(20)));
}

#[test]
fn the_truncation_marker_is_never_capped() {
    // It is the renderer's own text, not a file line: capping it would report
    // a column count no line in the file has.
    let payload = json!({
        "mode": "full", "hits": [{
            "file": "min.json", "line": 9, "text": Value::Null,
            "context": "aaaa\n... (truncated)"
        }]
    });
    let out = render_human(&payload, &capped(5));
    assert!(out.ends_with("    │ ... (truncated)\n"), "{out:?}");
}

#[test]
fn windowing_never_splits_a_codepoint() {
    // A minified line is very often UTF-8; an odd cap makes every boundary
    // decision land mid-character if the arithmetic is naive.
    let text = format!("{}NEEDLE{}", "é".repeat(80), "é".repeat(80));
    let out = render_human(&span_payload(&text, 160, 166), &capped(21));
    let shown = body(&out);
    assert!(shown.contains("NEEDLE"), "the match must survive the window: {shown}");
    assert!(shown.contains('…'), "{shown}");
    assert!(shown.ends_with("[line is 326 columns]"), "{shown}");
    // Nothing but whole "é"s either side of the match.
    let core = shown.trim_start_matches("▶ 1 │ ").trim_start_matches('…');
    let core = core.split(" [line").next().unwrap().trim_end_matches('…');
    assert!(core.chars().all(|c| c == 'é' || "NEEDLE".contains(c)), "{core:?}");
}

#[test]
fn the_column_count_is_thousands_separated() {
    let text = "x".repeat(48_213);
    let out = render_human(&span_payload(&text, 0, 1), &capped(20));
    assert!(body(&out).ends_with("[line is 48,213 columns]"), "{}", body(&out));
    assert_eq!(with_thousands(0), "0");
    assert_eq!(with_thousands(999), "999");
    assert_eq!(with_thousands(1_000), "1,000");
    assert_eq!(with_thousands(1_234_567), "1,234,567");
}

#[test]
fn the_note_is_dim_when_coloured() {
    let text = "x".repeat(300);
    let out = render_human(&span_payload(&text, 0, 1), &RenderOpts { color: true, ..capped(20) });
    assert!(out.contains("\x1b[2m [line is 300 columns]\x1b[0m"), "{out:?}");
}

#[test]
fn window_slides_inside_the_line_at_both_edges() {
    // The pure arithmetic, pinned directly: a centred window, then the two
    // edge cases where it has to slide back inside the line.
    assert_eq!(window(200, 20, 100, 106), (93, 113));
    assert_eq!(window(200, 20, 0, 6), (0, 20), "cannot start before the line");
    assert_eq!(window(200, 20, 194, 200), (180, 200), "cannot run past its end");
    assert_eq!(window(10, 20, 2, 4), (0, 10), "a line under the cap is whole");
    assert_eq!(window(200, 0, 100, 106), (0, 200), "cap 0 is unlimited");
    assert_eq!(window(200, 20, 50, 150), (50, 70), "a match wider than the cap starts it");
}

// ── vimgrep format ───────────────────────────────────────────────────

#[test]
fn vimgrep_emits_file_line_col_text() {
    let out = render_vimgrep(&unfiltered_payload());
    assert_eq!(
        out,
        "src/grep.rs:1:1: fn fail(a: usize) {\nsrc/extract.rs:98:1: fn fail(b: usize) {\n"
    );
}

#[test]
fn vimgrep_never_colours() {
    assert!(!render_vimgrep(&rerank_payload()).contains('\x1b'));
}

#[test]
fn vimgrep_emits_the_real_one_based_column() {
    // The payload's `col` is a 0-based byte offset; quickfix counts from 1,
    // and this formatter is the single place that conversion happens.
    let out = render_vimgrep(&span_payload("let needle = 1;", 4, 10));
    assert_eq!(out, "a.rs:1:5: let needle = 1;\n");
    // A match at the start of a line is column 1, never column 0.
    assert_eq!(render_vimgrep(&span_payload("needle", 0, 6)), "a.rs:1:1: needle\n");
}

#[test]
fn vimgrep_falls_back_to_column_one_without_a_column() {
    // Pre-P3 payloads and cut matched lines both land here; a quickfix entry
    // at column 1 still navigates to the right line.
    let payload = json!({
        "mode": "full",
        "hits": [{"file": "a.rs", "line": 7, "text": "x", "col": Value::Null}]
    });
    assert_eq!(render_vimgrep(&payload), "a.rs:7:1: x\n");
}

#[test]
fn vimgrep_reports_the_true_column_of_a_truncated_matched_line() {
    // The minified-JSON case P3 exists for: the context budget cut `text` at
    // 2 KB, but the match is at column 15,701 of the real file line and that is
    // where the editor must land.
    let payload = json!({
        "mode": "full", "hits": [{
            "file": "bundle.json", "line": 1, "text": "{\"pad_0\":\"xxx",
            "col": 15_700, "col_end": 15_706
        }]
    });
    assert_eq!(render_vimgrep(&payload), "bundle.json:1:15701: {\"pad_0\":\"xxx\n");
}

#[test]
fn vimgrep_ignores_the_column_cap() {
    // An editor parses these; a truncated line would misplace the column it
    // was just told to jump to.
    let text = "x".repeat(500);
    let out = render_vimgrep(&span_payload(&text, 0, 1));
    assert_eq!(out, format!("a.rs:1:1: {text}\n"));
}

#[test]
fn vimgrep_keeps_a_hit_whose_matched_line_was_cut() {
    // `text: null` means the context budget cut the block before the matched
    // line.  The hit is real, so the quickfix entry must survive — blank, but
    // navigable, and saying why it is blank.
    let payload = json!({
        "mode": "full",
        "hits": [{"file": "min.json", "line": 1, "text": Value::Null, "context": "..."}]
    });
    assert_eq!(render_vimgrep(&payload), "min.json:1:1: (matched line unavailable)\n");
}
