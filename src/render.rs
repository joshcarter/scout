//! Terminal rendering for `scout grep` (docs/search-cli.md §2, §8).
//!
//! A pure function from the grep JSON payload — the *same* payload the MCP
//! tool returns, which is a frozen contract — to styled text.  Nothing here
//! reads the filesystem, the config, or the terminal: the caller resolves
//! colour and context width into `RenderOpts` and hands the payload over.
//! That keeps the whole module unit-testable against fixed payloads and keeps
//! the MCP server (which never calls it) entirely unaffected.
//!
//! Three payload flavours are handled, all with the same layout:
//!
//! * `mode: "rerank"` — the LLM filtered; each hit carries a `why`.
//! * `mode: "full"` with an `intent` — short-list bypass, no `why`.
//! * `mode: "full"` with `intent: null` — unfiltered search, no `why`.
//!
//! The 1–5 score is deliberately never shown (docs/search-cli.md §9): it orders the hits
//! and stays in the JSON payload, but it is noise at a terminal.
//!
//! Colour is raw ANSI — no dependency.  The palette is ack's: path magenta,
//! line number green, matched line bold, gutter dim, and — from P3 — the
//! matched *pattern* within that line bold red, ripgrep-style.
//!
//! P3 also adds the per-line column cap (`--max-columns`, docs/search-cli.md §4): an
//! over-long *matched* line renders as a window around its match, an over-long
//! *context* line is simply cut at the cap.  That is a terminal concern only —
//! `--format json` and `--format vimgrep` are untouched by it.

use serde_json::Value;

// ── ANSI ─────────────────────────────────────────────────────────────

const RESET: &str = "\x1b[0m";
const BOLD: &str = "\x1b[1m";
const DIM: &str = "\x1b[2m";
const GREEN: &str = "\x1b[32m";
const MAGENTA: &str = "\x1b[35m";
/// The matched *pattern* inside the matched line (docs/search-cli.md §2).  Bold red, as
/// ripgrep highlights matches: the whole matched line is already bold, so the
/// hue — not the weight — is what has to carry the distinction, and re-asserting
/// bold means the span never reads as *lighter* than its surroundings.
const MATCH_HL: &str = "\x1b[1;31m";

/// The gutter separator, and the marker on the matched line.
const GUTTER: char = '│';
const MARKER: char = '▶';
/// Stands in for the part of an over-long line the column cap cut away.
const ELLIPSIS: char = '…';

/// What `source::extract_context` appends when a block blows its byte budget.
/// It is the renderer's own marker, not file text, so it gets no line number.
const TRUNCATION_MARKER: &str = "... (truncated)";

// ── Options ──────────────────────────────────────────────────────────

/// Everything the renderer needs beyond the payload itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RenderOpts {
    /// Emit ANSI escapes.  Resolved by the caller from `--color`, `NO_COLOR`
    /// and `stdout().is_terminal()` — the renderer never probes the terminal.
    pub color: bool,
    /// The `context_lines` the *search* ran with.  The payload carries the
    /// rendered block but not its starting line number, so the gutter has to
    /// recompute it the way `source::extract_context` laid the block out.
    pub context_lines: usize,
    /// Per-line render cap, in bytes, ripgrep's `--max-columns` (docs/search-cli.md §4).
    /// `0` disables it.  Purely a terminal concern: `--format json` returns the
    /// payload untouched, and the payload itself is already bounded by the
    /// search layer's `context_max_bytes`.
    pub max_columns: usize,
    /// Prefix each hit's header with its 1-based index (`scout edit`'s picker,
    /// docs/search-cli.md §6).  Off for `grep`/`find`, whose output is not something you
    /// answer a prompt about.
    pub numbered: bool,
}

impl Default for RenderOpts {
    fn default() -> Self {
        RenderOpts { color: false, context_lines: 2, max_columns: 150, numbered: false }
    }
}

// ── Human format ─────────────────────────────────────────────────────

/// Render a grep payload as human-readable text (docs/search-cli.md §2).
///
/// Returns the empty string when there are no hits — the caller puts the
/// "why there are none" message on stderr, since that is metadata, not output.
pub fn render_human(payload: &Value, opts: &RenderOpts) -> String {
    let hits = payload.get("hits").and_then(Value::as_array);
    let Some(hits) = hits else { return String::new() };

    // Right-aligned to the widest index, so a 12-hit list's headers all start
    // in the same column instead of stepping right at hit 10.
    let width = hits.len().to_string().len();

    let mut out = String::new();
    for (i, hit) in hits.iter().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        render_hit(&mut out, hit, if opts.numbered { Some((i + 1, width)) } else { None }, opts);
    }
    out
}

/// One hit: header line, then its gutter-numbered context block.
///
/// `index` is `Some((n, width))` only for `scout edit`'s picker, where each hit
/// needs a name the caller can type back.
fn render_hit(out: &mut String, hit: &Value, index: Option<(usize, usize)>, opts: &RenderOpts) {
    let file = hit.get("file").and_then(Value::as_str).unwrap_or("<unknown>");
    let line = hit.get("line").and_then(Value::as_u64).unwrap_or(0) as usize;

    if let Some((n, width)) = index {
        let label = format!("{n:>width$}. ");
        if opts.color {
            out.push_str(&format!("{BOLD}{label}{RESET}"));
        } else {
            out.push_str(&label);
        }
    }
    if opts.color {
        out.push_str(&format!("{MAGENTA}{file}{RESET}:{GREEN}{line}{RESET}"));
    } else {
        out.push_str(&format!("{file}:{line}"));
    }
    // Rerank mode only; the two bypass paths have no model verdict to show.
    if let Some(why) =
        hit.get("why").and_then(Value::as_str).map(str::trim).filter(|w| !w.is_empty())
    {
        out.push_str(&format!(" · {why}"));
    }
    out.push('\n');

    let context = hit.get("context").and_then(Value::as_str).unwrap_or("");
    render_context(out, context, line, match_span(hit), opts);
}

/// The `[col, col_end)` byte range of the match within the matched line.
///
/// `None` for any hit that does not carry one — a `text: null` hit (the
/// context budget cut the line away), or a payload written before P3 existed.
/// The renderer then falls back to the pre-P3 behaviour: no highlight, and an
/// over-long line truncates from the left edge instead of windowing.
fn match_span(hit: &Value) -> Option<(usize, usize)> {
    let col = hit.get("col").and_then(Value::as_u64)? as usize;
    let end = hit.get("col_end").and_then(Value::as_u64).unwrap_or(col as u64) as usize;
    Some((col, end.max(col)))
}

/// The ±`context_lines` block, gutter-numbered, with `▶` on the match.
///
/// `extract_context` renders `lines[i-N ..= i+N]` clamped at the top of the
/// file, so the match sits at index `min(line - 1, N)` and the block starts at
/// `line - that index`.  Same arithmetic as `grep::matched_line`.
fn render_context(
    out: &mut String,
    context: &str,
    line: usize,
    span: Option<(usize, usize)>,
    opts: &RenderOpts,
) {
    if context.is_empty() {
        return;
    }
    let block: Vec<&str> = context.split('\n').collect();
    let match_idx = line.saturating_sub(1).min(opts.context_lines);
    let first_line = line.saturating_sub(match_idx).max(1);
    // Width off the highest *numbered* line (the truncation marker gets no
    // number), so the gutter never jitters mid-block.
    let numbered = block.iter().filter(|t| **t != TRUNCATION_MARKER).count();
    let width = (first_line + numbered.saturating_sub(1)).to_string().len();

    for (i, text) in block.iter().enumerate() {
        let is_match = i == match_idx && *text != TRUNCATION_MARKER;
        let (marker, number) = if *text == TRUNCATION_MARKER {
            (' ', String::new())
        } else if is_match {
            (MARKER, (first_line + i).to_string())
        } else {
            (' ', (first_line + i).to_string())
        };

        let gutter = format!("{marker} {number:>width$} {GUTTER} ");
        if opts.color {
            out.push_str(DIM);
            out.push_str(&gutter);
            out.push_str(RESET);
        } else {
            out.push_str(&gutter);
        }
        // Only the matched line gets a window and a highlight; its neighbours
        // are just neighbours, so an over-long one is cut at the cap (§4).
        push_line(out, text, is_match, if is_match { span } else { None }, opts);
        out.push('\n');
    }
}

// ── Per-line column cap (docs/search-cli.md §4) ────────────────────────────────

/// Write one line of a context block, applying the column cap and — on the
/// matched line, when the payload carried a span — the in-line highlight.
///
/// `is_match` and `span` are separate on purpose: a hit whose payload predates
/// P3, or whose matched line the context budget cut away, is still *the* line
/// and still earns its bold and its `▶`; it just has no span to centre a window
/// on or to paint.
///
/// All offsets here are **byte** offsets, as ripgrep's `--max-columns` counts
/// them, and every one of them is snapped to a `char` boundary before it
/// reaches a slice: an over-long line is very often minified UTF-8, and the
/// cap has no business landing in the middle of a codepoint.
fn push_line(
    out: &mut String,
    text: &str,
    is_match: bool,
    span: Option<(usize, usize)>,
    opts: &RenderOpts,
) {
    let len = text.len();
    // The marker is the renderer's own text, not the file's — capping it would
    // announce a "line" length no line has.
    let capped = opts.max_columns > 0 && len > opts.max_columns && text != TRUNCATION_MARKER;

    // Clamp the span into the line before anything else.  The payload's column
    // is an offset into the file's line, and `text` may be only the prefix of
    // it that fit the context budget — so a span running past the end here is
    // routine, not a bug, and it correctly collapses to "nothing to highlight".
    // Flooring to a char boundary makes even a hand-written payload unable to
    // provoke a mid-codepoint slice.
    let span = span
        .map(|(a, b)| (floor_boundary(text, a), floor_boundary(text, b)))
        .filter(|(a, b)| b > a);

    let (start, end) = if !capped {
        (0, len)
    } else if let Some((col, col_end)) = span {
        window(len, opts.max_columns, col, col_end)
    } else {
        // No span to centre on — behave like a context line and show the head.
        (0, opts.max_columns)
    };
    let start = floor_boundary(text, start);
    let end = floor_boundary(text, end).max(start);

    if start > 0 {
        out.push(ELLIPSIS);
    }
    let slice = &text[start..end];
    // Translate the highlight out of line coordinates into window coordinates;
    // a match that straddles an edge is highlighted for the part on screen.
    let visible = span
        .map(|(a, b)| (a.clamp(start, end) - start, b.clamp(start, end) - start))
        .filter(|(a, b)| b > a);

    match visible {
        // Uncoloured output carries no escapes at all, so the highlight simply
        // has nowhere to go — the window and the bold still apply.
        Some((a, b)) if opts.color => {
            out.push_str(BOLD);
            out.push_str(&slice[..a]);
            out.push_str(MATCH_HL);
            out.push_str(&slice[a..b]);
            out.push_str(RESET);
            out.push_str(BOLD);
            out.push_str(&slice[b..]);
            out.push_str(RESET);
        }
        _ if is_match && opts.color => {
            out.push_str(BOLD);
            out.push_str(slice);
            out.push_str(RESET);
        }
        _ => out.push_str(slice),
    }
    if end < len {
        out.push(ELLIPSIS);
    }
    if capped {
        let note = format!(" [line is {} columns]", with_thousands(len));
        if opts.color {
            out.push_str(DIM);
            out.push_str(&note);
            out.push_str(RESET);
        } else {
            out.push_str(&note);
        }
    }
}

/// The byte range of the `cap`-wide window to show around `[col, col_end)`.
///
/// Centred on the match, then slid back inside the line, so a match near
/// either edge gets ellipsised on one side only.  A match *wider* than the cap
/// is shown from its own start — the head of a match is what identifies it.
fn window(len: usize, cap: usize, col: usize, col_end: usize) -> (usize, usize) {
    if cap == 0 || len <= cap {
        return (0, len);
    }
    let match_len = col_end.saturating_sub(col);
    if match_len >= cap {
        return (col, (col + cap).min(len));
    }
    let centre = col + match_len / 2;
    let mut start = centre.saturating_sub(cap / 2);
    if start + cap > len {
        start = len - cap; // len > cap here, so this cannot underflow
    }
    (start, start + cap)
}

/// Pull `i` back to the nearest `char` boundary at or before it.
fn floor_boundary(s: &str, i: usize) -> usize {
    let mut i = i.min(s.len());
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

/// `48213` → `48,213`.  The column count is the one number in this output a
/// human reads for magnitude rather than for its digits.
fn with_thousands(n: usize) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

// ── vimgrep format ───────────────────────────────────────────────────

/// Placeholder for a hit whose matched line the context budget cut off
/// (`text: null`).  Dropping the entry would silently lose a real hit from a
/// quickfix list, so the entry stays navigable and says why it is blank.
const NO_TEXT: &str = "(matched line unavailable)";

/// Render a grep payload as `file:line:col: text`, one hit per line.
///
/// Quickfix-compatible (`vim -q`), and the same formatter `scout edit`'s
/// `vim -q` path will want.  `col` is **1-based** here — that is the quickfix
/// convention, and it is the one place the payload's 0-based byte offset gets
/// converted.  A hit with no column (`text: null`, or a pre-P3 payload) falls
/// back to column 1 rather than dropping out of the list.
///
/// `max_columns` deliberately does not apply: this is a machine format an
/// editor consumes, and a truncated line would misplace every column after it.
pub fn render_vimgrep(payload: &Value) -> String {
    let Some(hits) = payload.get("hits").and_then(Value::as_array) else {
        return String::new();
    };
    let mut out = String::new();
    for hit in hits {
        let file = hit.get("file").and_then(Value::as_str).unwrap_or("<unknown>");
        let line = hit.get("line").and_then(Value::as_u64).unwrap_or(0);
        let col = hit.get("col").and_then(Value::as_u64).unwrap_or(0) + 1;
        let text = hit
            .get("text")
            .and_then(Value::as_str)
            .map(str::trim_end)
            .filter(|t| !t.is_empty())
            .unwrap_or(NO_TEXT);
        out.push_str(&format!("{file}:{line}:{col}: {text}\n"));
    }
    out
}

#[cfg(test)]
mod tests;
