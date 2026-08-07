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
// ```
//
// Deviation from ct: ct nested these under `[plugins.local-llm.extract]` /
// `[plugins.local-llm.grep]` because the file was shared with ct's own
// settings.  scout owns its config file outright (PLAN §5, clean break), so
// the tables sit at the top level.  Unknown keys are ignored, and a missing
// or malformed file silently yields the defaults — a broken config must not
// break a read-side filter.

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
/// underneath it (scout runs its own search; ct delegated to the daemon).
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

/// Load both configs, applying any overrides found in scout's config file.
/// Any read/parse problem silently yields defaults.
pub fn load() -> (ExtractConfig, GrepConfig) {
    let path = crate::config::config_path();
    let Ok(text) = std::fs::read_to_string(path) else {
        return (ExtractConfig::default(), GrepConfig::default());
    };
    parse_overrides(&text)
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
    fn llm_only_config_yields_defaults() {
        // The common case: a config file that configures the endpoint and
        // nothing else must not disturb the filter tunables.
        let (e, g) = parse_overrides("[llm]\nendpoint = \"http://h/v1\"\nmodel = \"m\"\n");
        assert_eq!(e, ExtractConfig::default());
        assert_eq!(g, GrepConfig::default());
    }
}
