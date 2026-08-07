mod check_output;
mod classify_command;
mod client;
mod config;
mod extract;
mod filter_config;
mod grep;
mod mcp_server;
mod presets;
mod render;
mod run_cmd;
mod select;
mod source;
mod stats;
mod task;
mod verify;

use clap::{Parser, Subcommand, ValueEnum};
use client::LlmClient;
use serde_json::json;
use std::io::IsTerminal;

/// How `scout grep` writes its results (SPEC-cli §1).
#[derive(Copy, Clone, PartialEq, Eq, ValueEnum)]
enum Format {
    /// ack-style text: `path:line`, the model's note, a gutter-numbered
    /// context block.  The default whether or not stdout is a terminal —
    /// piping must not explode into JSON.
    Human,
    /// The full JSON payload, pretty-printed: the same bytes the MCP tool
    /// returns.  Structure is opt-in, for scripts that want it.
    Json,
    /// `file:line:col: text`, one hit per line — quickfix-compatible
    /// (`vim -q`).  `col` is 1 until match offsets land.
    Vimgrep,
}

/// `--color`, the usual three-state.
#[derive(Copy, Clone, PartialEq, Eq, ValueEnum)]
enum ColorWhen {
    Auto,
    Always,
    Never,
}

impl ColorWhen {
    /// Resolve to a yes/no.  `auto` means "a terminal is watching and the user
    /// has not opted out via `NO_COLOR`" (https://no-color.org: set and
    /// non-empty).
    fn enabled(self) -> bool {
        match self {
            ColorWhen::Always => true,
            ColorWhen::Never => false,
            ColorWhen::Auto => {
                let opted_out = std::env::var("NO_COLOR").is_ok_and(|v| !v.is_empty());
                !opted_out && std::io::stdout().is_terminal()
            }
        }
    }

    /// Parse the `[cli] color` config value.  Already validated by
    /// `filter_config`, so anything unexpected is `auto`.
    fn from_config(value: &str) -> Self {
        match value {
            "always" => ColorWhen::Always,
            "never" => ColorWhen::Never,
            _ => ColorWhen::Auto,
        }
    }
}

/// Everything `run_filter` needs to render `grep`'s payload for a human.
///
/// Its presence is also what switches `run_filter` onto grep's exit-code
/// convention and installs the stderr progress sink; `extract` and `check`
/// pass `None` and keep their current JSON-and-exit-1 behavior (SPEC-cli §1
/// defers them to a later phase).
struct GrepOutput {
    format: Format,
    color: bool,
    /// The `context_lines` the search ran with — the gutter needs it to number
    /// the block, which the payload itself does not record.
    context_lines: usize,
    /// The effective `--max-hits`, for the "capped at top N" status line.
    max_hits: usize,
    /// `[grep] max_hits_scanned`, for the "search truncated" status line.
    max_hits_scanned: usize,
}

#[derive(Parser)]
#[command(
    name = "scout",
    version,
    about = "Local-LLM scout: offloads output-checking, command screening, and targeted code questions to a local model"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Serve MCP over stdio (used by the Claude Code plugin)
    Mcp,
    /// One-shot preset invocation: `scout run --preset <p> --arg k=v ...`
    ///
    /// Also accepts `--ping` (check endpoint only) and `--project PATH`. See
    /// `run_cmd.rs` for the full flag set — this variant just forwards the
    /// raw tail to run_cmd's own parser so hooks/scripts calling `scout run`
    /// keep working unchanged.
    #[command(disable_help_flag = true)]
    Run {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Ad-hoc escape hatch: send a raw prompt straight to the local LLM.
    Task {
        /// The prompt to send as the user message.
        prompt: String,
    },
    /// Intent-filtered grep: search the project, keep only what serves the intent.
    Grep {
        /// Search pattern (literal by default; see --regex).
        pattern: String,
        /// What you are actually looking for. Omit it to skip the LLM rerank
        /// entirely — an unfiltered structured search, capped at --max-hits.
        intent: Option<String>,
        /// Treat the pattern as a regex.
        #[arg(long)]
        regex: bool,
        /// Hits to return after filtering (default: `[cli] max_hits`, 20).
        /// A ceiling, not a quota — the model returns only what it kept.
        #[arg(short = 'n', long)]
        max_hits: Option<u64>,
        /// Context lines on each side of a match
        /// (default: `[cli] context`, else `[grep] context_lines`).
        #[arg(short = 'C', long)]
        context: Option<usize>,
        /// Output format (default: human text, colored only on a terminal).
        #[arg(long, value_enum)]
        format: Option<Format>,
        /// When to colorize human output (default: `[cli] color`, `auto`).
        #[arg(long, value_enum)]
        color: Option<ColorWhen>,
        /// Project root to search (default: $PWD).
        #[arg(long)]
        project: Option<String>,
    },
    /// Targeted file Q&A: answer a question with the file's relevant line ranges.
    Extract {
        /// Path to the file (absolute, or relative to the project root).
        file: String,
        /// What you want from the file.
        question: String,
        /// Budget for returned lines (default 120).
        #[arg(long)]
        max_lines: Option<u64>,
        /// Project root for relative paths (default: $PWD).
        #[arg(long)]
        project: Option<String>,
    },
    /// Run a build/test command and classify its output with the local model.
    Check {
        /// The command to run, e.g. "cargo test --quiet".
        command: String,
        /// Working directory for the command (default: the project root).
        #[arg(long)]
        cwd: Option<String>,
        /// Timeout in seconds (default 60, max 600).
        #[arg(long)]
        timeout_seconds: Option<u64>,
        /// Project root (default: $PWD).
        #[arg(long)]
        project: Option<String>,
    },
    /// Classify a Bash command (read from stdin) as a build/test invocation.
    ///
    /// Hook-internal plumbing for hooks/prefer-local-llm.sh — deliberately not
    /// an MCP tool. Reads the raw command from stdin (never argv: commands
    /// carry quotes, newlines and heredocs), prints
    /// `{"intercept":bool,"escape":bool}` and exits 0. Purely lexical: no
    /// config, no network, no local model.
    ClassifyCommand,
    /// Print the call log report (presets/tasks run so far).
    Stats,
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Mcp => mcp_server::serve(),
        Command::Run { args } => run_cmd::run_subcommand(&args),
        Command::Task { prompt } => run_task(&prompt),
        Command::Grep { pattern, intent, regex, max_hits, context, format, color, project } => {
            run_grep(pattern, intent, regex, max_hits, context, format, color, project)
        }
        Command::Extract { file, question, max_lines, project } => run_filter(
            "extract",
            project,
            json!({ "file": file, "question": question, "max_lines": max_lines }),
            None,
        ),
        Command::Check { command, cwd, timeout_seconds, project } => run_filter(
            "check_output",
            project,
            json!({ "command": command, "cwd": cwd, "timeout_seconds": timeout_seconds }),
            None,
        ),
        Command::ClassifyCommand => classify_command::run_subcommand(),
        Command::Stats => stats::print_report(),
    }
}

/// Load the preset set scout uses everywhere: the 6 embedded built-ins,
/// overlaid with any user overrides from `config::config_dir()/presets/`
/// — honors `$XDG_CONFIG_HOME`, default `~/.config/scout/presets/` —
/// (or `$SCOUT_PRESET_DIR`, for tests and non-standard installs).
fn load_presets() -> Vec<presets::Preset> {
    let user_dir = std::env::var("SCOUT_PRESET_DIR")
        .ok()
        .filter(|v| !v.is_empty())
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| config::config_dir().join("presets"));
    presets::load_all(Some(&user_dir))
}

/// Handle the three filter verbs (`grep`, `extract`, `check`).
///
/// Thin wrapper over exactly the code path the MCP tools use (PLAN §3): same
/// `Ctx`, same handlers, same payloads — only the argument parsing and the
/// rendering differ.  Config is loaded leniently, so the bypass paths (small
/// file, short hit list) still work with no `config.toml` at all; when the
/// model really is needed, the failure names both the reason and the raw tool
/// to fall back to.  Diverges via `std::process::exit`, like `run`/`task`.
/// Resolve `scout grep`'s flags against the `[cli]` / `[grep]` config tables
/// and hand off to `run_filter`.
///
/// The precedence is the usual one — explicit flag, then `[cli]`, then the
/// shared `[grep]` default — and it lives here rather than in `grep.rs` so
/// the MCP path never sees a terminal-only default.
#[allow(clippy::too_many_arguments)]
fn run_grep(
    pattern: String,
    intent: Option<String>,
    regex: bool,
    max_hits: Option<u64>,
    context: Option<usize>,
    format: Option<Format>,
    color: Option<ColorWhen>,
    project: Option<String>,
) -> ! {
    let (_, grep_cfg) = filter_config::load();
    let cli_cfg = filter_config::load_cli();

    let context_lines = context.or(cli_cfg.context).unwrap_or(grep_cfg.context_lines);
    let max_hits = max_hits.unwrap_or(cli_cfg.max_hits as u64);
    let color = color.unwrap_or_else(|| ColorWhen::from_config(&cli_cfg.color));

    run_filter(
        "grep",
        project,
        json!({
            "pattern": pattern,
            "intent": intent,
            "regex": regex,
            "max_hits": max_hits,
            "context_lines": context_lines,
        }),
        Some(GrepOutput {
            format: format.unwrap_or(Format::Human),
            color: color.enabled(),
            context_lines,
            // `grep::run` clamps; mirror it so "capped at top N" never claims
            // a cap the pipeline did not actually apply.
            max_hits: (max_hits as usize).clamp(1, 100),
            max_hits_scanned: grep_cfg.max_hits_scanned,
        }),
    )
}

fn run_filter(
    tool: &str,
    project: Option<String>,
    args: serde_json::Value,
    grep_out: Option<GrepOutput>,
) -> ! {
    let project = project.unwrap_or_else(|| {
        std::env::current_dir()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| ".".to_string())
    });

    let (client, client_error) = match config::load_config(&config::config_path()) {
        Ok(c) => (Some(LlmClient::new(c)), None),
        Err(e) => (None, Some(e)),
    };
    let presets = load_presets();
    let ctx = select::Ctx {
        client: client.as_ref(),
        client_error,
        presets: &presets,
        project,
        // Only the terminal path wants progress chatter, and only ever on
        // stderr — stdout carries the result and may be piped.
        progress: grep_out
            .as_ref()
            .map(|_| Box::new(|msg: &str| eprintln!("{msg}")) as select::ProgressSink),
    };

    let result = match tool {
        "grep" => grep::run(&ctx, &args),
        "extract" => extract::run(&ctx, &args),
        _ => check_output::run(&ctx, &args),
    };

    match result {
        Ok(payload) => match &grep_out {
            Some(out) => finish_grep(&payload, out),
            None => {
                println!("{}", pretty_json(&payload));
                std::process::exit(0);
            }
        },
        Err(e) => {
            // `ToolError::text` already names the tool and the fallback.
            eprintln!("{}", e.text());
            // grep uses the grep convention (2 = error); extract/check keep
            // their existing exit-1-on-error contract.
            std::process::exit(if grep_out.is_some() { 2 } else { 1 });
        }
    }
}

fn pretty_json(payload: &serde_json::Value) -> String {
    serde_json::to_string_pretty(payload).unwrap_or_else(|_| payload.to_string())
}

/// Write `grep`'s payload out and exit with the grep convention (SPEC-cli §1):
/// 0 when at least one hit came back, 1 when none did.
///
/// Results go to stdout, metadata to stderr — always, in every format, so a
/// script never has to strip status chatter out of the thing it is parsing.
fn finish_grep(payload: &serde_json::Value, out: &GrepOutput) -> ! {
    match out.format {
        Format::Json => println!("{}", pretty_json(payload)),
        Format::Vimgrep => print!("{}", render::render_vimgrep(payload)),
        Format::Human => print!(
            "{}",
            render::render_human(
                payload,
                &render::RenderOpts { color: out.color, context_lines: out.context_lines },
            )
        ),
    }
    for line in grep_status(payload, out) {
        eprintln!("{line}");
    }
    let returned = payload.get("hits").and_then(|h| h.as_array()).map_or(0, Vec::len);
    std::process::exit(if returned > 0 { 0 } else { 1 });
}

/// The stderr status lines for one grep result (SPEC-cli §2).
///
/// Everything here is read back out of the payload rather than threaded
/// through the pipeline: the payload already carries every counter a human
/// wants, and keeping the derivation in one pure function makes it testable.
fn grep_status(payload: &serde_json::Value, out: &GrepOutput) -> Vec<String> {
    let num = |key: &str| payload.get(key).and_then(serde_json::Value::as_u64).unwrap_or(0) as usize;
    let flag = |key: &str| payload.get(key).and_then(serde_json::Value::as_bool).unwrap_or(false);

    let returned = payload.get("hits").and_then(|h| h.as_array()).map_or(0, Vec::len);
    let hits_total = num("hits_total");
    let pattern = payload.get("pattern").and_then(serde_json::Value::as_str).unwrap_or("");

    // Empty is the one case a human can misread, so it says which empty it is.
    if returned == 0 {
        return vec![if flag("none_relevant") {
            format!(
                "local model judged none of the {hits_total} hits relevant — \
                 rerun without an intent to see all of them"
            )
        } else {
            format!("no matches for '{pattern}'")
        }];
    }

    let mode = payload.get("mode").and_then(serde_json::Value::as_str).unwrap_or("");
    let no_intent = payload.get("intent").is_none_or(serde_json::Value::is_null);
    let mut lines = vec![match (mode, no_intent) {
        ("rerank", _) => {
            format!("{returned} of {hits_total} hits kept · {} filtered by intent", num("dropped"))
        }
        (_, true) if returned < hits_total => {
            format!("{returned} of {hits_total} hits · no intent given, unfiltered")
        }
        (_, true) => format!("{returned} hits · no intent given, unfiltered"),
        _ => format!("{returned} hits · few enough to return unfiltered"),
    }];

    if flag("truncated_before_rerank") {
        lines.push(format!(
            "only the first {} hits reached the model ([grep] max_considered)",
            num("hits_considered")
        ));
    }
    if flag("search_truncated") {
        lines.push(format!(
            "search truncated at {} hits ([grep] max_hits_scanned)",
            out.max_hits_scanned
        ));
    }
    if returned >= out.max_hits && hits_total > returned {
        lines.push(format!("capped at top {} (--max-hits)", out.max_hits));
    }
    lines
}

/// Handle `scout task "<prompt>"` — the generic escape hatch. Sends the
/// prompt as-is to the local LLM under a minimal system prompt and prints
/// the response. Diverges via `std::process::exit`, mirroring run_cmd's
/// error-handling style.
fn run_task(prompt: &str) -> ! {
    let cfg_path = config::config_path();
    let cfg = match config::load_config(&cfg_path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("scout task: {e}");
            std::process::exit(1);
        }
    };
    let client = LlmClient::new(cfg);

    let system = "You are a helpful, concise assistant embedded in a coding agent's \
        local-LLM escape hatch. Answer the user's request directly.";
    let params = json!({"system": system, "user": prompt});

    match task::handle(&client, &params) {
        Ok(result) => {
            stats::log_call(
                "task",
                result["usage"]["prompt_tokens"].as_u64().unwrap_or(0),
                result["usage"]["completion_tokens"].as_u64().unwrap_or(0),
                result["duration_ms"].as_u64().unwrap_or(0),
                true,
            );
            println!("{}", result["text"].as_str().unwrap_or(""));
            std::process::exit(0);
        }
        Err(e) => {
            stats::log_call("task", 0, 0, 0, false);
            eprintln!("scout task: {:?}", e);
            std::process::exit(1);
        }
    }
}
