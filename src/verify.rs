//! Command execution for `check_output`.
//!
//! Deliberately small: `run_command_capture`, `truncate_diagnostic` and
//! `detect_language`, and nothing else.  An earlier design had a
//! place→build→restore→retry repair loop here, driven by `write_tests` and
//! `refactor` presets.  Those presets were cut — a local model is not good at
//! composing net-new code, and the loop and its whole support cast went with
//! them.
//!
//! What is left is one job: run a command the caller named, capture what it
//! printed, and cap the size of that capture before it reaches the model.

use std::path::Path;
use std::process::Command;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

// ── Language detection ────────────────────────────────────────────────

/// Project language inferred from manifest files in the project root.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Language {
    Rust,
    Go,
    Python,
    Node,
}

impl Language {
    /// Lower-case name, as injected into the `check_output` preset args.
    pub fn as_str(self) -> &'static str {
        match self {
            Language::Rust => "rust",
            Language::Go => "go",
            Language::Python => "python",
            Language::Node => "node",
        }
    }
}

/// Detect the primary language of a project by inspecting manifest files.
///
/// Checks in order: Cargo.toml → go.mod → pyproject.toml / setup.py → package.json.
/// Returns `None` if no known manifest is found.
pub fn detect_language(dir: &Path) -> Option<Language> {
    if dir.join("Cargo.toml").exists() {
        Some(Language::Rust)
    } else if dir.join("go.mod").exists() {
        Some(Language::Go)
    } else if dir.join("pyproject.toml").exists() || dir.join("setup.py").exists() {
        Some(Language::Python)
    } else if dir.join("package.json").exists() {
        Some(Language::Node)
    } else {
        None
    }
}

// ── Command capture ───────────────────────────────────────────────────

/// Default output cap for captured build/test output.  16 KB keeps both the
/// first actionable error (near the top) and the summary line (at the bottom)
/// via head+tail elision.
pub const MAX_OUTPUT_BYTES: usize = 16 * 1024;

/// Run a shell command string via `sh -c`, wait up to `timeout`, and return
/// `(success, output)`.  Output is captured from both stdout and stderr and
/// capped at `max_output_bytes` via head+tail elision.  Always captures output
/// so the classifier receives stdout from passing builds (e.g. "test result:
/// ok. N passed") as well as failure output.
pub fn run_command_capture(
    cmd: &str,
    dir: &Path,
    timeout: Duration,
    max_output_bytes: usize,
) -> (bool, String) {
    let child = match Command::new("sh")
        .args(["-c", cmd])
        .current_dir(dir)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => return (false, format!("sh: failed to spawn: {e}")),
    };

    let pid = child.id();
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let _ = tx.send(child.wait_with_output());
    });

    match rx.recv_timeout(timeout) {
        Err(_elapsed) => {
            // Kill the child process — it is still running in the spawned thread.
            #[cfg(unix)]
            let _ = Command::new("kill").args(["-9", &pid.to_string()]).status();
            #[cfg(windows)]
            let _ = Command::new("taskkill").args(["/PID", &pid.to_string(), "/F"]).status();
            (false, format!("sh: command timed out after {}s", timeout.as_secs()))
        }
        Ok(Err(e)) => (false, format!("sh: wait failed: {e}")),
        Ok(Ok(output)) => {
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            let mut combined = format!("{stdout}\n{stderr}").trim().to_string();
            truncate_diagnostic(&mut combined, max_output_bytes);
            (output.status.success(), combined)
        }
    }
}

/// Keep the first 75% and last 25% of `limit` bytes, replacing the middle with
/// a `"\n...[N bytes elided]...\n"` marker.  No-op when `s` fits.
///
/// Compiler/test output puts its summary line (`test result: FAILED`, `aborting
/// due to N errors`) at the bottom — a head-only truncation drops it.
/// Head+tail preserves both the first actionable error and the outcome line.
///
/// Both split points are walked back to the nearest UTF-8 char boundary so the
/// result is always valid UTF-8 regardless of where the limit falls.
pub fn truncate_diagnostic(s: &mut String, limit: usize) {
    assert!(limit > 0, "truncate_diagnostic: limit must be > 0");
    if s.len() <= limit {
        return;
    }
    let head_limit = limit * 3 / 4;
    let tail_limit = limit - head_limit;

    // Head: walk back to char boundary.
    let head_end = (0..=head_limit).rev().find(|&i| s.is_char_boundary(i)).unwrap_or(0);

    // Tail: walk forward from `s.len() - tail_limit` to find a char boundary.
    let tail_start_raw = s.len() - tail_limit;
    let tail_start = (tail_start_raw..=s.len()).find(|&i| s.is_char_boundary(i)).unwrap_or(s.len());

    let elided = tail_start - head_end;
    let marker = format!("\n...[{elided} bytes elided]...\n");
    let tail = s[tail_start..].to_string();
    s.truncate(head_end);
    s.push_str(&marker);
    s.push_str(&tail);
}

// ── Tests ─────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn make_temp() -> TempDir {
        tempfile::tempdir().expect("tempdir")
    }

    // ── detect_language ───────────────────────────────────────────────

    #[test]
    fn detect_rust_from_cargo_toml() {
        let dir = make_temp();
        fs::write(dir.path().join("Cargo.toml"), "[package]\nname=\"x\"").unwrap();
        assert_eq!(detect_language(dir.path()), Some(Language::Rust));
    }

    #[test]
    fn detect_go_from_go_mod() {
        let dir = make_temp();
        fs::write(dir.path().join("go.mod"), "module example.com/x\ngo 1.21").unwrap();
        assert_eq!(detect_language(dir.path()), Some(Language::Go));
    }

    #[test]
    fn detect_python_from_pyproject_toml() {
        let dir = make_temp();
        fs::write(dir.path().join("pyproject.toml"), "[build-system]").unwrap();
        assert_eq!(detect_language(dir.path()), Some(Language::Python));
    }

    #[test]
    fn detect_python_from_setup_py() {
        let dir = make_temp();
        fs::write(dir.path().join("setup.py"), "from setuptools import setup").unwrap();
        assert_eq!(detect_language(dir.path()), Some(Language::Python));
    }

    #[test]
    fn detect_node_from_package_json() {
        let dir = make_temp();
        fs::write(dir.path().join("package.json"), "{}").unwrap();
        assert_eq!(detect_language(dir.path()), Some(Language::Node));
    }

    #[test]
    fn detect_none_when_no_manifest() {
        let dir = make_temp();
        assert_eq!(detect_language(dir.path()), None);
    }

    #[test]
    fn detect_rust_takes_precedence_over_go() {
        // Cargo.toml + go.mod in same dir → Rust wins (checked first)
        let dir = make_temp();
        fs::write(dir.path().join("Cargo.toml"), "").unwrap();
        fs::write(dir.path().join("go.mod"), "").unwrap();
        assert_eq!(detect_language(dir.path()), Some(Language::Rust));
    }

    #[test]
    fn language_names_are_stable() {
        assert_eq!(Language::Rust.as_str(), "rust");
        assert_eq!(Language::Node.as_str(), "node");
    }

    // ── truncate_diagnostic ───────────────────────────────────────────

    #[test]
    fn truncate_diagnostic_no_op_when_under_limit() {
        let mut s = "short".to_string();
        truncate_diagnostic(&mut s, 8192);
        assert_eq!(s, "short", "short string should be unchanged");
    }

    #[test]
    fn truncate_diagnostic_head_tail_elision() {
        // 20_000 'x' bytes with limit=8192: head=6144, tail=2048, elide middle.
        let limit = 8192usize;
        let head = limit * 3 / 4; // 6144
        let tail = limit - head; // 2048
        let mut s = "x".repeat(20_000);
        truncate_diagnostic(&mut s, limit);
        assert!(s.starts_with(&"x".repeat(head)), "head not preserved");
        assert!(s.ends_with(&"x".repeat(tail)), "tail not preserved");
        assert!(s.contains("bytes elided"), "elision marker missing");
        let x_count = s.chars().filter(|&c| c == 'x').count();
        assert_eq!(x_count, head + tail, "should keep exactly head+tail x bytes");
    }

    #[test]
    fn truncate_diagnostic_respects_utf8_char_boundary() {
        // "é" is 2 bytes (0xC3 0xA9).  limit=100: head=75, tail=25.  Place "é"
        // at byte offset 74 so it straddles the head split point.
        let limit = 100usize;
        let mut s = "a".repeat(74) + "é" + "b".repeat(200).as_str();
        truncate_diagnostic(&mut s, limit);
        assert!(std::str::from_utf8(s.as_bytes()).is_ok(), "result is not valid UTF-8");
        assert!(s.contains("bytes elided"), "elision marker missing");
    }

    #[test]
    fn truncate_diagnostic_no_op_at_exact_limit() {
        let mut s = "x".repeat(100);
        truncate_diagnostic(&mut s, 100);
        assert_eq!(s.len(), 100, "string at exact limit should not be modified");
        assert!(!s.contains("elided"), "no marker should be added");
    }

    #[test]
    fn truncate_diagnostic_utf8_boundary_in_tail() {
        // "é" is 2 bytes. Place it at the tail split point to verify we don't
        // panic and the result is valid UTF-8.
        let limit = 100usize;
        let mut s = "a".repeat(100) + &"b".repeat(23) + "é" + "c";
        assert!(s.len() > limit);
        truncate_diagnostic(&mut s, limit);
        assert!(std::str::from_utf8(s.as_bytes()).is_ok(), "result is not valid UTF-8");
        assert!(s.contains("bytes elided"), "elision marker missing");
    }

    #[test]
    fn truncate_diagnostic_empty_string_no_op() {
        let mut s = String::new();
        truncate_diagnostic(&mut s, 100);
        assert!(s.is_empty(), "empty string should remain empty");
    }

    // ── run_command_capture ───────────────────────────────────────────

    #[test]
    fn run_command_capture_success_captures_stdout() {
        let dir = make_temp();
        let (ok, output) = run_command_capture(
            "echo hello-from-capture",
            dir.path(),
            Duration::from_secs(5),
            MAX_OUTPUT_BYTES,
        );
        assert!(ok, "expected exit 0");
        assert!(output.contains("hello-from-capture"), "stdout not captured: {output}");
    }

    #[test]
    fn run_command_capture_failure_captures_output_and_returns_false() {
        let dir = make_temp();
        let (ok, output) = run_command_capture(
            "echo error-text && exit 1",
            dir.path(),
            Duration::from_secs(5),
            MAX_OUTPUT_BYTES,
        );
        assert!(!ok, "expected non-zero exit");
        assert!(output.contains("error-text"), "stderr/stdout not captured: {output}");
    }

    #[test]
    fn run_command_capture_captures_stderr_too() {
        let dir = make_temp();
        let (ok, output) = run_command_capture(
            "echo to-stderr 1>&2",
            dir.path(),
            Duration::from_secs(5),
            MAX_OUTPUT_BYTES,
        );
        assert!(ok);
        assert!(output.contains("to-stderr"), "stderr not captured: {output}");
    }

    #[test]
    fn run_command_capture_timeout_returns_false() {
        let dir = make_temp();
        let t0 = std::time::Instant::now();
        let (ok, output) =
            run_command_capture("sleep 30", dir.path(), Duration::from_millis(200), MAX_OUTPUT_BYTES);
        let elapsed = t0.elapsed();
        assert!(!ok, "expected timeout failure");
        assert!(output.contains("timed out"), "expected timeout message, got: {output}");
        assert!(elapsed < Duration::from_secs(2), "did not respect timeout: {elapsed:?}");
    }

    #[test]
    fn run_command_capture_handles_special_chars_in_command() {
        // Commands with quotes, equals and spaces pass through `sh -c` intact.
        let dir = make_temp();
        let (ok, output) = run_command_capture(
            "echo 'key=value with spaces'",
            dir.path(),
            Duration::from_secs(5),
            MAX_OUTPUT_BYTES,
        );
        assert!(ok, "command with special chars should succeed");
        assert!(output.contains("key=value with spaces"), "output: {output}");
    }

    #[test]
    fn run_command_capture_caps_a_flood_of_output() {
        let dir = make_temp();
        let (_, output) = run_command_capture(
            "for i in $(seq 1 5000); do echo 'noisy line of build output'; done",
            dir.path(),
            Duration::from_secs(20),
            2048,
        );
        assert!(output.len() < 4096, "output not capped: {} bytes", output.len());
        assert!(output.contains("bytes elided"), "elision marker missing");
    }

    #[test]
    fn run_command_capture_runs_in_the_given_directory() {
        let dir = make_temp();
        fs::write(dir.path().join("marker.txt"), "x").unwrap();
        let (ok, output) =
            run_command_capture("ls", dir.path(), Duration::from_secs(5), MAX_OUTPUT_BYTES);
        assert!(ok);
        assert!(output.contains("marker.txt"), "wrong cwd: {output}");
    }
}
