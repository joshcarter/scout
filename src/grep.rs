//! `grep` — intent-filtered grep through the local LLM.
//!
//! scout runs its own search (ripgrep's libraries, see `source.rs` — nothing
//! is shelled out).  When the hit list is noisy the local model reranks it
//! against the caller's *intent* and returns hit ids only; scout materializes
//! the surviving hits from the original search results.
//!
//! Invariants:
//!
//! * Returned hits always come from the real search output.  The model can
//!   drop a hit, never invent one.
//! * Short hit lists bypass the LLM entirely (`mode: "full"`).
//! * An absent (or empty) `intent` means "no rerank": the search runs, the hit
//!   list is truncated to `max_hits`, and the local model is never called — so
//!   this path works with no LLM configured at all (`mode: "full"`).
//! * `none_relevant` is reported explicitly so an empty filtered list can never
//!   be mistaken for "the pattern had no matches".
//! * Every failure path returns a `ToolError` naming the raw Grep tool.
//!
//! Ported from ct's `local_grep.rs`.  The one rewiring: ct ran the daemon's
//! grep engine over the socket and `parse_hits` unpacked a `ct::Response`;
//! scout searches the filesystem and `parse_hits` unpacks the plain
//! `source::SearchResults` struct.  Everything downstream — batching, id
//! validation, materialization, the payload shapes — is unchanged.

use serde_json::Value;

use crate::filter_config::GrepConfig;
use crate::select::{
    call_preset, non_empty_arg, parse_selector_json, validate_keeps, Ctx, SelectedHit, ToolError,
    ToolResult,
};
use crate::source::{SearchOptions, SearchResults};

/// Raw tool to name whenever this filter cannot deliver.
const FALLBACK: &str = "the Grep tool for the unfiltered hit list";

/// Upper bound on `max_hits` — past this the caller wants raw grep.
const MAX_HITS_CEILING: usize = 100;

/// One hit from the search engine, before any filtering.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawHit {
    pub file: String,
    pub line: usize,
    /// The matched line itself, or `None` when the context block's byte budget
    /// cut it off before the match was reached (serialized as JSON null —
    /// never another line's text mislabeled as the match).
    pub text: Option<String>,
    /// The ±context_lines block around it, as the search engine rendered it.
    pub context: String,
}

/// Rerank search hits against the caller's *intent* using the local LLM.
///
/// Runs the filesystem search, then asks the local model which hits actually
/// serve the intent; short hit lists skip the model entirely.
pub fn run(ctx: &Ctx, args: &Value) -> ToolResult {
    let (_, mut cfg) = crate::filter_config::load();

    // `context_lines` is a CLI-only override (`-C`, or `[cli] context`).  It
    // is deliberately absent from the MCP tool schema — the frozen contract —
    // so for the MCP server this is always a no-op.
    if let Some(n) = args.get("context_lines").and_then(Value::as_u64) {
        cfg.context_lines = n as usize;
    }

    let pattern = non_empty_arg(args, "pattern")
        .ok_or_else(|| fail("'pattern' argument is required and must be non-empty"))?;
    // Absent, null or empty all mean the same thing: no rerank.  See step 2.
    let intent = non_empty_arg(args, "intent");
    let use_regex = args.get("regex").and_then(Value::as_bool).unwrap_or(false);
    let max_hits = args
        .get("max_hits")
        .and_then(Value::as_u64)
        .map(|n| n as usize)
        .unwrap_or(10)
        .clamp(1, MAX_HITS_CEILING);

    // ── 1. Run the real search engine ────────────────────────────────
    let root = std::path::Path::new(&ctx.project);
    let opts = search_options(&cfg, use_regex, root, args).map_err(|e| fail(&e))?;
    let results = crate::source::search(root, &pattern, &opts)
        .map_err(|e| fail(&format!("grep failed: {e}")))?;
    let search_truncated = results.truncated;
    let hits = parse_hits(&results, cfg.context_lines);
    let hits_total = hits.len();

    // ── 2. No intent: unfiltered search, the model is never involved ─
    //
    // This is the implicit `--no-filter`.  It must hold for any hit count, so
    // it is checked before the bypass threshold and truncates at `max_hits`
    // rather than returning an unbounded list.
    let Some(intent) = intent else {
        return Ok(unfiltered_payload(&pattern, &hits, max_hits, search_truncated));
    };

    // ── 3. Bypass: nothing for the model to filter ───────────────────
    if hits_total <= cfg.bypass_max_hits {
        return Ok(bypass_payload(&pattern, &intent, &hits, search_truncated));
    }

    // ── 4. Cap what the model sees; truncation stays visible ─────────
    let considered: &[RawHit] = &hits[..hits_total.min(cfg.max_considered)];

    // ── 5. Rerank, one call per batch ────────────────────────────────
    //
    // Batches run sequentially, matching ct.  Hit ids are global and 1-based,
    // so merging the score lists is a concatenation.
    // The rerank takes seconds; silence at a terminal looks like a hang.  This
    // is a no-op unless the caller installed a progress sink (SPEC-cli §2) —
    // the MCP server never does, because stdout is its transport.
    ctx.note(&format!(
        "filtering {} hits with {}…",
        considered.len(),
        ctx.client.map(|c| c.model()).unwrap_or("the local model")
    ));

    let batch_size = cfg.batch_size.max(1);
    let mut keeps: Vec<SelectedHit> = Vec::new();
    let mut none_relevant = true;
    let mut dropped_invalid = 0usize;
    let mut parsed_any = false;
    let mut last_error: Option<String> = None;

    let mut batch_start = 0usize;
    while batch_start < considered.len() {
        let batch_end = (batch_start + batch_size).min(considered.len());
        let batch = &considered[batch_start..batch_end];
        let first_id = batch_start + 1;
        batch_start = batch_end;

        let mut call_args = args.clone();
        call_args["hit_list"] = Value::String(render_hit_list(batch, first_id));
        call_args["hits_considered"] = Value::from(considered.len());
        call_args["max_hits"] = Value::from(max_hits);

        match call_preset(ctx, "grep", &call_args) {
            Ok(text) => match parse_selector_json(&text) {
                Some(v) => {
                    parsed_any = true;
                    let sel = validate_keeps(&v, first_id..=first_id + batch.len() - 1);
                    dropped_invalid += sel.dropped_invalid;
                    none_relevant &= sel.none_relevant;
                    keeps.extend(sel.keeps);
                }
                None => last_error = Some("local LLM returned unparseable output".to_string()),
            },
            Err(e) => last_error = Some(e),
        }
    }

    if !parsed_any {
        let detail = last_error.unwrap_or_else(|| "local LLM returned no usable output".to_string());
        return Err(fail(&detail));
    }

    // Cross-batch dedupe (id order first, so duplicates are adjacent), then
    // final ordering by score.
    keeps.sort_by_key(|k| k.id);
    keeps.dedup_by_key(|k| k.id);
    keeps.sort_by(|a, b| b.score.cmp(&a.score).then(a.id.cmp(&b.id)));
    keeps.truncate(max_hits);

    if keeps.is_empty() && !none_relevant {
        // The model kept nothing but also declined to say "none relevant" —
        // either every id it produced was invalid or the reply was incoherent.
        // Either way it is an LLM failure, not a verdict: fail open.
        return Err(fail(&format!(
            "local LLM returned neither usable hit ids nor a none_relevant verdict \
             ({dropped_invalid} invalid id(s))"
        )));
    }

    let returned: Vec<Value> = keeps
        .iter()
        .filter_map(|k| considered.get(k.id - 1).map(|h| materialize(h, k)))
        .collect();

    Ok(rerank_payload(
        &pattern,
        &intent,
        hits_total,
        considered.len(),
        returned,
        dropped_invalid,
        none_relevant,
        search_truncated,
    ))
}

/// Search knobs for one run: the grep config, the caller's `regex`, and the
/// optional type/glob filters (`types`, `types_not`, `globs`).
///
/// The three filter args are optional everywhere — absent means `None`, which
/// `source::search` treats as "do not touch the walker" — so an old caller
/// gets exactly the walk it got before these fields existed.  A bad type name
/// or malformed glob returns `Err`, which `run` turns into a fail-open error
/// (exit 2 at the CLI) rather than a silently unfiltered search.
pub fn search_options(
    cfg: &GrepConfig,
    regex: bool,
    root: &std::path::Path,
    args: &Value,
) -> Result<SearchOptions, String> {
    let types = crate::source::build_types(
        &string_list(args, "types"),
        &string_list(args, "types_not"),
    )?;
    let overrides = crate::source::build_overrides(root, &string_list(args, "globs"))?;
    Ok(SearchOptions {
        regex,
        context_lines: cfg.context_lines,
        context_max_bytes: cfg.context_max_bytes,
        max_file_bytes: cfg.max_file_bytes,
        max_hits: cfg.max_hits_scanned,
        types,
        overrides,
    })
}

/// Read an optional array-of-strings argument.  Anything else — a scalar, a
/// null, a missing key — reads as the empty list; blank entries are dropped so
/// a stray `-g ''` cannot turn into a match-nothing override.
fn string_list(args: &Value, key: &str) -> Vec<String> {
    args.get(key)
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

// ── Payload builders (pure — unit-tested directly) ───────────────────

/// Serialize raw hits verbatim — the shared body of both `mode: "full"` paths.
fn raw_hit_values(hits: &[RawHit]) -> Vec<Value> {
    hits.iter()
        .map(|h| {
            serde_json::json!({
                "file": h.file, "line": h.line, "text": h.text, "context": h.context,
            })
        })
        .collect()
}

/// Assemble a `mode: "full"` payload.  `hits_total` is the pre-truncation
/// count; `hits` is what actually goes back.
fn full_payload(
    pattern: &str,
    intent: Option<&str>,
    hits: &[RawHit],
    hits_total: usize,
    search_truncated: bool,
    hint: String,
) -> Value {
    serde_json::json!({
        "mode": "full",
        "pattern": pattern,
        "intent": intent,
        "hits_total": hits_total,
        "hits_considered": hits_total,
        "returned": hits.len(),
        "hits": raw_hit_values(hits),
        "dropped": 0,
        "none_relevant": false,
        "search_truncated": search_truncated,
        "hint": hint,
    })
}

/// Bypass result: every hit, verbatim, no LLM involved.
pub fn bypass_payload(pattern: &str, intent: &str, hits: &[RawHit], search_truncated: bool) -> Value {
    full_payload(
        pattern,
        Some(intent),
        hits,
        hits.len(),
        search_truncated,
        "few enough hits to return whole — no filtering was applied".to_string(),
    )
}

/// No-intent result: pure structured search, capped at `max_hits`.  The LLM is
/// never consulted, so this succeeds with no model configured.
pub fn unfiltered_payload(
    pattern: &str,
    hits: &[RawHit],
    max_hits: usize,
    search_truncated: bool,
) -> Value {
    let hits_total = hits.len();
    let shown = &hits[..hits_total.min(max_hits)];
    let hint = if shown.len() < hits_total {
        format!(
            "no intent given — unfiltered search, showing first {} of {hits_total} hits; \
             raise --max-hits or add an intent to filter",
            shown.len()
        )
    } else {
        "no intent given — unfiltered search, no filtering applied".to_string()
    };
    full_payload(pattern, None, shown, hits_total, search_truncated, hint)
}

/// Filtered result.  `dropped` and the hint always expose the lossiness.
#[allow(clippy::too_many_arguments)]
pub fn rerank_payload(
    pattern: &str,
    intent: &str,
    hits_total: usize,
    hits_considered: usize,
    returned: Vec<Value>,
    dropped_invalid: usize,
    none_relevant: bool,
    search_truncated: bool,
) -> Value {
    let hint = if returned.is_empty() {
        format!(
            "the local model judged none of the {hits_total} hit(s) relevant to this intent — \
             this is a filter verdict, NOT an empty match set; grep '{pattern}' for all {hits_total} hits"
        )
    } else {
        format!(
            "filtered view — {} of {hits_total} hit(s); grep '{pattern}' for the unfiltered list",
            returned.len()
        )
    };
    serde_json::json!({
        "mode": "rerank",
        "pattern": pattern,
        "intent": intent,
        "hits_total": hits_total,
        "hits_considered": hits_considered,
        "returned": returned.len(),
        "hits": returned,
        "dropped": hits_total.saturating_sub(returned.len()),
        "dropped_invalid": dropped_invalid,
        "none_relevant": none_relevant && returned.is_empty(),
        "truncated_before_rerank": hits_total > hits_considered,
        "search_truncated": search_truncated,
        "hint": hint,
    })
}

/// Render the numbered hit list the model scores.  Ids are 1-based and global
/// across batches, so `first_id` is the batch's offset into the considered list.
pub fn render_hit_list(hits: &[RawHit], first_id: usize) -> String {
    let mut out = String::new();
    for (i, h) in hits.iter().enumerate() {
        out.push_str(&format!("[{}] {}:{}\n{}\n\n", first_id + i, h.file, h.line, h.context));
    }
    out
}

/// Merge a search hit with its selection metadata into JSON output.
fn materialize(hit: &RawHit, keep: &SelectedHit) -> Value {
    serde_json::json!({
        "file": hit.file,
        "line": hit.line,
        "text": hit.text,
        "context": hit.context,
        "why": keep.why,
        "score": keep.score,
    })
}

// ── Search-result parsing ────────────────────────────────────────────

/// Convert the search layer's results into `RawHit`s.
///
/// ct's version took a `ct::Response` and dug through `data.hits`; the plain
/// struct makes the same journey trivial, but the matched-line recovery below
/// is unchanged because the context block is rendered the same way.
pub fn parse_hits(results: &SearchResults, context_lines: usize) -> Vec<RawHit> {
    results
        .hits
        .iter()
        .map(|h| RawHit {
            file: h.file.clone(),
            line: h.line,
            text: matched_line(&h.text, h.line, context_lines).map(str::to_string),
            context: h.text.clone(),
        })
        .collect()
}

/// Recover the matched line from a context block.
///
/// `source::extract_context` renders `lines[i-N ..= i+N]`, so the matched line
/// sits at index `min(line - 1, N)` within the block.  The renderer truncates
/// the joined block at a byte budget measured from its start, so long
/// *preceding* lines can cut it before the matched line is ever reached —
/// return `None` in that case rather than mislabeling a surviving neighbour's
/// text as the match.
pub fn matched_line(context: &str, line: usize, context_lines: usize) -> Option<&str> {
    let block: Vec<&str> = context.split('\n').collect();
    let idx = (line.saturating_sub(1)).min(context_lines);
    let candidate = block.get(idx).copied()?;
    // The truncation marker is the renderer's, not the file's.
    if candidate == "... (truncated)" {
        return None;
    }
    Some(candidate)
}

// ── Small helpers ────────────────────────────────────────────────────

/// Fail open, naming this filter's fallback (the raw Grep tool).
fn fail(reason: &str) -> ToolError {
    ToolError::new(format!("scout grep: {reason}"), FALLBACK)
}

#[cfg(test)]
mod tests;
