//! scout — a local-LLM scout for coding agents.
//!
//! Everything scout does lives here; `src/main.rs` is a thin binary that parses
//! argv and dispatches into `cli`.  The split exists for two reasons, both of
//! them structural rather than stylistic:
//!
//! * **It removes an inversion.**  When these modules were declared in
//!   `main.rs`, leaf services reached back up into the binary root for shared
//!   helpers — `mcp_server` and `run_cmd` both called `crate::load_presets()`,
//!   which was defined next to `fn main`.  A library has no root to reach into,
//!   so that helper had to move to where it belonged (`presets::load_presets`).
//!
//! * **It makes the internals reachable from `tests/`.**  A `#[[bin]]`-only
//!   crate can be tested from the outside only as a subprocess.  The pure parts
//!   of scout — `select`'s selector-JSON validation, `render`'s human output,
//!   `classify_command`'s lexer, `source`'s search, `verify`'s capture — are the
//!   parts most worth pinning, and none of them were importable.
//!
//! ## Layout
//!
//! * `cli` — the clap definition and the terminal-only rendering on top of it.
//! * `mcp_server` — the same pipelines, served over stdio to Claude Code.
//! * `check_output`, `wrap`, `extract`, `grep`, `find` — the filter pipelines.
//!   Each takes a `select::Ctx` and a JSON argument object and returns a
//!   payload, so the CLI and the MCP server run identical code.
//! * `select` — the shared LLM round-trip (`round_trip`, which `scout run` and
//!   `scout task` call too) and the validation of what comes back.
//! * `client`, `config`, `filter_config`, `presets` — configuration and the
//!   endpoint.
//! * `source`, `render`, `edit` — search, human rendering, `$EDITOR` handoff.
//! * `stats`, `live`, `dashboard` — the call log, its live IPC feed, and the
//!   web view over both, over the one row type in `record`.
//! * `classify_command`, `verify`, `run_cmd` — hook plumbing, subprocess
//!   capture, and `scout run`.  (`scout task`, the other direct-to-model verb,
//!   is a dozen lines in `cli` now that it shares `select::round_trip`; the
//!   `task` module it used to live in held nothing else.)
//!
//! Visibility follows one rule: `pub` is for what a caller outside the crate
//! legitimately needs — the binary, and tests pinning pure logic.  Anything
//! that is an implementation detail of the crate stays `pub(crate)`.

pub mod check_output;
pub mod classify_command;
pub mod cli;
pub mod client;
pub mod config;
pub mod dashboard;
pub mod edit;
pub mod extract;
pub mod filter_config;
pub mod find;
pub mod grep;
// The live feed is an implementation detail of the dashboard: an abstract
// unix-socket protocol between a running scout and whatever is watching it.
// Nothing outside the crate has business speaking it.
pub(crate) mod live;
pub mod mcp_server;
pub mod presets;
// The read-side row `dashboard` renders and `live` synthesizes.  It sits below
// both so neither has to copy the other's fields; like `live`, it describes an
// internal shape and nothing outside the crate speaks it.
pub(crate) mod record;
pub mod render;
pub mod run_cmd;
pub mod select;
pub mod source;
// The raw spool: the full captured output behind a filtered result, so the
// caller can escalate past the summary without re-running the command
// (docs/wrap-watch.md §2).
pub mod spool;
pub mod stats;
pub mod verify;
// Run any verbose command and return its output condensed, with the full
// capture spooled (docs/wrap-watch.md §3).  `check_output`'s sibling: same
// capture, different job — retrieval rather than a verdict.
pub mod wrap;

/// `--project`, or `$PWD`.
///
/// Shared by the CLI, `scout run`, and the MCP server so a missing project
/// argument always means the same thing. The project root is where
/// `extract`/`grep`/`find` resolve relative paths; three copies of this
/// fallback would be free to drift independently, and a divergence would be
/// quiet rather than loud.
pub fn resolve_project(project: Option<String>) -> String {
    project.unwrap_or_else(|| {
        std::env::current_dir().map_or_else(|_| ".".to_string(), |p| p.display().to_string())
    })
}

#[cfg(test)]
mod resolve_project_tests {
    use super::resolve_project;

    #[test]
    fn explicit_path_wins() {
        assert_eq!(resolve_project(Some("/tmp/somewhere".into())), "/tmp/somewhere");
    }

    #[test]
    fn missing_path_is_cwd_or_dot() {
        let got = resolve_project(None);
        let expected =
            std::env::current_dir().map_or_else(|_| ".".to_string(), |p| p.display().to_string());
        assert_eq!(got, expected);
    }
}
