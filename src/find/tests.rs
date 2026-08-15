//! Tests for `find`: candidate parsing, the degenerate guard, union/dedupe,
//! the tree sketch's byte budget, the retry-trigger logic, and the fail-open
//! contract.
//!
//! There is no mock LLM in this repo (see `grep/tests.rs`): the model-facing
//! paths are exercised through an offline `Ctx`, which makes `call_preset`
//! fail, and everything else is written as a pure function precisely so it can
//! be tested without one.

use super::*;
use crate::filter_config::FindConfig;
use crate::select::Ctx;

fn offline_ctx(project: &str) -> Ctx<'static> {
    Ctx {
        client_error: Some("no config in tests".into()),
        project: project.to_string(),
        // A test must never append to the developer's own call log.
        ledger: crate::stats::Ledger::silent(),
        ..Default::default()
    }
}

fn raw(file: &str, line: usize) -> RawHit {
    RawHit {
        file: file.to_string(),
        line,
        text: Some(format!("{file}:{line}")),
        col: Some(0),
        col_end: Some(0),
        context: format!("{file}:{line}"),
    }
}

fn result(pattern: &str, fate: Fate, hits: usize) -> CandidateResult {
    CandidateResult { pattern: pattern.to_string(), fate, hits }
}

// ── Candidate parsing ────────────────────────────────────────────────

#[test]
fn candidates_parse_from_a_clean_reply() {
    let c = parse_candidates(
        r#"{"patterns": [
            {"pattern": "load_config", "regex": false, "types": ["rust"]},
            {"pattern": "from_str", "globs": ["src/**"]}
        ]}"#,
        8,
    );
    assert_eq!(c.len(), 2);
    assert_eq!(c[0].pattern, "load_config");
    assert_eq!(c[0].types, vec!["rust"]);
    assert!(c[0].globs.is_empty() && !c[0].regex);
    assert_eq!(c[1].globs, vec!["src/**"]);
}

#[test]
fn candidates_survive_fences_prose_and_think_blocks() {
    // The same robustness `parse_selector_json` gives the rerank stage — the
    // model on the other end is small and does not reliably obey "JSON only".
    for reply in [
        "```json\n{\"patterns\": [\"load_config\"]}\n```",
        "<think>hmm, config…</think>{\"patterns\": [\"load_config\"]}",
        "Sure! Here are the patterns:\n{\"patterns\": [\"load_config\"]}\nHope that helps.",
        "{\n  \"patterns\": [ {\"pattern\": \"load_config\"} ]\n}",
    ] {
        let c = parse_candidates(reply, 8);
        assert_eq!(c.len(), 1, "reply: {reply}");
        assert_eq!(c[0].pattern, "load_config", "reply: {reply}");
    }
}

#[test]
fn bare_strings_are_accepted_alongside_objects() {
    let c = parse_candidates(r#"{"patterns": ["a_thing", {"pattern": "b_thing"}]}"#, 8);
    assert_eq!(c.iter().map(|c| c.pattern.as_str()).collect::<Vec<_>>(), vec!["a_thing", "b_thing"]);
}

#[test]
fn singular_hint_keys_and_scalar_hints_are_tolerated() {
    let c = parse_candidates(
        r#"{"patterns": [{"pattern": "x", "type": "rust", "glob": "src/**", "regex": true}]}"#,
        8,
    );
    assert_eq!(c[0].types, vec!["rust"]);
    assert_eq!(c[0].globs, vec!["src/**"]);
    assert!(c[0].regex);
}

#[test]
fn blank_duplicate_and_malformed_entries_are_dropped() {
    let c = parse_candidates(
        r#"{"patterns": ["  ", "dup", "dup", 42, null, {"nope": 1}, {"pattern": "  keep  "}]}"#,
        8,
    );
    assert_eq!(c.iter().map(|c| c.pattern.as_str()).collect::<Vec<_>>(), vec!["dup", "keep"]);
}

#[test]
fn the_candidate_list_is_capped_at_max_patterns() {
    // A runaway reply must not turn into fifty filesystem walks.
    let many: Vec<String> = (0..50).map(|i| format!("\"p{i}\"")).collect();
    let c = parse_candidates(&format!("{{\"patterns\": [{}]}}", many.join(",")), 8);
    assert_eq!(c.len(), 8);
    assert_eq!(c[7].pattern, "p7", "the cap keeps the model's own ordering");
}

#[test]
fn an_unusable_reply_yields_no_candidates() {
    for reply in ["", "I could not think of any patterns.", "{}", "{\"patterns\": []}", "[1,2]"] {
        assert!(parse_candidates(reply, 8).is_empty(), "reply: {reply:?}");
    }
}

// ── Question-token seeds ─────────────────────────────────────────────

#[test]
fn the_tokenizer_keeps_the_distinctive_words_and_drops_the_rest() {
    // The field case.  `waterslide` is the word that leads to the answer, and
    // it is precisely the word the synthesis model never tries.
    assert_eq!(
        question_tokens("main rendering function for the waterslide view"),
        vec!["rendering", "waterslide", "view"],
    );
    // Function words, generic programming words, sub-3-character words and
    // bare numbers all discriminate nothing.
    assert_eq!(
        question_tokens("where are the config file options parsed?"),
        vec!["config", "options", "parsed"],
    );
    assert_eq!(question_tokens("how does the main entry point of this code work"), vec!["point", "work"]);
    assert_eq!(question_tokens("what is an i8 vs a u8 in 2024"), vec!["vs"; 0]);
}

#[test]
fn the_tokenizer_splits_on_punctuation_but_never_inside_an_identifier() {
    // An identifier the caller typed is the best seed there is — splitting it
    // on `_` would throw that away.
    assert_eq!(question_tokens("who calls draw_waterslide()?"), vec!["draw_waterslide"]);
    assert_eq!(question_tokens("src/gui: the WaterslideView struct"), vec!["src", "gui", "waterslideview", "struct"]);
    // Repeats collapse: one walk per distinct word.
    assert_eq!(question_tokens("waterslide, waterslide, WATERSLIDE"), vec!["waterslide"]);
}

#[test]
fn seeds_are_case_insensitive_regexes_so_identifier_casing_cannot_hide_them() {
    // `waterslide` in the question must reach `WaterslideView` and
    // `WATERSLIDE_BINS` too — the caller typed a word, not an identifier.
    let seeds = seed_candidates("the waterslide view", &[], &[]);
    assert_eq!(seeds.iter().map(|c| c.pattern.as_str()).collect::<Vec<_>>(), vec!["(?i)waterslide", "(?i)view"]);
    assert!(seeds.iter().all(|c| c.regex), "a (?i) seed is only case-insensitive as a regex");
    // The tokenizer emits identifier characters only, so a seed can never
    // carry a metacharacter into that regex.
    for c in seed_candidates("what about a *b + c[0] (parens)?", &[], &[]) {
        assert!(
            c.pattern.trim_start_matches("(?i)").chars().all(|ch| ch.is_alphanumeric() || ch == '_'),
            "metacharacter leaked into a seed: {}",
            c.pattern
        );
    }
}

#[test]
fn seeds_never_duplicate_a_guess_or_a_pattern_already_tried() {
    // The model proposed `waterslide` itself: seeding it again is a second
    // identical walk for the same hits.
    let guesses = vec![Candidate { pattern: "waterslide".into(), ..Default::default() }];
    let seeds = seed_candidates("the waterslide view", &guesses, &[]);
    assert_eq!(seeds.iter().map(|c| c.pattern.as_str()).collect::<Vec<_>>(), vec!["(?i)view"]);

    // ...and a later round does not re-walk what round 1 already searched,
    // whichever spelling it was searched under.
    let tried = vec!["(?i)waterslide".to_string(), "View".to_string()];
    assert!(seed_candidates("the waterslide view", &[], &tried).is_empty());
}

#[test]
fn the_seed_count_is_bounded() {
    // Each seed is a filesystem walk; a rambling question must not turn into
    // twenty of them.
    let question = "explain the waterslide spectrogram colormap gradient palette histogram \
                    decimation resampling interpolation";
    assert_eq!(seed_candidates(question, &[], &[]).len(), MAX_SEEDS);
}

#[test]
fn a_seed_is_a_candidate_like_any_other_and_the_guard_judges_it() {
    // Seeds cost nothing but search time precisely because the degenerate
    // guard disposes of the useless ones before the model sees anything.
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.rs"), "pub fn draw_waterslide(v: WaterslideView) {}\n").unwrap();
    let common: String = (0..40).map(|i| format!("// view {i}\n")).collect();
    std::fs::write(dir.path().join("b.rs"), common).unwrap();

    let seeds = seed_candidates("main rendering function for the waterslide view", &[], &[]);
    let (results, kept, _) = search_candidates(
        dir.path(),
        &SearchOptions::default(),
        &seeds,
        &GrepConfig::default(),
        &FindConfig { degenerate_hit_cap: 10, ..Default::default() },
    );
    let fate = |p: &str| results.iter().find(|r| r.pattern == p).unwrap().fate;
    assert_eq!(fate("(?i)rendering"), Fate::Whiffed, "a word that is not in this tree costs one walk");
    assert_eq!(fate("(?i)waterslide"), Fate::Kept, "...and the distinctive one finds the answer");
    assert_eq!(fate("(?i)view"), Fate::TooCommon, "...while an everywhere-word is dropped whole");
    // The case-insensitive seed matched both spellings on the one line.
    assert_eq!(kept.len(), 1);
    assert_eq!(kept[0][0].file, "a.rs");
}

// ── Reflect: parsing ─────────────────────────────────────────────────

#[test]
fn a_reflection_parses_through_fences_and_prose() {
    for reply in [
        r#"{"answered": false, "patterns": ["draw_waterslide"]}"#,
        "```json\n{\"answered\": false, \"patterns\": [\"draw_waterslide\"]}\n```",
        "<think>the hits are all comments…</think>{\"answered\": false, \"patterns\": [\"draw_waterslide\"]}",
        "No, these miss it.\n{\"answered\": false, \"patterns\": [{\"pattern\": \"draw_waterslide\"}]}",
        // A small model may spell a bool as a word.
        r#"{"answered": "no", "patterns": ["draw_waterslide"]}"#,
    ] {
        let r = parse_reflection(reply, 4).unwrap_or_else(|| panic!("reply: {reply}"));
        assert!(!r.answered, "reply: {reply}");
        assert_eq!(r.patterns.iter().map(|c| c.pattern.as_str()).collect::<Vec<_>>(), vec!["draw_waterslide"]);
    }
}

#[test]
fn an_answered_reflection_carries_no_patterns_even_if_the_model_sent_some() {
    // "patterns are only meaningful when answered is false" — enforced here so
    // no caller has to remember it.
    let r = parse_reflection(r#"{"answered": true, "patterns": ["something_else"]}"#, 4).unwrap();
    assert!(r.answered);
    assert!(r.patterns.is_empty());
}

#[test]
fn an_unreadable_or_absent_verdict_reads_as_answered() {
    // The conservative direction: this stage exists to catch a wrong answer,
    // and missing one costs the status quo, while a spurious "no" costs a
    // whole extra round.
    assert!(parse_reflection(r#"{"patterns": ["x"]}"#, 4).unwrap().answered, "absent verdict");
    assert!(parse_reflection(r#"{"answered": "maybe"}"#, 4).unwrap().answered, "unreadable verdict");
    assert!(parse_reflection(r#"{"answered": 0}"#, 4).unwrap().answered, "a number is not a verdict");
    // Nothing parsed at all: the caller treats `None` exactly like "answered".
    for reply in ["", "I think so?", "[1,2]"] {
        assert_eq!(parse_reflection(reply, 4), None, "reply: {reply:?}");
    }
}

#[test]
fn the_reflect_pattern_list_is_capped() {
    let many: Vec<String> = (0..20).map(|i| format!("\"p{i}\"")).collect();
    let reply = format!("{{\"answered\": false, \"patterns\": [{}]}}", many.join(","));
    assert_eq!(parse_reflection(&reply, 4).unwrap().patterns.len(), 4);
}

// ── Reflect: loop semantics ──────────────────────────────────────────

fn reflection(answered: bool, patterns: &[&str]) -> Option<Reflection> {
    Some(Reflection {
        answered,
        patterns: patterns
            .iter()
            .map(|p| Candidate { pattern: (*p).to_string(), ..Default::default() })
            .collect(),
    })
}

#[test]
fn answered_stops_the_loop_and_unanswered_with_patterns_re_rounds() {
    assert_eq!(next_patterns(reflection(true, &[]), &[]), None, "answered: return what we have");
    assert_eq!(next_patterns(reflection(true, &["ignored"]), &[]), None, "answered wins over patterns");

    let next = next_patterns(reflection(false, &["draw_waterslide", "fn draw_waterslide"]), &[]).unwrap();
    assert_eq!(
        next.iter().map(|c| c.pattern.as_str()).collect::<Vec<_>>(),
        vec!["draw_waterslide", "fn draw_waterslide"]
    );
}

#[test]
fn a_parse_failure_or_an_empty_refinement_returns_the_current_results() {
    // Never fail toward discarding results: every uncertain reply stops the
    // loop with what the rerank already kept.
    assert_eq!(next_patterns(None, &[]), None, "unparseable reply");
    assert_eq!(next_patterns(reflection(false, &[]), &[]), None, "'no' with nothing to try instead");
}

#[test]
fn a_refinement_that_only_repeats_what_was_searched_stops_the_loop() {
    // Re-searching a tried pattern would spend a whole round reproducing the
    // result we are already holding.
    let tried = vec!["render".to_string(), "(?i)waterslide".to_string()];
    assert_eq!(next_patterns(reflection(false, &["render", "waterslide"]), &tried), None);
    // ...but one new pattern among repeats is still worth a round.
    let next = next_patterns(reflection(false, &["render", "draw_waterslide"]), &tried).unwrap();
    assert_eq!(next.len(), 1);
    assert_eq!(next[0].pattern, "draw_waterslide");
}

#[test]
fn the_reflect_stage_is_skipped_when_it_cannot_help() {
    let kept = json!({"hits": [{"file": "a.rs", "line": 1, "context": "x"}]});
    let on = FindConfig::default();
    assert!(reflect_due(&on, 1, 3, &kept));

    // The last allowed round has no round left to act on a "no": the call
    // would be pure latency.  This is where the budget is shared with the
    // whiff-retry — two whiffed rounds leave reflect nothing.
    assert!(!reflect_due(&on, 3, 3, &kept), "no round left to refine in");
    assert!(!reflect_due(&on, 1, 1, &kept), "--attempts 1 disables both retry kinds");

    // The knob turns the stage off outright.
    let off = FindConfig { reflect: false, ..Default::default() };
    assert!(!reflect_due(&off, 1, 3, &kept));

    // Nothing kept: no excerpts to read identifiers out of.
    assert!(!reflect_due(&on, 1, 3, &json!({"hits": []})));
    assert!(!reflect_due(&on, 1, 3, &json!({})));
}

#[test]
fn an_unreachable_model_leaves_the_results_alone() {
    // An LLM error in this stage must read as "answered": the rerank's result
    // is already in hand and reflect can only ever add to it.
    let payload = json!({"hits": [{"file": "a.rs", "line": 1, "text": "fn x() {}", "context": "fn x() {}"}]});
    assert_eq!(reflect(&offline_ctx("."), "anything", &payload, &[], 8), None);
}

// ── Reflect: the hit list it reads ───────────────────────────────────

#[test]
fn the_reflect_hit_list_numbers_the_hits_and_tags_comment_lines() {
    // "these are all comments *mentioning* the thing" is the shape of the
    // near-miss this stage exists to catch, so the tag matters more here than
    // anywhere else.
    let hits = vec![
        json!({"file": "panel.rs", "line": 12, "text": "    // calls draw_waterslide", "context": "a\n    // calls draw_waterslide\nb"}),
        json!({"file": "mod.rs", "line": 708, "text": "pub fn draw_buckets() {", "context": "pub fn draw_buckets() {"}),
        json!({"file": "cut.rs", "line": 3, "text": null, "context": "... (truncated)"}),
    ];
    let list = reflect_hit_list(&hits);
    assert!(list.starts_with("[1] panel.rs:12 (comment)\n"), "list: {list}");
    assert!(list.contains("[2] mod.rs:708 (code)\n"), "list: {list}");
    assert!(list.contains("[3] cut.rs:3\n"), "a null-text hit is untagged: {list}");
    assert!(list.contains("// calls draw_waterslide"), "the excerpt is what identifiers are read from");
    assert!(reflect_hit_list(&[]).is_empty());
}

#[test]
fn the_refining_status_line_names_what_it_will_search() {
    let next = vec![
        Candidate { pattern: "draw_waterslide".into(), ..Default::default() },
        Candidate { pattern: "fn draw_waterslide".into(), ..Default::default() },
    ];
    assert_eq!(
        refining_line(&next),
        "results may be off-target — refining with: draw_waterslide, fn draw_waterslide"
    );
}

// ── Degenerate-pattern guard ─────────────────────────────────────────

#[test]
fn the_guard_drops_whiffs_and_bad_discriminators() {
    assert_eq!(guard(0, 300), Fate::Whiffed);
    assert_eq!(guard(1, 300), Fate::Kept);
    assert_eq!(guard(299, 300), Fate::Kept);
    // The boundary is inclusive: the cap reads as "more than this is too many".
    assert_eq!(guard(300, 300), Fate::Kept);
    assert_eq!(guard(301, 300), Fate::TooCommon);
}

#[test]
fn an_over_cap_candidate_loses_every_hit_not_just_the_excess() {
    // The point of the guard is discrimination, not volume: 400 hits from
    // `parse` in a parser are not 300 good hits plus 100 bad ones.
    let dir = tempfile::tempdir().unwrap();
    let common: String = (0..40).map(|i| format!("fn parse_{i}() {{}}\n")).collect();
    std::fs::write(dir.path().join("a.rs"), common).unwrap();
    std::fs::write(dir.path().join("b.rs"), "fn load_config() {}\n").unwrap();

    let cfg = GrepConfig::default();
    let find_cfg = FindConfig { degenerate_hit_cap: 10, ..Default::default() };
    let candidates = vec![
        Candidate { pattern: "parse_".into(), ..Default::default() },
        Candidate { pattern: "load_config".into(), ..Default::default() },
        Candidate { pattern: "nothing_matches_this".into(), ..Default::default() },
    ];
    let (results, kept, _) =
        search_candidates(dir.path(), &SearchOptions::default(), &candidates, &cfg, &find_cfg);

    assert_eq!(results[0].fate, Fate::TooCommon);
    assert_eq!(results[0].hits, 40, "the count is still reported, only the hits are dropped");
    assert_eq!(results[1].fate, Fate::Kept);
    assert_eq!(results[2].fate, Fate::Whiffed);
    assert_eq!(kept.len(), 1, "only the discriminating candidate contributes");
    assert_eq!(kept[0].len(), 1);
    assert_eq!(kept[0][0].file, "b.rs");
}

#[test]
fn a_broken_regex_candidate_is_dropped_not_fatal() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.rs"), "fn load_config() {}\n").unwrap();
    let candidates = vec![
        Candidate { pattern: "(unclosed".into(), regex: true, ..Default::default() },
        Candidate { pattern: "load_config".into(), ..Default::default() },
    ];
    let (results, kept, _) = search_candidates(
        dir.path(),
        &SearchOptions::default(),
        &candidates,
        &GrepConfig::default(),
        &FindConfig::default(),
    );
    assert_eq!(results[0].fate, Fate::Unusable);
    assert_eq!(kept.len(), 1, "one bad guess must not sink the round");
}

// ── Hint narrowing ───────────────────────────────────────────────────

#[test]
fn a_hint_narrows_when_the_caller_left_the_dimension_open() {
    let base = SearchOptions::default();
    let c = Candidate {
        pattern: "x".into(),
        types: vec!["rust".into()],
        globs: vec!["src/**".into()],
        regex: true,
    };
    let opts = candidate_options(&base, &c, std::path::Path::new("."));
    assert!(opts.types.is_some() && opts.overrides.is_some());
    assert!(opts.regex, "the per-candidate regex flag is the model's to set");
}

#[test]
fn a_hint_can_never_widen_past_the_callers_own_flags() {
    // The caller said `-t rust -g '!vendor/**'`.  A model hint of `js` /
    // `vendor/**` would re-admit exactly what those flags removed, because
    // type selection is a union and overrides are last-match-wins — so in a
    // dimension the caller constrained, the hint is ignored outright.
    let root = std::path::Path::new(".");
    let base = SearchOptions {
        types: crate::source::build_types(&["rust".to_string()], &[]).unwrap(),
        overrides: crate::source::build_overrides(root, &["!vendor/**".to_string()]).unwrap(),
        ..SearchOptions::default()
    };
    let c = Candidate {
        pattern: "x".into(),
        types: vec!["js".into()],
        globs: vec!["vendor/**".into()],
        ..Default::default()
    };
    let opts = candidate_options(&base, &c, root);
    // Same compiled filters as the caller's, untouched.
    assert!(opts.types.as_ref().unwrap().matched("a.rs", false).is_whitelist());
    assert!(opts.types.as_ref().unwrap().matched("a.js", false).is_ignore());
    assert!(opts.overrides.as_ref().unwrap().matched("vendor/d.rs", false).is_ignore());
}

#[test]
fn an_uncompilable_hint_is_discarded_rather_than_fatal() {
    let c = Candidate {
        pattern: "x".into(),
        types: vec!["rustt".into()],
        globs: vec!["src/**/[".into()],
        ..Default::default()
    };
    let opts = candidate_options(&SearchOptions::default(), &c, std::path::Path::new("."));
    assert!(opts.types.is_none(), "a bad hint costs a broader search, not a failed one");
    assert!(opts.overrides.is_none());
}

// ── Union + dedupe ───────────────────────────────────────────────────

#[test]
fn the_union_dedupes_by_file_and_line() {
    // Two patterns hitting the same line is one hit — otherwise the reranker
    // spends its budget scoring the same code twice.
    let union = union_hits(vec![
        vec![raw("b.rs", 5), raw("a.rs", 2)],
        vec![raw("a.rs", 2), raw("a.rs", 9)],
    ]);
    let ids: Vec<(String, usize)> = union.iter().map(|h| (h.file.clone(), h.line)).collect();
    assert_eq!(
        ids,
        vec![("a.rs".to_string(), 2), ("a.rs".to_string(), 9), ("b.rs".to_string(), 5)],
        "sorted by (file, line) so the reranker's positional ids are stable"
    );
}

#[test]
fn the_union_of_nothing_is_empty() {
    assert!(union_hits(vec![]).is_empty());
    assert!(union_hits(vec![vec![], vec![]]).is_empty());
}

#[test]
fn a_refined_round_unions_with_the_prior_survivors_rather_than_replacing_them() {
    // The reflect stage asks for *more* evidence, so a worse second guess must
    // not lose what the first round found — and a line both rounds hit stays
    // one hit, or the reranker scores it twice.
    let prior = vec![raw("a.rs", 2), raw("b.rs", 5)];
    let refined = vec![raw("b.rs", 5), raw("c.rs", 700)];
    let union = union_hits(vec![refined, prior.clone()]);
    let ids: Vec<(&str, usize)> = union.iter().map(|h| (h.file.as_str(), h.line)).collect();
    assert_eq!(ids, vec![("a.rs", 2), ("b.rs", 5), ("c.rs", 700)]);

    // Same length as the prior list means the refined round found nothing new
    // — the loop's cue to return the payload it already built rather than
    // spend a rerank reproducing it.
    let nothing_new = union_hits(vec![vec![raw("a.rs", 2)], prior.clone()]);
    assert_eq!(nothing_new.len(), prior.len());
}

#[test]
fn the_union_is_order_independent() {
    // The model's pattern ordering must not change the hit ids, or two
    // identical runs would rerank different lists.
    let a = union_hits(vec![vec![raw("a.rs", 1)], vec![raw("b.rs", 1)]]);
    let b = union_hits(vec![vec![raw("b.rs", 1)], vec![raw("a.rs", 1)]]);
    assert_eq!(a, b);
}

// ── Retry trigger ────────────────────────────────────────────────────

#[test]
fn a_thin_but_nonzero_result_does_not_retry() {
    // SPEC §9: retry only when *all* patterns whiff.  One surviving hit is
    // answerable by the rerank stage, and a second LLM round is expensive.
    let kept = vec![vec![raw("a.rs", 1)]];
    assert!(!union_hits(kept).is_empty(), "a non-empty union goes straight to the rerank");
}

#[test]
fn an_all_whiff_round_produces_an_empty_union() {
    // ...and an empty union is precisely the retry trigger in `run`.
    assert!(union_hits(vec![]).is_empty());
}

#[test]
fn the_retry_note_names_the_failures_and_distinguishes_them() {
    // A pattern that matched nothing and one that matched everything call for
    // opposite corrections, so the prompt must not lump them together.
    assert_eq!(retry_note(&[], &[]), "", "the first round carries no note");

    let note = retry_note(&["flux".to_string(), "capacitor".to_string()], &["get".to_string()]);
    assert!(note.contains("matched nothing: flux, capacitor"), "{note}");
    assert!(note.contains("matched far too much to be useful: get"), "{note}");
    assert!(note.contains("propose different ones"), "{note}");
}

// ── stderr line ──────────────────────────────────────────────────────

#[test]
fn the_trying_line_matches_the_spec() {
    let results = vec![
        result("config", Fate::Kept, 12),
        result("toml", Fate::Kept, 4),
        result("load_config", Fate::Whiffed, 0),
        result("from_str", Fate::Whiffed, 0),
    ];
    assert_eq!(trying_line(&results), "trying: config, toml, load_config, from_str · 2 whiffed");
}

#[test]
fn the_trying_line_reports_the_guard_separately() {
    let results = vec![
        result("parse", Fate::TooCommon, 900),
        result("load_config", Fate::Kept, 3),
        result("(bad", Fate::Unusable, 0),
    ];
    let line = trying_line(&results);
    // An unusable pattern is a whiff from the caller's chair: it produced
    // nothing.  A too-common one produced too much, which is a different
    // problem and a different fix.
    assert_eq!(line, "trying: parse, load_config, (bad · 1 whiffed · 1 matched too much to discriminate");

    // An all-kept round says only what it tried.
    assert_eq!(trying_line(&[result("load_config", Fate::Kept, 3)]), "trying: load_config");
}

#[test]
fn the_payload_pattern_is_the_alternation_of_what_actually_hit() {
    // Renderer-compatible (it is a string in the `pattern` field) and runnable:
    // `scout grep --regex 'config|toml'` reproduces the unfiltered list.
    let results = vec![
        result("config", Fate::Kept, 12),
        result("parse", Fate::TooCommon, 900),
        result("toml", Fate::Kept, 4),
        result("nope", Fate::Whiffed, 0),
    ];
    assert_eq!(alternation(&results), "config|toml");
}

// ── Whiff payload ────────────────────────────────────────────────────

#[test]
fn the_whiff_payload_is_an_empty_result_not_an_error() {
    // The search ran; it just had nothing to show.  That is exit 1 (no hits),
    // not exit 2 (error) — so it must be a well-formed payload.
    let p = whiff_payload("where is the flux capacitor?", &["flux".into(), "capacitor".into()], 2);
    assert_eq!(p["mode"], "full");
    assert_eq!(p["returned"], 0);
    assert_eq!(p["hits"].as_array().unwrap().len(), 0);
    assert_eq!(p["none_relevant"], false, "nothing was judged — nothing was found");
    assert_eq!(p["pattern"], "flux|capacitor");
    assert_eq!(p["intent"], "where is the flux capacitor?");
    assert_eq!(p["find_attempts"], 2);
    assert_eq!(p["find_patterns"], serde_json::json!(["flux", "capacitor"]));
    assert!(p["hint"].as_str().unwrap().contains("flux, capacitor"));
}

// ── Tree sketch ──────────────────────────────────────────────────────

#[test]
fn the_sketch_is_paths_only_newline_separated() {
    let paths = vec!["src/main.rs".to_string(), "Cargo.toml".to_string()];
    assert_eq!(sketch(&paths, 8192), "src/main.rs\nCargo.toml\n");
}

#[test]
fn the_sketch_truncates_at_the_byte_cap_on_a_line_boundary() {
    let paths: Vec<String> = (0..500).map(|i| format!("src/file_{i:04}.rs")).collect();
    let out = sketch(&paths, 200);
    assert!(out.len() <= 200, "the whole sketch, marker included, fits the budget: {}", out.len());
    assert!(out.ends_with("... (truncated)\n"), "truncation must be announced: {out}");
    // Never half a path: every line is a real one.
    for line in out.lines().filter(|l| *l != "... (truncated)") {
        assert!(paths.iter().any(|p| p == line), "partial path leaked: {line}");
    }
}

#[test]
fn an_untruncated_sketch_carries_no_marker() {
    let paths = vec!["a.rs".to_string()];
    let out = sketch(&paths, 8192);
    assert_eq!(out, "a.rs\n");
    assert!(!out.contains("truncated"));
}

#[test]
fn a_budget_too_small_for_anything_yields_nothing_rather_than_a_fragment() {
    assert_eq!(sketch(&["src/main.rs".to_string()], 4), "");
    assert_eq!(sketch(&[], 8192), "");
}

#[test]
fn the_sketch_walk_respects_gitignore_hidden_files_and_the_callers_filters() {
    let dir = tempfile::tempdir().unwrap();
    for (rel, body) in [
        ("src/a.rs", "x"),
        ("src/b.js", "x"),
        ("target/junk.rs", "x"),
        (".hidden/c.rs", "x"),
        (".gitignore", "target/\n"),
    ] {
        let p = dir.path().join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, body).unwrap();
    }
    let all = crate::source::list_paths(dir.path(), &SearchOptions::default(), 1000);
    assert!(all.contains(&"src/a.rs".to_string()) && all.contains(&"src/b.js".to_string()));
    assert!(!all.iter().any(|p| p.starts_with("target/")), "gitignored: {all:?}");
    assert!(!all.iter().any(|p| p.starts_with(".hidden")), "hidden: {all:?}");

    // The caller's own -t narrows the sketch too: a `-t rust` run should not
    // spend its byte budget describing files it will never search.
    let rust_only = SearchOptions {
        types: crate::source::build_types(&["rust".to_string()], &[]).unwrap(),
        ..SearchOptions::default()
    };
    let listed = crate::source::list_paths(dir.path(), &rust_only, 1000);
    assert_eq!(listed, vec!["src/a.rs".to_string()]);

    // ...and the entry cap bounds the walk itself on a huge tree.
    assert_eq!(crate::source::list_paths(dir.path(), &SearchOptions::default(), 1).len(), 1);
}

// ── Fail-open contract ───────────────────────────────────────────────

/// Every failure must name an explicit `scout grep` as the way out (SPEC §5).
fn assert_fails_open(err: &crate::select::ToolError) -> String {
    let text = err.text();
    assert!(text.contains("scout grep"), "fallback must be named, got: {text}");
    text
}

#[test]
fn a_missing_question_fails_open() {
    let err = run(&offline_ctx("."), &serde_json::json!({})).unwrap_err();
    let text = assert_fails_open(&err);
    assert!(text.contains("'question'"), "text: {text}");
}

#[test]
fn an_unconfigured_model_fails_open_before_any_search() {
    // Unlike grep, find cannot degrade to a plain search — there is no pattern
    // to search for — so this is an error (exit 2), not an empty result.
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.rs"), "fn load_config() {}\n").unwrap();
    let err = run(
        &offline_ctx(&dir.path().to_string_lossy()),
        &serde_json::json!({"question": "where is config parsed?"}),
    )
    .unwrap_err();
    let text = assert_fails_open(&err);
    assert!(text.contains("not configured"), "text: {text}");
}

#[test]
fn a_bad_filter_arg_fails_open() {
    let err = run(
        &offline_ctx("."),
        &serde_json::json!({"question": "anything", "types": ["rustt"]}),
    )
    .unwrap_err();
    let text = assert_fails_open(&err);
    assert!(text.contains("invalid file type"), "text: {text}");
}

// ── Live-channel payloads (SPEC-dashboard §2.5, P4) ──────────────────
//
// The events are pure functions of values the round already computed, so they
// test without a socket or a model.  What matters is that each one says the
// thing the log cannot: which patterns were seeds, why a candidate was thrown
// away, what the reranker kept and why, and what the reflect stage made of it.

#[test]
fn the_patterns_event_separates_seeds_from_guesses() {
    let guesses = vec![
        Candidate { pattern: "render".into(), ..Default::default() },
        Candidate { pattern: "draw".into(), types: vec!["rust".into()], ..Default::default() },
    ];
    let mut all = guesses.clone();
    all.push(Candidate { pattern: "(?i)waterslide".into(), regex: true, ..Default::default() });

    let ev = patterns_event(&all, guesses.len(), false);
    assert_eq!(ev["source"], "synthesis");
    let p = ev["patterns"].as_array().unwrap();
    assert_eq!(p.len(), 3);
    assert_eq!(p[0]["seed"], false);
    assert_eq!(p[1]["types"][0], "rust");
    assert_eq!(p[2]["seed"], true, "the question's own word is a seed");
    assert_eq!(p[2]["regex"], true);

    assert_eq!(patterns_event(&all, all.len(), true)["source"], "reflect");
}

#[test]
fn the_hits_event_says_why_the_guard_dropped_each_candidate() {
    let results = vec![
        result("load_config", Fate::Kept, 4),
        result("config", Fate::TooCommon, 900),
        result("cfgg", Fate::Whiffed, 0),
        result("[unclosed", Fate::Unusable, 0),
    ];
    let union = vec![raw("a.rs", 1), raw("a.rs", 2), raw("b.rs", 9)];
    let ev = hits_event(&results, 200, &union, 1, true);

    assert_eq!(ev["union"], 3);
    assert_eq!(ev["carried"], 1);
    assert_eq!(ev["new"], 2, "a refined round is judged on what it added");
    assert_eq!(ev["degenerate_hit_cap"], 200);
    assert_eq!(ev["search_truncated"], true);

    let c = ev["candidates"].as_array().unwrap();
    assert_eq!(c[0]["fate"], "kept");
    assert_eq!(c[0]["dropped"], false);
    assert!(c[0]["why"].is_null(), "a kept candidate needs no excuse");
    assert_eq!(c[1]["fate"], "too_common");
    assert!(c[1]["why"].as_str().unwrap().contains("900"), "{}", c[1]["why"]);
    assert!(c[1]["why"].as_str().unwrap().contains("200"), "names the cap it broke");
    assert_eq!(c[2]["why"], "matched nothing");
    assert_eq!(c[3]["fate"], "unusable");
}

#[test]
fn the_rerank_event_reads_the_payload_the_caller_gets() {
    let payload = serde_json::json!({
        "hits_total": 40, "hits_considered": 30, "returned": 2, "dropped": 38,
        "none_relevant": false,
        "hits": [
            {"file": "src/a.rs", "line": 12, "score": 3, "why": "the definition", "context": "…"},
            {"file": "src/b.rs", "line": 3, "score": 1, "why": "a caller", "context": "…"},
        ],
    });
    let ev = rerank_event(&payload, "a|b");
    assert_eq!(ev["pattern"], "a|b");
    assert_eq!(ev["hits_total"], 40);
    assert_eq!(ev["dropped"], 38);
    let k = ev["keeps"].as_array().unwrap();
    assert_eq!(k.len(), 2);
    assert_eq!(k[0]["file"], "src/a.rs");
    assert_eq!(k[0]["score"], 3);
    assert_eq!(k[0]["why"], "the definition");
    // The excerpt itself stays out: the row's body already carries it, and a
    // keep list of full context blocks is what blows the datagram cap.
    assert!(k[0].get("context").is_none());
}

#[test]
fn the_reflect_event_distinguishes_asked_for_from_actually_searching() {
    let reflection = Reflection {
        answered: false,
        patterns: vec![
            Candidate { pattern: "draw_waterslide".into(), ..Default::default() },
            Candidate { pattern: "render".into(), ..Default::default() },
        ],
    };
    let next = vec![Candidate { pattern: "draw_waterslide".into(), ..Default::default() }];
    let ev = reflect_event(Some(&reflection), Some(&next));
    assert_eq!(ev["parsed"], true);
    assert_eq!(ev["answered"], false);
    assert_eq!(ev["patterns"].as_array().unwrap().len(), 2);
    assert_eq!(ev["refining"].as_array().unwrap().len(), 1, "`render` was already tried");
    assert_eq!(ev["refining"][0], "draw_waterslide");

    // An unreadable reply is recorded as what the loop does with it.
    let none = reflect_event(None, None);
    assert_eq!(none["parsed"], false);
    assert_eq!(none["answered"], true);
    assert_eq!(none["refining"].as_array().unwrap().len(), 0);
}

#[test]
fn a_silent_ledger_emits_no_find_events() {
    // The fixture ledger is silent, which must gate the live channel exactly as
    // it gates the log — otherwise a test run sprays a developer's dashboard.
    let ctx = offline_ctx(".");
    let mut called = false;
    live(&ctx, 1, "patterns", || {
        called = true;
        serde_json::json!({})
    });
    assert!(!called, "a silent ledger must not even build the payload");
}
