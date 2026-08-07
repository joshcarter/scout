//! `find` — intent-only search (SPEC-cli §5).
//!
//! `grep` makes the caller guess a pattern.  `find` removes that step: the
//! caller states what they want and the *local model* guesses the patterns.
//!
//! The model never searches.  It has exactly two jobs, both token-bounded:
//!
//! 1. **Guess patterns.**  One small call (`find_patterns` preset) sees the
//!    question and a paths-only sketch of the tree, and returns 3–8 candidates.
//! 2. **Judge hits.**  The union of what those patterns actually matched goes
//!    into `grep::rerank` — the existing stage, unchanged, with the question as
//!    the intent.
//!
//! Everything between the two is mechanical: scout runs each candidate through
//! `source::search` itself, unions and dedupes by `(file, line)`, and applies
//! the **degenerate-pattern guard** — 0 hits means the guess whiffed and is
//! dropped silently; more than `degenerate_hit_cap` hits means the guess is a
//! bad discriminator (the moral equivalent of low IDF: `parse` in a parser) and
//! *all* of its hits are dropped before the model ever sees them.
//!
//! Invariants, inherited or added:
//!
//! * Returned hits always come from a real search.  A hallucinated pattern
//!   costs one wasted walk; it can never contribute a hit that isn't there.
//! * A model hint (`types` / `globs`) may only *narrow* — never past the
//!   caller's own `-t` / `-g` flags.  See `candidate_options`.
//! * Retry only when *every* candidate whiffed (SPEC §9: a thin-but-nonzero
//!   result is answerable by the rerank stage), capped at `max_attempts`.
//! * Nothing skips the rerank.  `grep` returns a short hit list whole because
//!   the caller chose that pattern on purpose; here nobody did, so a short list
//!   is a short list of *guesses* and judging it is the entire verb.
//! * Unlike `grep`, this verb requires a configured LLM — there is no pattern
//!   to fall back on — so a missing config fails open naming `scout grep`.

use std::path::Path;

use serde_json::{json, Value};

use crate::filter_config::GrepConfig;
use crate::grep::RawHit;
use crate::select::{call_preset, non_empty_arg, parse_selector_json, Ctx, ToolError, ToolResult};
use crate::source::{self, SearchOptions};

/// What to reach for whenever this verb cannot deliver.  `find` exists to spare
/// the caller a pattern, so its failure mode is: here, have the pattern back.
const FALLBACK: &str = "`scout grep <pattern>` with a pattern of your own";

/// Upper bound on `max_hits`, matching `grep`'s.
const MAX_HITS_CEILING: usize = 100;

// ── Candidates ───────────────────────────────────────────────────────

/// One pattern the model proposed, with its optional narrowing hints.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Candidate {
    pub pattern: String,
    pub regex: bool,
    /// ripgrep type names (`rust`), advisory — see `candidate_options`.
    pub types: Vec<String>,
    /// ripgrep-syntax globs (`src/**`), advisory — see `candidate_options`.
    pub globs: Vec<String>,
}

/// What became of one candidate's search.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fate {
    /// Hits survived the guard and reach the reranker.
    Kept,
    /// Zero hits: a wrong guess.  Dropped silently, reported as "whiffed".
    Whiffed,
    /// Past `degenerate_hit_cap`: a bad discriminator.  Every one of its hits
    /// is dropped — a pattern that matches everything distinguishes nothing.
    TooCommon,
    /// The search itself refused the pattern (e.g. `regex: true` on a broken
    /// expression).  Never fatal: one bad guess must not sink the round.
    Unusable,
}

/// One candidate's outcome, for the stderr line and the retry prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CandidateResult {
    pub pattern: String,
    pub fate: Fate,
    /// Hits the search returned, before the guard dropped them (if it did).
    pub hits: usize,
}

// ── Entry point ──────────────────────────────────────────────────────

/// Answer a natural-language question by guessing patterns, searching for them,
/// and reranking the union against the question.
pub fn run(ctx: &Ctx, args: &Value) -> ToolResult {
    let (_, mut cfg) = crate::filter_config::load();
    let find_cfg = crate::filter_config::load_find();

    // `-C` / `[cli] context`, exactly as `grep::run` takes it.
    if let Some(n) = args.get("context_lines").and_then(Value::as_u64) {
        cfg.context_lines = n as usize;
    }

    // The one knob `find` sets differently from `grep`, and the reason is the
    // whole premise of the verb: grep's bypass exists because a *deliberate*
    // pattern with few hits is worth seeing whole, while find's patterns are
    // guesses — a handful of hits from a guess is exactly what needs judging,
    // and it is the cheapest possible rerank call.  Everything else about the
    // stage, its budgets and its payloads, is untouched.
    cfg.bypass_max_hits = 0;

    let question = non_empty_arg(args, "question")
        .ok_or_else(|| fail("'question' argument is required and must be non-empty"))?;
    let max_hits = args
        .get("max_hits")
        .and_then(Value::as_u64)
        .map(|n| n as usize)
        .unwrap_or(10)
        .clamp(1, MAX_HITS_CEILING);
    // `--attempts` overrides `[find] max_attempts`; 1 means "no retry".
    let attempts = args
        .get("attempts")
        .and_then(Value::as_u64)
        .map(|n| n as usize)
        .unwrap_or(find_cfg.max_attempts)
        .max(1);

    let root = Path::new(&ctx.project);
    // The caller's own filters are the baseline for every candidate search —
    // and for the tree sketch, so a `-t rust` run does not spend its byte
    // budget describing files it will never search.  Validated before the
    // client check so a typo in `-t` reads as a typo whatever the config is
    // doing.
    let base = crate::grep::search_options(&cfg, false, root, args).map_err(|e| fail(&e))?;
    // The model is not optional here.  Checking up front means a missing
    // config costs no walk and reads as a config problem, not a failed search.
    ctx.require_client().map_err(|e| fail(&e))?;

    let tree = sketch(&source::list_paths(root, &base, find_cfg.tree_max_bytes), find_cfg.tree_max_bytes);

    let mut tried: Vec<String> = Vec::new();
    let mut whiffed: Vec<String> = Vec::new();
    let mut too_common: Vec<String> = Vec::new();
    let mut parse_error: Option<String> = None;

    for round in 1..=attempts {
        let reply = call_preset(
            ctx,
            "find_patterns",
            &json!({
                "question": question,
                "tree": tree,
                "max_patterns": find_cfg.max_patterns,
                "failed": retry_note(&whiffed, &too_common),
            }),
        )
        .map_err(|e| fail(&e))?;

        let candidates = parse_candidates(&reply, find_cfg.max_patterns);
        if candidates.is_empty() {
            // Nothing to search and nothing to tell the next round.  Remember
            // it: if no round ever produces patterns, that is an LLM failure
            // (exit 2), not the "no pattern worked" verdict (exit 1).
            parse_error = Some("local LLM proposed no usable patterns".to_string());
            continue;
        }

        let (results, kept, search_truncated) = search_candidates(root, &base, &candidates, &cfg, &find_cfg);
        ctx.note(&trying_line(&results));
        for r in &results {
            tried.push(r.pattern.clone());
            match r.fate {
                Fate::TooCommon => too_common.push(r.pattern.clone()),
                Fate::Whiffed | Fate::Unusable => whiffed.push(r.pattern.clone()),
                Fate::Kept => {}
            }
        }

        let union = union_hits(kept);
        if union.is_empty() {
            continue; // every candidate whiffed — guess again if rounds remain
        }

        // Survivors flow into the existing rerank stage with the question as
        // the intent.  `pattern` is the alternation of the patterns that
        // actually contributed, so every message that quotes it — the hint,
        // the "no matches for" status line — hands back something the caller
        // can run verbatim as `scout grep --regex '<it>'`.
        let label = alternation(&results);
        let mut payload = crate::grep::rerank(
            ctx,
            &json!({"pattern": label, "intent": question, "max_hits": max_hits}),
            &cfg,
            &label,
            &question,
            &union,
            max_hits,
            search_truncated,
        )?;
        annotate(&mut payload, &tried, round);
        return Ok(payload);
    }

    if tried.is_empty() {
        // Every round failed to produce a single pattern: the model, not the
        // search, is what came up empty.  Fail open rather than reporting a
        // verdict scout never actually reached.
        return Err(fail(&parse_error.unwrap_or_else(|| "local LLM returned no patterns".to_string())));
    }
    Ok(whiff_payload(&question, &tried, attempts))
}

// ── Candidate parsing ────────────────────────────────────────────────

/// Parse the pattern-synthesis reply into candidates.
///
/// As forgiving as `parse_selector_json` (which it delegates the fence/prose
/// stripping to), because the model on the other end is small:
///
/// * `{"patterns": ["a", "b"]}` — bare strings are accepted alongside objects.
/// * `type` / `glob` are accepted as aliases for `types` / `globs`, and either
///   may be a single string instead of an array.
/// * Blank patterns and exact duplicates are dropped; the list is capped at
///   `max_patterns` so a runaway reply cannot turn into a hundred walks.
///
/// A reply that yields nothing returns an empty vec — the caller decides
/// whether that is a retry or a failure.
pub fn parse_candidates(text: &str, max_patterns: usize) -> Vec<Candidate> {
    let Some(v) = parse_selector_json(text) else { return Vec::new() };
    // `patterns` is what the preset asks for; `candidates` is the plausible
    // near-miss, and accepting it costs one line.
    let items = ["patterns", "candidates"]
        .iter()
        .find_map(|k| v.get(*k).and_then(Value::as_array))
        .cloned()
        .unwrap_or_default();

    let mut out: Vec<Candidate> = Vec::new();
    for item in items {
        let candidate = match &item {
            Value::String(s) => Candidate { pattern: s.trim().to_string(), ..Default::default() },
            Value::Object(_) => Candidate {
                pattern: item
                    .get("pattern")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .trim()
                    .to_string(),
                regex: item.get("regex").and_then(Value::as_bool).unwrap_or(false),
                types: string_list(&item, &["types", "type"]),
                globs: string_list(&item, &["globs", "glob"]),
            },
            _ => continue,
        };
        if candidate.pattern.is_empty() || out.iter().any(|c| c.pattern == candidate.pattern) {
            continue;
        }
        out.push(candidate);
        if out.len() >= max_patterns {
            break;
        }
    }
    out
}

/// Read a string-or-array-of-strings field under any of `keys`.  Anything else
/// reads as empty, and blank entries are dropped — a stray `""` glob would
/// otherwise compile into a match-nothing override.
fn string_list(item: &Value, keys: &[&str]) -> Vec<String> {
    for key in keys {
        match item.get(*key) {
            Some(Value::String(s)) if !s.trim().is_empty() => return vec![s.trim().to_string()],
            Some(Value::Array(a)) => {
                let list: Vec<String> = a
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
                    .collect();
                if !list.is_empty() {
                    return list;
                }
            }
            _ => {}
        }
    }
    Vec::new()
}

// ── Mechanical search ────────────────────────────────────────────────

/// Run every candidate, apply the guard, and return the per-candidate verdicts
/// alongside the hit lists that survived it.
fn search_candidates(
    root: &Path,
    base: &SearchOptions,
    candidates: &[Candidate],
    cfg: &GrepConfig,
    find_cfg: &crate::filter_config::FindConfig,
) -> (Vec<CandidateResult>, Vec<Vec<RawHit>>, bool) {
    let mut results = Vec::with_capacity(candidates.len());
    let mut kept: Vec<Vec<RawHit>> = Vec::new();
    let mut search_truncated = false;

    for c in candidates {
        let opts = candidate_options(base, c, root);
        let Ok(found) = source::search(root, &c.pattern, &opts) else {
            results.push(CandidateResult { pattern: c.pattern.clone(), fate: Fate::Unusable, hits: 0 });
            continue;
        };
        search_truncated |= found.truncated;
        let hits = crate::grep::parse_hits(&found, cfg.context_lines);
        let fate = guard(hits.len(), find_cfg.degenerate_hit_cap);
        if fate == Fate::Kept {
            kept.push(hits.clone());
        }
        results.push(CandidateResult { pattern: c.pattern.clone(), fate, hits: hits.len() });
    }
    (results, kept, search_truncated)
}

/// The degenerate-pattern guard (SPEC-cli §5 step 2).
///
/// The cap is inclusive — a candidate with exactly `cap` hits is kept, since
/// the config value reads as "more lines than this is too many".
pub fn guard(hit_count: usize, cap: usize) -> Fate {
    match hit_count {
        0 => Fate::Whiffed,
        n if n > cap => Fate::TooCommon,
        _ => Fate::Kept,
    }
}

/// Apply a candidate's hints on top of the caller's filters.
///
/// **Hints narrow, never widen.**  The rule is per-dimension and deliberately
/// coarse: in a dimension the caller already constrained, the model's hint is
/// ignored outright.  Merging would widen — `TypesBuilder::select` is a union,
/// and an `Override` resolves last-match-wins, so a model include glob would
/// happily re-admit a directory the caller's `--exclude-dir` had just removed.
/// A hint that fails to compile (an unknown type name) is discarded too: it is
/// advice, not a request, and losing it costs a broader search, not a wrong one.
fn candidate_options(base: &SearchOptions, c: &Candidate, root: &Path) -> SearchOptions {
    let mut opts = base.clone();
    opts.regex = c.regex;
    if base.types.is_none() && !c.types.is_empty() {
        opts.types = source::build_types(&c.types, &[]).unwrap_or(None);
    }
    if base.overrides.is_none() && !c.globs.is_empty() {
        opts.overrides = source::build_overrides(root, &c.globs).unwrap_or(None);
    }
    opts
}

/// Merge every surviving candidate's hits into one list, deduped by
/// `(file, line)` — the same line found by two patterns is one hit, not two.
///
/// Sorted by `(file, line)` so the reranker's positional ids are stable across
/// runs no matter what order the model happened to propose its patterns in.
pub fn union_hits(per_candidate: Vec<Vec<RawHit>>) -> Vec<RawHit> {
    let mut all: Vec<RawHit> = per_candidate.into_iter().flatten().collect();
    all.sort_by(|a, b| a.file.cmp(&b.file).then(a.line.cmp(&b.line)));
    all.dedup_by(|a, b| a.file == b.file && a.line == b.line);
    all
}

// ── Reporting ────────────────────────────────────────────────────────

/// The stderr line for one round (SPEC-cli §5): what was tried and what fell
/// away.  `trying: config, toml, load_config, from_str · 2 whiffed`
pub fn trying_line(results: &[CandidateResult]) -> String {
    let names: Vec<&str> = results.iter().map(|r| r.pattern.as_str()).collect();
    let mut line = format!("trying: {}", names.join(", "));
    let count = |f: Fate| results.iter().filter(|r| r.fate == f).count();
    // Unusable is a whiff from the caller's chair: the guess produced nothing.
    let whiffed = count(Fate::Whiffed) + count(Fate::Unusable);
    if whiffed > 0 {
        line.push_str(&format!(" · {whiffed} whiffed"));
    }
    let common = count(Fate::TooCommon);
    if common > 0 {
        line.push_str(&format!(" · {common} matched too much to discriminate"));
    }
    line
}

/// The sentence handed back to the preset on a retry, naming what already
/// failed and *how* — a pattern that matched nothing and one that matched
/// everything call for opposite corrections.  Empty on the first round.
pub fn retry_note(whiffed: &[String], too_common: &[String]) -> String {
    let mut parts: Vec<String> = Vec::new();
    if !whiffed.is_empty() {
        parts.push(format!("matched nothing: {}", whiffed.join(", ")));
    }
    if !too_common.is_empty() {
        parts.push(format!("matched far too much to be useful: {}", too_common.join(", ")));
    }
    if parts.is_empty() {
        return String::new();
    }
    format!(
        "\nThese patterns were already tried and did not work — propose different ones ({}).",
        parts.join("; ")
    )
}

/// The `pattern` field for the payload: the patterns that actually contributed
/// hits, as a regex alternation.  Runnable as-is via `scout grep --regex`.
fn alternation(results: &[CandidateResult]) -> String {
    results
        .iter()
        .filter(|r| r.fate == Fate::Kept)
        .map(|r| r.pattern.as_str())
        .collect::<Vec<_>>()
        .join("|")
}

/// Tag a payload as `find`'s, so the CLI's status lines can say
/// "try scout grep" instead of grep's "--no-filter" advice.  Additive: every
/// key the renderer and `grep_status` already read is untouched.
fn annotate(payload: &mut Value, tried: &[String], attempts: usize) {
    payload["find_patterns"] = json!(tried);
    payload["find_attempts"] = json!(attempts);
}

/// Every attempt whiffed.  This is a *result*, not an error — the search ran,
/// it just had nothing to show — so it is a well-formed empty payload the
/// renderer accepts, and the CLI turns it into exit 1.
pub fn whiff_payload(question: &str, tried: &[String], attempts: usize) -> Value {
    let mut payload = json!({
        "mode": "full",
        "pattern": tried.join("|"),
        "intent": question,
        "hits_total": 0,
        "hits_considered": 0,
        "returned": 0,
        "hits": [],
        "dropped": 0,
        "none_relevant": false,
        "search_truncated": false,
        "hint": format!(
            "no pattern guess produced hits after {attempts} attempt(s) — the patterns tried were: {}; \
             grep an explicit pattern instead",
            tried.join(", ")
        ),
    });
    annotate(&mut payload, tried, attempts);
    payload
}

// ── Tree sketch ──────────────────────────────────────────────────────

/// Render the paths-only project sketch, truncated to `max_bytes`.
///
/// Paths only, never symbols or contents (SPEC §9): symbol names would
/// reintroduce the parsing dependency PLAN §1 deliberately cut.  Truncation is
/// on a line boundary — half a path is worse than one path fewer — and it is
/// announced, so the model is told the list is partial rather than concluding
/// that an unlisted file does not exist.
/// The whole rendered sketch — marker included — fits in `max_bytes`.
pub fn sketch(paths: &[String], max_bytes: usize) -> String {
    const MARKER: &str = "... (truncated)\n";
    let mut lines: Vec<&str> = Vec::new();
    let mut used = 0usize;
    let mut truncated = false;
    for p in paths {
        // +1 for the newline this line will need.
        if used + p.len() + 1 > max_bytes {
            truncated = true;
            break;
        }
        used += p.len() + 1;
        lines.push(p);
    }
    if truncated {
        // Give the marker room by dropping paths, not by overrunning the cap.
        while !lines.is_empty() && used + MARKER.len() > max_bytes {
            used -= lines.pop().map(|l| l.len() + 1).unwrap_or(0);
        }
    }
    let mut out = String::with_capacity(used);
    for l in lines {
        out.push_str(l);
        out.push('\n');
    }
    if truncated && out.len() + MARKER.len() <= max_bytes {
        out.push_str(MARKER);
    }
    out
}

// ── Small helpers ────────────────────────────────────────────────────

/// Fail open, naming an explicit `scout grep` as the fallback.
fn fail(reason: &str) -> ToolError {
    ToolError::new(format!("scout find: {reason}"), FALLBACK)
}

#[cfg(test)]
mod tests;
