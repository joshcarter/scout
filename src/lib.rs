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
//! * `check_output`, `extract`, `grep`, `find` — the four filter pipelines.
//!   Each takes a `select::Ctx` and a JSON argument object and returns a
//!   payload, so the CLI and the MCP server run identical code.
//! * `select` — the shared LLM round-trip and the validation of what comes back.
//! * `client`, `config`, `filter_config`, `presets` — configuration and the
//!   endpoint.
//! * `source`, `render`, `edit` — search, human rendering, `$EDITOR` handoff.
//! * `stats`, `live`, `dashboard` — the call log, its live IPC feed, and the
//!   web view over both.
//! * `classify_command`, `verify`, `run_cmd`, `task` — hook plumbing, subprocess
//!   capture, and the two direct-to-model verbs.
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
pub mod render;
pub mod run_cmd;
pub mod select;
pub mod source;
pub mod stats;
// `task` is the one-function body of `scout task`; the CLI is its only caller.
pub(crate) mod task;
pub mod verify;
