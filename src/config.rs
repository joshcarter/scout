// scout config loader.
//
// Single file, single format: `$XDG_CONFIG_HOME/scout/config.toml` (default
// `~/.config/scout/config.toml`), `[llm]` section. Clean break from ct — no
// fallback to `~/.claude/ct/config.toml`, no `$CT_LLM_CONFIG`. Override the
// whole file path with `$SCOUT_CONFIG`.
//
// This is the sole config parser in scout (ct-local-llm carried two, with
// different timeout clamping — collapsed here to one, keeping the saner
// 1s..3600s clamp).

use crate::client::Config;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Returns scout's config directory.
///
/// `$XDG_CONFIG_HOME/scout`, falling back to `$HOME/.config/scout`, falling
/// back to the relative `.config/scout` when `$HOME` is unset. Empty env
/// values count as unset, matching the shell `${VAR:-...}` expansion the
/// hooks use (`hooks/shell-safety.sh` resolves the same file) — the binary
/// and the hooks must always agree on this path.
pub fn config_dir() -> PathBuf {
    std::env::var("XDG_CONFIG_HOME")
        .ok()
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var("HOME")
                .ok()
                .filter(|v| !v.is_empty())
                .map(|h| PathBuf::from(h).join(".config"))
        })
        .unwrap_or_else(|| PathBuf::from(".config"))
        .join("scout")
}

/// Returns the config file path.
///
/// Resolution order:
///   1. `$SCOUT_CONFIG` env var (tests + non-standard installs)
///   2. `config_dir()/config.toml` (honors `$XDG_CONFIG_HOME`)
pub fn config_path() -> PathBuf {
    if let Ok(p) = std::env::var("SCOUT_CONFIG") {
        if !p.is_empty() {
            return PathBuf::from(p);
        }
    }
    config_dir().join("config.toml")
}

/// The default config, embedded at build time.
///
/// Embedded rather than read from the plugin payload because the two surfaces
/// that need it most cannot see a payload: `make install` puts the binary on
/// `$PATH` with no plugin anywhere, and under Grok Build the SessionStart hook
/// that used to seed this file never runs at all (SPEC-grok-plugin.md §2.5).
const DEFAULT_CONFIG: &str = include_str!("../config.example.toml");

/// Write the bundled default config to `path`, creating parent directories.
///
/// Never overwrites: an existing file — even an unparseable one — is left
/// alone, because clobbering a config someone hand-edited is far worse than
/// surfacing a parse error. Returns `Ok(false)` when a file was already there.
pub fn seed_default_config(path: &Path) -> Result<bool, String> {
    if path.exists() {
        return Ok(false);
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("cannot create config dir {:?}: {e}", parent))?;
    }
    // create_new: two scout processes starting at once (an MCP server and a
    // CLI call, say) must not race into a half-written file. Losing the race
    // is success — the other process wrote the same bytes.
    match std::fs::OpenOptions::new().write(true).create_new(true).open(path) {
        Ok(mut f) => {
            use std::io::Write;
            f.write_all(DEFAULT_CONFIG.as_bytes())
                .map_err(|e| format!("cannot write config {:?}: {e}", path))?;
            Ok(true)
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(false),
        Err(e) => Err(format!("cannot create config {:?}: {e}", path)),
    }
}

/// Parse a `Config` from the `[llm]` section of `path`.
///
/// Returns `Err` with a human-readable message if the file is missing,
/// unparseable, or the required `endpoint` / `model` keys are absent.
///
/// First run seeds the default config. The guard is deliberate: only the path
/// scout resolves for itself gets seeded, so a caller passing an explicit path
/// (tests, `$SCOUT_CONFIG` pointing somewhere odd) still gets a clean "cannot
/// read" error instead of a file it did not ask for. Seeding failures are
/// swallowed — the read below reports the problem in terms the user can act
/// on, and a read-only home directory should not change the error message.
pub fn load_config(path: &Path) -> Result<Config, String> {
    if !path.exists() && path == config_path() {
        let _ = seed_default_config(path);
    }

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

    // The process environment is global and cargo runs tests on parallel
    // threads, so every test that touches SCOUT_CONFIG / XDG_CONFIG_HOME
    // must hold this lock. Recover from poisoning: a failed test must not
    // cascade into the others.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn env_lock() -> std::sync::MutexGuard<'static, ()> {
        ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    #[test]
    fn config_path_uses_scout_config_env_var() {
        let _guard = env_lock();
        std::env::set_var("SCOUT_CONFIG", "/custom/path/config.toml");
        std::env::set_var("XDG_CONFIG_HOME", "/xdg-should-lose");
        let p = config_path();
        std::env::remove_var("SCOUT_CONFIG");
        std::env::remove_var("XDG_CONFIG_HOME");
        // SCOUT_CONFIG wins even over an explicit XDG_CONFIG_HOME.
        assert_eq!(p, PathBuf::from("/custom/path/config.toml"));
    }

    #[test]
    fn config_path_honors_xdg_config_home() {
        let _guard = env_lock();
        std::env::remove_var("SCOUT_CONFIG");
        std::env::set_var("XDG_CONFIG_HOME", "/xdg/config");
        let p = config_path();
        std::env::remove_var("XDG_CONFIG_HOME");
        assert_eq!(p, PathBuf::from("/xdg/config/scout/config.toml"));
    }

    #[test]
    fn config_path_empty_xdg_config_home_falls_back_to_home() {
        // ${XDG_CONFIG_HOME:-...} in the hooks treats empty as unset; the
        // binary must agree.
        let _guard = env_lock();
        let saved_home = std::env::var("HOME").ok();
        std::env::remove_var("SCOUT_CONFIG");
        std::env::set_var("XDG_CONFIG_HOME", "");
        std::env::set_var("HOME", "/home/tester");
        let p = config_path();
        std::env::remove_var("XDG_CONFIG_HOME");
        match saved_home {
            Some(h) => std::env::set_var("HOME", h),
            None => std::env::remove_var("HOME"),
        }
        assert_eq!(p, PathBuf::from("/home/tester/.config/scout/config.toml"));
    }

    #[test]
    fn seed_writes_the_embedded_default_and_it_parses() {
        let dir = TempDir::new().unwrap();
        let p = dir.path().join("nested").join("config.toml");
        assert!(seed_default_config(&p).unwrap(), "should report it wrote");
        assert!(p.exists());
        // The shipped default has to be a config scout can actually load,
        // otherwise first run seeds a file and then fails on it.
        load_config(&p).expect("embedded default must parse");
    }

    #[test]
    fn seed_never_overwrites() {
        let dir = TempDir::new().unwrap();
        let p = dir.path().join("config.toml");
        std::fs::write(&p, "hand edited, do not clobber").unwrap();
        assert!(!seed_default_config(&p).unwrap(), "should report it skipped");
        assert_eq!(
            std::fs::read_to_string(&p).unwrap(),
            "hand edited, do not clobber"
        );
    }

    #[test]
    fn load_config_seeds_the_resolved_path_on_first_run() {
        let _guard = env_lock();
        let dir = TempDir::new().unwrap();
        std::env::remove_var("SCOUT_CONFIG");
        std::env::set_var("XDG_CONFIG_HOME", dir.path());
        let resolved = config_path();
        assert!(!resolved.exists());
        let loaded = load_config(&resolved);
        std::env::remove_var("XDG_CONFIG_HOME");
        assert!(resolved.exists(), "first run should have seeded {resolved:?}");
        assert!(loaded.is_ok(), "and then loaded it: {loaded:?}");
    }

    #[test]
    fn load_config_does_not_seed_a_path_it_was_handed() {
        // Only the path scout resolves for itself gets created. An explicit
        // path that is missing stays missing and reports a read error.
        let _guard = env_lock();
        let dir = TempDir::new().unwrap();
        std::env::remove_var("SCOUT_CONFIG");
        std::env::remove_var("XDG_CONFIG_HOME");
        let handed = dir.path().join("somewhere-else.toml");
        let result = load_config(&handed);
        assert!(result.is_err());
        assert!(!handed.exists(), "must not have been created");
    }
}
