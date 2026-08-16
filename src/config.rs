// scout config loader.
//
// Single file, single format: `$XDG_CONFIG_HOME/scout/config.toml` (default
// `~/.config/scout/config.toml`). Override the whole file path with
// `$SCOUT_CONFIG`.
//
// Every section goes through `read_toml` here. Absent is `None`; unreadable
// or unparseable is `Err`. Section loaders then apply one rule: a missing
// section is the defaults, a key that is present and unusable is an error.
// `[llm]` is the exception — it has required keys, so a missing section is
// also an error.
//
// `[llm]`, `[spool]`, `[wrap]`, `[check_output]` load in this file.
// `[extract]`, `[grep]`, `[cli]`, `[find]`, `[dashboard]` load in
// `filter_config.rs`, through the same reader and the same integer helpers.

use crate::check_output::CheckOutputConfig;
use crate::client::Config;
use crate::spool::SpoolConfig;
use crate::wrap::WrapConfig;
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
/// that used to seed this file never runs at all (docs/plugin-packaging.md §2.5).
const DEFAULT_CONFIG: &str = include_str!("../config.example.toml");

/// Create `dir` with mode `0700` if it does not already exist. Only `dir`
/// itself is tightened — missing ancestors (`~/.config`, say) are created at
/// the process umask, since they are shared system directories scout has no
/// business narrowing. Pre-existing directories are left alone: a mode the
/// user set on purpose is not ours to override after the fact.
#[cfg(unix)]
fn ensure_private_dir(dir: &Path) -> std::io::Result<()> {
    if dir.exists() {
        return Ok(());
    }
    if let Some(parent) = dir.parent() {
        std::fs::create_dir_all(parent)?;
    }
    use std::os::unix::fs::DirBuilderExt;
    match std::fs::DirBuilder::new().mode(0o700).create(dir) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        Err(e) => Err(e),
    }
}

#[cfg(not(unix))]
fn ensure_private_dir(dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)
}

/// Write the bundled default config to `path`, creating parent directories.
///
/// Never overwrites: an existing file — even an unparseable one — is left
/// alone, because clobbering a config someone hand-edited is far worse than
/// surfacing a parse error. Returns `Ok(false)` when a file was already there.
///
/// The file is created `0600`: `[llm] api_key` can live in here, so a config
/// seeded at the process umask (typically `0644`) would leave a secret
/// world-readable from the moment it exists. Only creation sets the mode —
/// an already-existing config, however it got its permissions, is untouched.
pub fn seed_default_config(path: &Path) -> Result<bool, String> {
    if path.exists() {
        return Ok(false);
    }
    if let Some(parent) = path.parent() {
        ensure_private_dir(parent)
            .map_err(|e| format!("cannot create config dir {}: {e}", parent.display()))?;
    }
    // create_new: two scout processes starting at once (an MCP server and a
    // CLI call, say) must not race into a half-written file. Losing the race
    // is success — the other process wrote the same bytes.
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    match opts.open(path) {
        Ok(mut f) => {
            use std::io::Write;
            f.write_all(DEFAULT_CONFIG.as_bytes())
                .map_err(|e| format!("cannot write config {}: {e}", path.display()))?;
            Ok(true)
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(false),
        Err(e) => Err(format!("cannot create config {}: {e}", path.display())),
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
        .map_err(|e| format!("cannot read config {}: {e}", path.display()))?;

    let root: toml::Value =
        toml::from_str(&content).map_err(|e| format!("config parse error: {e}"))?;

    let section = root
        .get("llm")
        .ok_or_else(|| format!("config: [llm] section not found in {}", path.display()))?;

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
        .and_then(toml::Value::as_integer)
        // Clamp before cast: negative values wrap to near-maxint u64 via `as`.
        // Minimum 1 s so the Duration is never zero; cap at 3600 s (1 hour).
        .map_or(120, |v| v.clamp(1, 3600) as u64);

    // The two progress budgets. Same clamp, same cast discipline, same
    // reason: `v as u64` on a negative i64 wraps to near-maxint, which turns a
    // typo into a deadline that never fires.
    //
    // They are separate from `timeout_seconds` because they measure different
    // things. A local model's cold load — several GB of weights off disk before
    // a single byte comes back — is not a stall, so the first-token budget is
    // generous; a stream that has been producing and then goes quiet is a
    // stall regardless of how recently the call started, so the idle budget is
    // tight. Streaming only: `stream = false` has no progress signal to watch.
    let first_token_timeout_seconds = section
        .get("first_token_timeout_seconds")
        .and_then(toml::Value::as_integer)
        .map_or(60, |v| v.clamp(1, 3600) as u64);

    let idle_timeout_seconds = section
        .get("idle_timeout_seconds")
        .and_then(toml::Value::as_integer)
        .map_or(15, |v| v.clamp(1, 3600) as u64);

    let api_key = section.get("api_key").and_then(|v| v.as_str()).map(String::from);

    let max_tokens =
        section.get("max_tokens").and_then(toml::Value::as_integer).map(|v| v.max(0) as u64);

    // Defaults on, and an unusable value keeps the default rather than
    // erroring — this is a diagnostic knob, not a load-bearing one, and no
    // caller's result changes with it (docs/dashboard.md §6).
    let stream = section.get("stream").and_then(toml::Value::as_bool).unwrap_or(true);

    Ok(Config {
        endpoint,
        model,
        timeout: Duration::from_secs(timeout_seconds),
        first_token_timeout: Duration::from_secs(first_token_timeout_seconds),
        idle_timeout: Duration::from_secs(idle_timeout_seconds),
        api_key,
        max_tokens,
        stream,
    })
}

/// Parse a `SpoolConfig` from the `[spool]` section of `path`.
///
/// Strict where it can be and permissive where the alternative would be
/// useless: both keys have defaults, so a missing file and a missing section
/// are simply "the defaults" — `scout gc` has to work before anyone has
/// written a config, and the spool is not an LLM feature. What *is* an error
/// is a key that is present and unusable: a wrong-typed or negative bound is a
/// typo, and silently substituting a default for it is how a user ends up
/// believing they raised a retention window they did not
/// (docs/wrap-watch.md §2.3). Callers on the write path swallow the error and
/// take the defaults anyway — a bad `[spool]` must never cost a command its
/// result — but `scout gc` prints it, which is where a human will see it.
pub fn load_spool_config(path: &Path) -> Result<SpoolConfig, String> {
    let Some(root) = read_toml(path)? else {
        return Ok(SpoolConfig::default());
    };
    let Some(section) = root.get("spool") else {
        return Ok(SpoolConfig::default());
    };

    let defaults = SpoolConfig::default();
    Ok(SpoolConfig {
        max_age_days: bound(section, "spool", "max_age_days", defaults.max_age_days)?,
        max_total_bytes: bound(section, "spool", "max_total_bytes", defaults.max_total_bytes)?,
    })
}

/// Parse a `WrapConfig` from the `[wrap]` section of `path`.
///
/// Strict, on the `[spool]` rule and for the `[spool]` reasons: every key has a
/// default, so an absent file or section is simply the defaults, while a key
/// that is present and unusable is a typo worth reporting rather than a
/// silently-restored default. It rides this parser rather than growing a
/// third reader: a new section must not pick a private `read_to_string`
/// by accident.
///
/// `wrap::run` swallows the error and takes the defaults anyway: a mistyped
/// bound must never cost the caller the command's result (§3.5).
pub fn load_wrap_config(path: &Path) -> Result<WrapConfig, String> {
    let Some(root) = read_toml(path)? else {
        return Ok(WrapConfig::default());
    };
    let Some(section) = root.get("wrap") else {
        return Ok(WrapConfig::default());
    };

    let defaults = WrapConfig::default();
    Ok(WrapConfig {
        passthrough_max_lines: bound(
            section,
            "wrap",
            "passthrough_max_lines",
            defaults.passthrough_max_lines,
        )?,
        passthrough_max_bytes: bound(
            section,
            "wrap",
            "passthrough_max_bytes",
            defaults.passthrough_max_bytes,
        )?,
        model_input_bytes: bound(section, "wrap", "model_input_bytes", defaults.model_input_bytes)?,
    })
}

/// Parse a `CheckOutputConfig` from the `[check_output]` section of `path`.
///
/// Same rule as `[wrap]`: an absent file or section is the defaults; a key
/// that is present and unusable is an error. `check_output::run` swallows
/// the error and takes the defaults, so a typo never costs the caller the
/// command. Zero is rejected — a zero idle deadline would kill a healthy
/// build on the first poll, and a zero wall clock is not a timeout.
pub fn load_check_output_config(path: &Path) -> Result<CheckOutputConfig, String> {
    let Some(root) = read_toml(path)? else {
        return Ok(CheckOutputConfig::default());
    };
    let Some(section) = root.get("check_output") else {
        return Ok(CheckOutputConfig::default());
    };

    let defaults = CheckOutputConfig::default();
    Ok(CheckOutputConfig {
        idle_timeout_seconds: positive(
            section,
            "check_output",
            "idle_timeout_seconds",
            defaults.idle_timeout_seconds,
        )?,
        default_timeout_seconds: positive(
            section,
            "check_output",
            "default_timeout_seconds",
            defaults.default_timeout_seconds,
        )?,
    })
}

/// The parsed config file, or `None` when there is no file at all.
///
/// Absent is not the same as unreadable, and only one of them is normal.
/// Callers that used to `read_to_string(...).ok()` conflated the two, which
/// made a permission error an invisible reset to defaults.
pub(crate) fn read_toml(path: &Path) -> Result<Option<toml::Value>, String> {
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("cannot read config {}: {e}", path.display())),
    };
    // An empty file is "no sections", not a parse error — same as absent.
    if content.trim().is_empty() {
        return Ok(Some(toml::Value::Table(toml::Table::new())));
    }
    toml::from_str(&content).map(Some).map_err(|e| format!("config parse error: {e}"))
}

/// One strictly-parsed non-negative bound from `section`.
///
/// Read as i64 and reject the negative rather than clamping it: unlike
/// `[llm]`'s timeouts, which have a defensible floor, "minus one day of
/// retention" has no reading at all — and `v as u64` on a negative would wrap
/// to a bound that never fires.
pub(crate) fn bound(
    section: &toml::Value,
    table: &str,
    key: &str,
    default: u64,
) -> Result<u64, String> {
    match section.get(key) {
        None => Ok(default),
        Some(v) => match v.as_integer() {
            Some(n) if n >= 0 => Ok(n.unsigned_abs()),
            _ => Err(format!("config: [{table}] {key} must be a non-negative integer")),
        },
    }
}

/// Like [`bound`], but zero is also unusable — a zero chunk, batch, or
/// attempt count would livelock or search nothing.
pub(crate) fn positive(
    section: &toml::Value,
    table: &str,
    key: &str,
    default: u64,
) -> Result<u64, String> {
    match section.get(key) {
        None => Ok(default),
        Some(v) => match v.as_integer() {
            Some(n) if n > 0 => Ok(n.unsigned_abs()),
            _ => Err(format!("config: [{table}] {key} must be a positive integer")),
        },
    }
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
        let path =
            write_config(&dir, "[llm]\nendpoint = \"http://localhost:11434/v1/\"\nmodel = \"m\"\n");
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
    fn load_config_streams_by_default_and_honors_the_escape_hatch() {
        let dir = tempfile::tempdir().unwrap();
        let base = "[llm]\nendpoint = \"http://h/v1\"\nmodel = \"m\"\n";
        assert!(load_config(&write_config(&dir, base)).unwrap().stream);
        assert!(
            !load_config(&write_config(&dir, &format!("{base}stream = false\n"))).unwrap().stream
        );
        // Parsed leniently, like every other tunable: a diagnostic knob must
        // never be the reason scout refuses to run.
        assert!(
            load_config(&write_config(&dir, &format!("{base}stream = \"yes\"\n"))).unwrap().stream
        );
    }

    #[test]
    fn the_bundled_default_config_documents_stream() {
        assert!(
            DEFAULT_CONFIG.contains("# stream = true"),
            "config.example.toml is the only place a user learns the knob exists"
        );
    }

    // ── [spool] ─────────────────────────────────────────────────────────

    #[test]
    fn spool_bounds_default_when_the_file_or_the_section_is_absent() {
        let dir = tempfile::tempdir().unwrap();
        // `scout gc` runs before anyone has written a config.
        assert_eq!(
            load_spool_config(&dir.path().join("nope.toml")).unwrap(),
            SpoolConfig::default()
        );
        let path = write_config(&dir, "[llm]\nendpoint = \"http://h/v1\"\nmodel = \"m\"\n");
        assert_eq!(load_spool_config(&path).unwrap(), SpoolConfig::default());
        assert_eq!(SpoolConfig::default().max_age_days, 7, "docs/wrap-watch.md §2.3");
        assert_eq!(SpoolConfig::default().max_total_bytes, 500 * 1024 * 1024);
    }

    #[test]
    fn spool_bounds_parse_and_a_single_key_leaves_the_other_at_its_default() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = load_spool_config(&write_config(&dir, "[spool]\nmax_age_days = 2\n")).unwrap();
        assert_eq!(cfg.max_age_days, 2);
        assert_eq!(cfg.max_total_bytes, SpoolConfig::default().max_total_bytes);

        let cfg = load_spool_config(&write_config(
            &dir,
            "[spool]\nmax_age_days = 0\nmax_total_bytes = 1048576\n",
        ))
        .unwrap();
        assert_eq!(cfg.max_age_days, 0, "zero is a valid bound: keep nothing");
        assert_eq!(cfg.max_total_bytes, 1_048_576);
    }

    #[test]
    fn a_present_but_unusable_spool_bound_errors_rather_than_silently_defaulting() {
        let dir = tempfile::tempdir().unwrap();
        for bad in ["max_age_days = -1", "max_total_bytes = \"500MB\"", "max_age_days = 1.5"] {
            let err = load_spool_config(&write_config(&dir, &format!("[spool]\n{bad}\n")))
                .expect_err("an unusable bound must be reported");
            assert!(err.contains("[spool]"), "{bad} -> {err}");
        }
    }

    #[test]
    fn a_config_that_exists_but_cannot_be_read_is_reported_not_silently_defaulted() {
        // /dev/null/config.toml is not "absent", it is unreadable, and the two
        // must not report the same thing.
        let err = load_spool_config(Path::new("/dev/null/config.toml"))
            .expect_err("an unreadable config must be reported");
        assert!(err.contains("cannot read"), "{err}");
    }

    #[test]
    fn the_bundled_default_config_documents_the_spool_bounds() {
        assert!(DEFAULT_CONFIG.contains("# max_age_days = 7"));
        assert!(DEFAULT_CONFIG.contains("# max_total_bytes = 524288000"));
        // The shipped default must be a config both parsers accept, and its
        // commented-out values must be the defaults they claim to show.
        let dir = tempfile::tempdir().unwrap();
        let path = write_config(&dir, DEFAULT_CONFIG);
        assert_eq!(load_spool_config(&path).unwrap(), SpoolConfig::default());
    }

    // ── [wrap] ──────────────────────────────────────────────────────────

    #[test]
    fn wrap_bounds_default_when_the_file_or_the_section_is_absent() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(load_wrap_config(&dir.path().join("nope.toml")).unwrap(), WrapConfig::default());
        let path = write_config(&dir, "[llm]\nendpoint = \"http://h/v1\"\nmodel = \"m\"\n");
        assert_eq!(load_wrap_config(&path).unwrap(), WrapConfig::default());
        assert_eq!(WrapConfig::default().passthrough_max_lines, 200, "docs/wrap-watch.md §3.2");
        assert_eq!(WrapConfig::default().passthrough_max_bytes, 16 * 1024);
        assert_eq!(WrapConfig::default().model_input_bytes, 16 * 1024);
    }

    #[test]
    fn wrap_bounds_parse_and_a_single_key_leaves_the_others_at_their_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let cfg =
            load_wrap_config(&write_config(&dir, "[wrap]\npassthrough_max_lines = 40\n")).unwrap();
        assert_eq!(cfg.passthrough_max_lines, 40);
        assert_eq!(cfg.passthrough_max_bytes, WrapConfig::default().passthrough_max_bytes);
        assert_eq!(cfg.model_input_bytes, WrapConfig::default().model_input_bytes);

        let cfg = load_wrap_config(&write_config(
            &dir,
            "[wrap]\npassthrough_max_lines = 0\npassthrough_max_bytes = 1\nmodel_input_bytes = 4096\n",
        ))
        .unwrap();
        assert_eq!(cfg.passthrough_max_lines, 0, "zero is a valid bound: filter everything");
        assert_eq!(cfg.passthrough_max_bytes, 1);
        assert_eq!(cfg.model_input_bytes, 4096);
    }

    #[test]
    fn a_present_but_unusable_wrap_bound_errors_rather_than_silently_defaulting() {
        // The strict parser's rule, and the reason [wrap] rides it (TODO.md,
        // "Do the parser unification first"): a typo must not read as a setting.
        let dir = tempfile::tempdir().unwrap();
        for bad in [
            "passthrough_max_lines = -1",
            "model_input_bytes = \"16k\"",
            "passthrough_max_bytes = 1.5",
        ] {
            let err = load_wrap_config(&write_config(&dir, &format!("[wrap]\n{bad}\n")))
                .expect_err("an unusable bound must be reported");
            assert!(err.contains("[wrap]"), "{bad} -> {err}");
        }
    }

    #[test]
    fn the_bundled_default_config_documents_the_wrap_bounds() {
        // config.example.toml is the only place a user learns a knob exists.
        assert!(DEFAULT_CONFIG.contains("# passthrough_max_lines = 200"));
        assert!(DEFAULT_CONFIG.contains("# passthrough_max_bytes = 16384"));
        assert!(DEFAULT_CONFIG.contains("# model_input_bytes = 16384"));
        // ...and its commented-out values must be the defaults they claim.
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            load_wrap_config(&write_config(&dir, DEFAULT_CONFIG)).unwrap(),
            WrapConfig::default()
        );
    }

    // ── [check_output] ──────────────────────────────────────────────────

    #[test]
    fn check_output_timeouts_default_when_the_file_or_the_section_is_absent() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            load_check_output_config(&dir.path().join("nope.toml")).unwrap(),
            CheckOutputConfig::default()
        );
        let path = write_config(&dir, "[llm]\nendpoint = \"http://h/v1\"\nmodel = \"m\"\n");
        assert_eq!(load_check_output_config(&path).unwrap(), CheckOutputConfig::default());
        assert_eq!(CheckOutputConfig::default().idle_timeout_seconds, 120);
        assert_eq!(CheckOutputConfig::default().default_timeout_seconds, 900);
    }

    #[test]
    fn check_output_timeouts_parse_and_a_single_key_leaves_the_other_at_its_default() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = load_check_output_config(&write_config(
            &dir,
            "[check_output]\nidle_timeout_seconds = 240\n",
        ))
        .unwrap();
        assert_eq!(cfg.idle_timeout_seconds, 240);
        assert_eq!(cfg.default_timeout_seconds, 900);

        let cfg = load_check_output_config(&write_config(
            &dir,
            "[check_output]\ndefault_timeout_seconds = 1800\n",
        ))
        .unwrap();
        assert_eq!(cfg.idle_timeout_seconds, 120);
        assert_eq!(cfg.default_timeout_seconds, 1800);
    }

    #[test]
    fn a_present_but_unusable_check_output_timeout_errors() {
        let dir = tempfile::tempdir().unwrap();
        for bad in [
            "idle_timeout_seconds = 0",
            "idle_timeout_seconds = -1",
            "default_timeout_seconds = \"15m\"",
            "default_timeout_seconds = 1.5",
        ] {
            let err =
                load_check_output_config(&write_config(&dir, &format!("[check_output]\n{bad}\n")))
                    .expect_err("an unusable timeout must be reported");
            assert!(err.contains("[check_output]"), "{bad} -> {err}");
        }
    }

    #[test]
    fn the_bundled_default_config_documents_check_output_timeouts() {
        assert!(DEFAULT_CONFIG.contains("[check_output]"));
        assert!(DEFAULT_CONFIG.contains("# idle_timeout_seconds = 120"));
        assert!(DEFAULT_CONFIG.contains("# default_timeout_seconds = 900"));
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            load_check_output_config(&write_config(&dir, DEFAULT_CONFIG)).unwrap(),
            CheckOutputConfig::default()
        );
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
    fn load_config_progress_budgets_default_and_parse() {
        let dir = tempfile::tempdir().unwrap();
        let base = "[llm]\nendpoint = \"http://h/v1\"\nmodel = \"m\"\n";
        let cfg = load_config(&write_config(&dir, base)).unwrap();
        assert_eq!(cfg.first_token_timeout, Duration::from_secs(60));
        assert_eq!(cfg.idle_timeout, Duration::from_secs(15));
        // Generous first token, tight idle gap: they measure different things
        // and the defaults have to reflect that or the split is pointless.
        assert!(cfg.idle_timeout < cfg.first_token_timeout);
        assert!(cfg.first_token_timeout < cfg.timeout);

        let cfg = load_config(&write_config(
            &dir,
            &format!("{base}first_token_timeout_seconds = 90\nidle_timeout_seconds = 5\n"),
        ))
        .unwrap();
        assert_eq!(cfg.first_token_timeout, Duration::from_secs(90));
        assert_eq!(cfg.idle_timeout, Duration::from_secs(5));
    }

    #[test]
    fn load_config_negative_progress_budgets_clamped_not_wrapped() {
        // Same regression as timeout_seconds: `v as u64` on a negative i64
        // wraps to near-maxint, which turns a typo into a budget that never
        // fires — the exact failure mode this feature exists to remove.
        let dir = tempfile::tempdir().unwrap();
        let path = write_config(
            &dir,
            "[llm]\nendpoint = \"http://h/v1\"\nmodel = \"m\"\nfirst_token_timeout_seconds = -5\nidle_timeout_seconds = 0\n",
        );
        let cfg = load_config(&path).unwrap();
        assert_eq!(
            cfg.first_token_timeout,
            Duration::from_secs(1),
            "{:?}",
            cfg.first_token_timeout
        );
        assert_eq!(cfg.idle_timeout, Duration::from_secs(1), "{:?}", cfg.idle_timeout);
    }

    #[test]
    fn load_config_progress_budgets_capped_at_one_hour() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_config(
            &dir,
            "[llm]\nendpoint = \"http://h/v1\"\nmodel = \"m\"\nfirst_token_timeout_seconds = 99999\nidle_timeout_seconds = 99999\n",
        );
        let cfg = load_config(&path).unwrap();
        assert_eq!(cfg.first_token_timeout, Duration::from_secs(3600));
        assert_eq!(cfg.idle_timeout, Duration::from_secs(3600));
    }

    #[test]
    fn the_bundled_default_config_documents_the_progress_budgets() {
        // config.example.toml is the only place a user learns a knob exists.
        assert!(DEFAULT_CONFIG.contains("# first_token_timeout_seconds = 60"));
        assert!(DEFAULT_CONFIG.contains("# idle_timeout_seconds = 15"));
    }

    #[test]
    fn the_bundled_default_config_documents_hook_timeouts() {
        // Hook-owned sections: the binary never reads these, but the
        // example is still the only place a user learns the knobs exist.
        assert!(DEFAULT_CONFIG.contains("[shell_safety]"));
        assert!(DEFAULT_CONFIG.contains("[prefer_local]"));
        assert!(DEFAULT_CONFIG.contains("# timeout_seconds = 5"));
        assert!(DEFAULT_CONFIG.contains("# timeout_seconds = 6"));
    }

    #[test]
    fn the_bundled_default_config_documents_dashboard() {
        assert!(
            DEFAULT_CONFIG.contains("[dashboard]"),
            "config.example.toml is the only place a user learns the knob exists"
        );
        assert!(DEFAULT_CONFIG.contains("# port = 13001"));
    }

    // The process environment is global and cargo runs tests on parallel
    // threads, so every test that touches SCOUT_CONFIG / XDG_CONFIG_HOME
    // must hold this lock. Recover from poisoning: a failed test must not
    // cascade into the others.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn env_lock() -> std::sync::MutexGuard<'static, ()> {
        ENV_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
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
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "hand edited, do not clobber");
    }

    #[cfg(unix)]
    fn mode_of(path: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    // `[llm] api_key` can live in this file, so a freshly-seeded config must
    // not land at the process umask (typically 0644, world-readable).
    #[cfg(unix)]
    #[test]
    fn seed_creates_the_config_file_0600_and_its_dir_0700() {
        let dir = TempDir::new().unwrap();
        // Nested, so the seed also has to create the config dir itself,
        // exercising both halves of the fix in one call.
        let p = dir.path().join("scout").join("config.toml");
        assert!(seed_default_config(&p).unwrap());
        assert_eq!(mode_of(&p), 0o600, "config.toml can hold an api_key");
        assert_eq!(
            mode_of(p.parent().unwrap()),
            0o700,
            "the config dir must not be group/other readable"
        );
    }

    #[cfg(unix)]
    #[test]
    fn seed_never_widens_a_pre_existing_configs_mode() {
        use std::os::unix::fs::PermissionsExt;
        let dir = TempDir::new().unwrap();
        let p = dir.path().join("config.toml");
        // A config that predates this fix, or that the user deliberately
        // loosened — seeding must never run for it (it already exists), and
        // if it somehow did, must not be the thing that changes its mode.
        std::fs::write(&p, "hand edited").unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(!seed_default_config(&p).unwrap());
        assert_eq!(mode_of(&p), 0o644, "an existing file's mode is not ours to change");
    }

    #[cfg(unix)]
    #[test]
    fn seed_never_widens_a_pre_existing_config_dirs_mode() {
        use std::os::unix::fs::PermissionsExt;
        let dir = TempDir::new().unwrap();
        let config_dir = dir.path().join("scout");
        std::fs::create_dir(&config_dir).unwrap();
        std::fs::set_permissions(&config_dir, std::fs::Permissions::from_mode(0o775)).unwrap();
        let p = config_dir.join("config.toml");
        assert!(seed_default_config(&p).unwrap());
        assert_eq!(mode_of(&config_dir), 0o775, "a pre-existing dir's mode is not ours to change");
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
