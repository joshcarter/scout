// Golden tests for the built-in TOML presets and load/resolve unit tests.
//
// These tests verify that the TOML files parse correctly and that the rendered
// (system, user) pair is self-consistent (same output on every run for the same
// fixture input).  They do NOT exercise live providers — context is injected via
// a mock context map that bypasses run_provider.
//
// Only `quality_review` and `test_review` have context-provider-driven golden
// tests here: they're the only two of the 6 kept presets that use context
// providers at all (`file_read`, `git_diff_range`). `check_output`, `shell_safety`,
// `extract`, `grep`, `find_patterns` and `find_reflect` are pure ${args.*} pass-throughs with no `[context.*]`
// sections — see `tests_selectors.rs` for `extract`/`grep` golden coverage.
//
// ## What "golden" means here
//
// The expected strings are hand-written once (when the preset is authored) and
// represent the canonical output.  If a preset is intentionally changed, update
// the expected strings and commit the diff alongside the TOML change so reviewers
// can see the prompt delta.

use super::*;
use serde_json::json;

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Parse a built-in TOML from the embedded defaults.
fn parse_builtin(name: &str, source: &str) -> Preset {
    loader::parse(source).unwrap_or_else(|e| panic!("failed to parse builtin '{name}': {e}"))
}

/// Render a preset with a pre-built context map, bypassing live providers.
/// Equivalent to the Pass-2 half of resolve().
fn render(preset: &Preset, context: &std::collections::HashMap<String, String>) -> (String, String) {
    let system = template::substitute_context(&preset.system_template, context);
    let user = template::substitute_context(&preset.user_template, context);
    (system, user)
}

// ── quality_review ────────────────────────────────────────────────────────────

const QUALITY_REVIEW_TOML: &str = include_str!("../../presets/quality_review.toml");

#[test]
fn quality_review_toml_parses() {
    let preset = parse_builtin("quality_review", QUALITY_REVIEW_TOML);
    assert_eq!(preset.name, "quality_review");
    assert_eq!(preset.context.len(), 2);
    let instr = preset.context.iter().find(|c| c.key == "review_instructions").expect("review_instructions context missing");
    assert_eq!(instr.provider, "file_read");
    assert!(instr.args.iter().any(|a| a.contains("${args.prompt_file}")), "prompt_file ref missing");
    let diff = preset.context.iter().find(|c| c.key == "diff").expect("diff context missing");
    assert_eq!(diff.provider, "git_diff_range");
    assert!(diff.args.iter().any(|a| a.contains("${args.git_diff_range}")), "git_diff_range ref missing");
}

#[test]
fn quality_review_input_schema_requires_both_args() {
    let preset = parse_builtin("quality_review", QUALITY_REVIEW_TOML);
    let required = preset.input_schema["required"].as_array().expect("required array");
    assert!(required.iter().any(|v| v.as_str() == Some("git_diff_range")), "should require git_diff_range");
    assert!(required.iter().any(|v| v.as_str() == Some("prompt_file")), "should require prompt_file");
}

#[test]
fn quality_review_has_no_verify() {
    // Text-output preset — no build/refactor verify step.
    let preset = parse_builtin("quality_review", QUALITY_REVIEW_TOML);
    assert!(preset.verify.is_none(), "quality_review should not have a verify step");
}

#[test]
fn quality_review_renders_with_fixture() {
    let preset = parse_builtin("quality_review", QUALITY_REVIEW_TOML);
    let mut ctx = std::collections::HashMap::new();
    ctx.insert("review_instructions".to_string(), "## Code Quality & Hygiene Review\n\nCheck for X, Y, Z.".to_string());
    ctx.insert("diff".to_string(), "--- a/foo.rs\n+++ b/foo.rs\n+fn new() {}".to_string());

    let (_system, user) = render(&preset, &ctx);

    assert!(user.contains("Code Quality & Hygiene Review"), "instructions missing from user: {user}");
    assert!(user.contains("--- a/foo.rs"), "diff missing from user: {user}");
}

// ── test_review ───────────────────────────────────────────────────────────────

const TEST_REVIEW_TOML: &str = include_str!("../../presets/test_review.toml");

#[test]
fn test_review_toml_parses() {
    let preset = parse_builtin("test_review", TEST_REVIEW_TOML);
    assert_eq!(preset.name, "test_review");
    assert_eq!(preset.context.len(), 2);
    let instr = preset.context.iter().find(|c| c.key == "review_instructions").expect("review_instructions context missing");
    assert_eq!(instr.provider, "file_read");
    assert!(instr.args.iter().any(|a| a.contains("${args.prompt_file}")), "prompt_file ref missing");
    let diff = preset.context.iter().find(|c| c.key == "diff").expect("diff context missing");
    assert_eq!(diff.provider, "git_diff_range");
    assert!(diff.args.iter().any(|a| a.contains("${args.git_diff_range}")), "git_diff_range ref missing");
}

#[test]
fn test_review_input_schema_requires_both_args() {
    let preset = parse_builtin("test_review", TEST_REVIEW_TOML);
    let required = preset.input_schema["required"].as_array().expect("required array");
    assert!(required.iter().any(|v| v.as_str() == Some("git_diff_range")), "should require git_diff_range");
    assert!(required.iter().any(|v| v.as_str() == Some("prompt_file")), "should require prompt_file");
}

#[test]
fn test_review_has_no_verify() {
    let preset = parse_builtin("test_review", TEST_REVIEW_TOML);
    assert!(preset.verify.is_none(), "test_review should not have a verify step");
}

// ── check_output / shell_safety — parse smoke tests ─────────────────────────
//
// No context providers, so no golden render test is needed beyond "it parses
// and has the expected shape" — the ${args.*} substitution paths are covered
// generically by template.rs's own tests.

const CHECK_OUTPUT_TOML: &str = include_str!("../../presets/check_output.toml");

#[test]
fn check_output_toml_parses_with_no_context() {
    let preset = parse_builtin("check_output", CHECK_OUTPUT_TOML);
    assert_eq!(preset.name, "check_output");
    assert!(preset.context.is_empty(), "check_output takes args only, no context providers");
    let required = preset.input_schema["required"].as_array().expect("required array");
    assert!(required.iter().any(|v| v.as_str() == Some("command")));
}

const SHELL_SAFETY_TOML: &str = include_str!("../../presets/shell_safety.toml");

#[test]
fn shell_safety_toml_parses_with_no_context() {
    let preset = parse_builtin("shell_safety", SHELL_SAFETY_TOML);
    assert_eq!(preset.name, "shell_safety");
    assert!(preset.context.is_empty(), "shell_safety takes args only, no context providers");
    let required = preset.input_schema["required"].as_array().expect("required array");
    assert!(required.iter().any(|v| v.as_str() == Some("command")));
}

// ── load_builtins() / load_all() integration ─────────────────────────────────

#[test]
fn load_builtins_returns_exactly_eight_presets() {
    let presets = load_builtins();
    let mut names: Vec<&str> = presets.iter().map(|p| p.name.as_str()).collect();
    names.sort_unstable();
    assert_eq!(
        names,
        vec![
            "check_output",
            "extract",
            "find_patterns",
            "find_reflect",
            "grep",
            "quality_review",
            "shell_safety",
            "test_review"
        ],
        "the 6 kept presets from PLAN.md §1 plus find_patterns and find_reflect \
         (SPEC-cli §5) must all be embedded and parse"
    );
}

#[test]
fn load_all_with_no_user_dir_returns_builtins_only() {
    let presets = load_all(None);
    assert_eq!(presets.len(), 8);
}

#[test]
fn load_all_user_preset_shadows_builtin() {
    let tmp = tempfile_dir();
    // A user override for `grep` with a distinguishable description.
    std::fs::write(
        tmp.join("grep.toml"),
        r#"
system = "overridden"
user   = "overridden"
[preset]
name = "grep"
description = "USER OVERRIDE"
"#,
    )
    .unwrap();
    let presets = load_all(Some(&tmp));
    assert_eq!(presets.len(), 8, "override replaces, does not add");
    let grep = presets.iter().find(|p| p.name == "grep").expect("grep missing");
    assert_eq!(grep.description, "USER OVERRIDE");
    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn load_all_user_preset_adds_new_name_alongside_builtins() {
    let tmp = tempfile_dir();
    std::fs::write(
        tmp.join("explain.toml"),
        r#"
system = "sys"
user   = "usr"
[preset]
name = "explain"
description = "Not a builtin."
"#,
    )
    .unwrap();
    let presets = load_all(Some(&tmp));
    assert_eq!(presets.len(), 9);
    assert!(presets.iter().any(|p| p.name == "explain"));
    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn load_all_missing_user_dir_does_not_error() {
    let presets = load_all(Some(std::path::Path::new("/definitely/does/not/exist")));
    assert_eq!(presets.len(), 8, "a missing override dir should silently fall back to builtins only");
}

// ── load() + resolve() unit tests ────────────────────────────────────────────

fn make_preset(system_template: &str, user_template: &str, context: Vec<ContextDef>) -> Preset {
    Preset {
        name: "test".into(),
        description: "test preset".into(),
        input_schema: json!({"type":"object"}),
        system_template: system_template.into(),
        user_template: user_template.into(),
        context,
        verify: None,
    }
}

#[test]
fn resolve_with_no_context_returns_templates_unchanged() {
    let preset = make_preset("System prompt.", "User message.", vec![]);
    let (sys, usr) = resolve(&preset, &json!({}), "/tmp");
    assert_eq!(sys, "System prompt.");
    assert_eq!(usr, "User message.");
}

#[test]
fn load_from_dir_returns_preset() {
    let toml = "system = \"sys\"\nuser = \"usr\"\n[preset]\nname = \"minimal\"\ndescription = \"Minimal.\"";
    let tmp = tempfile_dir();
    std::fs::write(tmp.join("minimal.toml"), toml).unwrap();
    let presets = load(&tmp);
    assert_eq!(presets.len(), 1);
    assert_eq!(presets[0].name, "minimal");
    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn load_ignores_non_toml_files() {
    let toml = "system = \"s\"\nuser = \"u\"\n[preset]\nname = \"only\"\ndescription = \"Only.\"";
    let tmp = tempfile_dir();
    std::fs::write(tmp.join("only.toml"), toml).unwrap();
    std::fs::write(tmp.join("readme.md"), "# not a preset").unwrap();
    let presets = load(&tmp);
    assert_eq!(presets.len(), 1, "non-toml files should be ignored");
    assert_eq!(presets[0].name, "only");
    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn load_skips_invalid_toml_gracefully() {
    let bad_toml = "this is not valid toml [[[";
    let good_toml = "system = \"s\"\nuser = \"u\"\n[preset]\nname = \"good\"\ndescription = \"Good.\"";
    let tmp = tempfile_dir();
    std::fs::write(tmp.join("a_bad.toml"), bad_toml).unwrap();
    std::fs::write(tmp.join("b_good.toml"), good_toml).unwrap();
    let presets = load(&tmp);
    assert_eq!(presets.len(), 1, "bad TOML skipped, good still loaded");
    assert_eq!(presets[0].name, "good");
    let _ = std::fs::remove_dir_all(&tmp);
}

// ── resolve pass-order: compiler brace tokens in args ────────────────────────

#[test]
fn resolve_arg_brace_tokens_not_reexpanded_by_context() {
    // If a compiler diagnostic like "expected {integer}, found {fn_body}" is
    // injected via an arg, the old pass order (args first, then context) would
    // re-scan the expanded text and replace "{fn_body}" with the real context
    // value. The fix runs context substitution first (on the clean template),
    // then arg substitution last — so brace tokens in arg values are never seen
    // by the context scanner.
    let mut ctx = std::collections::HashMap::new();
    ctx.insert("fn_body".to_string(), "THE REAL BODY".to_string());

    let tmpl = "Context: {fn_body}. Repair: ${args.repair_context}";
    let args = json!({"repair_context": "error: expected {fn_body} token"});

    // Simulate the fixed pass order.
    let after_context = template::substitute_context(tmpl, &ctx);
    let result = template::substitute_args(&after_context, &args);

    assert!(result.contains("THE REAL BODY"), "context key should be expanded");
    assert!(
        result.contains("{fn_body}"),
        "brace token inside arg value must be preserved literally, got: {result}"
    );
    assert!(
        !result.contains("error: expected THE REAL BODY"),
        "arg value must NOT be re-expanded by context pass, got: {result}"
    );
}

fn tempfile_dir() -> std::path::PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let id = COUNTER.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    let dir = std::env::temp_dir().join(format!("scout_preset_test_{pid}_{id}"));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}
