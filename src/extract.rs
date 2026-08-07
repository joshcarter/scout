//! `extract` — targeted file reading through the local LLM.
//!
//! The caller asks a question about a file.  scout reads the whole file off
//! disk, the local model reads it and replies with *line ranges only*, and
//! scout materializes those ranges from its own copy.  The caller receives a
//! handful of snippets plus a one-line answer instead of a 2000-line file.
//!
//! Same architecture as `check_output`: scout gathers the bulky input itself,
//! injects it into the preset args, and only the compact result reaches the
//! caller.  The extra step here is post-processing — selector validation
//! (`select`) and materialization.
//!
//! Invariants:
//!
//! * The model's text output is never quoted back to the caller.
//!   `snippets[].text` always comes from the file on disk.
//! * Small files bypass the LLM entirely (`mode: "full"`).
//! * Every failure path returns a `ToolError` naming the Read tool — a broken
//!   filter must never trap the caller.
//!
//! Ported from ct's `local_extract.rs`.  The one rewiring: ct resolved the
//! file through the daemon (`query_daemon_socket_with_project(sock, "read",
//! ...)`, line 76), which only ever fetched file content; scout calls
//! `source::read_file` instead.

use serde_json::Value;

use crate::select::{
    apply_line_budget, call_preset, non_empty_arg, parse_selector_json, validate_ranges, Ctx,
    RangeSelection, SelectedRange, ToolError, ToolResult,
};

/// Raw tool to name whenever this filter cannot deliver.
const FALLBACK: &str = "the Read tool for the full file";

/// Upper bound on `max_lines` — past this the caller should just read the file.
const MAX_LINES_CEILING: usize = 2000;

/// Extract the line ranges of a file relevant to the caller's question,
/// using the local LLM as the reader.
///
/// The model sees the whole (numbered) file and returns selectors only; scout
/// materializes the surviving ranges from disk.  Small files skip the model
/// entirely.
pub fn run(ctx: &Ctx, args: &Value) -> ToolResult {
    let (cfg, _) = crate::filter_config::load();

    let file = non_empty_arg(args, "file")
        .ok_or_else(|| fail("'file' argument is required and must be non-empty"))?;
    let question = non_empty_arg(args, "question")
        .ok_or_else(|| fail("'question' argument is required and must be non-empty"))?;
    let max_lines = args
        .get("max_lines")
        .and_then(Value::as_u64)
        .map(|n| n as usize)
        .unwrap_or(cfg.default_max_lines)
        .clamp(1, MAX_LINES_CEILING);

    // ── 1. Read the file ─────────────────────────────────────────────
    let project = std::path::Path::new(&ctx.project);
    let content = crate::source::read_file(project, &file, cfg.max_file_bytes)
        .map_err(|e| fail(&format!("could not read {file}: {e}")))?;
    let (resolved, lines) = (content.path, content.lines);
    let file_lines = lines.len();

    // ── 2. Bypass: the LLM adds nothing to a small file ──────────────
    if file_lines <= cfg.bypass_max_lines {
        return Ok(bypass_payload(&resolved, &lines));
    }

    // ── 3. Render numbered content, chunk if huge ────────────────────
    let chunks = chunk_numbered(&lines, cfg.chunk_bytes);
    let chunk_total = chunks.len();

    // ── 4. One preset call per chunk ─────────────────────────────────
    //
    // Chunks run sequentially, matching ct: with chunk_bytes at 384 KiB
    // chunking is already the rare case, and a local model serves one request
    // at a time anyway.
    let mut selections: Vec<RangeSelection> = Vec::with_capacity(chunk_total);
    let mut parse_failures = 0usize;
    let mut last_error: Option<String> = None;
    for (i, chunk) in chunks.iter().enumerate() {
        let mut call_args = args.clone();
        call_args["numbered_content"] = Value::String(chunk.clone());
        call_args["chunk_of"] = Value::from(i + 1);
        call_args["chunk_total"] = Value::from(chunk_total);
        call_args["file"] = Value::String(resolved.clone());
        call_args["file_lines"] = Value::from(file_lines);

        match call_preset(ctx, "extract", &call_args) {
            Ok(text) => match parse_selector_json(&text) {
                Some(v) => selections.push(validate_ranges(&v, file_lines)),
                None => parse_failures += 1,
            },
            Err(e) => {
                parse_failures += 1;
                last_error = Some(e);
            }
        }
    }

    if selections.is_empty() {
        let detail = last_error.unwrap_or_else(|| {
            format!("local LLM returned unparsable output for all {parse_failures} chunk(s)")
        });
        return Err(fail(&detail));
    }

    // ── 5. Merge chunk results, validate budget, materialize ─────────
    let merged = merge_selections(selections, file_lines);
    let dropped_invalid = merged.dropped_invalid;

    if merged.ranges.is_empty() {
        if merged.not_found {
            return Ok(not_found_payload(
                &resolved,
                file_lines,
                &question,
                merged.answer.as_deref(),
            ));
        }
        // Model produced only invalid selectors — that is an LLM failure.
        return Err(fail(&format!(
            "local LLM returned no usable line ranges ({dropped_invalid} invalid selector(s))"
        )));
    }

    let (kept, dropped_low_score) = apply_line_budget(merged.ranges, max_lines);
    let returned_lines: usize = kept.iter().map(SelectedRange::len).sum();
    let snippets: Vec<Value> = kept.iter().map(|r| materialize(&lines, r)).collect();

    Ok(serde_json::json!({
        "mode": "extract",
        "file": resolved,
        "file_lines": file_lines,
        "question": question,
        "answer": merged.answer,
        "not_found": false,
        "snippets": snippets,
        "coverage": {
            "returned_lines": returned_lines,
            "ranges": kept.len(),
            "dropped_low_score": dropped_low_score,
            "dropped_invalid": dropped_invalid,
            "chunks": chunk_total,
        },
        "hint": format!(
            "filtered view — {returned_lines} of {file_lines} lines. \
             Read {resolved} with an offset/limit for surrounding context, \
             or read it whole to judge for yourself"
        ),
    }))
}

// ── Payload builders (pure — unit-tested directly) ───────────────────

/// Bypass result: the whole file, verbatim, no LLM involved.
pub fn bypass_payload(resolved: &str, lines: &[String]) -> Value {
    serde_json::json!({
        "mode": "full",
        "file": resolved,
        "file_lines": lines.len(),
        "answer": Value::Null,
        "not_found": false,
        "content": render_numbered(lines, 1),
        "coverage": {
            "returned_lines": lines.len(),
            "ranges": 1,
            "dropped_low_score": 0,
            "dropped_invalid": 0,
            "chunks": 0,
        },
        "hint": "file is small enough to return whole — no filtering was applied",
    })
}

/// The model read the file and says it does not answer the question.
fn not_found_payload(resolved: &str, file_lines: usize, question: &str, answer: Option<&str>) -> Value {
    serde_json::json!({
        "mode": "extract",
        "file": resolved,
        "file_lines": file_lines,
        "question": question,
        "answer": answer,
        "not_found": true,
        "snippets": [],
        "coverage": {
            "returned_lines": 0, "ranges": 0, "dropped_low_score": 0, "dropped_invalid": 0,
        },
        "hint": format!(
            "the local model found nothing in {resolved} answering this question — \
             this is a filter verdict, not an empty file; read {resolved} to judge for yourself"
        ),
    })
}

/// Cut the line list into `N→line` blocks no larger than `chunk_bytes`.
/// Line numbers stay absolute across chunks so merging is a concatenation.
pub fn chunk_numbered(lines: &[String], chunk_bytes: usize) -> Vec<String> {
    let chunk_bytes = chunk_bytes.max(1);
    let mut chunks: Vec<String> = Vec::new();
    let mut current = String::new();
    for (i, line) in lines.iter().enumerate() {
        let rendered = format!("{:6}\u{2192}{}\n", i + 1, line);
        if !current.is_empty() && current.len() + rendered.len() > chunk_bytes {
            chunks.push(std::mem::take(&mut current));
        }
        current.push_str(&rendered);
    }
    if !current.is_empty() || chunks.is_empty() {
        chunks.push(current);
    }
    chunks
}

/// Render lines in the `N→line` form, starting at `first_line`.
pub fn render_numbered(lines: &[String], first_line: usize) -> String {
    let mut out = String::new();
    for (i, line) in lines.iter().enumerate() {
        out.push_str(&format!("{:6}\u{2192}{}\n", first_line + i, line));
    }
    out
}

/// Pull a validated range's real text out of the line list read from disk.
fn materialize(lines: &[String], r: &SelectedRange) -> Value {
    let start = r.start.saturating_sub(1).min(lines.len());
    let end = r.end.min(lines.len());
    let slice = &lines[start..end];
    serde_json::json!({
        "lines": format!("{}-{}", r.start, r.end),
        "why": r.why,
        "score": r.score,
        "text": render_numbered(slice, r.start),
    })
}

/// Fold per-chunk selections into one.  Ranges concatenate (line numbers are
/// absolute), then re-run validation so cross-chunk neighbours merge too.
fn merge_selections(selections: Vec<RangeSelection>, file_lines: usize) -> RangeSelection {
    if selections.len() == 1 {
        return selections.into_iter().next().unwrap();
    }
    let mut dropped_invalid = 0usize;
    let mut not_found = true;
    let mut answers: Vec<String> = Vec::new();
    let mut range_values: Vec<Value> = Vec::new();
    for s in selections {
        dropped_invalid += s.dropped_invalid;
        not_found &= s.not_found;
        if let Some(a) = s.answer {
            answers.push(a);
        }
        for r in s.ranges {
            range_values.push(serde_json::json!({
                "start": r.start, "end": r.end, "why": r.why, "score": r.score,
            }));
        }
    }
    let mut merged = validate_ranges(
        &serde_json::json!({ "ranges": range_values, "not_found": not_found }),
        file_lines,
    );
    merged.dropped_invalid += dropped_invalid;
    merged.answer = if answers.is_empty() { None } else { Some(answers.join(" ")) };
    merged
}

// ── Small helpers ────────────────────────────────────────────────────

/// Fail open, naming this filter's fallback (the Read tool).
fn fail(reason: &str) -> ToolError {
    ToolError::new(format!("scout extract: {reason}"), FALLBACK)
}

#[cfg(test)]
mod tests;
