//! The `scout` command line: the clap definition, the flag-precedence rules,
//! and the terminal-only rendering that sits on top of the pipelines.
//!
//! This lives in the library rather than in `main.rs` so it can be exercised
//! by tests.  The argv dialect (docs/search-cli.md) is a contract with hooks
//! and shell scripts; the parts of it that are pure — flag precedence, glob
//! folding, the stderr status lines — are worth pinning directly rather than
//! only through a subprocess.
//!
//! What stays in `main.rs` is the binary's own concern: `Cli::parse()` and the
//! dispatch table.  What is here diverges via `std::process::exit` on purpose
//! (see the individual `run_*` functions) — that is the CLI's contract with
//! its callers, and moving it into a library does not change it.

use crate::client::LlmClient;
use crate::{
    check_output, config, dashboard, edit, extract, filter_config, find, grep, presets, render,
    select, source, spool, stats, wrap,
};
use clap::{Parser, Subcommand, ValueEnum};
use serde_json::json;
use std::io::IsTerminal;

/// How `scout grep` writes its results (docs/search-cli.md §1).
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
    /// (`vim -q`).  `col` is the real 1-based match column.
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
/// pass `None` and keep their current JSON-and-exit-1 behavior (docs/search-cli.md §1
/// defers them to a later phase).
struct GrepOutput {
    format: Format,
    color: bool,
    /// The `context_lines` the search ran with — the gutter needs it to number
    /// the block, which the payload itself does not record.
    context_lines: usize,
    /// Per-line render cap (`-M`); 0 is unlimited.  Human format only.
    max_columns: usize,
    /// The effective `--max-hits`, for the "capped at top N" status line.
    max_hits: usize,
    /// `[grep] max_hits_scanned`, for the "search truncated" status line.
    max_hits_scanned: usize,
}

impl GrepOutput {
    /// The renderer's slice of these options.  `numbered` is off here — only
    /// `scout edit`'s picker turns it on, on its own copy.
    fn render_opts(&self) -> render::RenderOpts {
        render::RenderOpts {
            color: self.color,
            context_lines: self.context_lines,
            max_columns: self.max_columns,
            numbered: false,
        }
    }
}

#[derive(Parser)]
#[command(
    name = "scout",
    version,
    about = "Local-LLM scout: offloads output-checking, command screening, and targeted code questions to a local model"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand)]
pub enum Command {
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
    Grep(Box<GrepArgs>),
    /// Intent-only search: ask a question, the local model guesses the patterns.
    ///
    /// `scout find "where are the config file options parsed?"` — scout runs
    /// every guessed pattern itself and reranks the union against the question.
    /// Requires a configured local model (unlike `grep`, which degrades to a
    /// plain structured search).
    Find(Box<FindArgs>),
    /// Search, then open the result in $EDITOR.
    ///
    /// Fronts both search pipelines, chosen by how many positionals you give:
    /// `scout edit "<question>"` runs `find`, `scout edit <pattern> "<intent>"`
    /// runs the reranked `grep`, and `scout edit -p <pattern>` runs a plain
    /// pattern search. One hit opens straight away; several are listed and
    /// numbered for a one-keystroke pick.
    Edit(Box<EditArgs>),
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
    /// Run any verbose command and return its output condensed.
    ///
    /// `check_output`'s general form (docs/wrap-watch.md §3): no verdict, no
    /// advice — a faithful summary, the lines worth quoting verbatim, and the
    /// full raw output spooled to `raw_path` so nothing is lost behind the
    /// summary. Output at or under `[wrap] passthrough_max_lines` comes back
    /// whole, unfiltered.
    Wrap {
        /// The command to run, e.g. "git log --stat -n 200".
        command: String,
        /// Optional question to steer the condensation.
        question: Option<String>,
        /// Working directory for the command (default: the project root).
        #[arg(long)]
        cwd: Option<String>,
        /// Wall-clock ceiling in seconds (default 900, max 3600).
        #[arg(long, value_name = "SECONDS", alias = "timeout-seconds")]
        timeout: Option<u64>,
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
    /// Prune the raw spool: the same sweep every spooling write already does.
    ///
    /// Blobs past `[spool] max_age_days` go first, then the oldest remaining
    /// until the spool is under `[spool] max_total_bytes`. Nothing here is
    /// required for hygiene — the spool prunes itself on write
    /// (docs/wrap-watch.md §2.3) — it is the manual handle for "reclaim it
    /// now", and `--all` for "reclaim all of it".
    Gc {
        /// Empty the spool outright, ignoring both retention bounds.
        #[arg(long)]
        all: bool,
    },
    /// Serve the local web view of the call log on 127.0.0.1:13001.
    ///
    /// Starts a detached daemon and prints the URL. Idempotent: if one is
    /// already up it prints that URL and exits 0, which is what makes it safe
    /// to call from a shell profile.
    Dashboard(dashboard::Args),
}

/// The flags every search verb shares: what to search (docs/search-cli.md §3) and how to
/// style it (§1–2).  Flattened into all three so the dialect can never drift
/// between them — docs/search-cli.md §5 requires `find`'s filter flags to be *identical* to
/// grep's, and one struct is the only way to keep that true.
///
/// `--format` is deliberately *not* here.  `grep` and `find` declare it
/// themselves, because `edit` must not have it: its output is a picker, and
/// "render this as JSON, then ask me which one to open" is not a thing.
#[derive(clap::Args)]
struct SearchFlags {
    /// Only search these file types (repeatable), e.g. -t rust -t toml.
    /// See --type-list for the full set.
    #[arg(short = 't', long = "type", value_name = "TYPE")]
    r#type: Vec<String>,
    /// Exclude these file types (repeatable), e.g. -T md.
    #[arg(short = 'T', long = "type-not", value_name = "TYPE")]
    type_not: Vec<String>,
    /// Include/exclude by glob (repeatable); a leading '!' excludes,
    /// e.g. -g 'src/**' -g '!**/tests/**'.
    #[arg(short = 'g', long = "glob", value_name = "GLOB")]
    glob: Vec<String>,
    /// Restrict the search to this directory (repeatable).
    /// Sugar for -g 'PATH/**'.
    #[arg(long, value_name = "PATH")]
    dir: Vec<String>,
    /// Skip this directory (repeatable). Sugar for -g '!PATH/**'.
    #[arg(long, value_name = "PATH")]
    exclude_dir: Vec<String>,
    /// Hits to return after filtering (default: `[cli] max_hits`, 20).
    /// A ceiling, not a quota — the model returns only what it kept.
    #[arg(short = 'n', long)]
    max_hits: Option<u64>,
    /// Context lines on each side of a match
    /// (default: `[cli] context`, else `[grep] context_lines`).
    #[arg(short = 'C', long)]
    context: Option<usize>,
    /// Cap each rendered line at N columns (default: `[cli] max_columns`, 150;
    /// 0 disables). An over-long matched line shows a window around the match.
    #[arg(short = 'M', long, value_name = "N")]
    max_columns: Option<usize>,
    /// When to colorize human output (default: `[cli] color`, `auto`).
    #[arg(long, value_enum)]
    color: Option<ColorWhen>,
    /// Project root to search (default: $PWD).
    #[arg(long)]
    project: Option<String>,
}

/// `scout grep`'s flags.  Boxed into its own `Args` struct rather than an
/// inline variant: the filter set (docs/search-cli.md §3) makes it far larger than any
/// sibling, and `run_grep` wants to pass it around as one value.
#[derive(clap::Args)]
pub struct GrepArgs {
    /// Search pattern (literal by default; see --regex).
    #[arg(required_unless_present = "type_list")]
    pattern: Option<String>,
    /// What you are actually looking for. Omit it to skip the LLM rerank
    /// entirely — an unfiltered structured search, capped at --max-hits.
    intent: Option<String>,
    /// Treat the pattern as a regex.
    #[arg(long)]
    regex: bool,
    /// Print every known file type with its globs, then exit.
    /// Wins over everything else, as in ripgrep.
    #[arg(long)]
    type_list: bool,
    /// Skip the LLM rerank entirely: pure structured search, capped at
    /// --max-hits. Works with no model configured.
    #[arg(long, conflicts_with = "intent")]
    no_filter: bool,
    /// Output format (default: human text, colored only on a terminal).
    #[arg(long, value_enum)]
    format: Option<Format>,
    #[command(flatten)]
    flags: SearchFlags,
}

/// `scout find`'s flags: a question, the shared filter/render set, and the
/// guess-again budget.
///
/// No `--no-filter` and no `--regex`, deliberately.  The rerank *is* the verb —
/// without it there is nothing but a pile of guessed patterns — and the model
/// decides per candidate whether its pattern is a regex, so a global flag would
/// be overriding a decision the caller never made.
#[derive(clap::Args)]
pub struct FindArgs {
    /// What you are looking for, in words:
    /// "where are the config file options parsed?".
    question: String,
    /// Search rounds before giving up (default: `[find] max_attempts`, 3).
    /// A round is retried when every pattern whiffed, or when the model judges
    /// the results off-target and proposes better patterns. 1 disables both.
    #[arg(long, value_name = "N")]
    attempts: Option<u64>,
    /// Output format (default: human text, colored only on a terminal).
    #[arg(long, value_enum)]
    format: Option<Format>,
    #[command(flatten)]
    flags: SearchFlags,
}

/// `scout edit`'s flags: the two search verbs' positionals, plus their
/// verb-specific extras, and no `--format` (docs/search-cli.md §6).
///
/// Both positionals are optional at the clap level and the arity rule lives in
/// `edit::dispatch`; see that function for why.
#[derive(clap::Args)]
pub struct EditArgs {
    /// A question for the find pipeline — or, when an intent follows it,
    /// a search pattern for grep.
    #[arg(value_name = "QUESTION|PATTERN")]
    query: Option<String>,
    /// What you are actually looking for. Its presence is what makes the first
    /// positional a pattern rather than a question.
    intent: Option<String>,
    /// Search this pattern with no rerank: a plain structured search, no model.
    #[arg(short = 'p', long = "pattern", value_name = "PATTERN")]
    pattern: Option<String>,
    /// Treat the pattern as a regex (grep pipeline only).
    #[arg(long)]
    regex: bool,
    /// Search rounds before giving up (find pipeline only;
    /// default: `[find] max_attempts`, 3).
    #[arg(long, value_name = "N")]
    attempts: Option<u64>,
    #[command(flatten)]
    flags: SearchFlags,
}

/// Handle `scout extract` — the payload keys are the `extract` pipeline's, not
/// the binary's, so building them lives here rather than in `main`.
pub fn run_extract(
    file: &str,
    question: &str,
    max_lines: Option<u64>,
    project: Option<String>,
) -> ! {
    run_filter(
        "extract",
        project,
        &json!({ "file": file, "question": question, "max_lines": max_lines }),
        None,
    )
}

/// Handle `scout check` — as `run_extract`, for the `check_output` pipeline.
pub fn run_check(
    command: &str,
    cwd: Option<&str>,
    timeout_seconds: Option<u64>,
    project: Option<String>,
) -> ! {
    run_filter(
        "check_output",
        project,
        &json!({ "command": command, "cwd": cwd, "timeout_seconds": timeout_seconds }),
        None,
    )
}

/// Handle `scout wrap` — the `wrap` pipeline, with the child's exit status
/// passed on as scout's own.
///
/// docs/wrap-watch.md §8 left that leaning yes and it is decided yes: a wrapped
/// command is the caller's command, and in a pipeline (`scout wrap "make -n" &&
/// ...`) its status is the one that means something. scout's own failures — a
/// bad argument, an unusable cwd — still exit 1, so the two are never confused:
/// a payload printed means the command ran, and the code beside it is the
/// command's.
///
/// It does not go through `run_filter` for exactly that reason; everything else
/// about the run is identical.
pub fn run_wrap(
    command: &str,
    question: Option<&str>,
    cwd: Option<&str>,
    timeout_seconds: Option<u64>,
    project: Option<String>,
) -> ! {
    let args = json!({
        "command": command,
        "question": question,
        "cwd": cwd,
        "timeout_seconds": timeout_seconds,
    });
    match run_pipeline("wrap", "wrap", resolve_project(project), &args, false) {
        Ok(payload) => {
            println!("{}", pretty_json(&payload));
            // A command scout killed, or one a signal killed, reported no
            // status of its own; 1 is the honest stand-in for "did not exit 0".
            let code = payload["exit_code"].as_i64().unwrap_or(1);
            std::process::exit(i32::try_from(code).unwrap_or(1));
        }
        Err(e) => {
            eprintln!("{}", e.text());
            std::process::exit(1);
        }
    }
}

/// Resolve `scout grep`'s flags against the `[cli]` / `[grep]` config tables
/// and hand off to `run_filter`.
///
/// The precedence is the usual one — explicit flag, then `[cli]`, then the
/// shared `[grep]` default — and it lives here rather than in `grep.rs` so
/// the MCP path never sees a terminal-only default.
pub fn run_grep(a: GrepArgs) -> ! {
    // `--type-list` is informational and wins over everything, ripgrep-style:
    // it answers "what can I pass to -t?", so it must work before the caller
    // has a pattern in mind.
    if a.type_list {
        for (name, globs) in source::type_definitions() {
            println!("{name}: {}", globs.join(", "));
        }
        std::process::exit(0);
    }
    // clap's `required_unless_present` guarantees this.
    let pattern = a.pattern.expect("clap requires a pattern unless --type-list");
    let (mut args, out, project) = resolve_flags(a.flags, a.format);
    args["pattern"] = json!(pattern);
    args["intent"] = json!(a.intent);
    args["regex"] = json!(a.regex);

    // `--no-filter` needs no argument of its own: an absent intent already
    // means "no rerank" (docs/search-cli.md §9), and clap's `conflicts_with` makes
    // `--no-filter` with an intent an error rather than a silent no-op — so
    // by the time we get here, `--no-filter` and "no intent" are the same
    // state and the pipeline sees exactly one code path.
    run_filter("grep", project, &args, Some(&out))
}

/// Resolve `scout find`'s flags and hand off to `run_filter`.
///
/// Everything about the output side is grep's — same renderer, same status
/// lines, same exit codes (docs/search-cli.md §5) — so the only find-specific work here
/// is the question and the attempt budget.
pub fn run_find(a: FindArgs) -> ! {
    let (mut args, out, project) = resolve_flags(a.flags, a.format);
    args["question"] = json!(a.question);
    args["attempts"] = json!(a.attempts);
    run_filter("find", project, &args, Some(&out))
}

/// Run whichever pipeline `scout edit`'s positionals selected, then hand the
/// result to the picker (docs/search-cli.md §6).
///
/// The two checks that can fail without searching anything — the arity rule and
/// `$EDITOR` — run first, deliberately: a rerank takes seconds, and finding out
/// afterwards that there was never an editor to open would waste all of them.
pub fn run_edit(a: EditArgs) -> ! {
    let bail = |msg: String| -> ! {
        eprintln!("scout edit: {msg}");
        std::process::exit(2);
    };
    let pipeline = match edit::dispatch(a.query, a.intent, a.pattern, a.regex, a.attempts) {
        Ok(p) => p,
        Err(msg) => bail(msg),
    };
    let editor = match edit::editor_words() {
        Ok(words) => words,
        Err(msg) => bail(msg),
    };

    // `--format` does not exist on this verb; the picker is human output.
    let (mut args, out, project) = resolve_flags(a.flags, None);
    let tool = match pipeline {
        edit::Pipeline::Find { question, attempts } => {
            args["question"] = json!(question);
            args["attempts"] = json!(attempts);
            "find"
        }
        edit::Pipeline::Grep { pattern, intent, regex } => {
            args["pattern"] = json!(pattern);
            args["intent"] = json!(intent);
            args["regex"] = json!(regex);
            "grep"
        }
    };

    // The editor inherits the project root as its cwd, so the payload's
    // project-relative paths resolve — and the paths the picker printed are the
    // ones the editor opens.
    let project = resolve_project(project);
    let payload = match run_pipeline(tool, "edit", project.clone(), &args, true) {
        Ok(payload) => payload,
        Err(e) => {
            eprintln!("{}", e.text());
            std::process::exit(2);
        }
    };
    let status = grep_status(&payload, &out);
    edit::run(&payload, &out.render_opts(), &status, &project, &editor)
}

/// Turn the shared flag set into the pipeline's argument object plus the
/// renderer's options, applying the usual precedence: explicit flag, then
/// `[cli]`, then the shared `[grep]` default.
///
/// It lives here rather than in `grep.rs`/`find.rs` so the MCP path never sees
/// a terminal-only default.
fn resolve_flags(
    f: SearchFlags,
    format: Option<Format>,
) -> (serde_json::Value, GrepOutput, Option<String>) {
    let (_, grep_cfg) = filter_config::load();
    let cli_cfg = filter_config::load_cli();

    let context_lines = f.context.or(cli_cfg.context).unwrap_or(grep_cfg.context_lines);
    let max_hits = f.max_hits.unwrap_or(cli_cfg.max_hits as u64);
    let color = f.color.unwrap_or_else(|| ColorWhen::from_config(&cli_cfg.color));

    let args = json!({
        "max_hits": max_hits,
        "context_lines": context_lines,
        "types": f.r#type,
        "types_not": f.type_not,
        "globs": collect_globs(&f.glob, &f.dir, &f.exclude_dir),
    });
    let out = GrepOutput {
        format: format.unwrap_or(Format::Human),
        color: color.enabled(),
        context_lines,
        max_columns: f.max_columns.unwrap_or(cli_cfg.max_columns),
        // The pipelines clamp; mirror it so "capped at top N" never claims a
        // cap the pipeline did not actually apply.
        max_hits: (max_hits as usize).clamp(1, 100),
        max_hits_scanned: grep_cfg.max_hits_scanned,
    };
    (args, out, f.project)
}

/// Fold `-g`, `--dir` and `--exclude-dir` into the single glob list the search
/// layer takes.  The directory flags are pure sugar (docs/search-cli.md §3): `--dir X`
/// is `-g 'X/**'`, `--exclude-dir X` is `-g '!X/**'`.  Explicit `-g` globs come
/// first so the ordering a user typed is preserved among themselves; `ignore`
/// resolves conflicts by last-match-wins, so a later `--exclude-dir` beats an
/// earlier include, which is the intuitive reading.
fn collect_globs(globs: &[String], dirs: &[String], exclude_dirs: &[String]) -> Vec<String> {
    let trimmed = |p: &String| p.trim_end_matches('/').to_string();
    globs
        .iter()
        .cloned()
        .chain(dirs.iter().map(|d| format!("{}/**", trimmed(d))))
        .chain(exclude_dirs.iter().map(|d| format!("!{}/**", trimmed(d))))
        .collect()
}

/// `--project`, or `$PWD`.
fn resolve_project(project: Option<String>) -> String {
    project.unwrap_or_else(|| {
        std::env::current_dir().map_or_else(|_| ".".to_string(), |p| p.display().to_string())
    })
}

/// Build the invocation context and run one filter, returning its payload.
///
/// The one place the CLI enters a pipeline.  Split out of `run_filter` so
/// `scout edit` can reach a payload without also inheriting `run_filter`'s
/// "print it and exit" ending — everything about *how* the pipeline runs stays
/// identical for every verb.
///
/// `verb` is what the user typed and what the call log records as the `tool`;
/// `pipeline` is which filter runs. They differ only for `scout edit`, which
/// fronts both search pipelines and is its own operation to a reader of the
/// log.
fn run_pipeline(
    pipeline: &str,
    verb: &str,
    project: String,
    args: &serde_json::Value,
    progress: bool,
) -> select::ToolResult {
    let (client, client_error) = match config::load_config(&config::config_path()) {
        Ok(c) => (Some(LlmClient::new(c)), None),
        Err(e) => (None, Some(e)),
    };
    let presets = presets::load_presets();
    let ctx = select::Ctx {
        client: client.as_ref(),
        client_error,
        presets: &presets,
        project,
        via: stats::VIA_CLI,
        tool: verb.to_string(),
        // Only the terminal paths want progress chatter, and only ever on
        // stderr — stdout carries the result and may be piped.
        progress: progress
            .then(|| Box::new(|msg: &str| eprintln!("{msg}")) as select::ProgressSink),
        ..Default::default()
    };

    let result = match pipeline {
        "grep" => grep::run(&ctx, args),
        "find" => find::run(&ctx, args),
        "extract" => extract::run(&ctx, args),
        "wrap" => wrap::run(&ctx, args),
        _ => check_output::run(&ctx, args),
    };
    // The payload's size is half the context-saved metric, and this is the
    // first point that has it (docs/dashboard.md §3).
    match &result {
        Ok(payload) => ctx.ledger.finish(payload),
        Err(e) => ctx.ledger.fail(&e.text()),
    }
    result
}

/// Handle the three filter verbs (`grep`, `extract`, `check`).
///
/// Thin wrapper over exactly the code path the MCP tools use: same
/// `Ctx`, same handlers, same payloads — only the argument parsing and the
/// rendering differ.  Config is loaded leniently, so the bypass paths (small
/// file, short hit list) still work with no `config.toml` at all; when the
/// model really is needed, the failure names both the reason and the raw tool
/// to fall back to.  Diverges via `std::process::exit`, like `run`/`task`.
fn run_filter(
    tool: &str,
    project: Option<String>,
    args: &serde_json::Value,
    grep_out: Option<&GrepOutput>,
) -> ! {
    let project = resolve_project(project);
    let result = run_pipeline(tool, tool, project, args, grep_out.is_some());

    match result {
        Ok(payload) => {
            if let Some(out) = grep_out {
                finish_grep(&payload, out)
            } else {
                println!("{}", pretty_json(&payload));
                std::process::exit(0);
            }
        }
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

/// Write `grep`'s payload out and exit with the grep convention (docs/search-cli.md §1):
/// 0 when at least one hit came back, 1 when none did.
///
/// Results go to stdout, metadata to stderr — always, in every format, so a
/// script never has to strip status chatter out of the thing it is parsing.
fn finish_grep(payload: &serde_json::Value, out: &GrepOutput) -> ! {
    match out.format {
        Format::Json => println!("{}", pretty_json(payload)),
        Format::Vimgrep => print!("{}", render::render_vimgrep(payload)),
        Format::Human => print!("{}", render::render_human(payload, &out.render_opts())),
    }
    for line in grep_status(payload, out) {
        eprintln!("{line}");
    }
    let returned = payload.get("hits").and_then(|h| h.as_array()).map_or(0, Vec::len);
    std::process::exit(i32::from(returned == 0));
}

/// The stderr status lines for one grep result (docs/search-cli.md §2).
///
/// Everything here is read back out of the payload rather than threaded
/// through the pipeline: the payload already carries every counter a human
/// wants, and keeping the derivation in one pure function makes it testable.
fn grep_status(payload: &serde_json::Value, out: &GrepOutput) -> Vec<String> {
    let num =
        |key: &str| payload.get(key).and_then(serde_json::Value::as_u64).unwrap_or(0) as usize;
    let flag = |key: &str| payload.get(key).and_then(serde_json::Value::as_bool).unwrap_or(false);

    let returned = payload.get("hits").and_then(|h| h.as_array()).map_or(0, Vec::len);
    let hits_total = num("hits_total");
    let pattern = payload.get("pattern").and_then(serde_json::Value::as_str).unwrap_or("");

    // `find` payloads carry the attempt count; grep's never do.  The two verbs
    // need different advice on an empty result — `find` has no `--no-filter`
    // (the rerank is the whole verb) and no pattern to re-run, so both of its
    // empty cases point at an explicit `scout grep` instead (docs/search-cli.md §5).
    let find_attempts = payload.get("find_attempts").and_then(serde_json::Value::as_u64);

    // Empty is the one case a human can misread, so it says which empty it is.
    if returned == 0 {
        return vec![match (find_attempts, flag("none_relevant")) {
            (Some(n), false) => format!(
                "no pattern guess produced hits after {n} attempt{} — \
                 try scout grep with an explicit pattern",
                if n == 1 { "" } else { "s" }
            ),
            (Some(_), true) => format!(
                "local model judged none of the {hits_total} hits relevant — \
                 try scout grep with an explicit pattern"
            ),
            (None, true) => format!(
                "local model judged none of the {hits_total} hits relevant — \
                 rerun with --no-filter to see all of them"
            ),
            (None, false) => format!("no matches for '{pattern}'"),
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
///
/// The call itself is `select::round_trip`, the same one the filters and
/// `scout run` make. It used to be a `task` module of its own, whose docstring
/// promised it would back "preset dispatch" in a later step that never came;
/// what it actually did was hold the oldest of three copies of this round-trip,
/// and the copy that never streamed.
pub fn run_task(prompt: &str) -> ! {
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

    // One record so start and end share an `id` with the log line.
    let rec = stats::CallRecord::new("task", "task")
        .via(stats::VIA_CLI)
        .project(&resolve_project(None))
        .endpoint(client.model(), client.endpoint())
        .input(stats::input_summary("task", &json!({ "prompt": prompt })))
        .raw_bytes(prompt.len() as u64);

    let (rec, result) = select::round_trip(&client, rec, system, prompt);
    match result {
        Ok(text) => {
            // Trimmed for both stdout and the byte count: the escape hatch
            // prints an answer, not the model's leading newline.
            let text = text.trim();
            rec.returned_bytes(text.len() as u64).log();
            println!("{text}");
            std::process::exit(0);
        }
        Err(e) => {
            rec.log();
            eprintln!("scout task: {e:?}");
            std::process::exit(1);
        }
    }
}

/// Handle `scout gc` — run the spool sweep and say what it freed.
///
/// Returns `Ok(())` even when nothing was deleted and when the spool does not
/// exist at all: garbage collection has no failure mode a caller could act on,
/// and a `[spool]` that will not parse is reported on stderr and then ignored,
/// exactly as the write path ignores it (docs/wrap-watch.md §2.3).
pub fn run_gc(all: bool) -> anyhow::Result<()> {
    let cfg = config::load_spool_config(&config::config_path()).unwrap_or_else(|e| {
        eprintln!("scout gc: {e}; using defaults");
        spool::SpoolConfig::default()
    });

    let base = spool::cache_dir();
    let swept = if all { spool::purge_in(&base) } else { spool::sweep_in(&base, &cfg) };

    println!(
        "scout gc: freed {} in {} file(s); {} left in {}",
        stats::human_bytes(swept.bytes_freed),
        swept.files_deleted,
        stats::human_bytes(swept.bytes_remaining),
        spool::raw_dir(&base).display()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    /// Parse an argv, returning grep's flags.
    fn grep_args(argv: &[&str]) -> GrepArgs {
        let mut full = vec!["scout", "grep"];
        full.extend_from_slice(argv);
        match Cli::try_parse_from(full).expect("should parse").command {
            Command::Grep(a) => *a,
            _ => panic!("not the grep subcommand"),
        }
    }

    fn grep_err(argv: &[&str]) -> clap::Error {
        let mut full = vec!["scout", "grep"];
        full.extend_from_slice(argv);
        match Cli::try_parse_from(full) {
            Err(e) => e,
            Ok(_) => panic!("argv {argv:?} should have been rejected"),
        }
    }

    #[test]
    fn the_cli_definition_is_internally_consistent() {
        // Catches duplicate shorts/longs and bad `conflicts_with` targets,
        // which clap only reports at runtime otherwise.
        Cli::command().debug_assert();
    }

    #[test]
    fn task_without_a_prompt_is_refused_at_parse_time() {
        // Successor to the `task` module's "missing required 'user' param"
        // test. That module took the prompt through a JSON params object and
        // had to check it was there; the prompt now arrives as a `&str`
        // straight from clap, which is where the same invariant is enforced.
        // (Its "missing 'system'" sibling has no successor by construction:
        // the system prompt is a literal in `run_task` and cannot go absent.)
        assert!(Cli::try_parse_from(["scout", "task"]).is_err());
        let cli = Cli::try_parse_from(["scout", "task", "why is the sky blue"]).expect("parses");
        assert!(matches!(cli.command, Command::Task { prompt } if prompt == "why is the sky blue"));
    }

    #[test]
    fn gc_defaults_to_the_bounded_sweep_and_takes_all_for_the_whole_spool() {
        let bare = Cli::try_parse_from(["scout", "gc"]).expect("parses");
        assert!(matches!(bare.command, Command::Gc { all: false }));
        let all = Cli::try_parse_from(["scout", "gc", "--all"]).expect("parses");
        assert!(matches!(all.command, Command::Gc { all: true }));
    }

    #[test]
    fn wrap_takes_a_command_an_optional_question_and_the_check_style_flags() {
        let cli = Cli::try_parse_from([
            "scout",
            "wrap",
            "git log --stat",
            "which commit changed the retry default?",
            "--cwd",
            "/tmp",
            "--timeout",
            "30",
        ])
        .expect("parses");
        match cli.command {
            Command::Wrap { command, question, cwd, timeout, project } => {
                assert_eq!(command, "git log --stat");
                assert_eq!(question.as_deref(), Some("which commit changed the retry default?"));
                assert_eq!(cwd.as_deref(), Some("/tmp"));
                assert_eq!(timeout, Some(30));
                assert_eq!(project, None);
            }
            _ => panic!("not the wrap subcommand"),
        }

        // The question is optional — a bare `scout wrap "<cmd>"` is the common
        // shape — and `scout check`'s spelling of the deadline still works.
        let bare =
            Cli::try_parse_from(["scout", "wrap", "docker logs api", "--timeout-seconds", "5"])
                .expect("parses");
        assert!(matches!(bare.command, Command::Wrap { question: None, timeout: Some(5), .. }));
        assert!(Cli::try_parse_from(["scout", "wrap"]).is_err(), "a command is required");
    }

    #[test]
    fn type_and_glob_flags_are_repeatable() {
        let a = grep_args(&["needle", "-t", "rust", "-t", "toml", "-T", "md", "-g", "src/**"]);
        assert_eq!(a.flags.r#type, vec!["rust", "toml"]);
        assert_eq!(a.flags.type_not, vec!["md"]);
        assert_eq!(a.flags.glob, vec!["src/**"]);
    }

    #[test]
    fn dir_flags_are_sugar_over_globs() {
        let a = grep_args(&["needle", "-g", "*.rs", "--dir", "src", "--exclude-dir", "vendor/"]);
        assert_eq!(
            collect_globs(&a.flags.glob, &a.flags.dir, &a.flags.exclude_dir),
            vec!["*.rs", "src/**", "!vendor/**"],
            "--dir includes, --exclude-dir negates, and a trailing slash is tolerated"
        );
    }

    #[test]
    fn collect_globs_of_nothing_is_empty() {
        // The no-op guarantee starts here: no glob flags means no override.
        assert!(collect_globs(&[], &[], &[]).is_empty());
    }

    #[test]
    fn type_list_does_not_require_a_pattern() {
        let a = grep_args(&["--type-list"]);
        assert!(a.type_list && a.pattern.is_none());
        // ...and still wins when a pattern happens to be present (rg parity).
        assert!(grep_args(&["needle", "--type-list"]).type_list);
    }

    #[test]
    fn a_missing_pattern_without_type_list_is_rejected() {
        assert_eq!(grep_err(&[]).kind(), clap::error::ErrorKind::MissingRequiredArgument);
    }

    #[test]
    fn no_filter_and_an_intent_are_mutually_exclusive() {
        // Silently ignoring the intent would be the worst outcome: the caller
        // would think the rerank ran.
        assert_eq!(
            grep_err(&["needle", "an intent", "--no-filter"]).kind(),
            clap::error::ErrorKind::ArgumentConflict
        );
        // Alone, it is just the no-intent path spelled out.
        let a = grep_args(&["needle", "--no-filter"]);
        assert!(a.no_filter && a.intent.is_none());
    }

    /// Parse an argv, returning find's flags.
    fn find_args(argv: &[&str]) -> FindArgs {
        let mut full = vec!["scout", "find"];
        full.extend_from_slice(argv);
        match Cli::try_parse_from(full).expect("should parse").command {
            Command::Find(a) => *a,
            _ => panic!("not the find subcommand"),
        }
    }

    #[test]
    fn find_takes_the_same_filter_flags_as_grep() {
        // docs/search-cli.md §5: "filter flags are identical to scout grep".  The shared
        // `SearchFlags` makes that structural, and this pins it.
        let a = find_args(&[
            "where is config parsed?",
            "-t",
            "rust",
            "-T",
            "md",
            "-g",
            "src/**",
            "--exclude-dir",
            "vendor",
            "-n",
            "5",
            "-C",
            "1",
            "-M",
            "80",
            "--format",
            "vimgrep",
        ]);
        assert!(matches!(a.format, Some(Format::Vimgrep)));
        assert_eq!(a.question, "where is config parsed?");
        assert_eq!(a.flags.r#type, vec!["rust"]);
        assert_eq!(a.flags.type_not, vec!["md"]);
        assert_eq!(
            collect_globs(&a.flags.glob, &a.flags.dir, &a.flags.exclude_dir),
            vec!["src/**", "!vendor/**"]
        );
        assert_eq!(a.flags.max_hits, Some(5));
        assert_eq!(a.flags.context, Some(1));
        assert_eq!(a.flags.max_columns, Some(80));
    }

    #[test]
    fn find_attempts_defaults_to_the_config_and_overrides_cleanly() {
        assert_eq!(find_args(&["a question"]).attempts, None, "unset means [find] max_attempts");
        assert_eq!(find_args(&["a question", "--attempts", "1"]).attempts, Some(1));
    }

    #[test]
    fn find_rejects_the_flags_that_would_contradict_it() {
        // `--no-filter` would remove the only stage that makes find work, and
        // `--regex` would override a per-candidate decision the caller never
        // made — neither exists on this verb.
        for flag in ["--no-filter", "--regex"] {
            match Cli::try_parse_from(["scout", "find", "a question", flag]) {
                Err(e) => {
                    assert_eq!(e.kind(), clap::error::ErrorKind::UnknownArgument, "flag: {flag}");
                }
                Ok(_) => panic!("find must not accept {flag}"),
            }
        }
    }

    #[test]
    fn find_requires_a_question() {
        match Cli::try_parse_from(["scout", "find"]) {
            Err(e) => assert_eq!(e.kind(), clap::error::ErrorKind::MissingRequiredArgument),
            Ok(_) => panic!("find must require a question"),
        }
    }

    #[test]
    fn a_whiffed_find_names_scout_grep_not_no_filter() {
        let out = test_output();
        let payload = json!({
            "mode": "full", "pattern": "quantum|flux", "intent": "a question",
            "hits_total": 0, "hits": [], "none_relevant": false,
            "find_attempts": 2, "find_patterns": ["quantum", "flux"],
        });
        let lines = grep_status(&payload, &out);
        assert_eq!(lines.len(), 1);
        assert_eq!(
            lines[0],
            "no pattern guess produced hits after 2 attempts — try scout grep with an explicit pattern"
        );
        // ...and one attempt is singular, because a status line that says
        // "1 attempts" reads as a bug in the tool.
        let single = json!({"hits": [], "find_attempts": 1, "none_relevant": false});
        assert!(
            grep_status(&single, &out)[0].contains("after 1 attempt —"),
            "{:?}",
            grep_status(&single, &out)
        );
    }

    #[test]
    fn a_none_relevant_find_also_points_at_scout_grep() {
        // find has no --no-filter to rerun with, so grep's advice would be a
        // dead end here.
        let payload = json!({
            "mode": "rerank", "pattern": "a|b", "intent": "a question",
            "hits_total": 31, "hits": [], "none_relevant": true, "find_attempts": 1,
        });
        let line = grep_status(&payload, &test_output()).remove(0);
        assert!(line.contains("none of the 31 hits relevant"), "{line}");
        assert!(line.contains("scout grep"), "{line}");
        assert!(!line.contains("--no-filter"), "{line}");
    }

    /// Parse an argv, returning edit's flags.
    fn edit_args(argv: &[&str]) -> EditArgs {
        let mut full = vec!["scout", "edit"];
        full.extend_from_slice(argv);
        match Cli::try_parse_from(full).expect("should parse").command {
            Command::Edit(a) => *a,
            _ => panic!("not the edit subcommand"),
        }
    }

    #[test]
    fn edit_takes_the_same_filter_flags_as_grep() {
        // The shared `SearchFlags` makes this structural; this pins it, and
        // pins that the arity positionals still land where they should.
        let a =
            edit_args(&["load_config", "the toml parse", "-t", "rust", "--dir", "src", "-n", "5"]);
        assert_eq!(a.query.as_deref(), Some("load_config"));
        assert_eq!(a.intent.as_deref(), Some("the toml parse"));
        assert_eq!(a.flags.r#type, vec!["rust"]);
        assert_eq!(a.flags.dir, vec!["src"]);
        assert_eq!(a.flags.max_hits, Some(5));
    }

    #[test]
    fn edit_has_no_format_flag() {
        // docs/search-cli.md §6: the output is a picker.  "Render this as JSON, then ask me
        // which one to open" is not a thing, so the flag must not exist —
        // and it must not silently parse as something else either.
        match Cli::try_parse_from(["scout", "edit", "a question", "--format", "json"]) {
            Err(e) => assert_eq!(e.kind(), clap::error::ErrorKind::UnknownArgument),
            Ok(_) => panic!("edit must not accept --format"),
        }
    }

    #[test]
    fn edit_positionals_are_optional_at_the_clap_level() {
        // The arity rule lives in `edit::dispatch`, which owns the error text;
        // clap must therefore let all three shapes through to it.
        assert!(edit_args(&[]).query.is_none(), "bare `scout edit` reaches dispatch");
        assert_eq!(edit_args(&["-p", "needle"]).pattern.as_deref(), Some("needle"));
        assert_eq!(edit_args(&["a question"]).query.as_deref(), Some("a question"));
    }

    fn test_output() -> GrepOutput {
        GrepOutput {
            format: Format::Human,
            color: false,
            context_lines: 2,
            max_columns: 150,
            max_hits: 20,
            max_hits_scanned: 2000,
        }
    }

    #[test]
    fn none_relevant_status_points_at_no_filter() {
        let out = GrepOutput {
            format: Format::Human,
            color: false,
            context_lines: 2,
            max_columns: 150,
            max_hits: 20,
            max_hits_scanned: 2000,
        };
        let payload = json!({
            "mode": "rerank", "pattern": "needle", "intent": "something",
            "hits_total": 214, "hits": [], "none_relevant": true,
        });
        let lines = grep_status(&payload, &out);
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("--no-filter"), "{}", lines[0]);
        assert!(lines[0].contains("214"), "{}", lines[0]);
    }
}
