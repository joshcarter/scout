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
// endpoint = "http://localhost:11434/v1"
// model    = "qwen3:27b"
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
// rather than nested under a plugin namespace.  Unknown keys are ignored,
// and a missing or malformed file silently yields the defaults — a broken
// config must not break a read-side filter.

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
/// file.  Any read/parse problem silently yields defaults.
pub fn load() -> (ExtractConfig, GrepConfig) {
    match read_config() {
        Some(text) => parse_overrides(&text),
        None => (ExtractConfig::default(), GrepConfig::default()),
    }
}

/// Load the `[cli]` table.  Separate from `load` so the two MCP-facing
/// configs keep their existing arity and call sites.
pub fn load_cli() -> CliConfig {
    match read_config() {
        Some(text) => parse_cli_overrides(&text),
        None => CliConfig::default(),
    }
}

/// Load the `[find]` table.  Separate from `load` for the same reason
/// `load_cli` is: the MCP-facing configs keep their existing arity.
pub fn load_find() -> FindConfig {
    match read_config() {
        Some(text) => parse_find_overrides(&text),
        None => FindConfig::default(),
    }
}

/// Load the `[dashboard]` table.  Separate from `load` for the same reason
/// `load_cli` is.
pub fn load_dashboard() -> DashboardConfig {
    match read_config() {
        Some(text) => parse_dashboard_overrides(&text),
        None => DashboardConfig::default(),
    }
}

fn read_config() -> Option<String> {
    std::fs::read_to_string(crate::config::config_path()).ok()
}

/// Parse `[cli]` overrides out of a scout config file body.  As lenient as
/// `parse_overrides`: unknown keys, wrong types and a malformed file all
/// silently keep the defaults.
pub fn parse_cli_overrides(toml_text: &str) -> CliConfig {
    let mut cli = CliConfig::default();
    let Ok(root) = toml::from_str::<toml::Table>(toml_text) else {
        return cli;
    };
    let Some(t) = root.get("cli") else { return cli };

    if let Some(v) = t.get("color").and_then(toml::Value::as_str) {
        let v = v.trim().to_ascii_lowercase();
        if matches!(v.as_str(), "auto" | "always" | "never") {
            cli.color = v;
        }
    }
    // `context = 0` is meaningful here (show the matched line only), so this
    // one accepts zero — unlike the budget knobs above, where zero livelocks.
    if let Some(v) = t.get("context").and_then(toml::Value::as_integer) {
        if v >= 0 {
            cli.context = Some(v as usize);
        }
    }
    set_usize(&mut cli.max_hits, t, "max_hits");
    // `max_columns = 0` means "no cap" (docs/search-cli.md §7), so this one accepts zero
    // for the same reason `context` does.
    if let Some(v) = t.get("max_columns").and_then(toml::Value::as_integer) {
        if v >= 0 {
            cli.max_columns = v as usize;
        }
    }
    cli
}

/// Parse `[find]` overrides out of a scout config file body.  Every knob here
/// has no meaningful zero — a zero attempt count, pattern budget, hit cap or
/// tree budget all mean "never search" — so `set_usize`'s positive-only rule
/// is exactly right, and junk keeps the default.
pub fn parse_find_overrides(toml_text: &str) -> FindConfig {
    let mut find = FindConfig::default();
    let Ok(root) = toml::from_str::<toml::Table>(toml_text) else {
        return find;
    };
    let Some(t) = root.get("find") else { return find };
    set_usize(&mut find.max_attempts, t, "max_attempts");
    set_usize(&mut find.max_patterns, t, "max_patterns");
    set_usize(&mut find.degenerate_hit_cap, t, "degenerate_hit_cap");
    set_usize(&mut find.tree_max_bytes, t, "tree_max_bytes");
    // The one non-numeric knob here: a bool, so anything else keeps the default.
    if let Some(v) = t.get("reflect").and_then(toml::Value::as_bool) {
        find.reflect = v;
    }
    find
}

/// Parse `[dashboard]` overrides out of a scout config file body.
///
/// A port outside 1..=65535 keeps the default rather than wrapping: `port = 0`
/// would ask the OS for a random one, which a daemon on a well-known port that
/// clients probe by number cannot use.
pub fn parse_dashboard_overrides(toml_text: &str) -> DashboardConfig {
    let mut dash = DashboardConfig::default();
    let Ok(root) = toml::from_str::<toml::Table>(toml_text) else {
        return dash;
    };
    let Some(t) = root.get("dashboard") else { return dash };
    if let Some(v) = t.get("port").and_then(toml::Value::as_integer) {
        if (1..=65535).contains(&v) {
            dash.port = v as u16;
        }
    }
    dash
}

/// Parse `[extract]` / `[grep]` overrides out of a scout config file body.
pub fn parse_overrides(toml_text: &str) -> (ExtractConfig, GrepConfig) {
    let mut extract = ExtractConfig::default();
    let mut grep = GrepConfig::default();
    // toml 1.0: `Value: FromStr` parses a bare *value*, not a document —
    // deserialize into a Table so the whole config file parses.
    let Ok(root) = toml::from_str::<toml::Table>(toml_text) else {
        return (extract, grep);
    };

    if let Some(t) = root.get("extract") {
        set_usize(&mut extract.bypass_max_lines, t, "bypass_max_lines");
        set_usize(&mut extract.chunk_bytes, t, "chunk_bytes");
        set_usize(&mut extract.default_max_lines, t, "default_max_lines");
        set_u64(&mut extract.max_file_bytes, t, "max_file_bytes");
    }
    if let Some(t) = root.get("grep") {
        set_usize(&mut grep.bypass_max_hits, t, "bypass_max_hits");
        set_usize(&mut grep.max_considered, t, "max_considered");
        set_usize(&mut grep.batch_size, t, "batch_size");
        set_usize(&mut grep.context_lines, t, "context_lines");
        set_usize(&mut grep.context_max_bytes, t, "context_max_bytes");
        set_u64(&mut grep.max_file_bytes, t, "max_file_bytes");
        set_usize(&mut grep.max_hits_scanned, t, "max_hits_scanned");
    }
    (extract, grep)
}

/// Apply a positive integer override; zero and negatives keep the default
/// (a zero chunk size or batch size would livelock the chunker).
fn set_usize(slot: &mut usize, table: &toml::Value, key: &str) {
    if let Some(v) = table.get(key).and_then(toml::Value::as_integer) {
        if v > 0 {
            *slot = v as usize;
        }
    }
}

fn set_u64(slot: &mut u64, table: &toml::Value, key: &str) {
    if let Some(v) = table.get(key).and_then(toml::Value::as_integer) {
        if v > 0 {
            *slot = v as u64;
        }
    }
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
        );
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
        );
        assert_eq!(e.max_file_bytes, 123);
        assert_eq!(g.max_file_bytes, 456);
        assert_eq!(g.max_hits_scanned, 7);
        assert_eq!(g.context_max_bytes, 89);
    }

    #[test]
    fn malformed_or_absent_config_yields_defaults() {
        assert_eq!(parse_overrides("not = = toml").0, ExtractConfig::default());
        assert_eq!(parse_overrides("").1, GrepConfig::default());
        // Non-positive values are ignored rather than creating a zero budget.
        let (e, _) = parse_overrides("[extract]\nchunk_bytes = 0\nbypass_max_lines = -5\n");
        assert_eq!(e, ExtractConfig::default());
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
        assert_eq!(parse_cli_overrides("[cli]\nmax_columns = 0\n").max_columns, 0);
        assert_eq!(parse_cli_overrides("[cli]\nmax_columns = 400\n").max_columns, 400);
        // ...but a negative is still junk, and junk keeps the default.
        assert_eq!(parse_cli_overrides("[cli]\nmax_columns = -1\n").max_columns, 150);
        assert_eq!(parse_cli_overrides("[cli]\nmax_columns = \"wide\"\n").max_columns, 150);
    }

    #[test]
    fn cli_overrides_are_applied() {
        let c = parse_cli_overrides(
            "[cli]\ncolor = \"never\"\ncontext = 4\nmax_hits = 50\nmax_columns = 80\n",
        );
        assert_eq!(c.color, "never");
        assert_eq!(c.context, Some(4));
        assert_eq!(c.max_hits, 50);
        assert_eq!(c.max_columns, 80);
        // context = 0 is a real choice (matched line only), unlike the budgets.
        assert_eq!(parse_cli_overrides("[cli]\ncontext = 0\n").context, Some(0));
    }

    #[test]
    fn cli_junk_values_keep_defaults() {
        assert_eq!(parse_cli_overrides("[cli]\ncolor = \"chartreuse\"\n"), CliConfig::default());
        assert_eq!(parse_cli_overrides("[cli]\ncolor = 3\nmax_hits = -1\n"), CliConfig::default());
        assert_eq!(parse_cli_overrides("[cli]\ncontext = -2\n").context, None);
        assert_eq!(parse_cli_overrides("not = = toml"), CliConfig::default());
        assert_eq!(parse_cli_overrides(""), CliConfig::default());
        // Case and padding are forgiven; the value is normalized.
        assert_eq!(parse_cli_overrides("[cli]\ncolor = \" Always \"\n").color, "always");
    }

    #[test]
    fn cli_table_does_not_disturb_the_filter_tables() {
        let text = "[cli]\nmax_hits = 50\ncontext = 9\n";
        let (e, g) = parse_overrides(text);
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
        );
        assert_eq!(f.max_attempts, 4);
        assert_eq!(f.max_patterns, 5);
        assert_eq!(f.degenerate_hit_cap, 50);
        assert_eq!(f.tree_max_bytes, 1024);
        // An unset key keeps its default rather than zeroing out.
        let partial = parse_find_overrides("[find]\nmax_attempts = 1\n");
        assert_eq!(partial.max_attempts, 1);
        assert_eq!(partial.max_patterns, 8);
    }

    #[test]
    fn reflect_is_a_bool_and_junk_keeps_it_on() {
        assert!(!parse_find_overrides("[find]\nreflect = false\n").reflect);
        assert!(parse_find_overrides("[find]\nreflect = true\n").reflect);
        // Not a bool — the stage stays on rather than silently disappearing.
        assert!(parse_find_overrides("[find]\nreflect = 0\n").reflect);
        assert!(parse_find_overrides("[find]\nreflect = \"no\"\n").reflect);
    }

    #[test]
    fn find_junk_values_keep_defaults() {
        // Zero is junk for every knob in this table: no attempts, no patterns,
        // a zero hit cap and a zero tree budget all mean "never find anything".
        assert_eq!(
            parse_find_overrides("[find]\nmax_attempts = 0\ndegenerate_hit_cap = 0\n"),
            FindConfig::default()
        );
        assert_eq!(parse_find_overrides("[find]\nmax_patterns = -3\n"), FindConfig::default());
        assert_eq!(
            parse_find_overrides("[find]\ntree_max_bytes = \"lots\"\n"),
            FindConfig::default()
        );
        assert_eq!(parse_find_overrides("not = = toml"), FindConfig::default());
        assert_eq!(parse_find_overrides(""), FindConfig::default());
    }

    #[test]
    fn find_table_does_not_disturb_the_other_tables() {
        let text = "[find]\nmax_attempts = 3\ntree_max_bytes = 99\n";
        let (e, g) = parse_overrides(text);
        assert_eq!(e, ExtractConfig::default());
        assert_eq!(g, GrepConfig::default());
        assert_eq!(parse_cli_overrides(text), CliConfig::default());
    }

    #[test]
    fn dashboard_defaults_and_overrides() {
        assert_eq!(DashboardConfig::default().port, 13001);
        assert_eq!(parse_dashboard_overrides("[dashboard]\nport = 8080\n").port, 8080);
    }

    #[test]
    fn dashboard_junk_ports_keep_the_default() {
        // 0 would mean "any free port", which nothing could then probe for.
        for text in [
            "[dashboard]\nport = 0\n",
            "[dashboard]\nport = -1\n",
            "[dashboard]\nport = 70000\n",
            "[dashboard]\nport = \"13001\"\n",
            "not = = toml",
            "",
        ] {
            assert_eq!(parse_dashboard_overrides(text), DashboardConfig::default(), "{text:?}");
        }
    }

    #[test]
    fn llm_only_config_yields_defaults() {
        // The common case: a config file that configures the endpoint and
        // nothing else must not disturb the filter tunables.
        let text = "[llm]\nendpoint = \"http://h/v1\"\nmodel = \"m\"\n";
        let (e, g) = parse_overrides(text);
        assert_eq!(e, ExtractConfig::default());
        assert_eq!(g, GrepConfig::default());
        assert_eq!(parse_cli_overrides(text), CliConfig::default());
        assert_eq!(parse_find_overrides(text), FindConfig::default());
        assert_eq!(parse_dashboard_overrides(text), DashboardConfig::default());
    }
}
