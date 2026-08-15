//! The `scout` binary: parse argv, dispatch, and nothing else.
//!
//! Every verb's implementation lives in `scout_llm` (see `src/lib.rs` for why
//! the split exists).  Most arms below diverge — the CLI's error contract is
//! `std::process::exit` with a verb-specific code — which is why the match
//! mixes `!` and `anyhow::Result<()>`.

use clap::Parser;
use scout_llm::cli::{self, Cli, Command};
use scout_llm::{classify_command, dashboard, mcp_server, run_cmd, stats};

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Mcp => mcp_server::serve(),
        Command::Run { args } => run_cmd::run_subcommand(&args),
        Command::Task { prompt } => cli::run_task(&prompt),
        Command::Grep(args) => cli::run_grep(*args),
        Command::Find(args) => cli::run_find(*args),
        Command::Edit(args) => cli::run_edit(*args),
        Command::Extract { file, question, max_lines, project } => {
            cli::run_extract(&file, &question, max_lines, project)
        }
        Command::Check { command, cwd, timeout_seconds, project } => {
            cli::run_check(&command, cwd.as_deref(), timeout_seconds, project)
        }
        Command::Wrap { command, question, cwd, timeout, project } => {
            cli::run_wrap(&command, question.as_deref(), cwd.as_deref(), timeout, project)
        }
        Command::ClassifyCommand => classify_command::run_subcommand(),
        Command::Stats => stats::print_report(),
        Command::Gc { all } => cli::run_gc(all),
        Command::Dashboard(args) => dashboard::run(&args),
    }
}
