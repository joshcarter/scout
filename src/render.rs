//! Terminal rendering for `scout grep` (SPEC-cli §2, §8).
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
//! The 1–5 score is deliberately never shown (SPEC §9): it orders the hits
//! and stays in the JSON payload, but it is noise at a terminal.
//!
//! Colour is raw ANSI — no dependency.  The palette is ack's: path magenta,
//! line number green, matched line bold, gutter dim.

use serde_json::Value;

// ── ANSI ─────────────────────────────────────────────────────────────

const RESET: &str = "\x1b[0m";
const BOLD: &str = "\x1b[1m";
const DIM: &str = "\x1b[2m";
const GREEN: &str = "\x1b[32m";
const MAGENTA: &str = "\x1b[35m";

/// The gutter separator, and the marker on the matched line.
const GUTTER: char = '│';
const MARKER: char = '▶';

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
}

impl Default for RenderOpts {
    fn default() -> Self {
        RenderOpts { color: false, context_lines: 2 }
    }
}

// ── Human format ─────────────────────────────────────────────────────

/// Render a grep payload as human-readable text (SPEC §2).
///
/// Returns the empty string when there are no hits — the caller puts the
/// "why there are none" message on stderr, since that is metadata, not output.
pub fn render_human(payload: &Value, opts: &RenderOpts) -> String {
    let hits = payload.get("hits").and_then(Value::as_array);
    let Some(hits) = hits else { return String::new() };

    let mut out = String::new();
    for (i, hit) in hits.iter().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        render_hit(&mut out, hit, opts);
    }
    out
}

/// One hit: header line, then its gutter-numbered context block.
fn render_hit(out: &mut String, hit: &Value, opts: &RenderOpts) {
    let file = hit.get("file").and_then(Value::as_str).unwrap_or("<unknown>");
    let line = hit.get("line").and_then(Value::as_u64).unwrap_or(0) as usize;

    if opts.color {
        out.push_str(&format!("{MAGENTA}{file}{RESET}:{GREEN}{line}{RESET}"));
    } else {
        out.push_str(&format!("{file}:{line}"));
    }
    // Rerank mode only; the two bypass paths have no model verdict to show.
    if let Some(why) = hit.get("why").and_then(Value::as_str).map(str::trim).filter(|w| !w.is_empty())
    {
        out.push_str(&format!(" · {why}"));
    }
    out.push('\n');

    let context = hit.get("context").and_then(Value::as_str).unwrap_or("");
    render_context(out, context, line, opts);
}

/// The ±`context_lines` block, gutter-numbered, with `▶` on the match.
///
/// `extract_context` renders `lines[i-N ..= i+N]` clamped at the top of the
/// file, so the match sits at index `min(line - 1, N)` and the block starts at
/// `line - that index`.  Same arithmetic as `grep::matched_line`.
fn render_context(out: &mut String, context: &str, line: usize, opts: &RenderOpts) {
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
            if is_match {
                out.push_str(BOLD);
                out.push_str(text);
                out.push_str(RESET);
            } else {
                out.push_str(text);
            }
        } else {
            out.push_str(&gutter);
            out.push_str(text);
        }
        out.push('\n');
    }
}

// ── vimgrep format ───────────────────────────────────────────────────

/// Placeholder for a hit whose matched line the context budget cut off
/// (`text: null`).  Dropping the entry would silently lose a real hit from a
/// quickfix list, so the entry stays navigable and says why it is blank.
const NO_TEXT: &str = "(matched line unavailable)";

/// Render a grep payload as `file:line:col: text`, one hit per line.
///
/// Quickfix-compatible (`vim -q`), and the same formatter `scout edit`'s
/// `vim -q` path will want.  `col` is hardcoded to 1 until P3 captures real
/// match offsets in `source.rs`.
pub fn render_vimgrep(payload: &Value) -> String {
    let Some(hits) = payload.get("hits").and_then(Value::as_array) else {
        return String::new();
    };
    let mut out = String::new();
    for hit in hits {
        let file = hit.get("file").and_then(Value::as_str).unwrap_or("<unknown>");
        let line = hit.get("line").and_then(Value::as_u64).unwrap_or(0);
        let text = hit
            .get("text")
            .and_then(Value::as_str)
            .map(str::trim_end)
            .filter(|t| !t.is_empty())
            .unwrap_or(NO_TEXT);
        out.push_str(&format!("{file}:{line}:1: {text}\n"));
    }
    out
}

#[cfg(test)]
mod tests;
