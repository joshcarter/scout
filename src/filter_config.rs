// Tunables for the read-side filters (`extract`, `grep`).
//
// Defaults are sized for a 128K-token local model (qwen3.6-35b-a3b): a 384 KiB
// chunk is ~110k tokens of code, so chunking is the rare case and a 500-hit
// grep list reranks in a single call.
//
// Overrides live in scout's own config file (`~/.config/scout/config.toml`,
// or `$SCOUT_CONFIG`) in two top-level tables:
//
// ```toml
// [llm]                      # see config.rs — endpoint/model/timeout
// endpoint = "http://localhost:1234/v1"
// model    = "qwen/qwen3.6-35b-a3b"
//
// [extract]
// bypass_max_lines  = 200
// chunk_bytes       = 393216
// default_max_lines = 120
// max_file_bytes    = 4194304
//
// [grep]
// bypass_max_hits   = 8
// max_considered    = 500
// batch_size        = 250
// context_lines     = 2
// context_max_bytes = 2000
// max_file_bytes    = 1048576
// max_hits_scanned  = 2000
//
// [cli]                      # terminal rendering only — MCP never reads it
// color       = "auto"
// context     = 2
// max_hits    = 20
// max_columns = 150
//
// [find]                     # `scout find` only — also never read by MCP
// max_attempts       = 3
// max_patterns       = 8
// degenerate_hit_cap = 300
// tree_max_bytes     = 8192
// reflect            = true
//
// [dashboard]                # `scout dashboard` only
// port = 13001
// ```
//
// scout owns its config file outright, so the tables sit at the top level
// rather than nested under a plugin namespace.  Unknown keys are ignored.
// A missing file or section is the defaults; a key that is present and
// unusable is an error, same as `[spool]` / `[wrap]`.  Callers that must
// not fail a read-side filter swallow the error and take the defaults.

/// Tunables for `extract`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtractConfig {
    /// Files at or below this many lines skip the LLM entirely.
    pub bypass_max_lines: usize,
    /// Numbered-content bytes per LLM call before splitting on line boundaries.
    pub chunk_bytes: usize,
    /// Default budget for materialized lines when the caller omits `max_lines`.
    pub default_max_lines: usize,
    /// Refuse to read a file larger than this (bytes).  Past this size the
    /// caller wants a targeted range, not a whole-file read.
    pub max_file_bytes: u64,
}

impl Default for ExtractConfig {
    fn default() -> Self {
        ExtractConfig {
            bypass_max_lines: 200,
            chunk_bytes: 393_216,
            default_max_lines: 120,
            max_file_bytes: 4 * 1024 * 1024,
        }
    }
}

/// Tunables for `grep` — both the LLM rerank stage and the filesystem search
/// underneath it — scout runs the search itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrepConfig {
    /// Hit lists at or below this size skip the LLM entirely.
    pub bypass_max_hits: usize,
    /// Hard cap on hits shown to the model; the rest are reported as truncated.
    pub max_considered: usize,
    /// Hits per LLM call when the considered list is large.
    pub batch_size: usize,
    /// Context lines rendered on each side of a match.
    pub context_lines: usize,
    /// Byte budget for one rendered context block before truncation.
    pub context_max_bytes: usize,
    /// Files larger than this (bytes) are skipped by the search walk.
    pub max_file_bytes: u64,
    /// Hard cap on hits collected by the search walk before it stops.
    pub max_hits_scanned: usize,
}

impl Default for GrepConfig {
    fn default() -> Self {
        GrepConfig {
            bypass_max_hits: 8,
            max_considered: 500,
            batch_size: 250,
            context_lines: 2,
            context_max_bytes: 2000,
            max_file_bytes: 1024 * 1024,
            max_hits_scanned: 2000,
        }
    }
}

/// Terminal-only tunables (`[cli]`, docs/search-cli.md §7).
///
/// These exist purely for the CLI renderer — the MCP server never reads them,
/// so nothing here can change what Claude sees.  That is also why `max_hits`
/// differs from `grep`'s wire default of 10: a human at a terminal wants a
/// fuller list, and the cap is a ceiling rather than a quota (docs/search-cli.md §9).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CliConfig {
    /// `auto` | `always` | `never`.  Kept as a string here and validated by
    /// the CLI, so an unknown value degrades to `auto` rather than erroring.
    pub color: String,
    /// Default `-C` for terminal use.  `None` means "fall back to
    /// `[grep] context_lines`" — the fallback lives in the caller because
    /// only it knows the resolved `GrepConfig`.
    pub context: Option<usize>,
    /// Default result cap for terminal invocations.
    pub max_hits: usize,
    /// Per-line render cap in bytes (`-M`, docs/search-cli.md §4).  `0` is unlimited —
    /// a real choice here, unlike the budget knobs, so zero is accepted.
    pub max_columns: usize,
}

impl Default for CliConfig {
    fn default() -> Self {
        CliConfig { color: "auto".to_string(), context: None, max_hits: 20, max_columns: 150 }
    }
}

/// Tunables for `find` — the intent-only search (docs/search-cli.md §5, §7).
///
/// CLI-only, like `[cli]`: `find` is deliberately not an MCP tool (docs/search-cli.md §9
/// defers it until the pattern-synthesis preset proves out), so nothing here
/// can change what Claude sees.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FindConfig {
    /// Rounds before giving up, shared by both retry kinds — the all-whiff
    /// retry and the reflect-refine retry.  `1` means no retry of either;
    /// `--attempts` overrides.
    pub max_attempts: usize,
    /// Candidate patterns requested from the model per round.
    pub max_patterns: usize,
    /// A candidate matching more lines than this is a bad discriminator — the
    /// moral equivalent of low IDF — and *all* of its hits are dropped before
    /// the reranker sees them.
    pub degenerate_hit_cap: usize,
    /// Byte cap on the file-tree sketch sent to the pattern preset.
    pub tree_max_bytes: usize,
    /// Run the reflect-and-refine stage: after the rerank keeps a list, ask the
    /// model once whether those hits actually answer the question, and re-round
    /// with the patterns it proposes when they do not.  Set `false` to spend
    /// one fewer LLM call per run.
    pub reflect: bool,
}

impl Default for FindConfig {
    fn default() -> Self {
        FindConfig {
            // 3, not 2: the budget is now shared between the all-whiff retry
            // and the reflect-refine retry, so the old value would have let one
            // whiffed round consume the entire self-correction budget.
            max_attempts: 3,
            max_patterns: 8,
            degenerate_hit_cap: 300,
            tree_max_bytes: 8192,
            reflect: true,
        }
    }
}

/// Tunables for `scout dashboard` (docs/dashboard.md §5).
///
/// Only the port, deliberately.  The bind *address* is 127.0.0.1 and is not
/// configurable at all: scout's payloads carry file contents from every repo
/// the user works in, so there is no other address worth supporting and no
/// knob to get wrong.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DashboardConfig {
    pub port: u16,
}

impl Default for DashboardConfig {
    fn default() -> Self {
        DashboardConfig { port: 13001 }
    }
}

/// Load both filter configs, applying any overrides found in scout's config
/// file.  A missing file is the defaults.  A present-but-unusable key is
/// reported once on stderr and then ignored, so a typo never costs the
/// caller the filter — the same swallow `[wrap]` uses on the write path.
pub fn load() -> (ExtractConfig, GrepConfig) {
    match crate::config::read_toml(&crate::config::config_path()) {
        Ok(None) => (ExtractConfig::default(), GrepConfig::default()),
        Ok(Some(root)) => parse_overrides_value(&root).unwrap_or_else(|e| {
            warn_config(&e);
            (ExtractConfig::default(), GrepConfig::default())
        }),
        Err(e) => {
            warn_config(&e);
            (ExtractConfig::default(), GrepConfig::default())
        }
    }
}

/// Load the `[cli]` table.  Separate from `load` so the two MCP-facing
/// configs keep their existing arity and call sites.
pub fn load_cli() -> CliConfig {
    load_section(parse_cli_overrides_value, CliConfig::default)
}

/// Load the `[find]` table.  Separate from `load` for the same reason
/// `load_cli` is: the MCP-facing configs keep their existing arity.
pub fn load_find() -> FindConfig {
    load_section(parse_find_overrides_value, FindConfig::default)
}

/// Load the `[dashboard]` table.  Separate from `load` for the same reason
/// `load_cli` is.
pub fn load_dashboard() -> DashboardConfig {
    load_section(parse_dashboard_overrides_value, DashboardConfig::default)
}

fn load_section<T>(parse: fn(&toml::Value) -> Result<T, String>, default: fn() -> T) -> T {
    match crate::config::read_toml(&crate::config::config_path()) {
        Ok(None) => default(),
        Ok(Some(root)) => parse(&root).unwrap_or_else(|e| {
            warn_config(&e);
            default()
        }),
        Err(e) => {
            warn_config(&e);
            default()
        }
    }
}

fn warn_config(err: &str) {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        eprintln!("scout: {err}; using defaults");
    });
}

fn parse_root(toml_text: &str) -> Result<toml::Value, String> {
    // An empty body is "no sections", not a parse error.
    if toml_text.trim().is_empty() {
        return Ok(toml::Value::Table(toml::Table::new()));
    }
    toml::from_str(toml_text).map_err(|e| format!("config parse error: {e}"))
}

/// Parse `[cli]` overrides out of a scout config file body.
pub fn parse_cli_overrides(toml_text: &str) -> Result<CliConfig, String> {
    parse_cli_overrides_value(&parse_root(toml_text)?)
}

fn parse_cli_overrides_value(root: &toml::Value) -> Result<CliConfig, String> {
    let mut cli = CliConfig::default();
    let Some(t) = root.get("cli") else { return Ok(cli) };

    if let Some(v) = t.get("color") {
        let Some(s) = v.as_str() else {
            return Err("config: [cli] color must be a string".into());
        };
        let s = s.trim().to_ascii_lowercase();
        if !matches!(s.as_str(), "auto" | "always" | "never") {
            return Err("config: [cli] color must be auto, always, or never".into());
        }
        cli.color = s;
    }
    // `context = 0` is meaningful (show the matched line only).
    if t.get("context").is_some() {
        cli.context = Some(crate::config::bound(t, "cli", "context", 0)? as usize);
    }
    cli.max_hits = crate::config::positive(t, "cli", "max_hits", cli.max_hits as u64)? as usize;
    // `max_columns = 0` means "no cap" (docs/search-cli.md §7).
    cli.max_columns =
        crate::config::bound(t, "cli", "max_columns", cli.max_columns as u64)? as usize;
    Ok(cli)
}

/// Parse `[find]` overrides out of a scout config file body.
pub fn parse_find_overrides(toml_text: &str) -> Result<FindConfig, String> {
    parse_find_overrides_value(&parse_root(toml_text)?)
}

fn parse_find_overrides_value(root: &toml::Value) -> Result<FindConfig, String> {
    let mut find = FindConfig::default();
    let Some(t) = root.get("find") else { return Ok(find) };
    find.max_attempts =
        crate::config::positive(t, "find", "max_attempts", find.max_attempts as u64)? as usize;
    find.max_patterns =
        crate::config::positive(t, "find", "max_patterns", find.max_patterns as u64)? as usize;
    find.degenerate_hit_cap =
        crate::config::positive(t, "find", "degenerate_hit_cap", find.degenerate_hit_cap as u64)?
            as usize;
    find.tree_max_bytes =
        crate::config::positive(t, "find", "tree_max_bytes", find.tree_max_bytes as u64)? as usize;
    if let Some(v) = t.get("reflect") {
        let Some(b) = v.as_bool() else {
            return Err("config: [find] reflect must be a boolean".into());
        };
        find.reflect = b;
    }
    Ok(find)
}

/// Parse `[dashboard]` overrides out of a scout config file body.
///
/// A port outside 1..=65535 is an error rather than wrapping: `port = 0`
/// would ask the OS for a random one, which a daemon on a well-known port
/// that clients probe by number cannot use.
pub fn parse_dashboard_overrides(toml_text: &str) -> Result<DashboardConfig, String> {
    parse_dashboard_overrides_value(&parse_root(toml_text)?)
}

fn parse_dashboard_overrides_value(root: &toml::Value) -> Result<DashboardConfig, String> {
    let mut dash = DashboardConfig::default();
    let Some(t) = root.get("dashboard") else { return Ok(dash) };
    if let Some(v) = t.get("port") {
        let Some(n) = v.as_integer() else {
            return Err("config: [dashboard] port must be an integer 1..=65535".into());
        };
        if !(1..=65535).contains(&n) {
            return Err("config: [dashboard] port must be an integer 1..=65535".into());
        }
        dash.port = n as u16;
    }
    Ok(dash)
}

/// Parse `[extract]` / `[grep]` overrides out of a scout config file body.
pub fn parse_overrides(toml_text: &str) -> Result<(ExtractConfig, GrepConfig), String> {
    parse_overrides_value(&parse_root(toml_text)?)
}

fn parse_overrides_value(root: &toml::Value) -> Result<(ExtractConfig, GrepConfig), String> {
    let mut extract = ExtractConfig::default();
    let mut grep = GrepConfig::default();

    if let Some(t) = root.get("extract") {
        extract.bypass_max_lines = crate::config::positive(
            t,
            "extract",
            "bypass_max_lines",
            extract.bypass_max_lines as u64,
        )? as usize;
        extract.chunk_bytes =
            crate::config::positive(t, "extract", "chunk_bytes", extract.chunk_bytes as u64)?
                as usize;
        extract.default_max_lines = crate::config::positive(
            t,
            "extract",
            "default_max_lines",
            extract.default_max_lines as u64,
        )? as usize;
        extract.max_file_bytes =
            crate::config::positive(t, "extract", "max_file_bytes", extract.max_file_bytes)?;
    }
    if let Some(t) = root.get("grep") {
        grep.bypass_max_hits =
            crate::config::positive(t, "grep", "bypass_max_hits", grep.bypass_max_hits as u64)?
                as usize;
        grep.max_considered =
            crate::config::positive(t, "grep", "max_considered", grep.max_considered as u64)?
                as usize;
        grep.batch_size =
            crate::config::positive(t, "grep", "batch_size", grep.batch_size as u64)? as usize;
        // `context_lines = 0` is meaningful (matched line only), same as `[cli] context`.
        grep.context_lines =
            crate::config::bound(t, "grep", "context_lines", grep.context_lines as u64)? as usize;
        grep.context_max_bytes =
            crate::config::positive(t, "grep", "context_max_bytes", grep.context_max_bytes as u64)?
                as usize;
        grep.max_file_bytes =
            crate::config::positive(t, "grep", "max_file_bytes", grep.max_file_bytes)?;
        grep.max_hits_scanned =
            crate::config::positive(t, "grep", "max_hits_scanned", grep.max_hits_scanned as u64)?
                as usize;
    }
    Ok((extract, grep))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_spec() {
        let e = ExtractConfig::default();
        assert_eq!(e.bypass_max_lines, 200);
        assert_eq!(e.chunk_bytes, 393_216);
        assert_eq!(e.default_max_lines, 120);
        let g = GrepConfig::default();
        assert_eq!(g.bypass_max_hits, 8);
        assert_eq!(g.max_considered, 500);
        assert_eq!(g.batch_size, 250);
        assert_eq!(g.context_lines, 2);
    }

    #[test]
    fn overrides_are_applied() {
        let (e, g) = parse_overrides(
            r#"
[llm]
endpoint = "http://localhost:1234/v1"

[extract]
bypass_max_lines = 50
default_max_lines = 40

[grep]
bypass_max_hits = 3
batch_size = 100
context_lines = 4
"#,
        )
        .unwrap();
        assert_eq!(e.bypass_max_lines, 50);
        assert_eq!(e.default_max_lines, 40);
        assert_eq!(e.chunk_bytes, 393_216, "unset key keeps the default");
        assert_eq!(g.bypass_max_hits, 3);
        assert_eq!(g.batch_size, 100);
        assert_eq!(g.context_lines, 4);
        assert_eq!(g.max_considered, 500);
    }

    #[test]
    fn search_knobs_are_tunable() {
        let (e, g) = parse_overrides(
            "[extract]\nmax_file_bytes = 123\n\n[grep]\nmax_file_bytes = 456\nmax_hits_scanned = 7\ncontext_max_bytes = 89\n",
        )
        .unwrap();
        assert_eq!(e.max_file_bytes, 123);
        assert_eq!(g.max_file_bytes, 456);
        assert_eq!(g.max_hits_scanned, 7);
        assert_eq!(g.context_max_bytes, 89);
    }

    #[test]
    fn absent_section_or_empty_file_yields_defaults() {
        assert_eq!(parse_overrides("").unwrap().1, GrepConfig::default());
        assert_eq!(
            parse_overrides("[llm]\nendpoint = \"http://h/v1\"\n").unwrap().0,
            ExtractConfig::default()
        );
    }

    #[test]
    fn malformed_or_unusable_values_are_errors() {
        assert!(parse_overrides("not = = toml").is_err());
        let err = parse_overrides("[extract]\nchunk_bytes = 0\n").unwrap_err();
        assert!(err.contains("[extract]"), "{err}");
        assert!(err.contains("chunk_bytes"), "{err}");
        let err = parse_overrides("[extract]\nbypass_max_lines = -5\n").unwrap_err();
        assert!(err.contains("bypass_max_lines"), "{err}");
    }

    #[test]
    fn cli_defaults_match_spec() {
        let c = CliConfig::default();
        assert_eq!(c.color, "auto");
        assert_eq!(c.context, None, "unset means: fall back to [grep] context_lines");
        assert_eq!(c.max_hits, 20, "terminal default, not grep's wire default of 10");
        assert_eq!(c.max_columns, 150, "docs/search-cli.md §9 fixed the per-line cap at 150");
    }

    #[test]
    fn max_columns_accepts_zero_as_unlimited() {
        // Unlike the budget knobs, 0 is a real setting here (docs/search-cli.md §7) — it is
        // how a user turns the cap off for good rather than typing -M 0 daily.
        assert_eq!(parse_cli_overrides("[cli]\nmax_columns = 0\n").unwrap().max_columns, 0);
        assert_eq!(parse_cli_overrides("[cli]\nmax_columns = 400\n").unwrap().max_columns, 400);
        assert!(parse_cli_overrides("[cli]\nmax_columns = -1\n").is_err());
        assert!(parse_cli_overrides("[cli]\nmax_columns = \"wide\"\n").is_err());
    }

    #[test]
    fn cli_overrides_are_applied() {
        let c = parse_cli_overrides(
            "[cli]\ncolor = \"never\"\ncontext = 4\nmax_hits = 50\nmax_columns = 80\n",
        )
        .unwrap();
        assert_eq!(c.color, "never");
        assert_eq!(c.context, Some(4));
        assert_eq!(c.max_hits, 50);
        assert_eq!(c.max_columns, 80);
        // context = 0 is a real choice (matched line only), unlike the budgets.
        assert_eq!(parse_cli_overrides("[cli]\ncontext = 0\n").unwrap().context, Some(0));
    }

    #[test]
    fn cli_junk_values_are_errors() {
        assert!(parse_cli_overrides("[cli]\ncolor = \"chartreuse\"\n").is_err());
        assert!(parse_cli_overrides("[cli]\ncolor = 3\n").is_err());
        assert!(parse_cli_overrides("[cli]\nmax_hits = -1\n").is_err());
        assert!(parse_cli_overrides("[cli]\ncontext = -2\n").is_err());
        assert!(parse_cli_overrides("not = = toml").is_err());
        assert_eq!(parse_cli_overrides("").unwrap(), CliConfig::default());
        // Case and padding are forgiven; the value is normalized.
        assert_eq!(parse_cli_overrides("[cli]\ncolor = \" Always \"\n").unwrap().color, "always");
    }

    #[test]
    fn cli_table_does_not_disturb_the_filter_tables() {
        let text = "[cli]\nmax_hits = 50\ncontext = 9\n";
        let (e, g) = parse_overrides(text).unwrap();
        assert_eq!(e, ExtractConfig::default());
        assert_eq!(g, GrepConfig::default());
    }

    #[test]
    fn find_defaults_match_spec() {
        let f = FindConfig::default();
        assert_eq!(f.max_attempts, 3, "the budget is shared by whiff- and reflect-retries");
        assert_eq!(f.max_patterns, 8);
        assert_eq!(f.degenerate_hit_cap, 300);
        assert_eq!(f.tree_max_bytes, 8192);
        assert!(f.reflect, "self-correction is on unless it is turned off");
    }

    #[test]
    fn find_overrides_are_applied() {
        let f = parse_find_overrides(
            "[find]\nmax_attempts = 4\nmax_patterns = 5\ndegenerate_hit_cap = 50\ntree_max_bytes = 1024\n",
        )
        .unwrap();
        assert_eq!(f.max_attempts, 4);
        assert_eq!(f.max_patterns, 5);
        assert_eq!(f.degenerate_hit_cap, 50);
        assert_eq!(f.tree_max_bytes, 1024);
        // An unset key keeps its default rather than zeroing out.
        let partial = parse_find_overrides("[find]\nmax_attempts = 1\n").unwrap();
        assert_eq!(partial.max_attempts, 1);
        assert_eq!(partial.max_patterns, 8);
    }

    #[test]
    fn reflect_is_a_bool_and_junk_is_an_error() {
        assert!(!parse_find_overrides("[find]\nreflect = false\n").unwrap().reflect);
        assert!(parse_find_overrides("[find]\nreflect = true\n").unwrap().reflect);
        assert!(parse_find_overrides("[find]\nreflect = 0\n").is_err());
        assert!(parse_find_overrides("[find]\nreflect = \"no\"\n").is_err());
    }

    #[test]
    fn find_junk_values_are_errors() {
        // Zero is unusable for every knob in this table: no attempts, no
        // patterns, a zero hit cap and a zero tree budget all mean "never
        // find anything".
        assert!(parse_find_overrides("[find]\nmax_attempts = 0\n").is_err());
        assert!(parse_find_overrides("[find]\nmax_patterns = -3\n").is_err());
        assert!(parse_find_overrides("[find]\ntree_max_bytes = \"lots\"\n").is_err());
        assert!(parse_find_overrides("not = = toml").is_err());
        assert_eq!(parse_find_overrides("").unwrap(), FindConfig::default());
    }

    #[test]
    fn find_table_does_not_disturb_the_other_tables() {
        let text = "[find]\nmax_attempts = 3\ntree_max_bytes = 99\n";
        let (e, g) = parse_overrides(text).unwrap();
        assert_eq!(e, ExtractConfig::default());
        assert_eq!(g, GrepConfig::default());
        assert_eq!(parse_cli_overrides(text).unwrap(), CliConfig::default());
    }

    #[test]
    fn dashboard_defaults_and_overrides() {
        assert_eq!(DashboardConfig::default().port, 13001);
        assert_eq!(parse_dashboard_overrides("[dashboard]\nport = 8080\n").unwrap().port, 8080);
    }

    #[test]
    fn dashboard_junk_ports_are_errors() {
        // 0 would mean "any free port", which nothing could then probe for.
        for text in [
            "[dashboard]\nport = 0\n",
            "[dashboard]\nport = -1\n",
            "[dashboard]\nport = 70000\n",
            "[dashboard]\nport = \"13001\"\n",
            "not = = toml",
        ] {
            assert!(parse_dashboard_overrides(text).is_err(), "{text:?}");
        }
        assert_eq!(parse_dashboard_overrides("").unwrap(), DashboardConfig::default());
    }

    #[test]
    fn llm_only_config_yields_defaults() {
        // The common case: a config file that configures the endpoint and
        // nothing else must not disturb the filter tunables.
        let text = "[llm]\nendpoint = \"http://h/v1\"\nmodel = \"m\"\n";
        let (e, g) = parse_overrides(text).unwrap();
        assert_eq!(e, ExtractConfig::default());
        assert_eq!(g, GrepConfig::default());
        assert_eq!(parse_cli_overrides(text).unwrap(), CliConfig::default());
        assert_eq!(parse_find_overrides(text).unwrap(), FindConfig::default());
        assert_eq!(parse_dashboard_overrides(text).unwrap(), DashboardConfig::default());
    }
}
