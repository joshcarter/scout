// Golden tests for the selector-mode presets (`extract.toml`, `grep.toml`).
//
// These presets differ from the others in two ways worth pinning down:
//
//   1. They have NO context providers — the caller injects every bulky value
//      as an arg (`numbered_content`, `hit_list`), the same trick `check_output`
//      uses for `output`. A stray `[context.*]` section would make scout shell
//      out on every call.
//   2. Their system prompts contain literal JSON braces.  `substitute_context`
//      must leave those untouched, or the schema the model is shown is corrupt.

use super::*;
use serde_json::json;

fn parse_builtin(name: &str, source: &str) -> Preset {
    loader::parse(source).unwrap_or_else(|e| panic!("failed to parse builtin '{name}': {e}"))
}

// ── extract ───────────────────────────────────────────────────────────────────

const EXTRACT_TOML: &str = include_str!("../../presets/extract.toml");

#[test]
fn extract_toml_parses() {
    let preset = parse_builtin("extract", EXTRACT_TOML);
    assert_eq!(preset.name, "extract");
    assert!(preset.context.is_empty(), "extract must have no context providers — caller injects everything");
    assert!(preset.verify.is_none(), "extract is a selector preset, not a code-placement preset");
}

#[test]
fn extract_input_schema_advertises_only_caller_args() {
    let preset = parse_builtin("extract", EXTRACT_TOML);
    let required: Vec<&str> = preset.input_schema()["required"]
        .as_array()
        .expect("required array")
        .iter()
        .filter_map(|v| v.as_str())
        .collect();
    assert_eq!(required, vec!["file", "question"]);

    let props = preset.input_schema()["properties"].as_object().expect("properties object");
    let mut keys: Vec<&str> = props.keys().map(String::as_str).collect();
    keys.sort_unstable();
    assert_eq!(keys, vec!["file", "max_lines", "question"]);
    // The caller-injected args must NOT be advertised — Claude never supplies them.
    assert!(!props.contains_key("numbered_content"));
    assert!(!props.contains_key("chunk_of"));
    assert!(!props.contains_key("file_lines"));
}

#[test]
fn extract_system_prompt_states_the_selector_contract() {
    let preset = parse_builtin("extract", EXTRACT_TOML);
    let system = template::substitute_context(&preset.system_template, &std::collections::HashMap::new());
    // Schema braces survive the context pass verbatim.
    assert!(system.contains("\"ranges\""), "system: {system}");
    assert!(system.contains("\"not_found\""), "system: {system}");
    assert!(system.contains("\"score\": 0-3"), "score scale must survive: {system}");
    // The rules that keep code out of Claude's context via the model.
    assert!(system.contains("1-based inclusive"), "system: {system}");
    assert!(system.contains("At most 8 ranges"), "system: {system}");
    assert!(system.contains("LABEL, not a quote"), "system: {system}");
    assert!(system.contains("must not reproduce code"), "system: {system}");
    assert!(system.contains("first character must be '{'"), "system: {system}");
}

#[test]
fn extract_renders_injected_args() {
    let preset = parse_builtin("extract", EXTRACT_TOML);
    let args = json!({
        "file": "internal/ec/repair.go",
        "question": "where is the retry loop?",
        "numbered_content": "   210\u{2192}for i := 0; i < n; i++ {",
        "file_lines": 2140,
        "chunk_of": 1,
        "chunk_total": 1,
    });
    let (_system, user) = resolve(&preset, &args, ".").unwrap();

    assert!(user.contains("where is the retry loop?"), "question missing: {user}");
    assert!(user.contains("internal/ec/repair.go"), "file missing: {user}");
    assert!(user.contains("2140 lines total"), "file_lines missing: {user}");
    assert!(user.contains("Chunk 1 of 1"), "chunk markers missing: {user}");
    assert!(user.contains("210\u{2192}for i := 0"), "numbered content missing: {user}");
    assert!(!user.contains("${args."), "unsubstituted arg ref left in user: {user}");
}

#[test]
fn extract_chunk_markers_default_when_unchunked() {
    // The single-chunk path omits chunk_of/chunk_total; the `:-1` fallback
    // must keep the prompt readable rather than emitting an empty slot.
    let preset = parse_builtin("extract", EXTRACT_TOML);
    let args = json!({"file": "a.rs", "question": "q", "numbered_content": "x", "file_lines": 300});
    let (_system, user) = resolve(&preset, &args, ".").unwrap();
    assert!(user.contains("Chunk 1 of 1"), "user: {user}");
}

// ── grep ──────────────────────────────────────────────────────────────────────

const GREP_TOML: &str = include_str!("../../presets/grep.toml");

#[test]
fn grep_toml_parses() {
    let preset = parse_builtin("grep", GREP_TOML);
    assert_eq!(preset.name, "grep");
    assert!(preset.context.is_empty(), "grep must have no context providers — caller injects everything");
    assert!(preset.verify.is_none(), "grep is a selector preset, not a code-placement preset");
}

#[test]
fn grep_input_schema_advertises_only_caller_args() {
    let preset = parse_builtin("grep", GREP_TOML);
    let required: Vec<&str> = preset.input_schema()["required"]
        .as_array()
        .expect("required array")
        .iter()
        .filter_map(|v| v.as_str())
        .collect();
    assert_eq!(required, vec!["pattern", "intent"]);

    let props = preset.input_schema()["properties"].as_object().expect("properties object");
    let mut keys: Vec<&str> = props.keys().map(String::as_str).collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        vec!["globs", "intent", "max_hits", "pattern", "regex", "types", "types_not"]
    );
    assert!(!props.contains_key("hit_list"), "caller-injected arg must not be advertised");
    assert!(!props.contains_key("hits_considered"));

    // The filter args are arrays of strings — Claude sending a bare string
    // must be a schema violation, not a silently-empty filter.
    for key in ["types", "types_not", "globs"] {
        assert_eq!(props[key]["type"], "array", "{key}");
        assert_eq!(props[key]["items"]["type"], "string", "{key}");
    }
}

#[test]
fn grep_system_prompt_states_the_selector_contract() {
    let preset = parse_builtin("grep", GREP_TOML);
    let system = template::substitute_context(&preset.system_template, &std::collections::HashMap::new());
    assert!(system.contains("\"keep\""), "system: {system}");
    assert!(system.contains("\"none_relevant\""), "system: {system}");
    assert!(system.contains("INTENT, not the pattern"), "system: {system}");
    assert!(system.contains("invented ids are discarded"), "system: {system}");
    assert!(system.contains("LABEL, not a quote"), "system: {system}");
    assert!(system.contains("first character must be '{'"), "system: {system}");
}

#[test]
fn grep_renders_injected_args() {
    let preset = parse_builtin("grep", GREP_TOML);
    let args = json!({
        "pattern": "WritePack",
        "intent": "call sites that ignore the error return",
        "hit_list": "[1] internal/ec/builder.go:412\nctx\n\n",
        "hits_considered": 57,
        "max_hits": 4,
    });
    let (_system, user) = resolve(&preset, &args, ".").unwrap();

    assert!(user.contains("call sites that ignore the error return"), "intent missing: {user}");
    assert!(user.contains("WritePack"), "pattern missing: {user}");
    assert!(user.contains("Keep at most 4 hits out of 57 shown."), "limits missing: {user}");
    assert!(user.contains("[1] internal/ec/builder.go:412"), "hit list missing: {user}");
    assert!(!user.contains("${args."), "unsubstituted arg ref left in user: {user}");
}

#[test]
fn grep_max_hits_defaults_when_caller_omits_it() {
    let preset = parse_builtin("grep", GREP_TOML);
    let args = json!({"pattern": "x", "intent": "y", "hit_list": "[1] a.rs:1\n", "hits_considered": 20});
    let (_system, user) = resolve(&preset, &args, ".").unwrap();
    assert!(user.contains("Keep at most 10 hits"), "user: {user}");
}
