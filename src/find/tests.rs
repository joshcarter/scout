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
        client: None,
        client_error: Some("no config in tests".into()),
        presets: &[],
        project: project.to_string(),
        progress: None,
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
