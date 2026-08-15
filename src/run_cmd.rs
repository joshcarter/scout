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
pub(crate) fn run_subcommand(raw_args: &[String]) -> ! {
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
    let loaded = crate::load_presets();
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

    let (system, user) = presets::resolve(preset, &args_value, &project);

    // The preset name is the operation here — `shell_safety` is what a human
    // reading the log is looking for, not "run".  `via` distinguishes a hook
    // from a shell: `run_cmd` cannot tell, so the caller says so through
    // `$SCOUT_VIA` (hooks/shell-safety.sh sets it).
    let prompt_bytes = (system.len() + user.len()) as u64;
    // One record for the whole invocation so `call.start` and the log line
    // share an `id` (SPEC-dashboard P3). The old closure reminted on every
    // arm and would have broken reconciliation.
    let mut rec = crate::stats::CallRecord::new(&preset_name, &preset_name)
        .via(&crate::stats::via_from_env(crate::stats::VIA_RUN))
        .project(&project)
        .endpoint(client.model(), client.endpoint())
        .input(crate::stats::input_summary(&preset_name, &args_value))
        .raw_bytes(prompt_bytes);

    let messages = vec![
        json!({"role": "system", "content": system}),
        json!({"role": "user",   "content": user}),
    ];

    crate::live::emit_start(&rec, &system, &user);
    let start = std::time::Instant::now();
    // Streams `call.token` to the dashboard while the reply arrives (P5); a
    // no-op sink when nothing is listening.
    let result =
        crate::live::with_token_stream(&rec, |sink| client.complete_streaming(messages, None, sink));
    let (text, usage) = match result {
        Ok(r) => r,
        Err(e) => {
            rec = rec
                .ms(start.elapsed().as_millis() as u64)
                .outcome(e.outcome())
                .summary(e.to_string());
            crate::live::emit_end(&rec, None);
            rec.log();
            eprintln!("scout run: LLM call failed: {:?}", e);
            std::process::exit(1);
        }
    };
    let duration_ms = start.elapsed().as_millis() as u64;

    if text.trim().is_empty() {
        rec = rec
            .ms(duration_ms)
            .outcome(crate::stats::Outcome::EmptyResponse)
            .summary("the model returned nothing");
        crate::live::emit_end(&rec, None);
        rec.log();
        eprintln!("scout run: LLM returned empty response");
        std::process::exit(1);
    }

    rec = rec.usage(&usage).ms(duration_ms).returned_bytes(text.len() as u64);
    crate::live::emit_end(&rec, Some(&text));
    rec.log();

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
