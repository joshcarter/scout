// scout config loader.
//
// Single file, single format: `~/.config/scout/config.toml`, `[llm]` section.
// Clean break from ct — no fallback to `~/.claude/ct/config.toml`, no
// `$CT_LLM_CONFIG`. Override the whole file path with `$SCOUT_CONFIG`.
//
// This is the sole config parser in scout (ct-local-llm carried two, with
// different timeout clamping — collapsed here to one, keeping the saner
// 1s..3600s clamp).

use crate::client::Config;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Returns the config file path.
///
/// Resolution order:
///   1. `$SCOUT_CONFIG` env var (tests + non-standard installs)
///   2. `~/.config/scout/config.toml`
///   3. Relative fallback `.config/scout/config.toml` (when `$HOME` is unset)
pub fn config_path() -> PathBuf {
    if let Ok(p) = std::env::var("SCOUT_CONFIG") {
        return PathBuf::from(p);
    }
    if let Ok(home) = std::env::var("HOME") {
        return PathBuf::from(home)
            .join(".config")
            .join("scout")
            .join("config.toml");
    }
    PathBuf::from(".config/scout/config.toml")
}

/// Parse a `Config` from the `[llm]` section of `path`.
///
/// Returns `Err` with a human-readable message if the file is missing,
/// unparseable, or the required `endpoint` / `model` keys are absent.
pub fn load_config(path: &Path) -> Result<Config, String> {
    let content = std::fs::read_to_string(path)
        .map_err(|e| format!("cannot read config {:?}: {e}", path))?;

    let root: toml::Value =
        toml::from_str(&content).map_err(|e| format!("config parse error: {e}"))?;

    let section = root
        .get("llm")
        .ok_or_else(|| format!("config: [llm] section not found in {:?}", path))?;

    let endpoint = section
        .get("endpoint")
        .and_then(|v| v.as_str())
        .ok_or("config: missing 'endpoint' in [llm]")?
        .trim_end_matches('/')
        .to_string();

    let model = section
        .get("model")
        .and_then(|v| v.as_str())
        .ok_or("config: missing 'model' in [llm]")?
        .to_string();

    let timeout_seconds = section
        .get("timeout_seconds")
        .and_then(|v| v.as_integer())
        // Clamp before cast: negative values wrap to near-maxint u64 via `as`.
        // Minimum 1 s so the Duration is never zero; cap at 3600 s (1 hour).
        .map(|v| v.clamp(1, 3600) as u64)
        .unwrap_or(120);

    let api_key = section
        .get("api_key")
        .and_then(|v| v.as_str())
        .map(String::from);

    let max_tokens = section
        .get("max_tokens")
        .and_then(|v| v.as_integer())
        .map(|v| v.max(0) as u64);

    Ok(Config {
        endpoint,
        model,
        timeout: Duration::from_secs(timeout_seconds),
        api_key,
        max_tokens,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn write_config(dir: &TempDir, content: &str) -> PathBuf {
        let p = dir.path().join("config.toml");
        fs::write(&p, content).unwrap();
        p
    }

    #[test]
    fn load_config_parses_required_fields() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_config(
            &dir,
            "[llm]\nendpoint = \"http://localhost:11434/v1\"\nmodel = \"qwen3:27b\"\n",
        );
        let cfg = load_config(&path).unwrap();
        assert_eq!(cfg.endpoint, "http://localhost:11434/v1");
        assert_eq!(cfg.model, "qwen3:27b");
        assert_eq!(cfg.timeout, Duration::from_secs(120)); // default
        assert!(cfg.api_key.is_none());
    }

    #[test]
    fn load_config_strips_trailing_slash() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_config(
            &dir,
            "[llm]\nendpoint = \"http://localhost:11434/v1/\"\nmodel = \"m\"\n",
        );
        let cfg = load_config(&path).unwrap();
        assert_eq!(cfg.endpoint, "http://localhost:11434/v1");
    }

    #[test]
    fn load_config_optional_fields() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_config(
            &dir,
            "[llm]\nendpoint = \"http://h/v1\"\nmodel = \"m\"\ntimeout_seconds = 60\napi_key = \"secret\"\nmax_tokens = 2048\n",
        );
        let cfg = load_config(&path).unwrap();
        assert_eq!(cfg.timeout, Duration::from_secs(60));
        assert_eq!(cfg.api_key.as_deref(), Some("secret"));
        assert_eq!(cfg.max_tokens, Some(2048));
    }

    #[test]
    fn load_config_missing_file_errors() {
        let result = load_config(Path::new("/nonexistent/path/config.toml"));
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("cannot read"));
    }

    #[test]
    fn load_config_missing_section_errors() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_config(&dir, "[other]\nkey = \"val\"\n");
        let result = load_config(&path);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("[llm]"));
    }

    #[test]
    fn load_config_missing_endpoint_errors() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_config(&dir, "[llm]\nmodel = \"m\"\n");
        let result = load_config(&path);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("endpoint"));
    }

    #[test]
    fn load_config_missing_model_errors() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_config(&dir, "[llm]\nendpoint = \"http://h/v1\"\n");
        let result = load_config(&path);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("model"));
    }

    #[test]
    fn load_config_timeout_zero_clamped_to_one() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_config(
            &dir,
            "[llm]\nendpoint = \"http://h/v1\"\nmodel = \"m\"\ntimeout_seconds = 0\n",
        );
        let cfg = load_config(&path).unwrap();
        assert!(cfg.timeout >= Duration::from_secs(1));
    }

    #[test]
    fn load_config_negative_timeout_clamped_not_wrapped() {
        // Regression: `v as u64` on a negative i64 wraps to near-maxint.
        // Clamp before cast: .max(1).min(3600) as u64.
        let dir = tempfile::tempdir().unwrap();
        let path = write_config(
            &dir,
            "[llm]\nendpoint = \"http://h/v1\"\nmodel = \"m\"\ntimeout_seconds = -5\n",
        );
        let cfg = load_config(&path).unwrap();
        assert_eq!(
            cfg.timeout,
            Duration::from_secs(1),
            "negative timeout must clamp to 1s, not wrap to near-maxint; got {:?}",
            cfg.timeout
        );
    }

    #[test]
    fn load_config_timeout_capped_at_one_hour() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_config(
            &dir,
            "[llm]\nendpoint = \"http://h/v1\"\nmodel = \"m\"\ntimeout_seconds = 99999\n",
        );
        let cfg = load_config(&path).unwrap();
        assert_eq!(cfg.timeout, Duration::from_secs(3600));
    }

    #[test]
    fn config_path_uses_scout_config_env_var() {
        // Temporarily set SCOUT_CONFIG; restore afterward.
        std::env::set_var("SCOUT_CONFIG", "/custom/path/config.toml");
        let p = config_path();
        std::env::remove_var("SCOUT_CONFIG");
        assert_eq!(p, PathBuf::from("/custom/path/config.toml"));
    }
}
