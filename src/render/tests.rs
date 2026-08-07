//! Renderer tests — fixed payloads in, exact text out.
//!
//! The fixtures below are the three payload flavours `grep::run` produces,
//! trimmed to the fields the renderer reads.  They are written out literally
//! rather than generated so a change to the frozen payload contract shows up
//! here as a failing test rather than as a silently different fixture.

use super::*;
use serde_json::json;

fn plain() -> RenderOpts {
    RenderOpts { color: false, context_lines: 2 }
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
    let out = render_human(&payload, &RenderOpts { color: false, context_lines: 1 });
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
    let out = render_human(&rerank_payload(), &RenderOpts { color: true, context_lines: 2 });
    // path magenta, line number green (ack's scheme).
    assert!(out.starts_with("\x1b[35msrc/select.rs\x1b[0m:\x1b[32m212\x1b[0m ·"), "{out:?}");
    // The matched line — and only the matched line — is bold.
    assert!(out.contains("\x1b[1m    none_relevant &= sel.none_relevant;\x1b[0m"), "{out:?}");
    assert_eq!(out.matches("\x1b[1m").count(), 1, "only the match is bold:\n{out:?}");
    // Gutter is dim on every line of the block.
    assert_eq!(out.matches("\x1b[2m").count(), 5, "{out:?}");
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
