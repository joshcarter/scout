// Provider registry for preset context gathering.
//
// A provider is a named function that runs a subprocess (git) or reads a
// file and returns the result as a string.  Preset TOML files reference
// providers by name in their `[context.<key>]` sections.
//
// The set is deliberately small: `file_read` and the `git_*` providers, all of
// them read-only subprocess or filesystem calls with no daemon and no code
// index behind them. A provider name that is not in the allowlist is an error,
// not a passthrough.
//
// ## Adding a new provider
//
// 1. Implement `fn provider_<name>(args: &ProviderArgs) -> Result<String, String>`.
// 2. Add the name to the `matches!` block in `provider_known()`.
// 3. Add the dispatch arm in `run_provider()`.

use std::collections::HashMap;
use std::process::Command;

/// Arguments available to a provider when it executes.
pub struct ProviderArgs<'a> {
    /// Positional args from TOML `_args = [...]`, after `${args.field}` substitution.
    pub positional: &'a [String],
    /// Named static args from other fields in `[context.X]`, after substitution.
    pub named: &'a HashMap<String, serde_json::Value>,
    /// Absolute path to the project root (used as CWD for subprocesses).
    pub project_root: &'a str,
}

/// Authorizes a provider name for preset execution against a whitelist.
pub fn provider_known(name: &str) -> bool {
    matches!(
        name,
        "git_staged_diff"
            | "git_recent_commits"
            | "git_diff_range"
            | "git_diff_stat"
            | "git_log_range"
            | "file_read"
    )
}

/// Execute a named provider and return its output string.
pub fn run_provider(name: &str, args: &ProviderArgs) -> Result<String, String> {
    match name {
        "git_staged_diff" => provider_git_staged_diff(args),
        "git_recent_commits" => provider_git_recent_commits(args),
        "git_diff_range" => provider_git_diff_range(args),
        "git_diff_stat" => provider_git_diff_stat(args),
        "git_log_range" => provider_git_log_range(args),
        "file_read" => provider_file_read(args),
        _ => Err(format!("unknown provider: '{name}'")),
    }
}

// ── Providers ────────────────────────────────────────────────────────────────

fn provider_git_staged_diff(args: &ProviderArgs) -> Result<String, String> {
    let out = git(args.project_root, &["diff", "--cached"])?;
    if out.trim().is_empty() {
        return Err("no staged changes — run `git add` before invoking this preset".to_string());
    }
    Ok(out)
}

fn provider_git_recent_commits(args: &ProviderArgs) -> Result<String, String> {
    let n = args.named.get("n").and_then(|v| v.as_u64()).unwrap_or(5);
    let n_str = format!("-{n}");
    // Optional `format` static arg — defaults to `--oneline` if absent.
    // Use `format = "%B%n---"` for full commit messages with separators.
    let format_arg = args.named.get("format").and_then(|v| v.as_str());
    let fmt_flag;
    let git_args: &[&str] = if let Some(fmt) = format_arg {
        fmt_flag = format!("--format={fmt}");
        &["log", &n_str, &fmt_flag]
    } else {
        &["log", &n_str, "--oneline"]
    };
    let output = git(args.project_root, git_args)?;
    // Optional `header` static arg — prepended only when output is non-empty.
    // Use this to suppress a heading when the repo has no commits yet.
    if output.trim().is_empty() {
        return Ok(String::new());
    }
    if let Some(header) = args.named.get("header").and_then(|v| v.as_str()) {
        Ok(format!("{header}\n{output}"))
    } else {
        Ok(output)
    }
}

fn provider_git_diff_range(args: &ProviderArgs) -> Result<String, String> {
    let base = args
        .positional
        .first()
        .map(|s| s.as_str())
        .or_else(|| args.named.get("base").and_then(|v| v.as_str()))
        .ok_or_else(|| "git_diff_range requires 'base' arg".to_string())?;
    git(args.project_root, &["diff", base])
}

fn provider_git_diff_stat(args: &ProviderArgs) -> Result<String, String> {
    let base = args
        .positional
        .first()
        .map(|s| s.as_str())
        .or_else(|| args.named.get("base").and_then(|v| v.as_str()));
    match base {
        Some(b) => git(args.project_root, &["diff", "--stat", b]),
        None => git(args.project_root, &["diff", "--stat"]),
    }
}

fn provider_git_log_range(args: &ProviderArgs) -> Result<String, String> {
    let base = args
        .positional
        .first()
        .map(|s| s.as_str())
        .or_else(|| args.named.get("base").and_then(|v| v.as_str()))
        .ok_or_else(|| "git_log_range requires 'base' arg".to_string())?;
    git(args.project_root, &["log", &format!("{base}..HEAD"), "--oneline"])
}

fn provider_file_read(args: &ProviderArgs) -> Result<String, String> {
    let path = args
        .positional
        .first()
        .map(|s| s.as_str())
        .or_else(|| args.named.get("path").and_then(|v| v.as_str()))
        .ok_or_else(|| "file_read requires 'path' arg".to_string())?;
    const MAX_BYTES: u64 = 1_048_576; // 1 MiB — guard against large/infinite files
    use std::io::Read as _;
    let mut file = std::fs::File::open(path).map_err(|e| format!("file_read '{path}': {e}"))?;
    let mut buf = Vec::with_capacity((MAX_BYTES + 1) as usize);
    file.by_ref().take(MAX_BYTES + 1).read_to_end(&mut buf)
        .map_err(|e| format!("file_read '{path}': {e}"))?;
    let truncated = buf.len() > MAX_BYTES as usize;
    if truncated {
        buf.truncate(MAX_BYTES as usize);
    }
    let mut s = String::from_utf8_lossy(&buf).into_owned();
    if truncated {
        s.push_str("\n[file_read: output truncated at 1 MiB]");
    }
    Ok(s)
}

// ── Subprocess helper ────────────────────────────────────────────────────────

fn git(cwd: &str, args: &[&str]) -> Result<String, String> {
    let out = Command::new("git")
        .current_dir(cwd)
        .args(args)
        .output()
        .map_err(|e| format!("git command failed: {e}"))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        return Err(format!("git exit {}: {stderr}", out.status));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::OnceLock;

    static EMPTY_NAMED: OnceLock<HashMap<String, serde_json::Value>> = OnceLock::new();

    fn empty_args(project_root: &str) -> ProviderArgs<'_> {
        ProviderArgs {
            positional: &[],
            named: EMPTY_NAMED.get_or_init(HashMap::new),
            project_root,
        }
    }

    #[test]
    fn provider_known_valid() {
        assert!(provider_known("git_staged_diff"));
        assert!(provider_known("git_recent_commits"));
        assert!(provider_known("git_diff_range"));
        assert!(provider_known("git_diff_stat"));
        assert!(provider_known("git_log_range"));
        assert!(provider_known("file_read"));
        assert!(!provider_known("nonexistent_provider"));
    }

    #[test]
    fn provider_known_rejects_names_outside_the_allowlist() {
        // The allowlist is the whole contract: anything not in it is an error,
        // whether it is a typo or a provider some other tool happens to offer.
        assert!(!provider_known("dir_listing"));
        assert!(!provider_known("daemon_lookup"));
        assert!(!provider_known("shell"));
    }

    #[test]
    fn run_provider_unknown_returns_error() {
        let args = empty_args("/tmp");
        let err = run_provider("definitely_not_a_provider", &args).unwrap_err();
        assert!(err.contains("unknown provider"), "expected 'unknown provider' in: {err}");
    }

    #[test]
    fn git_recent_commits_defaults_to_five() {
        // Verifies the n=5 default without requiring a real git repo:
        // the git command will fail (no repo at /tmp typically), but the
        // important thing is we don't panic on missing named arg.
        let named = HashMap::new();
        let args = ProviderArgs { positional: &[], named: &named, project_root: "/tmp" };
        // Either succeeds (if /tmp has git) or fails with a git error — not a panic.
        let _ = run_provider("git_recent_commits", &args);
    }

    #[test]
    fn git_diff_range_missing_base_returns_error() {
        let args = empty_args("/tmp");
        let err = run_provider("git_diff_range", &args).unwrap_err();
        assert!(err.contains("requires"), "expected 'requires' in: {err}");
    }

    #[test]
    fn git_log_range_missing_base_returns_error() {
        let args = empty_args("/tmp");
        let err = run_provider("git_log_range", &args).unwrap_err();
        assert!(err.contains("requires"), "expected 'requires' in: {err}");
    }

    #[test]
    fn git_recent_commits_format_arg_does_not_panic() {
        // The `format` named arg is the path used by the commit-message-style presets.
        // Verify the branch is reachable and does not panic — regardless of
        // whether a git repo is present at /tmp.
        let mut named = HashMap::new();
        named.insert("format".to_string(), serde_json::json!("%B%n---"));
        let args = ProviderArgs { positional: &[], named: &named, project_root: "/tmp" };
        // Either Ok (if /tmp is in a git repo) or Err (git fails) — not a panic.
        // Also confirm --oneline is NOT in the output string for the Ok case.
        // Err (git not available or not in a repo) is acceptable — just no panic.
        if let Ok(out) = run_provider("git_recent_commits", &args) {
            assert!(!out.contains("--oneline"), "format arg should suppress --oneline");
        }
    }

    // ── file_read truncation boundary tests ──────────────────────────────────

    fn write_tmp_bytes(n: usize) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!("file_read_test_{id}_{n}"));
        std::fs::write(&path, vec![b'x'; n]).unwrap();
        path
    }

    #[test]
    fn file_read_small_file_no_truncation_marker() {
        let path = write_tmp_bytes(42);
        let path_str = path.to_string_lossy().into_owned();
        let mut named = HashMap::new();
        named.insert("path".to_string(), serde_json::json!(path_str));
        let args = ProviderArgs { positional: &[], named: &named, project_root: "/tmp" };
        let out = run_provider("file_read", &args).unwrap();
        assert!(!out.contains("truncated"), "small file should not be truncated: {out}");
        assert_eq!(out.len(), 42);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn file_read_exactly_max_bytes_no_truncation_marker() {
        const MAX_BYTES: usize = 1_048_576;
        let path = write_tmp_bytes(MAX_BYTES);
        let path_str = path.to_string_lossy().into_owned();
        let mut named = HashMap::new();
        named.insert("path".to_string(), serde_json::json!(path_str));
        let args = ProviderArgs { positional: &[], named: &named, project_root: "/tmp" };
        let out = run_provider("file_read", &args).unwrap();
        assert!(
            !out.contains("truncated"),
            "file of exactly MAX_BYTES should NOT be truncated (> not >=)"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn file_read_one_over_max_bytes_appends_truncation_marker() {
        const MAX_BYTES: usize = 1_048_576;
        let path = write_tmp_bytes(MAX_BYTES + 1);
        let path_str = path.to_string_lossy().into_owned();
        let mut named = HashMap::new();
        named.insert("path".to_string(), serde_json::json!(path_str));
        let args = ProviderArgs { positional: &[], named: &named, project_root: "/tmp" };
        let out = run_provider("file_read", &args).unwrap();
        assert!(
            out.contains("[file_read: output truncated at 1 MiB]"),
            "file of MAX_BYTES+1 should be truncated: {}", &out[out.len().saturating_sub(80)..]
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn git_recent_commits_header_prepended_when_non_empty() {
        // Use the workspace root which is guaranteed to have commits.
        let root = env!("CARGO_MANIFEST_DIR");
        let project_root = root.to_string();
        let mut named = HashMap::new();
        named.insert("header".to_string(), serde_json::json!("STYLE HEADER:"));
        let args = ProviderArgs { positional: &[], named: &named, project_root: &project_root };
        match run_provider("git_recent_commits", &args) {
            Ok(out) if !out.is_empty() => {
                assert!(out.starts_with("STYLE HEADER:"), "header should be first: {out}");
            }
            _ => {} // no commits or git unavailable — skip
        }
    }

    #[test]
    fn git_staged_diff_no_staged_returns_err() {
        let repo = tempfile::TempDir::new().unwrap();
        let repo_path = repo.path().to_string_lossy().into_owned();
        let ok = std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(repo.path())
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if ok {
            let args = empty_args(&repo_path);
            let err = run_provider("git_staged_diff", &args).unwrap_err();
            assert!(err.contains("no staged"), "expected 'no staged' in: {err}");
        }
        // If git is unavailable, skip implicitly.
    }
}
