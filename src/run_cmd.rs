// `scout run` subcommand.
//
// One-shot preset invocation from the shell — no MCP loop, no stdin/stdout
// JSON-RPC protocol.  Designed for use from shell scripts (e.g. claude-review's
// lib-launch.sh):
//
//   scout run --ping
//   scout run --preset quality_review --arg git_diff_range=HEAD~1..HEAD --arg prompt_file=/path/to/quality.md
//
// Exit codes:
//   0 — success (--ping: endpoint reachable; --preset: LLM returned non-empty output)
//   1 — failure (endpoint unreachable, config missing, preset not found, empty output)
//
// Config is read from `$XDG_CONFIG_HOME/scout/config.toml`, default
// `~/.config/scout/config.toml` (see config.rs). Override path via
// `$SCOUT_CONFIG` for testing.

use crate::client::LlmClient;
use crate::config;
use crate::presets;
use serde_json::json;
use std::collections::HashMap;

// ── Argument parsing ──────────────────────────────────────────────────────────

/// Parsed flags for the `run` subcommand.
#[derive(Debug)]
pub(crate) struct RunArgs {
    /// `--preset NAME`
    pub preset: Option<String>,
    /// `--arg k=v` pairs (repeatable)
    pub named: HashMap<String, String>,
    /// `--ping` — check endpoint only, no preset call
    pub ping: bool,
    /// `--project PATH` — project root (default: `$PWD`)
    pub project: Option<String>,
}

/// Parse the argument list that follows the `run` subcommand token.
///
/// `args` must NOT include the `"run"` token itself.
pub(crate) fn parse_run_args(args: &[String]) -> Result<RunArgs, String> {
    let mut preset: Option<String> = None;
    let mut named: HashMap<String, String> = HashMap::new();
    let mut ping = false;
    let mut project: Option<String> = None;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--ping" => {
                ping = true;
            }
            "--preset" => {
                i += 1;
                let val = args
                    .get(i)
                    .ok_or("--preset requires a value")?
                    .clone();
                preset = Some(val);
            }
            "--arg" => {
                i += 1;
                let pair = args.get(i).ok_or("--arg requires a value")?;
                let (k, v) = pair
                    .split_once('=')
                    .ok_or_else(|| format!("--arg: expected k=v, got {:?}", pair))?;
                named.insert(k.to_string(), v.to_string());
            }
            "--project" => {
                i += 1;
                let val = args
                    .get(i)
                    .ok_or("--project requires a value")?
                    .clone();
                project = Some(val);
            }
            other => return Err(format!("unknown flag: {other}")),
        }
        i += 1;
    }

    Ok(RunArgs { preset, named, ping, project })
}

// ── Subcommand entry point ────────────────────────────────────────────────────

/// Handle `scout run [flags...]`.
///
/// This function calls `std::process::exit` — it does not return.  All output
/// goes to stdout (LLM response) or stderr (diagnostics).
pub fn run_subcommand(raw_args: &[String]) -> ! {
    // Skip the "run" token if it's still present at the front.
    let tail: &[String] = if raw_args.first().map(|s| s.as_str()) == Some("run") {
        &raw_args[1..]
    } else {
        raw_args
    };

    let args = match parse_run_args(tail) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("scout run: {e}");
            eprintln!("Usage: scout run [--ping] [--preset NAME] [--arg k=v ...] [--project PATH]");
            std::process::exit(1);
        }
    };

    // Load config (needed for both --ping and preset invocation).
    let cfg_path = config::config_path();
    let cfg = match config::load_config(&cfg_path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("scout run: {e}");
            std::process::exit(1);
        }
    };

    let client = LlmClient::new(cfg);

    // ── --ping path ──────────────────────────────────────────────────────────
    if args.ping {
        let (reachable, _ms) = client.check_endpoint();
        if reachable {
            std::process::exit(0);
        } else {
            eprintln!("scout run: endpoint unreachable");
            std::process::exit(1);
        }
    }

    // ── Preset invocation path ───────────────────────────────────────────────
    let preset_name = match &args.preset {
        Some(n) => n.clone(),
        None => {
            eprintln!("scout run: --preset NAME is required (or use --ping)");
            std::process::exit(1);
        }
    };

    // Load presets from the same source the MCP server uses: embedded
    // built-ins overlaid with any user overrides.
    let loaded = presets::load_presets();
    let preset = match loaded.iter().find(|p| p.name == preset_name) {
        Some(p) => p,
        None => {
            let available: Vec<&str> = loaded.iter().map(|p| p.name.as_str()).collect();
            eprintln!(
                "scout run: preset {:?} not found (available: {:?})",
                preset_name, available
            );
            std::process::exit(1);
        }
    };

    // Build a JSON Value of the named args for `presets::resolve`.
    let mut args_json = serde_json::Map::new();
    for (k, v) in &args.named {
        args_json.insert(k.clone(), json!(v));
    }
    let args_value = serde_json::Value::Object(args_json);

    // Project root defaults to $PWD.
    let project = args.project.unwrap_or_else(|| {
        std::env::current_dir()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| ".".to_string())
    });

    // A context provider that failed (no staged diff, a `git` timeout, an
    // unreadable prompt file) is a reason to stop, not a string to paste into
    // the prompt and review.  Exit before the call rather than after it.
    let (system, user) = match presets::resolve(preset, &args_value, &project) {
        Ok(pair) => pair,
        Err(e) => {
            eprintln!("scout run: {e}");
            std::process::exit(1);
        }
    };

    // The preset name is the operation here — `shell_safety` is what a human
    // reading the log is looking for, not "run".  `via` distinguishes a hook
    // from a shell: `run_cmd` cannot tell, so the caller says so through
    // `$SCOUT_VIA` (hooks/shell-safety.sh sets it).
    let prompt_bytes = (system.len() + user.len()) as u64;
    // One record for the whole invocation so `call.start` and the log line
    // share an `id` (docs/dashboard.md P3). The old closure reminted on every
    // arm and would have broken reconciliation.
    let rec = crate::stats::CallRecord::new(&preset_name, &preset_name)
        .via(&crate::stats::via_from_env(crate::stats::VIA_RUN))
        .project(&project)
        .endpoint(client.model(), client.endpoint())
        .input(crate::stats::input_summary(&preset_name, &args_value))
        .raw_bytes(prompt_bytes);

    // `select::round_trip` owns the call and its telemetry; what stays here is
    // what only a subcommand that never returns can decide — writing the row
    // outright (there is no ledger holding an operation open) and diverging.
    let (rec, result) = crate::select::round_trip(&client, rec, &system, &user);
    let text = match result {
        Ok(t) => t,
        Err(e) => {
            // Two failures, two messages: a reply of nothing is not a call that
            // did not complete, and telling the user it was sends them looking
            // at the endpoint instead of at the prompt.
            let empty = rec.outcome == crate::stats::Outcome::EmptyResponse;
            rec.log();
            if empty {
                eprintln!("scout run: LLM returned empty response");
            } else {
                eprintln!("scout run: LLM call failed: {:?}", e);
            }
            std::process::exit(1);
        }
    };

    rec.returned_bytes(text.len() as u64).log();

    print!("{text}");
    std::process::exit(0);
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── parse_run_args ────────────────────────────────────────────────────────

    #[test]
    fn parse_ping_flag() {
        let args: Vec<String> = vec!["--ping".into()];
        let parsed = parse_run_args(&args).unwrap();
        assert!(parsed.ping);
        assert!(parsed.preset.is_none());
    }

    #[test]
    fn parse_preset_and_args() {
        let args: Vec<String> = vec![
            "--preset".into(), "quality_review".into(),
            "--arg".into(), "git_diff_range=HEAD~1..HEAD".into(),
            "--arg".into(), "prompt_file=/tmp/quality.md".into(),
        ];
        let parsed = parse_run_args(&args).unwrap();
        assert_eq!(parsed.preset.as_deref(), Some("quality_review"));
        assert_eq!(parsed.named.get("git_diff_range").map(String::as_str), Some("HEAD~1..HEAD"));
        assert_eq!(parsed.named.get("prompt_file").map(String::as_str), Some("/tmp/quality.md"));
        assert!(!parsed.ping);
    }

    #[test]
    fn parse_project_flag() {
        let args: Vec<String> = vec!["--project".into(), "/tmp/myproject".into()];
        let parsed = parse_run_args(&args).unwrap();
        assert_eq!(parsed.project.as_deref(), Some("/tmp/myproject"));
    }

    #[test]
    fn parse_unknown_flag_errors() {
        let args: Vec<String> = vec!["--unknown".into()];
        assert!(parse_run_args(&args).is_err());
    }

    #[test]
    fn parse_preset_missing_value_errors() {
        let args: Vec<String> = vec!["--preset".into()];
        assert!(parse_run_args(&args).is_err());
    }

    #[test]
    fn parse_arg_missing_equals_errors() {
        let args: Vec<String> = vec!["--arg".into(), "noequals".into()];
        assert!(parse_run_args(&args).is_err());
    }

    #[test]
    fn parse_empty_args_gives_defaults() {
        let parsed = parse_run_args(&[]).unwrap();
        assert!(!parsed.ping);
        assert!(parsed.preset.is_none());
        assert!(parsed.named.is_empty());
        assert!(parsed.project.is_none());
    }
}
