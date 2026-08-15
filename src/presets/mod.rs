// Preset subsystem for scout.
//
// Presets are named, parameterized prompt templates stored as TOML files.
// scout embeds its 8 built-in presets at compile time and advertises them
// on the CLI (`scout run --preset <name>`) and, in a later step, as MCP
// tools.
//
// ## Data flow
//
//   TOML file
//     → loader::parse()          → Preset struct
//     → mod::load(dir)           → Vec<Preset>  (all *.toml in one directory)
//     → mod::load_all(user_dir)  → Vec<Preset>  (embedded built-ins + user overrides)
//     → mod::resolve(preset, ..) → (system: String, user: String)
//     → LLM call                 → text
//
// ## Built-ins vs. user overrides
//
// The built-in presets are embedded in the binary via `include_str!` so a
// binary-only install always has them, with no directory to seed. A user
// preset directory (`~/.config/scout/presets/` by default) is layered on
// top: a user preset whose `[preset].name` matches a built-in shadows it;
// anything else is added alongside the built-ins.
//
// ## Preset struct fields
//
//   name            — unique identifier (e.g. "quality_review")
//   description     — shown in MCP tools/list
//   input_schema    — JSON Schema for the MCP tool's input parameters
//   system_template — system prompt; `{key}` slots filled by context providers
//   user_template   — user message; same substitution
//   context         — ordered list of providers to run before filling templates

mod loader;
pub mod providers;
pub mod template;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod tests_selectors;

use providers::{run_provider, ProviderArgs};
use serde_json::Value;
use std::collections::HashMap;
use std::path::Path;

// ── Preset data model ─────────────────────────────────────────────────────────

/// A parsed, validated preset ready for resolution.
///
/// `description` and `input_schema` are what the MCP server advertises in
/// `tools/list` (see `mcp_server.rs`): editing a preset TOML changes what the
/// calling model is told about the tool, with no code change.
#[derive(Debug, Clone)]
pub struct Preset {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
    pub system_template: String,
    pub user_template: String,
    pub context: Vec<ContextDef>,
    /// Caller-side verify kind.  `Some("build")` means the caller should run a
    /// build check after applying the LLM output and retry once on failure.
    /// `None` means no automatic verify (free-text output). No built-in preset
    /// sets this and scout runs no such loop — the field is carried purely for
    /// TOML-schema forward compatibility with user-authored presets.
    #[allow(dead_code)]
    pub verify: Option<String>,
}

/// One `[context.<key>]` section from a preset TOML file.
#[derive(Debug, Clone)]
pub struct ContextDef {
    /// The key used in `{key}` template slots.
    pub key: String,
    /// Provider name (e.g. `"git_diff_range"`, `"file_read"`).
    pub provider: String,
    /// Positional args for the provider (may contain `${args.field}` refs).
    pub args: Vec<String>,
    /// Named static args from other fields in the `[context.key]` section.
    pub extra: HashMap<String, Value>,
}

// ── Built-in presets (embedded at compile time) ─────────────────────────────

/// The 8 built-in presets, embedded so a binary-only install always has
/// them. The last two, `find_patterns` and `find_reflect`
/// (docs/search-cli.md §5), back `scout find`'s two model stages and are
/// deliberately CLI-only.
const BUILTIN_TOML: &[&str] = &[
    include_str!("../../presets/check_output.toml"),
    include_str!("../../presets/shell_safety.toml"),
    include_str!("../../presets/extract.toml"),
    include_str!("../../presets/grep.toml"),
    include_str!("../../presets/find_patterns.toml"),
    include_str!("../../presets/find_reflect.toml"),
    include_str!("../../presets/quality_review.toml"),
    include_str!("../../presets/test_review.toml"),
];

/// Parse the embedded built-in presets. Parse errors are a programming error
/// (the TOML ships in the binary) but are still reported rather than
/// panicking, so a bad build doesn't crash every invocation.
fn load_builtins() -> Vec<Preset> {
    let mut presets = Vec::with_capacity(BUILTIN_TOML.len());
    for source in BUILTIN_TOML {
        match loader::parse(source) {
            Ok(p) => presets.push(p),
            Err(e) => eprintln!("scout: embedded builtin preset failed to parse: {e}"),
        }
    }
    presets
}

// ── Public API ────────────────────────────────────────────────────────────────

/// Load all `*.toml` presets from `dir`.
///
/// Files are loaded in sorted order for deterministic results.  Parse errors
/// are logged to stderr and skipped; remaining presets are still returned.
/// If the directory cannot be read, an empty list is returned.
pub fn load(dir: &Path) -> Vec<Preset> {
    let mut presets: Vec<Preset> = Vec::new();

    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("scout: cannot read preset dir {:?}: {e}", dir);
            return presets;
        }
    };

    let mut paths: Vec<_> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|ext| ext == "toml"))
        .collect();
    paths.sort(); // deterministic load order

    for path in paths {
        match std::fs::read_to_string(&path) {
            Err(e) => eprintln!("scout: cannot read {:?}: {e}", path),
            Ok(source) => match loader::parse(&source) {
                Err(e) => eprintln!("scout: preset {:?} parse error: {e}", path),
                Ok(p) => presets.push(p),
            },
        }
    }

    presets
}

/// Load the embedded built-in presets, then overlay any user presets found
/// in `user_dir` (if given and it exists) — a user preset whose
/// `[preset].name` matches a built-in shadows it; anything else is added.
///
/// Returned in sorted-by-name order for deterministic `scout run` behavior.
pub fn load_all(user_dir: Option<&Path>) -> Vec<Preset> {
    let mut by_name: HashMap<String, Preset> = HashMap::new();
    for p in load_builtins() {
        by_name.insert(p.name.clone(), p);
    }
    if let Some(dir) = user_dir {
        if dir.exists() {
            for p in load(dir) {
                by_name.insert(p.name.clone(), p);
            }
        }
    }
    let mut presets: Vec<Preset> = by_name.into_values().collect();
    presets.sort_by(|a, b| a.name.cmp(&b.name));
    presets
}

/// Execute a preset against the given caller args and project root.
///
/// 1. For each context entry: substitute `${args.field}` in `_args` and extra
///    values, then run the provider.
/// 2. Build context map: key → output string.
/// 3. Substitute `{key}` in system_template and user_template.
/// 4. Return `(system, user)`.
///
/// Provider failures are soft: the failed key maps to an error message string
/// rather than aborting the whole resolution.
pub fn resolve(preset: &Preset, args: &Value, project: &str) -> (String, String) {
    let mut context_map: HashMap<String, String> = HashMap::new();

    for def in &preset.context {
        // Substitute ${args.field} in positional args
        let positional: Vec<String> = def
            .args
            .iter()
            .map(|a| template::substitute_args(a, args))
            .collect();

        // Substitute ${args.field} in extra (string values only)
        let extra: HashMap<String, Value> = def
            .extra
            .iter()
            .map(|(k, v)| {
                let subbed = if let Some(s) = v.as_str() {
                    Value::String(template::substitute_args(s, args))
                } else {
                    v.clone()
                };
                (k.clone(), subbed)
            })
            .collect();

        let provider_args = ProviderArgs {
            positional: &positional,
            named: &extra,
            project_root: project,
        };

        let output = run_provider(&def.provider, &provider_args).unwrap_or_else(|e| {
            eprintln!("scout: provider '{}' error for key '{}': {e}", def.provider, def.key);
            format!("[provider '{}' error: {e}]", def.provider)
        });
        context_map.insert(def.key.clone(), output);
    }

    let system = template::substitute_context(&preset.system_template, &context_map);
    // Two-pass substitution on the user template:
    // Pass 1: substitute {key} context refs on the raw template — context values come
    //   from trusted providers, so this is safe and produces a context-key-free string.
    // Pass 2: substitute ${args.*} refs on that result.
    //   This ordering is critical: LLM/compiler output in args can contain brace-wrapped
    //   tokens (e.g. `error[E0308]: expected {integer}, found {f64}`) that would collide
    //   with context keys if Pass 2 ran first.
    let user_with_context = template::substitute_context(&preset.user_template, &context_map);
    let user = template::substitute_args(&user_with_context, args);
    (system, user)
}
