// Call log: write path (used by every LLM-calling command) and the `scout
// stats` report (read path). Merges ct-local-llm's stats.rs (writer) with
// cmd/ct's local_stats.rs (reader) into one module.

use serde_json::json;
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::time::SystemTime;

/// Path to the call log.
///
/// Resolution order:
///   1. `$SCOUT_CALLS_LOG` env var (tests and custom layouts)
///   2. `$XDG_STATE_HOME/scout/calls.jsonl`
///   3. `~/.local/state/scout/calls.jsonl`
pub fn log_path() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("SCOUT_CALLS_LOG") {
        return Some(PathBuf::from(p));
    }
    let state_dir = std::env::var("XDG_STATE_HOME")
        .ok()
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var("HOME")
                .ok()
                .map(|h| PathBuf::from(h).join(".local").join("state"))
        })?;
    Some(state_dir.join("scout").join("calls.jsonl"))
}

/// Append a JSONL record to the call log. Silently ignored on any I/O error
/// (including a missing parent dir) so a full disk or unset $HOME never
/// breaks a scout command.
pub fn log_call(preset: &str, tokens_in: u64, tokens_out: u64, ms: u64, ok: bool) {
    if let Some(path) = log_path() {
        write_record(&path, preset, tokens_in, tokens_out, ms, ok);
    }
}

fn write_record(path: &std::path::Path, preset: &str, tokens_in: u64, tokens_out: u64, ms: u64, ok: bool) {
    let ts = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let line = json!({
        "ts": ts,
        "preset": preset,
        "tokens_in": tokens_in,
        "tokens_out": tokens_out,
        "ms": ms,
        "ok": ok,
    });
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = writeln!(f, "{line}");
    }
}

// ── `scout stats` report ────────────────────────────────────────────────────

struct PresetStats {
    calls: u64,
    ok: u64,
    tokens_in: u64,
    tokens_out: u64,
    // Only summed over successful calls so avg_ms(ok) isn't diluted by
    // error paths that log ms=0.
    ok_total_ms: u64,
}

struct Report {
    rows: Vec<(String, PresetStats)>,
    parse_errors: u64,
}

fn parse_log(path: &std::path::Path) -> std::io::Result<Report> {
    let f = std::fs::File::open(path)?;
    let mut by_preset: HashMap<String, PresetStats> = HashMap::new();
    let mut parse_errors: u64 = 0;

    for line in BufReader::new(f).lines().map_while(Result::ok) {
        if line.trim().is_empty() {
            continue;
        }
        let v: serde_json::Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => {
                parse_errors += 1;
                continue;
            }
        };
        let preset = v["preset"].as_str().unwrap_or("unknown").to_string();
        let entry = by_preset.entry(preset).or_insert(PresetStats {
            calls: 0,
            ok: 0,
            tokens_in: 0,
            tokens_out: 0,
            ok_total_ms: 0,
        });
        entry.calls += 1;
        let call_ok = v["ok"].as_bool().unwrap_or(false);
        if call_ok {
            entry.ok += 1;
            entry.ok_total_ms += v["ms"].as_u64().unwrap_or(0);
        }
        entry.tokens_in += v["tokens_in"].as_u64().unwrap_or(0);
        entry.tokens_out += v["tokens_out"].as_u64().unwrap_or(0);
    }

    let mut rows: Vec<(String, PresetStats)> = by_preset.into_iter().collect();
    rows.sort_by_key(|r| std::cmp::Reverse(r.1.calls));
    Ok(Report { rows, parse_errors })
}

/// Print the `scout stats` report: per-preset call counts, pass rate, token
/// totals, and average latency of successful calls, plus an overall total.
pub fn print_report() -> anyhow::Result<()> {
    let path = match log_path() {
        Some(p) => p,
        None => {
            println!("scout stats: $HOME not set and $SCOUT_CALLS_LOG not set; no log to read");
            return Ok(());
        }
    };

    if !path.exists() {
        println!("No calls recorded yet.");
        println!("Run `scout run --preset <name> ...` or `scout task \"...\"` to populate data.");
        return Ok(());
    }

    let report = parse_log(&path)?;

    if report.rows.is_empty() {
        println!("No calls recorded yet.");
        return Ok(());
    }

    println!(
        "{:<25} {:>6} {:>7} {:>10} {:>10} {:>11}",
        "preset", "calls", "pass%", "tok_in", "tok_out", "avg_ms(ok)"
    );
    println!("{}", "-".repeat(75));
    for (name, s) in &report.rows {
        let pass_pct = if s.calls > 0 {
            s.ok as f64 / s.calls as f64 * 100.0
        } else {
            0.0
        };
        let avg_ms = match s.ok_total_ms.checked_div(s.ok) {
            Some(v) => v.to_string(),
            None => "-".to_string(),
        };
        println!(
            "{:<25} {:>6} {:>6.1}% {:>10} {:>10} {:>11}",
            name, s.calls, pass_pct, s.tokens_in, s.tokens_out, avg_ms
        );
    }

    let total_calls: u64 = report.rows.iter().map(|(_, s)| s.calls).sum();
    let total_ok: u64 = report.rows.iter().map(|(_, s)| s.ok).sum();
    let overall_pass = if total_calls > 0 {
        total_ok as f64 / total_calls as f64 * 100.0
    } else {
        0.0
    };
    println!("{}", "-".repeat(75));
    println!(
        "{:<25} {:>6} {:>6.1}%  (overall)",
        "TOTAL", total_calls, overall_pass
    );

    if report.parse_errors > 0 {
        eprintln!("  ({} unreadable log lines skipped)", report.parse_errors);
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::NamedTempFile;

    // ── log_path / log_call ──────────────────────────────────────────────

    // Env vars are process-global and tests run in parallel: every test that
    // sets SCOUT_CALLS_LOG — or asserts on a log_path() derived without it —
    // must hold this lock.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn env_lock() -> std::sync::MutexGuard<'static, ()> {
        ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    #[test]
    fn log_path_contains_state_dir() {
        let _g = env_lock();
        if let Some(p) = log_path() {
            let s = p.to_string_lossy();
            assert!(s.contains("scout"), "expected 'scout' in path: {s}");
            assert!(s.ends_with("calls.jsonl"));
        }
    }

    #[test]
    fn log_path_honours_scout_calls_log_override() {
        let _g = env_lock();
        std::env::set_var("SCOUT_CALLS_LOG", "/tmp/scout-stats-test.jsonl");
        let p = log_path().unwrap();
        std::env::remove_var("SCOUT_CALLS_LOG");
        assert_eq!(p, PathBuf::from("/tmp/scout-stats-test.jsonl"));
    }

    #[test]
    fn write_record_produces_parseable_json_with_correct_fields() {
        let tmp = NamedTempFile::new().unwrap();
        write_record(tmp.path(), "quality_review", 123, 45, 678, true);
        write_record(tmp.path(), "shell_safety", 0, 0, 0, false);

        let lines: Vec<String> = BufReader::new(tmp.reopen().unwrap())
            .lines()
            .map_while(Result::ok)
            .collect();
        assert_eq!(lines.len(), 2, "expected 2 log lines");

        let v0: serde_json::Value = serde_json::from_str(&lines[0]).unwrap();
        assert_eq!(v0["preset"], "quality_review");
        assert_eq!(v0["tokens_in"], 123);
        assert_eq!(v0["tokens_out"], 45);
        assert_eq!(v0["ms"], 678);
        assert_eq!(v0["ok"], true);
        assert!(v0["ts"].as_u64().unwrap_or(0) > 0, "ts should be a unix timestamp");

        let v1: serde_json::Value = serde_json::from_str(&lines[1]).unwrap();
        assert_eq!(v1["preset"], "shell_safety");
        assert_eq!(v1["ok"], false);
    }

    #[test]
    fn log_call_with_unwritable_path_does_not_panic() {
        let _g = env_lock();
        // Ensure fail-open: an uncreatable log path must not panic. Pointing
        // at /dev/null/... also keeps the test from appending a synthetic row
        // to the developer's real calls.jsonl.
        std::env::set_var("SCOUT_CALLS_LOG", "/dev/null/calls.jsonl");
        log_call("test_preset", 100, 50, 200, true);
        std::env::remove_var("SCOUT_CALLS_LOG");
    }

    // ── parse_log / report aggregation ───────────────────────────────────

    fn write_log(lines: &[&str]) -> NamedTempFile {
        let mut f = NamedTempFile::new().unwrap();
        for line in lines {
            writeln!(f, "{line}").unwrap();
        }
        f
    }

    #[test]
    fn empty_file_produces_empty_report() {
        let f = write_log(&[]);
        let r = parse_log(f.path()).unwrap();
        assert!(r.rows.is_empty());
        assert_eq!(r.parse_errors, 0);
    }

    #[test]
    fn malformed_lines_counted_as_parse_errors() {
        let f = write_log(&[
            r#"{"preset":"quality_review","tokens_in":100,"tokens_out":50,"ms":200,"ok":true}"#,
            "not json at all",
            r#"{"preset":"quality_review","tokens_in":10,"tokens_out":5,"ms":100,"ok":false}"#,
        ]);
        let r = parse_log(f.path()).unwrap();
        assert_eq!(r.parse_errors, 1);
        assert_eq!(r.rows.len(), 1);
        assert_eq!(r.rows[0].1.calls, 2);
    }

    #[test]
    fn single_successful_call_aggregates_correctly() {
        let f = write_log(&[
            r#"{"preset":"quality_review","tokens_in":100,"tokens_out":50,"ms":200,"ok":true}"#,
        ]);
        let r = parse_log(f.path()).unwrap();
        assert_eq!(r.rows.len(), 1);
        let s = &r.rows[0].1;
        assert_eq!(s.calls, 1);
        assert_eq!(s.ok, 1);
        assert_eq!(s.tokens_in, 100);
        assert_eq!(s.tokens_out, 50);
        assert_eq!(s.ok_total_ms, 200);
    }

    #[test]
    fn failed_calls_do_not_count_toward_ok_ms() {
        // Failures log ms=0 — avg_ms(ok) should only cover successful calls.
        let f = write_log(&[
            r#"{"preset":"test_review","tokens_in":200,"tokens_out":80,"ms":500,"ok":true}"#,
            r#"{"preset":"test_review","tokens_in":0,"tokens_out":0,"ms":0,"ok":false}"#,
            r#"{"preset":"test_review","tokens_in":0,"tokens_out":0,"ms":0,"ok":false}"#,
        ]);
        let r = parse_log(f.path()).unwrap();
        let s = &r.rows[0].1;
        assert_eq!(s.calls, 3);
        assert_eq!(s.ok, 1);
        assert_eq!(s.ok_total_ms, 500); // only the 500ms success counts
    }

    #[test]
    fn multiple_presets_sorted_by_calls_descending() {
        let f = write_log(&[
            r#"{"preset":"a","tokens_in":1,"tokens_out":1,"ms":10,"ok":true}"#,
            r#"{"preset":"b","tokens_in":1,"tokens_out":1,"ms":10,"ok":true}"#,
            r#"{"preset":"b","tokens_in":1,"tokens_out":1,"ms":10,"ok":true}"#,
            r#"{"preset":"b","tokens_in":1,"tokens_out":1,"ms":10,"ok":true}"#,
        ]);
        let r = parse_log(f.path()).unwrap();
        assert_eq!(r.rows[0].0, "b"); // 3 calls first
        assert_eq!(r.rows[1].0, "a"); // 1 call second
    }
}
