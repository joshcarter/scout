// Two-pass template substitution for preset prompts.
//
// ## Pass 1 — arg substitution (${args.<field>})
//
// Applied to provider positional arg strings and named arg values inside
// `[context.<key>]` sections BEFORE the provider runs.  Replaces
// `${args.<field>}` with the corresponding field from the caller's MCP args.
// Missing fields become empty string.  Unrecognised `${...}` patterns (not
// `args.`) are left literal.
//
// ## Pass 2 — context substitution ({<key>})
//
// Applied to `system` and `user` template strings AFTER all providers have
// run and produced their output strings.  Replaces `{key}` with the
// corresponding provider output.  Unrecognised `{...}` references are left
// literal, so Rust-style format strings in prompt text don't break.

use std::collections::HashMap;

/// Pass 1: replace `${args.<field>}` in `s` using `caller_args`.
///
/// - `${args.symbol}` → caller_args["symbol"] as string (strings verbatim;
///   numbers/booleans converted via Display; missing or null → "")
/// - `${args.base_branch:-main}` → caller_args["base_branch"] if present
///   and non-empty, otherwise the literal `main`.  The `:-default` suffix
///   follows shell conventions: absent, null, and empty-string all fall back.
/// - Unmatched `${...}` (no closing `}`, or not `args.` prefix) → kept literal
pub fn substitute_args(s: &str, caller_args: &serde_json::Value) -> String {
    let mut result = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(dollar_pos) = rest.find("${args.") {
        result.push_str(&rest[..dollar_pos]);
        let after_prefix = &rest[dollar_pos + 7..]; // skip "${args."
        if let Some(close) = after_prefix.find('}') {
            let field_expr = &after_prefix[..close];
            // Split on `:-` to extract optional fallback default.
            let (field, fallback) = if let Some(sep) = field_expr.find(":-") {
                (&field_expr[..sep], &field_expr[sep + 2..])
            } else {
                (field_expr, "")
            };
            let owned: String;
            let value: &str = match caller_args.get(field) {
                Some(v) if v.is_string() => {
                    let s = v.as_str().unwrap();
                    if s.is_empty() { fallback } else { s }
                }
                Some(v) if !v.is_null() => { owned = v.to_string(); &owned }
                _ => fallback,
            };
            result.push_str(value);
            rest = &after_prefix[close + 1..];
        } else {
            // No closing brace — emit literally and stop scanning
            result.push_str("${args.");
            rest = after_prefix;
        }
    }
    result.push_str(rest);
    result
}

/// Pass 2: replace `{<key>}` in `s` using `context` output map.
///
/// - `{diff}` → context["diff"] if present; left literal `{diff}` if not.
/// - `{{` / `}}` are left as-is (not escape sequences — just brace literals).
/// - Any `{...}` whose key is not in `context` is left literal.
pub fn substitute_context(s: &str, context: &HashMap<String, String>) -> String {
    let mut result = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(open) = rest.find('{') {
        result.push_str(&rest[..open]);
        let after_open = &rest[open + 1..];
        if let Some(close) = after_open.find('}') {
            let key = &after_open[..close];
            if let Some(value) = context.get(key) {
                result.push_str(value);
            } else {
                // Unknown key — leave the braces and content literal
                result.push('{');
                result.push_str(&after_open[..close + 1]); // includes '}'
            }
            rest = &after_open[close + 1..];
        } else {
            // No closing brace — emit the '{' literally and advance
            result.push('{');
            rest = after_open;
        }
    }
    result.push_str(rest);
    result
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ── substitute_args tests ─────────────────────────────────────────────────

    #[test]
    fn substitute_args_replaces_known_field() {
        let result = substitute_args("lookup ${args.symbol}", &json!({"symbol": "my_func"}));
        assert_eq!(result, "lookup my_func");
    }

    #[test]
    fn substitute_args_missing_field_becomes_empty() {
        let result = substitute_args("lookup ${args.symbol}", &json!({}));
        assert_eq!(result, "lookup ");
    }

    #[test]
    fn substitute_args_non_string_field_converted_to_string() {
        // numbers and booleans are converted via serde_json Display (not as_str)
        assert_eq!(substitute_args("n=${args.count}", &json!({"count": 42})), "n=42");
        assert_eq!(substitute_args("flag=${args.on}", &json!({"on": true})), "flag=true");
    }

    #[test]
    fn substitute_args_null_field_becomes_empty() {
        // null and missing fields still become ""
        assert_eq!(substitute_args("n=${args.missing}", &json!({})), "n=");
        assert_eq!(substitute_args("n=${args.nul}", &json!({"nul": null})), "n=");
    }

    #[test]
    fn substitute_args_multiple_refs() {
        let result = substitute_args(
            "${args.a} and ${args.b}",
            &json!({"a": "hello", "b": "world"}),
        );
        assert_eq!(result, "hello and world");
    }

    #[test]
    fn substitute_args_no_refs_unchanged() {
        let s = "no substitution here";
        assert_eq!(substitute_args(s, &json!({})), s);
    }

    #[test]
    fn substitute_args_unclosed_brace_left_literal() {
        let result = substitute_args("${args.oops", &json!({"oops": "x"}));
        assert_eq!(result, "${args.oops");
    }

    #[test]
    fn substitute_args_ignores_non_args_dollar_braces() {
        // ${other.field} is not an args ref — left literal
        let result = substitute_args("${other.field}", &json!({"field": "x"}));
        assert_eq!(result, "${other.field}");
    }

    // ── substitute_args fallback ──────────────────────────────────────────────

    #[test]
    fn substitute_args_fallback_field_present_uses_field() {
        let result = substitute_args("base=${args.base_branch:-main}", &json!({"base_branch": "develop"}));
        assert_eq!(result, "base=develop");
    }

    #[test]
    fn substitute_args_fallback_field_absent_uses_default() {
        let result = substitute_args("base=${args.base_branch:-main}", &json!({}));
        assert_eq!(result, "base=main");
    }

    #[test]
    fn substitute_args_fallback_field_empty_uses_default() {
        let result = substitute_args("base=${args.base_branch:-main}", &json!({"base_branch": ""}));
        assert_eq!(result, "base=main");
    }

    #[test]
    fn substitute_args_fallback_field_null_uses_default() {
        let result = substitute_args("base=${args.base_branch:-main}", &json!({"base_branch": null}));
        assert_eq!(result, "base=main");
    }

    #[test]
    fn substitute_args_fallback_default_with_spaces() {
        let result = substitute_args("x=${args.val:-hello world}", &json!({}));
        assert_eq!(result, "x=hello world");
    }

    #[test]
    fn substitute_args_no_fallback_syntax_unchanged() {
        // Existing behaviour: ${args.field} with no :- is unchanged
        let result = substitute_args("${args.symbol}", &json!({"symbol": "my_func"}));
        assert_eq!(result, "my_func");
    }

    #[test]
    fn substitute_args_fallback_multiple_refs() {
        // Mix of fallback and plain refs in same string
        let result = substitute_args(
            "--base=${args.base:-main} --branch=${args.branch}",
            &json!({"branch": "feature/x"}),
        );
        assert_eq!(result, "--base=main --branch=feature/x");
    }

    // ── substitute_context tests ──────────────────────────────────────────────

    #[test]
    fn substitute_context_replaces_known_key() {
        let mut ctx = HashMap::new();
        ctx.insert("diff".to_string(), "--- a\n+++ b".to_string());
        let result = substitute_context("Staged diff:\n{diff}", &ctx);
        assert_eq!(result, "Staged diff:\n--- a\n+++ b");
    }

    #[test]
    fn substitute_context_unknown_key_left_literal() {
        let ctx = HashMap::new();
        let result = substitute_context("see {unknown}", &ctx);
        assert_eq!(result, "see {unknown}");
    }

    #[test]
    fn substitute_context_multiple_keys() {
        let mut ctx = HashMap::new();
        ctx.insert("a".to_string(), "AAA".to_string());
        ctx.insert("b".to_string(), "BBB".to_string());
        let result = substitute_context("{a} then {b}", &ctx);
        assert_eq!(result, "AAA then BBB");
    }

    #[test]
    fn substitute_context_no_refs_unchanged() {
        let ctx = HashMap::new();
        let s = "no braces here";
        assert_eq!(substitute_context(s, &ctx), s);
    }

    #[test]
    fn substitute_context_unclosed_brace_left_literal() {
        let ctx = HashMap::new();
        let result = substitute_context("oops {unclosed", &ctx);
        assert_eq!(result, "oops {unclosed");
    }

    #[test]
    fn substitute_context_mixed_known_and_unknown() {
        let mut ctx = HashMap::new();
        ctx.insert("known".to_string(), "VALUE".to_string());
        let result = substitute_context("{known} and {unknown}", &ctx);
        assert_eq!(result, "VALUE and {unknown}");
    }
}
