mod check_output;
mod client;
mod config;
mod extract;
mod filter_config;
mod grep;
mod mcp_server;
mod presets;
mod run_cmd;
mod select;
mod source;
mod stats;
mod task;
mod verify;

use clap::{Parser, Subcommand};
use client::LlmClient;
use serde_json::json;

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
        /// What you are actually looking for.
        intent: String,
        /// Treat the pattern as a regex.
        #[arg(long)]
        regex: bool,
        /// Hits to return after filtering (default 10).
        #[arg(long)]
        max_hits: Option<u64>,
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
    /// Print the call log report (presets/tasks run so far).
    Stats,
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Mcp => mcp_server::serve(),
        Command::Run { args } => run_cmd::run_subcommand(&args),
        Command::Task { prompt } => run_task(&prompt),
        Command::Grep { pattern, intent, regex, max_hits, project } => run_filter(
            "grep",
            project,
            json!({
                "pattern": pattern,
                "intent": intent,
                "regex": regex,
                "max_hits": max_hits,
            }),
        ),
        Command::Extract { file, question, max_lines, project } => run_filter(
            "extract",
            project,
            json!({ "file": file, "question": question, "max_lines": max_lines }),
        ),
        Command::Check { command, cwd, timeout_seconds, project } => run_filter(
            "check_output",
            project,
            json!({ "command": command, "cwd": cwd, "timeout_seconds": timeout_seconds }),
        ),
        Command::Stats => stats::print_report(),
    }
}

/// Load the preset set scout uses everywhere: the 6 embedded built-ins,
/// overlaid with any user overrides from `~/.config/scout/presets/`
/// (or `$SCOUT_PRESET_DIR`, for tests and non-standard installs).
fn load_presets() -> Vec<presets::Preset> {
    let user_dir = std::env::var("SCOUT_PRESET_DIR")
        .ok()
        .map(std::path::PathBuf::from)
        .or_else(|| {
            std::env::var("HOME")
                .ok()
                .map(|h| std::path::PathBuf::from(h).join(".config").join("scout").join("presets"))
        });
    presets::load_all(user_dir.as_deref())
}

/// Handle the three filter verbs (`grep`, `extract`, `check`).
///
/// Thin wrapper over exactly the code path the MCP tools use (PLAN §3): same
/// `Ctx`, same handlers, same payloads — only the argument parsing and the
/// rendering differ.  Config is loaded leniently, so the bypass paths (small
/// file, short hit list) still work with no `config.toml` at all; when the
/// model really is needed, the failure names both the reason and the raw tool
/// to fall back to.  Diverges via `std::process::exit`, like `run`/`task`.
fn run_filter(tool: &str, project: Option<String>, args: serde_json::Value) -> ! {
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
    };

    let result = match tool {
        "grep" => grep::run(&ctx, &args),
        "extract" => extract::run(&ctx, &args),
        _ => check_output::run(&ctx, &args),
    };

    match result {
        Ok(payload) => {
            println!("{}", serde_json::to_string_pretty(&payload).unwrap_or_else(|_| payload.to_string()));
            std::process::exit(0);
        }
        Err(e) => {
            // `ToolError::text` already names the tool and the fallback.
            eprintln!("{}", e.text());
            std::process::exit(1);
        }
    }
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
