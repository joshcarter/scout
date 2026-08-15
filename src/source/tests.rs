//! Tests for the filesystem backend: file reads, and the search layer's
//! gitignore/binary/context/cap behaviour.
//!
//! These run without an LLM and without a config file — they are what proves
//! `scout grep` does the right thing at the search layer even when the model
//! call later fails.

use super::*;
use std::fs;
use tempfile::TempDir;

fn opts() -> SearchOptions {
    SearchOptions {
        regex: false,
        context_lines: 2,
        context_max_bytes: 2000,
        max_file_bytes: 1024 * 1024,
        max_hits: 1000,
        types: None,
        overrides: None,
    }
}

/// The `(line, col, col_end)` triples of every hit, in walk order.
fn spans(dir: &TempDir, pattern: &str, o: &SearchOptions) -> Vec<(usize, usize, usize)> {
    search(dir.path(), pattern, o).unwrap().hits.iter().map(|h| (h.line, h.col, h.col_end)).collect()
}

/// Collect the hit files for `pattern` under `dir`, in walk order.
fn files(dir: &TempDir, pattern: &str, o: &SearchOptions) -> Vec<String> {
    search(dir.path(), pattern, o).unwrap().hits.iter().map(|h| h.file.clone()).collect()
}

/// A small mixed-language tree, for the type/glob filter tests.
fn mixed_tree() -> TempDir {
    let dir = tempfile::tempdir().unwrap();
    write(&dir, "src/a.rs", "needle\n");
    write(&dir, "src/b.js", "needle\n");
    write(&dir, "docs/c.md", "needle\n");
    write(&dir, "vendor/d.rs", "needle\n");
    dir
}

fn write(dir: &TempDir, rel: &str, body: &str) {
    let p = dir.path().join(rel);
    if let Some(parent) = p.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(p, body).unwrap();
}

// ── read_file ────────────────────────────────────────────────────────

#[test]
fn read_file_splits_lines_and_relativizes_the_path() {
    let dir = tempfile::tempdir().unwrap();
    write(&dir, "src/lib.rs", "one\ntwo\nthree\n");
    let f = read_file(dir.path(), "src/lib.rs", 1 << 20).unwrap();
    assert_eq!(f.path, "src/lib.rs");
    assert_eq!(f.lines, vec!["one", "two", "three"]);
}

#[test]
fn read_file_accepts_an_absolute_path() {
    let dir = tempfile::tempdir().unwrap();
    write(&dir, "a.rs", "x\n");
    let abs = dir.path().join("a.rs");
    let f = read_file(dir.path(), &abs.to_string_lossy(), 1 << 20).unwrap();
    assert_eq!(f.path, "a.rs", "an in-project absolute path still displays relative");
    assert_eq!(f.lines, vec!["x"]);
}

#[test]
fn read_file_without_trailing_newline_keeps_every_line() {
    let dir = tempfile::tempdir().unwrap();
    write(&dir, "a.rs", "one\ntwo");
    let f = read_file(dir.path(), "a.rs", 1 << 20).unwrap();
    assert_eq!(f.lines, vec!["one", "two"]);
}

#[test]
fn read_file_reports_missing_directory_binary_and_oversize() {
    let dir = tempfile::tempdir().unwrap();
    assert!(read_file(dir.path(), "nope.rs", 1 << 20).is_err());

    fs::create_dir(dir.path().join("adir")).unwrap();
    let e = read_file(dir.path(), "adir", 1 << 20).unwrap_err();
    assert!(e.contains("directory"), "{e}");

    fs::write(dir.path().join("bin.dat"), [b'a', 0, b'b']).unwrap();
    let e = read_file(dir.path(), "bin.dat", 1 << 20).unwrap_err();
    assert!(e.contains("binary"), "{e}");

    write(&dir, "big.rs", &"x".repeat(500));
    let e = read_file(dir.path(), "big.rs", 100).unwrap_err();
    assert!(e.contains("read cap"), "{e}");
}

#[test]
fn split_lines_handles_crlf_and_empty_input() {
    assert_eq!(split_lines("a\r\nb\r\n"), vec!["a", "b"]);
    assert!(split_lines("").is_empty());
    assert_eq!(split_lines("\n\n"), vec!["", ""]);
}

// ── search: matching and context ─────────────────────────────────────

#[test]
fn search_finds_literal_hits_with_context_lines() {
    let dir = tempfile::tempdir().unwrap();
    write(&dir, "src/a.rs", "l1\nl2\nWritePack(&w)\nl4\nl5\nl6\n");
    let r = search(dir.path(), "WritePack", &opts()).unwrap();
    assert_eq!(r.hits.len(), 1);
    assert_eq!(r.hits[0].file, "src/a.rs");
    assert_eq!(r.hits[0].line, 3);
    assert_eq!(
        r.hits[0].text, "l1\nl2\nWritePack(&w)\nl4\nl5",
        "±2 lines of context, newline-joined"
    );
    assert!(!r.truncated);
}

#[test]
fn search_context_is_clamped_at_the_file_edges() {
    let dir = tempfile::tempdir().unwrap();
    write(&dir, "a.rs", "MATCH\nb\nc\n");
    let r = search(dir.path(), "MATCH", &opts()).unwrap();
    assert_eq!(r.hits[0].line, 1);
    assert_eq!(r.hits[0].text, "MATCH\nb\nc", "no lines above line 1");
}

#[test]
fn search_context_lines_is_tunable() {
    let dir = tempfile::tempdir().unwrap();
    write(&dir, "a.rs", "1\n2\n3\nHIT\n5\n6\n7\n");
    let mut o = opts();
    o.context_lines = 0;
    let r = search(dir.path(), "HIT", &o).unwrap();
    assert_eq!(r.hits[0].text, "HIT");
}

#[test]
fn search_context_block_is_truncated_at_the_byte_budget() {
    let dir = tempfile::tempdir().unwrap();
    write(&dir, "a.rs", &format!("{}\nHIT\n", "x".repeat(400)));
    let mut o = opts();
    o.context_max_bytes = 100;
    let r = search(dir.path(), "HIT", &o).unwrap();
    assert!(r.hits[0].text.ends_with("... (truncated)"), "{}", r.hits[0].text);
    assert!(r.hits[0].text.len() < 200);
}

#[test]
fn search_literal_mode_does_not_interpret_regex_metacharacters() {
    let dir = tempfile::tempdir().unwrap();
    write(&dir, "a.rs", "fn f(x: u8) {}\nlet a_b = 1;\n");
    // As a literal, "a.b" matches nothing; as a regex it matches "a_b".
    assert!(search(dir.path(), "a.b", &opts()).unwrap().hits.is_empty());
    let mut o = opts();
    o.regex = true;
    assert_eq!(search(dir.path(), "a.b", &o).unwrap().hits.len(), 1);
}

#[test]
fn search_reports_multiple_hits_in_one_file_in_line_order() {
    let dir = tempfile::tempdir().unwrap();
    write(&dir, "a.rs", "hit\nx\nhit\nx\nhit\n");
    let r = search(dir.path(), "hit", &opts()).unwrap();
    let lines: Vec<usize> = r.hits.iter().map(|h| h.line).collect();
    assert_eq!(lines, vec![1, 3, 5]);
}

#[test]
fn search_visits_files_in_sorted_path_order() {
    let dir = tempfile::tempdir().unwrap();
    write(&dir, "b.rs", "needle\n");
    write(&dir, "a.rs", "needle\n");
    write(&dir, "c.rs", "needle\n");
    let files: Vec<String> =
        search(dir.path(), "needle", &opts()).unwrap().hits.iter().map(|h| h.file.clone()).collect();
    assert_eq!(files, vec!["a.rs", "b.rs", "c.rs"], "hit ids must be stable across runs");
}

#[test]
fn search_rejects_an_invalid_regex_rather_than_returning_nothing() {
    let dir = tempfile::tempdir().unwrap();
    write(&dir, "a.rs", "x\n");
    let mut o = opts();
    o.regex = true;
    let e = search(dir.path(), "(unclosed", &o).unwrap_err();
    assert!(e.contains("invalid pattern"), "{e}");
}

// ── search: match columns (docs/search-cli.md §4) ──────────────────────────────

#[test]
fn search_records_the_match_column_as_a_byte_offset() {
    let dir = tempfile::tempdir().unwrap();
    write(&dir, "a.rs", "let needle = 1;\n");
    // 0-based: "needle" starts at byte 4 and ends (exclusive) at 10.
    assert_eq!(spans(&dir, "needle", &opts()), vec![(1, 4, 10)]);
}

#[test]
fn a_match_at_the_start_of_a_line_has_column_zero() {
    // The one value that distinguishes 0-based from 1-based capture.
    let dir = tempfile::tempdir().unwrap();
    write(&dir, "a.rs", "needle first\n");
    assert_eq!(spans(&dir, "needle", &opts()), vec![(1, 0, 6)]);
}

#[test]
fn only_the_first_match_on_a_line_is_recorded() {
    // docs/search-cli.md §4 wants one column per hit: a window centres on one span and
    // quickfix carries one column, so later matches have no consumer.
    let dir = tempfile::tempdir().unwrap();
    write(&dir, "a.rs", "xx needle yy needle\n");
    assert_eq!(spans(&dir, "needle", &opts()), vec![(1, 3, 9)]);
}

#[test]
fn match_columns_are_byte_offsets_not_character_offsets() {
    // "é" is two bytes, so a character-counting capture would report 4 here
    // and the renderer would slice mid-codepoint.
    let dir = tempfile::tempdir().unwrap();
    write(&dir, "a.rs", "éééé needle\n");
    let s = spans(&dir, "needle", &opts());
    assert_eq!(s, vec![(1, 9, 15)]);
    // ...and the offsets really do address the match in the file's bytes.
    let body = fs::read_to_string(dir.path().join("a.rs")).unwrap();
    assert_eq!(&body[9..15], "needle");
}

#[test]
fn regex_matches_report_the_span_they_actually_matched() {
    let dir = tempfile::tempdir().unwrap();
    write(&dir, "a.rs", "let value_42 = 1;\n");
    let mut o = opts();
    o.regex = true;
    assert_eq!(spans(&dir, r"value_\d+", &o), vec![(1, 4, 12)]);
}

#[test]
fn every_hit_in_a_file_carries_its_own_column() {
    let dir = tempfile::tempdir().unwrap();
    write(&dir, "a.rs", "hit\n  hit\n     hit\n");
    assert_eq!(spans(&dir, "hit", &opts()), vec![(1, 0, 3), (2, 2, 5), (3, 5, 8)]);
}

// ── search: what it refuses to look at ───────────────────────────────

#[test]
fn search_respects_gitignore() {
    let dir = tempfile::tempdir().unwrap();
    write(&dir, ".gitignore", "target/\nignored.rs\n");
    write(&dir, "kept.rs", "needle\n");
    write(&dir, "ignored.rs", "needle\n");
    write(&dir, "target/generated.rs", "needle\n");
    let files: Vec<String> =
        search(dir.path(), "needle", &opts()).unwrap().hits.iter().map(|h| h.file.clone()).collect();
    assert_eq!(files, vec!["kept.rs"], "gitignored paths must not be searched");
}

#[test]
fn search_skips_hidden_directories() {
    let dir = tempfile::tempdir().unwrap();
    write(&dir, ".secret/notes.rs", "needle\n");
    write(&dir, "visible.rs", "needle\n");
    let files: Vec<String> =
        search(dir.path(), "needle", &opts()).unwrap().hits.iter().map(|h| h.file.clone()).collect();
    assert_eq!(files, vec!["visible.rs"]);
}

#[test]
fn search_skips_binary_files() {
    let dir = tempfile::tempdir().unwrap();
    let mut blob = b"needle".to_vec();
    blob.push(0);
    blob.extend_from_slice(b"needle\n");
    fs::write(dir.path().join("blob.bin"), blob).unwrap();
    write(&dir, "text.rs", "needle\n");
    let files: Vec<String> =
        search(dir.path(), "needle", &opts()).unwrap().hits.iter().map(|h| h.file.clone()).collect();
    assert_eq!(files, vec!["text.rs"], "binary files must never enter the hit list");
}

#[test]
fn search_skips_files_over_the_size_cap() {
    let dir = tempfile::tempdir().unwrap();
    write(&dir, "small.rs", "needle\n");
    write(&dir, "huge.rs", &format!("needle\n{}", "x".repeat(5000)));
    let mut o = opts();
    o.max_file_bytes = 1000;
    let files: Vec<String> =
        search(dir.path(), "needle", &o).unwrap().hits.iter().map(|h| h.file.clone()).collect();
    assert_eq!(files, vec!["small.rs"]);
}

#[test]
fn search_stops_at_the_hit_cap_and_says_so() {
    let dir = tempfile::tempdir().unwrap();
    let body: String = (0..50).map(|_| "needle\n").collect();
    write(&dir, "a.rs", &body);
    write(&dir, "b.rs", &body);
    let mut o = opts();
    o.max_hits = 10;
    let r = search(dir.path(), "needle", &o).unwrap();
    assert_eq!(r.hits.len(), 10, "the cap is a hard stop");
    assert!(r.truncated, "truncation must be visible, never silent");
}

#[test]
fn search_of_an_empty_tree_is_an_empty_result_not_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let r = search(dir.path(), "needle", &opts()).unwrap();
    assert!(r.hits.is_empty());
    assert!(!r.truncated);
}

// ── search: type and glob filters (docs/search-cli.md §3) ──────────────────────

#[test]
fn no_filters_is_an_exact_no_op() {
    // The contract that lets every existing caller keep its behaviour: an
    // explicitly-None pair must walk precisely the tree it always did.
    let dir = mixed_tree();
    let baseline = files(&dir, "needle", &opts());
    assert_eq!(baseline, vec!["docs/c.md", "src/a.rs", "src/b.js", "vendor/d.rs"]);

    let mut o = opts();
    o.types = build_types(&[], &[]).unwrap();
    o.overrides = build_overrides(dir.path(), &[]).unwrap();
    assert!(o.types.is_none() && o.overrides.is_none(), "empty filter lists must stay None");
    assert_eq!(files(&dir, "needle", &o), baseline);
}

#[test]
fn selecting_a_type_keeps_only_that_type() {
    let dir = mixed_tree();
    let mut o = opts();
    o.types = build_types(&["rust".to_string()], &[]).unwrap();
    assert_eq!(files(&dir, "needle", &o), vec!["src/a.rs", "vendor/d.rs"]);
}

#[test]
fn selecting_several_types_is_a_union() {
    let dir = mixed_tree();
    let mut o = opts();
    o.types = build_types(&["rust".to_string(), "md".to_string()], &[]).unwrap();
    assert_eq!(files(&dir, "needle", &o), vec!["docs/c.md", "src/a.rs", "vendor/d.rs"]);
}

#[test]
fn negating_a_type_drops_only_that_type() {
    let dir = mixed_tree();
    let mut o = opts();
    o.types = build_types(&[], &["js".to_string()]).unwrap();
    assert_eq!(files(&dir, "needle", &o), vec!["docs/c.md", "src/a.rs", "vendor/d.rs"]);
}

#[test]
fn an_unknown_type_name_is_an_error_not_an_empty_result() {
    // Silently searching nothing would read as "no matches" — the one answer
    // a typo must never produce.
    let e = build_types(&["rustt".to_string()], &[]).unwrap_err();
    assert!(e.contains("invalid file type"), "{e}");
    assert!(build_types(&[], &["nosuchtype".to_string()]).is_err());
}

#[test]
fn an_include_glob_restricts_the_walk() {
    let dir = mixed_tree();
    let mut o = opts();
    o.overrides = build_overrides(dir.path(), &["src/**".to_string()]).unwrap();
    assert_eq!(files(&dir, "needle", &o), vec!["src/a.rs", "src/b.js"]);
}

#[test]
fn a_negated_glob_excludes_and_leaves_everything_else() {
    let dir = mixed_tree();
    let mut o = opts();
    o.overrides = build_overrides(dir.path(), &["!vendor/**".to_string()]).unwrap();
    assert_eq!(files(&dir, "needle", &o), vec!["docs/c.md", "src/a.rs", "src/b.js"]);
}

#[test]
fn globs_and_types_compose() {
    let dir = mixed_tree();
    let mut o = opts();
    o.types = build_types(&["rust".to_string()], &[]).unwrap();
    o.overrides = build_overrides(dir.path(), &["!vendor/**".to_string()]).unwrap();
    assert_eq!(files(&dir, "needle", &o), vec!["src/a.rs"], "both filters must apply");
}

#[test]
fn a_malformed_glob_is_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let e = build_overrides(dir.path(), &["src/**/[".to_string()]).unwrap_err();
    assert!(e.contains("invalid glob"), "{e}");
}

#[test]
fn type_definitions_are_ripgreps_and_sorted() {
    let defs = type_definitions();
    assert!(defs.len() > 50, "the built-in list should be large, got {}", defs.len());
    let names: Vec<&str> = defs.iter().map(|(n, _)| n.as_str()).collect();
    for expected in ["rust", "js", "json", "md", "toml"] {
        assert!(names.contains(&expected), "missing type {expected}");
    }
    let mut sorted = names.clone();
    sorted.sort_unstable();
    assert_eq!(names, sorted, "--type-list output must be stable");
    let (_, globs) = defs.iter().find(|(n, _)| n == "rust").unwrap();
    assert!(globs.iter().any(|g| g == "*.rs"), "globs must be carried along: {globs:?}");
}

// ── extract_context ──────────────────────────────────────────────────

#[test]
fn extract_context_is_a_window_around_the_line() {
    let lines = vec!["a", "b", "c", "d", "e", "f", "g"];
    assert_eq!(extract_context(&lines, 3, 2, 2000), "b\nc\nd\ne\nf");
    assert_eq!(extract_context(&lines, 0, 2, 2000), "a\nb\nc");
    assert_eq!(extract_context(&lines, 6, 2, 2000), "e\nf\ng");
    assert_eq!(extract_context(&[], 0, 2, 2000), "");
}

#[test]
fn extract_context_truncation_respects_utf8_boundaries() {
    let long = "é".repeat(300);
    let lines = vec![long.as_str(), "HIT"];
    let ctx = extract_context(&lines, 1, 2, 101);
    assert!(std::str::from_utf8(ctx.as_bytes()).is_ok());
    assert!(ctx.ends_with("... (truncated)"));
}
