//! `find` — intent-only search (docs/search-cli.md §5).
//!
//! `grep` makes the caller guess a pattern.  `find` removes that step: the
//! caller states what they want and the *local model* guesses the patterns.
//!
//! The model never searches.  It has exactly three jobs, all token-bounded:
//!
//! 1. **Guess patterns.**  One small call (`find_patterns` preset) sees the
//!    question and a paths-only sketch of the tree, and returns 3–8 candidates.
//! 2. **Judge hits.**  The union of what those patterns actually matched goes
//!    into `grep::rerank` — the existing stage, unchanged, with the question as
//!    the intent.
//! 3. **Judge the answer.**  One more small call (`find_reflect` preset) sees
//!    the question and the hits the rerank kept, and says whether they actually
//!    answer it — proposing better patterns when they do not.
//!
//! Stage 3 exists because stages 1–2 cannot fail *visibly*.  The synthesis
//! model guesses synonyms; the reranker scores each hit against the intent in
//! isolation.  A round of plausible-but-wrong hits therefore terminates as a
//! success, and nothing ever asks "did these results answer the question?".
//! The field case: `find "main rendering function for the waterslide view"`
//! returned an inner spectrogram helper because every guess was a synonym of
//! *render* and none was the question's own most distinctive word.  Two fixes
//! meet there — the question's own tokens are seeded as candidates
//! mechanically (`seed_candidates`), and the reflect stage re-searches for the
//! identifiers it can *see* in the excerpts (a comment naming
//! `draw_waterslide` is a pointer to the answer, and grepping that pointer is
//! the only way to reach a definition line that contains no "render" at all).
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
//! * A round is retried when *every* candidate whiffed (docs/search-cli.md §9: a
//!   thin-but-nonzero result is answerable by the rerank stage) **or** when the
//!   reflect stage says the kept hits miss the question.  Both kinds share the
//!   one `max_attempts` budget, so `--attempts 1` disables both.
//! * A refined round only ever *adds*: its hits are unioned with the previous
//!   round's survivors before the rerank, so a worse second guess cannot lose
//!   what the first one found.
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

    let tree =
        sketch(&source::list_paths(root, &base, find_cfg.tree_max_bytes), find_cfg.tree_max_bytes);

    let mut tried: Vec<String> = Vec::new();
    let mut whiffed: Vec<String> = Vec::new();
    let mut too_common: Vec<String> = Vec::new();
    // Every round's contributing patterns, joined into the payload's `pattern`
    // field — a refined round's hits are in the union, so its patterns belong
    // in the alternation that reproduces it.
    let mut label_parts: Vec<String> = Vec::new();
    let mut parse_error: Option<String> = None;
    // Carried across rounds: the hits that survived the guard so far, the
    // payload built from them, and the patterns the reflect stage asked for
    // next.  Together they make a refined round strictly additive.
    let mut prior: Vec<RawHit> = Vec::new();
    let mut prior_payload: Option<Value> = None;
    let mut refined: Option<Vec<Candidate>> = None;
    let mut truncated = false;

    for round in 1..=attempts {
        // Every call-log row this round writes is tagged with it, which is how
        // the log tells a first guess from a reflect-driven retry.
        ctx.attempt.set(round as u64);
        ctx.ledger.raw_bytes(tree.len() as u64);
        // Either the reflect stage already said what to search next, or this is
        // a synthesis round and the pattern preset gets asked.
        let mut from_reflect = false;
        let mut candidates = match refined.take() {
            Some(c) => {
                from_reflect = true;
                c
            }
            None => {
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

                let c = parse_candidates(&reply, find_cfg.max_patterns);
                if c.is_empty() {
                    // Nothing to search and nothing to tell the next round.
                    // Remember it: if no round ever produces patterns, that is
                    // an LLM failure (exit 2), not the "no pattern worked"
                    // verdict (exit 1).
                    parse_error = Some("local LLM proposed no usable patterns".to_string());
                    continue;
                }
                c
            }
        };
        // The question's own words are candidates too, and they cost no LLM
        // call at all — see `seed_candidates`.
        let guessed = candidates.len();
        candidates.extend(seed_candidates(&question, &candidates, &tried));
        live(ctx, round, "patterns", || patterns_event(&candidates, guessed, from_reflect));

        let (results, kept, search_truncated) =
            search_candidates(root, &base, &candidates, &cfg, &find_cfg);
        ctx.note(&trying_line(&results));
        truncated |= search_truncated;
        for r in &results {
            tried.push(r.pattern.clone());
            match r.fate {
                Fate::TooCommon => too_common.push(r.pattern.clone()),
                Fate::Whiffed | Fate::Unusable => whiffed.push(r.pattern.clone()),
                Fate::Kept => {}
            }
        }
        let round_label = alternation(&results);
        if !round_label.is_empty() {
            label_parts.push(round_label);
        }

        // A refined round adds to the previous round's survivors rather than
        // replacing them: the reflect stage asked for *more* evidence, and a
        // worse second guess must not lose what the first one found.
        let mut lists = kept;
        let previously = prior.len();
        lists.push(std::mem::take(&mut prior));
        let union = union_hits(lists);
        live(ctx, round, "hits", || {
            hits_event(&results, find_cfg.degenerate_hit_cap, &union, previously, truncated)
        });
        if union.is_empty() {
            continue; // every candidate whiffed — guess again if rounds remain
        }
        if union.len() == previously {
            // A refined round that found nothing new: re-reranking the same
            // list would spend a call to reproduce the answer we already have.
            return Ok(prior_payload.expect("a non-empty prior implies a prior payload"));
        }

        // Survivors flow into the existing rerank stage with the question as
        // the intent.  `pattern` is the alternation of the patterns that
        // actually contributed, so every message that quotes it — the hint,
        // the "no matches for" status line — hands back something the caller
        // can run verbatim as `scout grep --regex '<it>'`.
        let label = label_parts.join("|");
        let mut payload = crate::grep::rerank(
            ctx,
            &json!({"pattern": label, "intent": question, "max_hits": max_hits}),
            &cfg,
            &label,
            &question,
            &union,
            max_hits,
            truncated,
        )?;
        annotate(&mut payload, &tried, round);
        live(ctx, round, "rerank", || rerank_event(&payload, &label));

        // ── Reflect: did those hits actually answer the question? ────
        //
        // Only worth asking while a round remains to act on the answer, and
        // only about a non-empty keep list — there is nothing to read
        // identifiers out of otherwise.
        if reflect_due(&find_cfg, round, attempts, &payload) {
            if let Some(next) = reflect(ctx, &question, &payload, &tried, find_cfg.max_patterns) {
                ctx.note(&refining_line(&next));
                refined = Some(next);
                prior = union;
                prior_payload = Some(payload);
                continue;
            }
        }
        return Ok(payload);
    }

    if tried.is_empty() {
        // Every round failed to produce a single pattern: the model, not the
        // search, is what came up empty.  Fail open rather than reporting a
        // verdict scout never actually reached.
        return Err(fail(
            &parse_error.unwrap_or_else(|| "local LLM returned no patterns".to_string()),
        ));
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
    candidates_from(&v, max_patterns)
}

/// The `patterns` array of an already-parsed reply, as candidates.  Shared by
/// the synthesis reply and the reflect reply — both name their patterns the
/// same way, and both are produced by the same small model.
fn candidates_from(v: &Value, max_patterns: usize) -> Vec<Candidate> {
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

// ── Question-token seeds ─────────────────────────────────────────────

/// Words too common to discriminate: English function words, plus the generic
/// vocabulary of asking a question *about code* ("the main rendering
/// **function**", "**where** is the **config** **file** parsed").
///
/// Deliberately short.  The degenerate-pattern guard already throws away a
/// candidate that matches half the project, so this list is a latency
/// optimization, not a correctness mechanism — every word left off it costs at
/// most one wasted walk, while every distinctive word wrongly added to it
/// costs the answer.
const STOPWORDS: &[&str] = &[
    // English function words and common verbs
    "about",
    "after",
    "all",
    "and",
    "any",
    "are",
    "back",
    "been",
    "before",
    "being",
    "but",
    "call",
    "called",
    "calls",
    "can",
    "did",
    "does",
    "doing",
    "done",
    "for",
    "from",
    "get",
    "gets",
    "getting",
    "give",
    "had",
    "has",
    "have",
    "her",
    "here",
    "him",
    "his",
    "how",
    "into",
    "its",
    "just",
    "like",
    "make",
    "makes",
    "many",
    "more",
    "most",
    "much",
    "must",
    "not",
    "now",
    "off",
    "one",
    "only",
    "our",
    "out",
    "over",
    "own",
    "put",
    "puts",
    "same",
    "see",
    "set",
    "sets",
    "she",
    "should",
    "some",
    "such",
    "than",
    "that",
    "the",
    "their",
    "them",
    "then",
    "there",
    "these",
    "they",
    "thing",
    "things",
    "this",
    "those",
    "through",
    "under",
    "use",
    "used",
    "uses",
    "using",
    "very",
    "was",
    "way",
    "were",
    "what",
    "when",
    "where",
    "which",
    "while",
    "who",
    "why",
    "will",
    "with",
    "would",
    "you",
    "your",
    // Generic programming vocabulary — true of nearly every line of code
    "actual",
    "actually",
    "code",
    "codebase",
    "define",
    "defined",
    "defines",
    "definition",
    "entry",
    "file",
    "files",
    "fns",
    "func",
    "funcs",
    "function",
    "functions",
    "implement",
    "implementation",
    "implemented",
    "implements",
    "line",
    "lines",
    "logic",
    "main",
    "method",
    "methods",
    "module",
    "modules",
    "primary",
    "program",
    "project",
    "repo",
    "repository",
    "routine",
    "source",
    "stuff",
    "top",
    "value",
    "values",
];

/// Upper bound on seeded candidates.  Each one is a filesystem walk, and a
/// question with more than a handful of content words is describing something
/// too diffuse for its own vocabulary to pin down anyway.
const MAX_SEEDS: usize = 6;

/// The question's own distinctive words, lowercased, in the order they appear.
///
/// Split on anything that cannot appear in an identifier, so `draw_waterslide`
/// survives whole.  Words shorter than 3 characters and pure numbers are
/// dropped along with the stopwords: neither discriminates.
pub fn question_tokens(question: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for raw in question.split(|c: char| !(c.is_alphanumeric() || c == '_')) {
        let token = raw.to_lowercase();
        if token.chars().count() < 3
            || token.chars().all(|c| c.is_numeric())
            || STOPWORDS.contains(&token.as_str())
            || out.contains(&token)
        {
            continue;
        }
        out.push(token);
    }
    out
}

/// Turn the question's own words into candidates, alongside the model's guesses.
///
/// This is the mechanical half of the field fix.  The synthesis model reaches
/// for synonyms of what it thinks the question means (`render`, `draw`) and
/// routinely never tries the question's *own* most distinctive word — yet
/// `waterslide` as a literal pattern leads straight to `draw_waterslide`, via
/// the panel that calls it and the comments that name it.  A word the caller
/// typed is evidence; a synonym is a hypothesis.
///
/// Seeds are candidates like any other, so the degenerate-pattern guard
/// disposes of the useless ones (a 0-hit typo, an everywhere-word) before the
/// model sees a thing.  The only cost is search time, and it is bounded by
/// `MAX_SEEDS`.
///
/// **Case:** seeded as a case-insensitive regex (`(?i)waterslide`) rather than
/// as a literal.  Identifiers vary in case around a single concept —
/// `waterslide`, `WaterslideView`, `WATERSLIDE_BINS` — and the caller typed one
/// spelling of a word, not one spelling of an identifier.  The tokenizer only
/// ever emits `[A-Za-z0-9_]`, so a seed can never carry regex metacharacters.
///
/// Deduped against the model's guesses for this round *and* against everything
/// already tried, so a re-round never re-walks the tree for a pattern whose
/// answer is already known.
pub fn seed_candidates(question: &str, guesses: &[Candidate], tried: &[String]) -> Vec<Candidate> {
    let mut out: Vec<Candidate> = Vec::new();
    for token in question_tokens(question) {
        if out.len() >= MAX_SEEDS {
            break;
        }
        let pattern = format!("(?i){token}");
        let known =
            guesses.iter().map(|c| c.pattern.as_str()).chain(tried.iter().map(String::as_str));
        if known.into_iter().any(|p| same_pattern(p, &pattern)) {
            continue;
        }
        out.push(Candidate { pattern, regex: true, ..Default::default() });
    }
    out
}

/// Are these two patterns the same search?  Compares case-insensitively and
/// ignores a leading `(?i)`, so a seeded `(?i)waterslide` and a model-proposed
/// `waterslide` count as one pattern rather than two identical walks.
fn same_pattern(a: &str, b: &str) -> bool {
    let bare = |s: &str| s.trim_start_matches("(?i)").to_ascii_lowercase();
    bare(a) == bare(b)
}

// ── Reflect and refine ───────────────────────────────────────────────

/// The reflect stage's verdict.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reflection {
    /// Do the hits shown actually answer the question?
    pub answered: bool,
    /// What to search instead.  Only meaningful when `answered` is false.
    pub patterns: Vec<Candidate>,
}

/// Patterns requested from the reflect stage.  Smaller than the synthesis
/// budget on purpose: this stage is not brainstorming, it is naming the one or
/// two identifiers it can see in the excerpts.
const REFLECT_MAX_PATTERNS: usize = 4;

/// Parse the reflect reply, as forgiving as every other selector parse.
///
/// `answered` defaults to **true** when absent or unreadable: this stage exists
/// to catch a wrong answer, and the cost of missing one is the status quo,
/// while the cost of a spurious "no" is a needless round.  `None` means nothing
/// parsed at all — the caller treats that identically to "answered".
pub fn parse_reflection(text: &str, max_patterns: usize) -> Option<Reflection> {
    let v = parse_selector_json(text)?;
    let answered = v.get("answered").and_then(as_flag).unwrap_or(true);
    let patterns = if answered { Vec::new() } else { candidates_from(&v, max_patterns) };
    Some(Reflection { answered, patterns })
}

/// Read a boolean that a small model may have spelled as a string.
fn as_flag(v: &Value) -> Option<bool> {
    match v {
        Value::Bool(b) => Some(*b),
        Value::String(s) => match s.trim().to_ascii_lowercase().as_str() {
            "true" | "yes" | "y" => Some(true),
            "false" | "no" | "n" => Some(false),
            _ => None,
        },
        _ => None,
    }
}

/// Is the reflect stage worth running for this round?
///
/// Three ways it is not: the knob is off; this is the last allowed round, so
/// there is no round left to act on a "no" and the call would be pure latency;
/// or the rerank kept nothing, so there are no excerpts to read identifiers out
/// of.  The round check is where `max_attempts` becomes a *shared* budget —
/// whiff-retries and reflect-retries draw on the same count.
fn reflect_due(
    find_cfg: &crate::filter_config::FindConfig,
    round: usize,
    attempts: usize,
    payload: &Value,
) -> bool {
    find_cfg.reflect
        && round < attempts
        && payload.get("hits").and_then(Value::as_array).is_some_and(|h| !h.is_empty())
}

/// Ask whether the kept hits answer the question; return the patterns to search
/// next, or `None` to keep what we have.
///
/// Every uncertain outcome — an LLM error, an unparseable reply, a refusal to
/// name new patterns, patterns that were all searched already — returns `None`
/// via `next_patterns`.  The stage can only ever *add* a round; it can never
/// turn a result into no result.
fn reflect(
    ctx: &Ctx,
    question: &str,
    payload: &Value,
    tried: &[String],
    max_patterns: usize,
) -> Option<Vec<Candidate>> {
    let hits = payload.get("hits").and_then(Value::as_array)?;
    let budget = REFLECT_MAX_PATTERNS.min(max_patterns.max(1));
    let reply = call_preset(
        ctx,
        "find_reflect",
        &json!({
            "question": question,
            "hit_list": reflect_hit_list(hits),
            "max_patterns": budget,
        }),
    )
    .ok()?;
    let reflection = parse_reflection(&reply, budget);
    let next = next_patterns(reflection.clone(), tried);
    // The round is the one `run` set on the ledger's counter before this
    // iteration — reading it back is cheaper than threading it down here, and
    // it is the same number every log row of the round already carries.
    live(ctx, ctx.attempt.get() as usize, "reflect", || {
        reflect_event(reflection.as_ref(), next.as_deref())
    });
    next
}

/// Turn a reflection into the next round's candidates, or `None` to stop.
///
/// The whole loop rule, as one pure function: stop on "answered", stop on a
/// reply that did not parse (`None` — fail toward returning what we have), stop
/// when the patterns proposed were all searched already, since re-searching
/// them would spend a round reproducing the result we are looking at.
fn next_patterns(reflection: Option<Reflection>, tried: &[String]) -> Option<Vec<Candidate>> {
    let reflection = reflection?;
    if reflection.answered {
        return None;
    }
    let next: Vec<Candidate> = reflection
        .patterns
        .into_iter()
        .filter(|c| !tried.iter().any(|t| same_pattern(t, &c.pattern)))
        .collect();
    (!next.is_empty()).then_some(next)
}

/// Render the kept hits for the reflect stage: the same numbered shape the
/// reranker sees, built from the payload rather than from `RawHit`s because
/// the payload is what "kept" means by this point.
///
/// Already token-bounded by construction — at most `max_hits` hits, each with a
/// context block the search layer capped at `context_max_bytes`.  The
/// `(code)` / `(comment)` tag is the same lexical hint the rerank list carries,
/// and it matters more here: "these are all comments *mentioning* the thing" is
/// exactly the shape of a near-miss this stage is meant to catch.
pub fn reflect_hit_list(hits: &[Value]) -> String {
    let mut out = String::new();
    for (i, h) in hits.iter().enumerate() {
        let tag = match crate::grep::line_kind(h.get("text").and_then(Value::as_str)) {
            Some(kind) => format!(" ({kind})"),
            None => String::new(),
        };
        out.push_str(&format!(
            "[{}] {}:{}{tag}\n{}\n\n",
            i + 1,
            h.get("file").and_then(Value::as_str).unwrap_or("?"),
            h.get("line").and_then(Value::as_u64).unwrap_or(0),
            h.get("context").and_then(Value::as_str).unwrap_or(""),
        ));
    }
    out
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
            results.push(CandidateResult {
                pattern: c.pattern.clone(),
                fate: Fate::Unusable,
                hits: 0,
            });
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

/// The degenerate-pattern guard (docs/search-cli.md §5 step 2).
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

/// The stderr line for one round (docs/search-cli.md §5): what was tried and what fell
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

/// The stderr line for a reflect-driven retry: the verdict and what it wants
/// searched instead.  `results may be off-target — refining with: draw_waterslide`
///
/// There is deliberately no line for the other verdict.  "Answered" is the
/// common case and the normal outcome; announcing it would be noise on every
/// successful run.
pub fn refining_line(next: &[Candidate]) -> String {
    let names: Vec<&str> = next.iter().map(|c| c.pattern.as_str()).collect();
    format!("results may be off-target — refining with: {}", names.join(", "))
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
/// Paths only, never symbols or contents (docs/search-cli.md §9): symbol names would
/// reintroduce a source-parsing dependency scout deliberately does not have.  Truncation is
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

// ── Live channel (docs/dashboard.md §2, P4) ───────────────────────────
//
// Observability only.  Every value below is one the round already computed —
// nothing here searches, calls, or decides anything, and a `find` behaves
// identically whether or not a dashboard is listening.
//
// The four events are the ones the *log* serves worst: it records a round as
// three unrelated-looking preset rows, and the interesting part — which guesses
// whiffed, which matched everything, what the reranker kept and why, what the
// reflect stage made of it — never reaches disk at all.

/// Emit one `find.*` event, if anyone is listening.
///
/// `fields` is a closure so an unwatched run never builds the payload: the
/// cost of instrumenting a round with no dashboard attached is one cached
/// socket check per event. A silent ledger (test fixtures) emits nothing, the
/// same rule `call.start` follows.
fn live(ctx: &Ctx, round: usize, kind: &str, fields: impl FnOnce() -> Value) {
    if ctx.ledger.is_silent() || !crate::live::is_listening() {
        return;
    }
    crate::live::emit_find(ctx.ledger.op(), round as u64, kind, fields());
}

/// `find.patterns` — what this round is about to search for.
///
/// `seed` separates the model's guesses from the question's own words
/// (`seed_candidates`), which is the distinction worth watching: a run whose
/// answer only ever comes from seeds is a run whose synthesis prompt is not
/// earning its call.
fn patterns_event(candidates: &[Candidate], guessed: usize, from_reflect: bool) -> Value {
    json!({
        "source": if from_reflect { "reflect" } else { "synthesis" },
        "patterns": candidates
            .iter()
            .enumerate()
            .map(|(i, c)| json!({
                "pattern": c.pattern,
                "regex": c.regex,
                "seed": i >= guessed,
                "types": c.types,
                "globs": c.globs,
            }))
            .collect::<Vec<_>>(),
    })
}

/// `find.hits` — what each pattern actually matched, and what the guard did.
fn hits_event(
    results: &[CandidateResult],
    cap: usize,
    union: &[RawHit],
    previously: usize,
    truncated: bool,
) -> Value {
    json!({
        "degenerate_hit_cap": cap,
        "union": union.len(),
        // A refined round only ever adds (see the module header), so "new" is
        // the number that says whether it was worth making.
        "new": union.len().saturating_sub(previously),
        "carried": previously,
        "search_truncated": truncated,
        "candidates": results
            .iter()
            .map(|r| json!({
                "pattern": r.pattern,
                "hits": r.hits,
                "fate": fate_name(r.fate),
                "dropped": r.fate != Fate::Kept,
                "why": drop_reason(r, cap),
            }))
            .collect::<Vec<_>>(),
    })
}

fn fate_name(fate: Fate) -> &'static str {
    match fate {
        Fate::Kept => "kept",
        Fate::Whiffed => "whiffed",
        Fate::TooCommon => "too_common",
        Fate::Unusable => "unusable",
    }
}

/// Why the guard threw this candidate away, in the words the stderr line uses.
fn drop_reason(r: &CandidateResult, cap: usize) -> Value {
    match r.fate {
        Fate::Kept => Value::Null,
        Fate::Whiffed => json!("matched nothing"),
        Fate::TooCommon => {
            json!(format!(
                "{} hits, past the degenerate cap of {cap} — matches too much to discriminate",
                r.hits
            ))
        }
        Fate::Unusable => json!("the search refused this pattern"),
    }
}

/// `find.rerank` — the keeps, with their scores and `why`.
///
/// Read out of the payload the rerank just returned rather than recomputed:
/// this is the same list the caller is about to receive.
fn rerank_event(payload: &Value, pattern: &str) -> Value {
    let keeps: Vec<Value> = payload
        .get("hits")
        .and_then(Value::as_array)
        .map(|hits| {
            hits.iter()
                .map(|h| {
                    json!({
                        "file": h.get("file"),
                        "line": h.get("line"),
                        "score": h.get("score"),
                        "why": h.get("why"),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    json!({
        "pattern": pattern,
        "hits_total": payload.get("hits_total"),
        "hits_considered": payload.get("hits_considered"),
        "returned": payload.get("returned"),
        "dropped": payload.get("dropped"),
        "none_relevant": payload.get("none_relevant"),
        "keeps": keeps,
    })
}

/// `find.reflect` — the verdict, and the patterns it named.
///
/// `patterns` is everything the stage asked for; `refining` is what survived
/// the already-tried filter and will actually be searched. They differ exactly
/// when the stage went in a circle, which is worth seeing.
fn reflect_event(reflection: Option<&Reflection>, next: Option<&[Candidate]>) -> Value {
    let names = |cs: &[Candidate]| cs.iter().map(|c| c.pattern.clone()).collect::<Vec<_>>();
    json!({
        "parsed": reflection.is_some(),
        // Unparseable reads as "answered", which is what the loop does with it.
        "answered": reflection.map(|r| r.answered).unwrap_or(true),
        "patterns": reflection.map(|r| names(&r.patterns)).unwrap_or_default(),
        "refining": next.map(names).unwrap_or_default(),
    })
}

// ── Small helpers ────────────────────────────────────────────────────

/// Fail open, naming an explicit `scout grep` as the fallback.
fn fail(reason: &str) -> ToolError {
    ToolError::new(format!("scout find: {reason}"), FALLBACK)
}

#[cfg(test)]
mod tests;
