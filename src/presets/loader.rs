// TOML preset loader.
//
// Parses `.toml` preset files into `Preset` structs.  Validates that every
// `[context.<key>]` section references a known provider.
//
// ## TOML schema
//
// `system` and `user` are top-level keys and must appear before any `[section]`
// headers in the file.
//
// ```toml
// system = "You are a code reviewer…"
// user   = "{review_instructions}\n\nDiff:\n{diff}\n"
//
// [preset]
// name        = "quality_review"
// description = "Route the Quality Pedant reviewer through the local LLM."
//
// # input_schema is optional; defaults to an empty object schema.
// [preset.input_schema]
// type     = "object"
// required = []
//
// [context.diff]
// provider = "git_diff_range"
// _args    = ["${args.git_diff_range}"]
//
// [context.review_instructions]
// provider = "file_read"
// _args    = ["${args.prompt_file}"]
// ```

use super::providers::provider_known;
use super::Preset;
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashMap;

// ── Serde structs (mirroring the TOML schema) ─────────────────────────────────

#[derive(Deserialize)]
struct PresetFile {
    preset: PresetMeta,
    #[serde(default)]
    context: HashMap<String, ContextEntry>,
    system: String,
    user: String,
}

#[derive(Deserialize)]
struct PresetMeta {
    name: String,
    description: String,
    /// JSON Schema for the tool's input parameters.  Defaults to `{"type":"object"}`.
    #[serde(default = "default_input_schema")]
    input_schema: Value,
    /// Caller-side verify kind.  `"build"` triggers apply-diff + build check + one repair retry.
    #[serde(default)]
    verify: Option<String>,
}

fn default_input_schema() -> Value {
    json!({"type": "object", "properties": {}, "required": []})
}

/// A single `[context.<key>]` section.
#[derive(Deserialize)]
struct ContextEntry {
    provider: String,
    /// `_args`: positional arguments passed to the provider (may contain `${args.field}` refs).
    #[serde(rename = "_args", default)]
    args: Vec<String>,
    /// All other fields in this section become named static args.
    #[serde(flatten)]
    extra: HashMap<String, Value>,
}

// ── Public parse function ─────────────────────────────────────────────────────

/// Parse a TOML string into a `Preset`, validating all provider references.
///
/// Returns `Err(String)` with a human-readable message if:
/// - The TOML is malformed
/// - A `[context.<key>]` section references an unknown or blocked provider
pub fn parse(source: &str) -> Result<Preset, String> {
    let file: PresetFile =
        toml::from_str(source).map_err(|e| format!("TOML parse error: {e}"))?;

    // Validate providers
    let mut context_defs = Vec::with_capacity(file.context.len());
    for (key, entry) in file.context {
        if !provider_known(&entry.provider) {
            return Err(format!(
                "[context.{key}] references unknown or blocked provider '{}'",
                entry.provider
            ));
        }
        context_defs.push(super::ContextDef {
            key,
            provider: entry.provider,
            args: entry.args,
            extra: entry.extra,
        });
    }
    // Sort by key for deterministic ordering (TOML HashMaps are unordered)
    context_defs.sort_by(|a, b| a.key.cmp(&b.key));

    Ok(Preset {
        name: file.preset.name,
        description: file.preset.description,
        input_schema: file.preset.input_schema,
        system_template: file.system,
        user_template: file.user,
        context: context_defs,
        verify: file.preset.verify,
    })
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // Note: `system` and `user` must be top-level TOML keys (before any [section]).
    const MINIMAL_TOML: &str = r#"
system = "You are a helper."
user   = "Help with: {context_key}"

[preset]
name = "my_task"
description = "A test preset."

[context.context_key]
provider = "git_diff_range"
_args    = ["HEAD~1"]
"#;

    const WITH_SCHEMA_TOML: &str = r#"
system = "You are a test writer."
user   = "Read this: {file_body}"

[preset]
name = "read_file"
description = "Read a file."

[preset.input_schema]
type = "object"
required = ["path"]

[preset.input_schema.properties.path]
type = "string"
description = "File path"

[context.file_body]
provider = "file_read"
_args    = ["${args.path}"]
"#;

    #[test]
    fn parse_minimal_preset() {
        let preset = parse(MINIMAL_TOML).expect("parse failed");
        assert_eq!(preset.name, "my_task");
        assert_eq!(preset.description, "A test preset.");
        assert_eq!(preset.system_template, "You are a helper.");
        assert_eq!(preset.user_template, "Help with: {context_key}");
        assert_eq!(preset.context.len(), 1);
        assert_eq!(preset.context[0].key, "context_key");
        assert_eq!(preset.context[0].provider, "git_diff_range");
    }

    #[test]
    fn parse_defaults_input_schema_to_empty_object() {
        let preset = parse(MINIMAL_TOML).expect("parse failed");
        assert_eq!(preset.input_schema["type"], "object");
    }

    #[test]
    fn parse_custom_input_schema() {
        let preset = parse(WITH_SCHEMA_TOML).expect("parse failed");
        assert_eq!(preset.input_schema["type"], "object");
        let required = preset.input_schema["required"].as_array().unwrap();
        assert!(required.iter().any(|v| v.as_str() == Some("path")));
    }

    #[test]
    fn parse_context_with_args() {
        let preset = parse(WITH_SCHEMA_TOML).expect("parse failed");
        let ctx = preset.context.iter().find(|c| c.key == "file_body").expect("file_body missing");
        assert_eq!(ctx.provider, "file_read");
        assert_eq!(ctx.args, vec!["${args.path}"]);
    }

    #[test]
    fn parse_unknown_provider_returns_error() {
        let toml = r#"
system = "sys"
user   = "usr"
[preset]
name = "bad"
description = "Bad preset."
[context.x]
provider = "nonexistent_provider_xyz"
"#;
        let err = parse(toml).unwrap_err();
        assert!(err.contains("unknown or blocked"), "expected 'unknown or blocked' in: {err}");
        assert!(err.contains("nonexistent_provider_xyz"), "provider name missing from: {err}");
    }

    #[test]
    fn parse_blocked_provider_returns_error() {
        // ct_* providers are gone entirely in scout — any ct_* reference is
        // unknown/blocked, same as a made-up name.
        let toml = r#"
system = "sys"
user   = "usr"
[preset]
name = "bad"
description = "Tries to use a provider outside the allowlist."
[context.x]
provider = "daemon_lookup"
"#;
        let err = parse(toml).unwrap_err();
        assert!(err.contains("unknown or blocked"), "expected 'unknown or blocked' in: {err}");
    }

    #[test]
    fn parse_named_static_arg() {
        let toml = r#"
system = "sys"
user   = "{log}"
[preset]
name = "recent"
description = "Recent commits."
[context.log]
provider = "git_recent_commits"
n = 10
"#;
        let preset = parse(toml).expect("parse failed");
        let ctx = &preset.context[0];
        assert_eq!(ctx.provider, "git_recent_commits");
        assert_eq!(ctx.extra.get("n").and_then(|v| v.as_u64()), Some(10));
    }
}
