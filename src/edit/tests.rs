//! Unit tests for `scout edit`'s pure half (docs/search-cli.md §6).
//!
//! Everything here is a pure function: the arity rule, the per-editor
//! invocation table, the picker's input grammar, `$EDITOR` word splitting and
//! the payload → target conversion.  The effectful half (`run`, `exec_editor`)
//! is covered by the fake-editor smoke test.

use super::*;
use serde_json::json;

// ── Arity dispatch ───────────────────────────────────────────────────

/// `dispatch` with the common shape: positionals only.
fn positionals(query: Option<&str>, intent: Option<&str>) -> Result<Pipeline, String> {
    dispatch(query.map(String::from), intent.map(String::from), None, false, None)
}

#[test]
fn one_positional_is_a_question_for_find() {
    assert_eq!(
        positionals(Some("where is config parsed?"), None),
        Ok(Pipeline::Find { question: "where is config parsed?".into(), attempts: None })
    );
}

#[test]
fn two_positionals_are_a_pattern_and_an_intent_for_grep() {
    assert_eq!(
        positionals(Some("load_config"), Some("the toml parse")),
        Ok(Pipeline::Grep {
            pattern: "load_config".into(),
            intent: Some("the toml parse".into()),
            regex: false,
        })
    );
}

#[test]
fn dash_p_is_a_pattern_with_no_rerank() {
    assert_eq!(
        dispatch(None, None, Some("needle".into()), false, None),
        Ok(Pipeline::Grep { pattern: "needle".into(), intent: None, regex: false })
    );
    // ...and it carries --regex, which is the one flag it shares with grep.
    assert_eq!(
        dispatch(None, None, Some("nee.le".into()), true, None),
        Ok(Pipeline::Grep { pattern: "nee.le".into(), intent: None, regex: true })
    );
}

#[test]
fn dash_p_beside_a_positional_is_an_error() {
    // The ambiguity is real — is the positional a second pattern? an intent? —
    // so it is refused rather than resolved by a rule nobody would remember.
    for (query, intent) in [(Some("x"), None), (Some("x"), Some("y")), (None, Some("y"))] {
        let err = dispatch(
            query.map(String::from),
            intent.map(String::from),
            Some("needle".into()),
            false,
            None,
        )
        .expect_err("-p plus a positional must be rejected");
        assert!(err.contains("-p already carries the pattern"), "{err}");
    }
}

#[test]
fn no_positionals_and_no_pattern_names_all_three_forms() {
    let err = positionals(None, None).expect_err("nothing to search for");
    for form in ["scout edit <question>", "scout edit <pattern> <intent>", "scout edit -p"] {
        assert!(err.contains(form), "{err} should name {form}");
    }
}

#[test]
fn regex_is_rejected_on_the_find_path() {
    // Same reason `scout find` has no --regex: the model decides per candidate.
    let err = dispatch(Some("a question".into()), None, None, true, None)
        .expect_err("--regex on a question must be rejected");
    assert!(err.contains("--regex"), "{err}");
}

#[test]
fn attempts_is_rejected_on_both_grep_paths() {
    // A pattern-guess budget on a path that never guesses would silently do
    // nothing, and the caller would believe a retry loop ran.
    let two = dispatch(Some("p".into()), Some("i".into()), None, false, Some(3));
    let dash_p = dispatch(None, None, Some("p".into()), false, Some(3));
    for err in [two.expect_err("two positionals"), dash_p.expect_err("-p")] {
        assert!(err.contains("--attempts"), "{err}");
    }
    // On the find path it is exactly what it says.
    assert_eq!(
        dispatch(Some("a question".into()), None, None, false, Some(3)),
        Ok(Pipeline::Find { question: "a question".into(), attempts: Some(3) })
    );
}

// ── Editor classification ────────────────────────────────────────────

#[test]
fn every_row_of_the_editor_table_is_recognised() {
    let rows = [
        ("vi", EditorKind::Vi),
        ("vim", EditorKind::Vi),
        ("nvim", EditorKind::Vi),
        ("emacs", EditorKind::Emacs),
        ("emacsclient", EditorKind::Emacs),
        ("hx", EditorKind::Helix),
        ("code", EditorKind::VsCode),
        ("codium", EditorKind::VsCode),
        ("cursor", EditorKind::VsCode),
        ("zed", EditorKind::Zed),
        ("ed", EditorKind::Unknown),
        ("nano", EditorKind::Unknown),
        ("", EditorKind::Unknown),
    ];
    for (name, kind) in rows {
        assert_eq!(classify(name), kind, "bare name: {name}");
        // $EDITOR is just as often an absolute path.
        assert_eq!(classify(&format!("/usr/local/bin/{name}")), kind, "path: {name}");
    }
}

#[test]
fn a_lookalike_prefix_is_not_the_editor() {
    // Substring matching would make `vimtutor` and `codex` positionable.
    for name in ["vimtutor", "codex", "zedit", "emacsen", "hxd"] {
        assert_eq!(classify(name), EditorKind::Unknown, "{name}");
    }
}

// ── Invocation table ─────────────────────────────────────────────────

/// `open_args` for one file, at line 212 column 9.
fn one(kind: EditorKind) -> Vec<String> {
    open_args(kind, &["src/select.rs".to_string()], 212, 9)
}

#[test]
fn each_editor_gets_the_positioning_the_spec_table_names() {
    assert_eq!(one(EditorKind::Vi), ["+212", "src/select.rs"]);
    assert_eq!(one(EditorKind::Emacs), ["+212:9", "src/select.rs"]);
    assert_eq!(one(EditorKind::Helix), ["src/select.rs:212:9"]);
    assert_eq!(one(EditorKind::VsCode), ["-g", "src/select.rs:212:9"]);
    assert_eq!(one(EditorKind::Zed), ["src/select.rs:212:9"]);
    // Unknown gets no position at all — `run` prints the line instead.
    assert_eq!(one(EditorKind::Unknown), ["src/select.rs"]);
}

#[test]
fn extra_files_follow_the_positioned_one() {
    let files = ["a.rs".to_string(), "b.rs".to_string(), "c.rs".to_string()];
    assert_eq!(open_args(EditorKind::Vi, &files, 7, 3), ["+7", "a.rs", "b.rs", "c.rs"]);
    assert_eq!(open_args(EditorKind::Helix, &files, 7, 3), ["a.rs:7:3", "b.rs", "c.rs"]);
    assert_eq!(open_args(EditorKind::VsCode, &files, 7, 3), ["-g", "a.rs:7:3", "b.rs", "c.rs"]);
    assert_eq!(open_args(EditorKind::Unknown, &files, 7, 3), ["a.rs", "b.rs", "c.rs"]);
}

#[test]
fn no_files_is_an_empty_argv_not_a_panic() {
    assert!(open_args(EditorKind::Vi, &[], 1, 1).is_empty());
}

#[test]
fn the_quickfix_invocation_is_dash_q() {
    assert_eq!(quickfix_args("/tmp/scout-quickfix-1.txt"), ["-q", "/tmp/scout-quickfix-1.txt"]);
}

// ── The quickfix temp file itself ──────────────────────────────────────
//
// `write_quickfix_file` is the fallible half of `quickfix` — split out so it
// can be called directly instead of through the effectful path, which ends
// in a real `std::process::exit` and can't run inside `cargo test`.

#[test]
fn the_quickfix_file_is_created_safely_and_still_readable_afterwards() {
    let payload = json!({"hits": [{"file": "src/a.rs", "line": 3, "col": 1}]});
    let path = write_quickfix_file(&payload).expect("temp file creation should succeed");

    // Still on disk (not deleted by NamedTempFile's own Drop) — `quickfix`
    // hands this path to `-q` and the editor must be able to open it.
    let content = std::fs::read_to_string(&path).expect("the launched editor must be able to read it");
    assert_eq!(content, render::render_vimgrep(&payload), "same formatter the module doc promises");

    // Not a predictable name: the whole point of moving off
    // `scout-quickfix-<pid>.txt` was that another local user can't guess it
    // ahead of time and pre-plant a symlink there.
    let name = path.file_name().unwrap().to_string_lossy().into_owned();
    assert!(name.starts_with("scout-quickfix-") && name.ends_with(".txt"), "{name}");
    assert_ne!(
        name,
        format!("scout-quickfix-{}.txt", std::process::id()),
        "must not be the old pid-based, guessable name"
    );

    let _ = std::fs::remove_file(&path);
}

#[cfg(unix)]
#[test]
fn the_quickfix_file_is_created_0600() {
    use std::os::unix::fs::PermissionsExt;
    let payload = json!({"hits": []});
    let path = write_quickfix_file(&payload).unwrap();
    let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600, "NamedTempFile's default mode — no one else on the box should read it");
    let _ = std::fs::remove_file(&path);
}

// ── $EDITOR word splitting ───────────────────────────────────────────

#[test]
fn editor_may_carry_arguments() {
    assert_eq!(split_words("vim"), ["vim"]);
    assert_eq!(split_words("code -w"), ["code", "-w"]);
    assert_eq!(split_words("  emacsclient   -nw  "), ["emacsclient", "-nw"]);
}

#[test]
fn quotes_and_escapes_survive_the_split() {
    // The two shapes that actually occur: a path with a space, and the empty
    // alternate-editor argument `emacsclient -a ''`.
    assert_eq!(split_words(r#""/Applications/My Editor" -w"#), ["/Applications/My Editor", "-w"]);
    assert_eq!(split_words(r"/opt/my\ editor/vim"), ["/opt/my editor/vim"]);
    assert_eq!(split_words("emacsclient -a ''"), ["emacsclient", "-a", ""]);
    // A backslash is literal inside single quotes, as in sh.
    assert_eq!(split_words(r"'a\b'"), [r"a\b"]);
}

#[test]
fn an_empty_editor_yields_no_words() {
    for raw in ["", "   ", "\t\n"] {
        assert!(split_words(raw).is_empty(), "{raw:?}");
    }
}

// ── Picker input ─────────────────────────────────────────────────────

#[test]
fn a_number_in_range_selects_that_hit() {
    assert_eq!(parse_choice("1", 12), Choice::One(1));
    assert_eq!(parse_choice("12", 12), Choice::One(12));
    // Whatever the terminal appended.
    assert_eq!(parse_choice("  7\n", 12), Choice::One(7));
}

#[test]
fn a_number_out_of_range_is_invalid_not_clamped() {
    // Clamping would open a hit the caller did not choose.
    for input in ["0", "13", "-1", "999999999999999999999"] {
        assert_eq!(parse_choice(input, 12), Choice::Invalid, "{input}");
    }
}

#[test]
fn a_and_q_are_accepted_long_and_short_and_in_either_case() {
    for input in ["a", "A", "all", " all \n"] {
        assert_eq!(parse_choice(input, 3), Choice::All, "{input}");
    }
    for input in ["q", "Q", "quit", "\tquit\r\n"] {
        assert_eq!(parse_choice(input, 3), Choice::Quit, "{input}");
    }
}

#[test]
fn garbage_and_emptiness_are_invalid() {
    for input in ["", "   ", "\n", "x", "1a", "a1", "1 2", "yes"] {
        assert_eq!(parse_choice(input, 3), Choice::Invalid, "{input:?}");
    }
}

// ── Payload → targets ────────────────────────────────────────────────

#[test]
fn columns_are_converted_from_zero_based_bytes_to_one_based() {
    let payload = json!({"hits": [
        {"file": "a.rs", "line": 10, "col": 0},
        {"file": "b.rs", "line": 20, "col": 41},
    ]});
    assert_eq!(
        hits(&payload),
        [
            Hit { file: "a.rs".into(), line: 10, col: 1 },
            Hit { file: "b.rs".into(), line: 20, col: 42 },
        ]
    );
}

#[test]
fn a_null_column_falls_back_to_one() {
    // `col: null` means the context budget cut the matched line away before the
    // match — the line is still exact, and column 1 is the honest answer.
    let payload = json!({"hits": [
        {"file": "a.rs", "line": 10, "col": null},
        {"file": "b.rs", "line": 11},
    ]});
    assert_eq!(hits(&payload).iter().map(|h| h.col).collect::<Vec<_>>(), [1, 1]);
}

#[test]
fn a_missing_or_zero_line_becomes_line_one() {
    // No editor in the table accepts `+0`.
    let payload = json!({"hits": [{"file": "a.rs", "line": 0}, {"file": "b.rs"}]});
    assert_eq!(hits(&payload).iter().map(|h| h.line).collect::<Vec<_>>(), [1, 1]);
}

#[test]
fn hits_without_a_file_are_dropped() {
    // There is nothing to open; passing "" would make a stray unnamed buffer.
    let payload = json!({"hits": [{"line": 3}, {"file": "", "line": 4}, {"file": "c.rs", "line": 5}]});
    assert_eq!(hits(&payload), [Hit { file: "c.rs".into(), line: 5, col: 1 }]);
}

#[test]
fn an_empty_or_absent_hit_list_yields_nothing() {
    assert!(hits(&json!({"hits": []})).is_empty());
    assert!(hits(&json!({})).is_empty());
    assert!(hits(&json!({"hits": "nonsense"})).is_empty());
}

#[test]
fn all_opens_each_file_once_in_first_seen_order() {
    let list = [
        Hit { file: "b.rs".into(), line: 1, col: 1 },
        Hit { file: "a.rs".into(), line: 2, col: 1 },
        Hit { file: "b.rs".into(), line: 3, col: 1 },
    ];
    assert_eq!(distinct_files(&list), ["b.rs", "a.rs"]);
}

// ── Quickfix content ─────────────────────────────────────────────────

#[test]
fn the_quickfix_list_is_exactly_format_vimgrep() {
    // docs/search-cli.md §9: `--format vimgrep` exists *because* it is the formatter this
    // path needs.  Reusing it is what keeps `scout edit`'s list and
    // `scout grep --format vimgrep | vim -q -` navigating identically.
    let payload = json!({"hits": [
        {"file": "src/a.rs", "line": 12, "col": 4, "text": "  let x = 1;"},
        {"file": "src/b.rs", "line": 3, "col": null, "text": null},
    ]});
    assert_eq!(
        render::render_vimgrep(&payload),
        "src/a.rs:12:5:   let x = 1;\nsrc/b.rs:3:1: (matched line unavailable)\n"
    );
}
