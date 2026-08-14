//! Shared helpers for the read-side filters (`extract`, `grep`).
//!
//! ## Selector mode
//!
//! The local model never returns file text.  It returns *selectors* — line
//! ranges or hit ids — plus short labels and scores.  scout validates those
//! selectors against ground truth (the file's real line count, the real hit
//! list) and materializes the actual text itself.  The model can misjudge
//! relevance but can never misquote code into the caller's context.
//!
//! This module owns:
//!
//! * `parse_selector_json` — tolerant JSON extraction from LLM text
//!   (markdown fences, leading prose, `<think>` blocks).
//! * `validate_ranges` — clamp / drop / merge / sort line ranges.
//! * `validate_keeps` — validate, dedupe and sort hit ids.
//! * `apply_line_budget` — enforce a returned-line budget, lowest score first.
//! * `call_preset` — one preset round-trip against the local LLM.
//! * `Ctx` / `ToolError` — the invocation context shared by every filter and
//!   the fail-open error carrying the raw tool to fall back to.
//! * `non_empty_arg` — required-arg plucking.
//!
//! Ported from ct's `local_select.rs`.  The plugin round-trip
//! (`call_plugin_preset`) became a direct `LlmClient` call, and the MCP
//! envelope builders (`ok_result`/`err_result`) moved out to the callers —
//! `mcp_server.rs` renders `ToolError` into an rmcp `CallToolResult`, the CLI
//! renders it onto stderr.  Everything else moves verbatim.

use serde_json::Value;

use crate::client::LlmClient;
use crate::presets::{self, Preset};

/// Where a filter sends human-facing progress notes, when anyone is listening.
pub type ProgressSink<'a> = Box<dyn Fn(&str) + 'a>;

/// Ranges separated by at most this many lines are merged into one.
pub const MERGE_GAP: usize = 3;

/// `why` labels are capped so a chatty model cannot inflate the result.
const MAX_WHY_LEN: usize = 120;

// ── Invocation context ───────────────────────────────────────────────

/// Everything a filter needs besides its own arguments.
///
/// `client` is optional on purpose, mirroring ct's `Option<&mut Plugin>`: the
/// bypass paths (small file, short hit list) do real work with no model at
/// all, so a missing or broken config must not turn those into errors.
pub struct Ctx<'a> {
    pub client: Option<&'a LlmClient>,
    /// Why `client` is absent — surfaced verbatim in the failure message so a
    /// malformed config reads as a config problem, not a mystery.
    pub client_error: Option<String>,
    pub presets: &'a [Preset],
    /// Project root: the base for relative paths and the search walk.
    pub project: String,
    /// How this invocation was reached: `mcp` | `cli` (SPEC-dashboard §3).
    ///
    /// Set by whoever built the `Ctx` — the MCP server or the CLI dispatcher —
    /// because only they know, and a value derived any later could drift.
    pub via: &'static str,
    /// The user-facing operation this context belongs to (`find`, `edit`),
    /// which is not the preset a given round-trip sends (`find_patterns`).
    /// One `find` writes three or four rows; they share this and a `run` id,
    /// which is what lets the log group them back into one operation.
    pub tool: String,
    /// `find`'s round counter, read by every record written during that round.
    pub attempt: std::cell::Cell<u64>,
    /// Byte accounting for the operation in flight — see `stats::Ledger`.
    pub ledger: crate::stats::Ledger,
    /// Optional sink for human-facing progress notes (SPEC-cli §2).
    ///
    /// The filters are shared verbatim with the MCP server, which speaks
    /// JSON-RPC over stdio and must stay silent — so a filter can *never*
    /// print unconditionally.  The CLI installs a closure that writes to
    /// stderr; `mcp_server.rs` and the tests leave this `None`, which makes
    /// silence the default rather than something each caller must remember
    /// to arrange.
    pub progress: Option<ProgressSink<'a>>,
}

/// A context with nothing configured: no model, no project, the CLI's `via`.
///
/// Exists so the four new logging fields cost a test fixture one line rather
/// than four — `Ctx { project: p, ..Default::default() }`.
impl Default for Ctx<'_> {
    fn default() -> Self {
        Ctx {
            client: None,
            client_error: None,
            presets: &[],
            project: String::new(),
            via: crate::stats::VIA_CLI,
            tool: String::new(),
            attempt: std::cell::Cell::new(1),
            ledger: crate::stats::Ledger::default(),
            progress: None,
        }
    }
}

impl Ctx<'_> {
    /// A call-log record for this context, pre-filled with everything it
    /// already knows: the tool, how it was reached, which round it is on, the
    /// project, and the model behind it.  The caller adds the outcome.
    ///
    /// `args` is the preset's own argument object; `stats::input_summary`
    /// takes the handful of fields worth keeping from it.
    pub fn record(&self, preset: &str, args: &serde_json::Value) -> crate::stats::CallRecord {
        let mut rec = crate::stats::CallRecord::new(&self.tool, preset)
            .via(self.via)
            .attempt(self.attempt.get())
            .project(&self.project)
            .input(crate::stats::input_summary(preset, args));
        if let Some(c) = self.client {
            rec = rec.endpoint(c.model(), c.endpoint());
        }
        rec
    }

    /// Report progress to whoever asked for it; a no-op when nobody did.
    pub fn note(&self, msg: &str) {
        if let Some(sink) = &self.progress {
            sink(msg);
        }
    }

    /// The client, or a caller-facing explanation of why there isn't one.
    pub fn require_client(&self) -> Result<&LlmClient, String> {
        match (self.client, &self.client_error) {
            (Some(c), _) => Ok(c),
            (None, Some(e)) => Err(format!("local LLM is not configured: {e}")),
            (None, None) => Err("local LLM is not configured (see ~/.config/scout/config.toml)".to_string()),
        }
    }

    pub fn preset(&self, name: &str) -> Result<&Preset, String> {
        self.presets
            .iter()
            .find(|p| p.name == name)
            .ok_or_else(|| format!("preset {name:?} not found"))
    }
}

/// A filter failure, carrying the raw tool the caller should use instead.
///
/// Every failure path in every filter goes through this: a broken filter must
/// never trap the caller with "no result", it must name the tool that always
/// works.  This is ct's `err_result(reason, fallback)` contract, minus the
/// JSON-RPC envelope.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolError {
    pub message: String,
    pub fallback: String,
}

impl ToolError {
    pub fn new(message: impl Into<String>, fallback: impl Into<String>) -> Self {
        ToolError { message: message.into(), fallback: fallback.into() }
    }

    /// The caller-visible text: reason plus the named fallback.
    pub fn text(&self) -> String {
        format!("{} — fall back to {}", self.message, self.fallback)
    }
}

/// What every filter returns: a compact JSON payload, or a fail-open error.
pub type ToolResult = Result<Value, ToolError>;

// ── Tolerant JSON extraction ─────────────────────────────────────────

/// Parse an LLM reply into a JSON object.
///
/// The preset prompts forbid prose and fences; reality does not comply.  This
/// strips `<think>` blocks, markdown fences and any leading prose, then takes
/// the first balanced `{...}` object.  Returns `None` when nothing parses —
/// callers treat that as an LLM failure and fail open.
pub fn parse_selector_json(text: &str) -> Option<Value> {
    let without_think = strip_think(text);
    let candidates = [without_think.trim(), strip_fences(without_think.trim()).trim()];
    for c in candidates {
        if let Ok(v) = serde_json::from_str::<Value>(c) {
            if v.is_object() {
                return Some(v);
            }
        }
    }
    let stripped = strip_fences(without_think.trim()).to_string();
    let obj = first_json_object(&stripped)?;
    serde_json::from_str::<Value>(&obj).ok().filter(Value::is_object)
}

/// Remove `<think>...</think>` reasoning blocks emitted by reasoning models.
/// An unterminated `<think>` swallows the rest of the text — that reply had no
/// answer in it anyway.
fn strip_think(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(open) = rest.find("<think>") {
        out.push_str(&rest[..open]);
        match rest[open..].find("</think>") {
            Some(close) => rest = &rest[open + close + "</think>".len()..],
            None => return out,
        }
    }
    out.push_str(rest);
    out
}

/// Return the body of the first ``` fenced block, or the input unchanged.
fn strip_fences(text: &str) -> &str {
    let Some(open) = text.find("```") else { return text };
    let after = &text[open + 3..];
    // Skip an optional language tag on the fence line.
    let body_start = match after.find('\n') {
        Some(nl) if after[..nl].trim().chars().all(|c| c.is_ascii_alphanumeric()) => nl + 1,
        _ => 0,
    };
    let body = &after[body_start..];
    match body.find("```") {
        Some(close) => &body[..close],
        None => body,
    }
}

/// Extract the first balanced `{...}` object, respecting string literals and
/// escapes so braces inside strings do not unbalance the scan.
fn first_json_object(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let start = bytes.iter().position(|&b| b == b'{')?;
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    for (i, &b) in bytes.iter().enumerate().skip(start) {
        if in_string {
            if escaped {
                escaped = false;
            } else if b == b'\\' {
                escaped = true;
            } else if b == b'"' {
                in_string = false;
            }
            continue;
        }
        match b {
            b'"' => in_string = true,
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(text[start..=i].to_string());
                }
            }
            _ => {}
        }
    }
    None
}

// ── Range selectors (extract) ────────────────────────────────────────

/// One validated, in-bounds line range.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectedRange {
    /// 1-based inclusive first line.
    pub start: usize,
    /// 1-based inclusive last line.
    pub end: usize,
    /// Short label — never code.
    pub why: String,
    /// Model confidence, clamped to 0..=3.
    pub score: i64,
}

impl SelectedRange {
    pub fn len(&self) -> usize {
        self.end.saturating_sub(self.start) + 1
    }
}

/// Result of validating one `extract` reply.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct RangeSelection {
    pub ranges: Vec<SelectedRange>,
    pub answer: Option<String>,
    pub not_found: bool,
    /// Selectors thrown away by validation (out of bounds, inverted, malformed).
    pub dropped_invalid: usize,
}

/// Validate the `ranges` array of an `extract` reply against `file_lines`.
///
/// Out-of-bounds starts, inverted ranges and malformed entries are dropped and
/// counted.  Surviving ranges are clamped, merged when they overlap or sit
/// within `MERGE_GAP` lines of each other, then sorted score-desc / start-asc.
pub fn validate_ranges(v: &Value, file_lines: usize) -> RangeSelection {
    let mut sel = RangeSelection {
        answer: v
            .get("answer")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string),
        not_found: v.get("not_found").and_then(Value::as_bool).unwrap_or(false),
        ..Default::default()
    };

    let mut raw: Vec<SelectedRange> = Vec::new();
    let items = v.get("ranges").and_then(Value::as_array).cloned().unwrap_or_default();
    for item in items {
        let (Some(start), Some(end)) = (
            item.get("start").and_then(Value::as_u64),
            item.get("end").and_then(Value::as_u64),
        ) else {
            sel.dropped_invalid += 1;
            continue;
        };
        let (start, end) = (start as usize, end as usize);
        // Inverted, or entirely past EOF — clamping either would fabricate a
        // range the model never asked for.
        if start > end || start > file_lines || file_lines == 0 {
            sel.dropped_invalid += 1;
            continue;
        }
        raw.push(SelectedRange {
            start: start.max(1),
            end: end.clamp(start.max(1), file_lines),
            why: truncate_why(item.get("why").and_then(Value::as_str).unwrap_or("")),
            score: item.get("score").and_then(Value::as_i64).unwrap_or(1).clamp(0, 3),
        });
    }

    raw.sort_by_key(|r| (r.start, r.end));
    let mut merged: Vec<SelectedRange> = Vec::with_capacity(raw.len());
    for r in raw {
        match merged.last_mut() {
            Some(prev) if r.start <= prev.end + MERGE_GAP + 1 => {
                prev.end = prev.end.max(r.end);
                if r.score > prev.score {
                    prev.score = r.score;
                    if !r.why.is_empty() {
                        prev.why = r.why;
                    }
                } else if prev.why.is_empty() {
                    prev.why = r.why;
                }
            }
            _ => merged.push(r),
        }
    }
    merged.sort_by(|a, b| b.score.cmp(&a.score).then(a.start.cmp(&b.start)));
    sel.ranges = merged;
    sel
}

/// Keep the highest-scoring ranges that fit in `max_lines`.
///
/// `ranges` must already be sorted score-desc.  Returns the kept ranges sorted
/// by position (reading order) plus the number dropped for budget.  When the
/// single best range alone exceeds the budget it is truncated rather than
/// dropped — returning nothing would be a worse answer than a partial one.
pub fn apply_line_budget(
    ranges: Vec<SelectedRange>,
    max_lines: usize,
) -> (Vec<SelectedRange>, usize) {
    let mut kept: Vec<SelectedRange> = Vec::new();
    let mut used = 0usize;
    let mut dropped = 0usize;
    for r in ranges {
        let len = r.len();
        if used + len <= max_lines {
            used += len;
            kept.push(r);
        } else if kept.is_empty() && max_lines > 0 {
            let mut t = r;
            t.end = t.start + max_lines - 1;
            used = t.len();
            kept.push(t);
        } else {
            dropped += 1;
        }
    }
    kept.sort_by_key(|r| (r.start, r.end));
    (kept, dropped)
}

// ── Id selectors (grep) ──────────────────────────────────────────────

/// One validated hit id kept by the reranker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectedHit {
    /// 1-based index into the *considered* hit list.
    pub id: usize,
    pub why: String,
    pub score: i64,
}

/// Result of validating one `grep` rerank reply.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct KeepSelection {
    pub keeps: Vec<SelectedHit>,
    pub none_relevant: bool,
    pub dropped_invalid: usize,
}

/// Validate the `keep` array of a rerank reply against the ids that call saw.
///
/// `valid_ids` is the calling batch's own id window, not the global hit
/// count: an id outside it is a hallucination even when it names a real hit
/// from *another* batch, since this reply never saw that hit's content.
/// Dropped and counted.  Duplicates collapse to the highest score.  Output
/// is score-desc / id-asc.
pub fn validate_keeps(v: &Value, valid_ids: std::ops::RangeInclusive<usize>) -> KeepSelection {
    let mut sel = KeepSelection {
        none_relevant: v.get("none_relevant").and_then(Value::as_bool).unwrap_or(false),
        ..Default::default()
    };

    let items = v.get("keep").and_then(Value::as_array).cloned().unwrap_or_default();
    let mut keeps: Vec<SelectedHit> = Vec::new();
    for item in items {
        let Some(id) = item.get("id").and_then(Value::as_u64) else {
            sel.dropped_invalid += 1;
            continue;
        };
        let id = id as usize;
        if !valid_ids.contains(&id) {
            sel.dropped_invalid += 1;
            continue;
        }
        let hit = SelectedHit {
            id,
            why: truncate_why(item.get("why").and_then(Value::as_str).unwrap_or("")),
            score: item.get("score").and_then(Value::as_i64).unwrap_or(1).clamp(0, 3),
        };
        match keeps.iter_mut().find(|k| k.id == id) {
            Some(existing) => {
                sel.dropped_invalid += 1;
                if hit.score > existing.score {
                    *existing = hit;
                }
            }
            None => keeps.push(hit),
        }
    }
    keeps.sort_by(|a, b| b.score.cmp(&a.score).then(a.id.cmp(&b.id)));
    sel.keeps = keeps;
    sel
}

fn truncate_why(why: &str) -> String {
    let why = why.trim();
    if why.chars().count() <= MAX_WHY_LEN {
        return why.to_string();
    }
    why.chars().take(MAX_WHY_LEN).collect()
}

// ── Preset invocation ────────────────────────────────────────────────

/// Run one preset round-trip against the local LLM and return the reply text.
///
/// This is ct's `call_plugin_preset` with the plugin hop removed: scout owns
/// the preset table and the HTTP client, so resolving the templates and
/// calling the model happen in-process.  Every round-trip through here writes
/// one call-log row (SPEC-dashboard §3) via the context's ledger, which is
/// what makes a `find`'s internal rounds visible as themselves.
pub fn call_preset(ctx: &Ctx, preset_name: &str, args: &Value) -> Result<String, String> {
    let client = ctx.require_client()?;
    let preset = ctx.preset(preset_name)?;
    let (system, user) = presets::resolve(preset, args, &ctx.project);

    let messages = vec![
        serde_json::json!({"role": "system", "content": system}),
        serde_json::json!({"role": "user", "content": user}),
    ];

    let rec = ctx.record(preset_name, args);
    let start = std::time::Instant::now();
    match client.complete(messages, None) {
        Ok((text, usage)) => {
            let ms = start.elapsed().as_millis() as u64;
            ctx.ledger.record(rec.usage(&usage).ms(ms));
            Ok(text)
        }
        Err(e) => {
            // The elapsed time of a failure is worth keeping — a 30-second
            // timeout and an instant connection refusal are the same `ok:
            // false` and very different problems.  `scout stats` still averages
            // successes only, so this changes no existing number.
            let ms = start.elapsed().as_millis() as u64;
            ctx.ledger.record(rec.ms(ms).outcome(e.outcome()).summary(e.to_string()));
            Err(format!("local LLM call failed: {e}"))
        }
    }
}

// ── Shared handler helpers ───────────────────────────────────────────

/// Pluck a required string arg, treating whitespace-only as absent.
pub fn non_empty_arg(args: &Value, key: &str) -> Option<String> {
    args.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

#[cfg(test)]
mod tests;
