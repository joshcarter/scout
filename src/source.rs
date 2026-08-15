// Filesystem backend for the read-side filters.
//
// scout owns no code index and talks to no daemon, so this module is the
// whole of the "where does code come from" layer:
//
// * `read_file` — `std::fs::read_to_string` + line splitting.
// * `search` — gitignore-aware repo walk (`ignore`) plus ripgrep's match
//   engine (`grep-regex` + `grep-searcher`).  No dependency on an installed
//   `rg`/`grep`; identical on macOS and Linux.
//
// Search behaviour (all tunable via `filter_config`):
// respects `.gitignore`/`.ignore` and the global gitignore, skips hidden
// directories and binary files, renders ±`context_lines` around each match,
// skips files over `max_file_bytes`, and stops after `max_hits_scanned` hits
// (reporting the truncation rather than silently dropping it).

use std::path::{Path, PathBuf};

use grep_matcher::Matcher;
use grep_regex::RegexMatcherBuilder;
use grep_searcher::sinks::UTF8;
use grep_searcher::{BinaryDetection, SearcherBuilder};
use ignore::overrides::{Override, OverrideBuilder};
use ignore::types::{Types, TypesBuilder};
use ignore::WalkBuilder;

// ── File reads ────────────────────────────────────────────────────────

/// A file resolved and read off disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileContent {
    /// Path as it should be shown to the caller: project-relative when the
    /// file lives under the project root, otherwise the path as given.
    pub path: String,
    /// The file's lines, terminators stripped.
    pub lines: Vec<String>,
}

/// Read `file` (absolute, or relative to `project`) and split it into lines.
///
/// Returns `Err` with a caller-facing message for anything that makes the
/// read useless: missing file, a directory, a non-UTF-8 (binary) file, or a
/// file past `max_bytes`.  Callers fail open on `Err`.
///
/// `metadata().is_dir()` alone is not enough of a gate: FIFOs, character
/// devices, block devices and sockets all report `len() == 0`, so an
/// `is_dir()`-only check lets them sail past the size cap into a `fs::read`
/// that never returns (a FIFO with nothing writing to it) or that grows
/// without bound (`/dev/zero`). The pre-check below is a cheap early-out, not
/// the enforcement — the real cap is the bounded read through `.take`, which
/// also closes the TOCTOU between the `metadata` call and the read (a file
/// growing between the two, which is exactly the shape of the "tail a log"
/// use case `extract` is meant to support).
pub fn read_file(project: &Path, file: &str, max_bytes: u64) -> Result<FileContent, String> {
    let raw = Path::new(file);
    let abs: PathBuf = if raw.is_absolute() { raw.to_path_buf() } else { project.join(raw) };

    let meta = std::fs::metadata(&abs).map_err(|e| format!("{}: {e}", abs.display()))?;
    if meta.is_dir() {
        return Err(format!("{} is a directory, not a file", abs.display()));
    }
    if !meta.is_file() {
        return Err(format!("{} is not a regular file", abs.display()));
    }
    // Cheap early-out: skip opening/reading a file `stat` already says is
    // too big. Not the enforcement on its own — `meta` can be stale by the
    // time the read below runs (a file growing between the two calls), which
    // is why the bounded `.take` read is the real cap.
    if meta.len() > max_bytes {
        return Err(format!(
            "{} is {} bytes, past the {max_bytes}-byte read cap",
            abs.display(),
            meta.len()
        ));
    }

    use std::io::Read as _;
    let mut f = std::fs::File::open(&abs).map_err(|e| format!("{}: {e}", abs.display()))?;
    // Re-check after open: a symlink or mount swapped underfoot between the
    // `metadata` call above and this `open` would otherwise still reach the
    // unbounded read below.
    let opened_meta = f.metadata().map_err(|e| format!("{}: {e}", abs.display()))?;
    if !opened_meta.is_file() {
        return Err(format!("{} is not a regular file", abs.display()));
    }
    let mut bytes = Vec::new();
    f.by_ref()
        .take(max_bytes + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| format!("{}: {e}", abs.display()))?;
    if bytes.len() as u64 > max_bytes {
        return Err(format!("{} is past the {max_bytes}-byte read cap", abs.display()));
    }

    if is_binary(&bytes) {
        return Err(format!("{} looks like a binary file", abs.display()));
    }
    let text = String::from_utf8(bytes)
        .map_err(|_| format!("{} is not valid UTF-8 text", abs.display()))?;

    Ok(FileContent { path: display_path(project, &abs), lines: split_lines(&text) })
}

/// Split file text into lines, dropping the terminators.  A trailing newline
/// does not produce a final empty line, so the count matches what an editor
/// reports.
pub fn split_lines(text: &str) -> Vec<String> {
    let mut lines: Vec<String> =
        text.split('\n').map(|l| l.trim_end_matches('\r').to_string()).collect();
    if lines.last().map(String::is_empty).unwrap_or(false) {
        lines.pop();
    }
    lines
}

/// Render a path relative to the project root when possible.
fn display_path(project: &Path, abs: &Path) -> String {
    abs.strip_prefix(project).unwrap_or(abs).to_string_lossy().to_string()
}

/// Heuristic binary check: a NUL byte in the first 8 KiB.  Same rule ripgrep
/// uses by default, applied here so `read_file` refuses binaries too.
fn is_binary(bytes: &[u8]) -> bool {
    bytes.iter().take(8192).any(|&b| b == 0)
}

// ── Repo search ───────────────────────────────────────────────────────

/// One raw search hit, in the shape the daemon's `grep --context N` response
/// used to carry: the matched line's number plus the rendered context block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchHit {
    /// Project-relative path.
    pub file: String,
    /// 1-based line number of the matched line.
    pub line: usize,
    /// The ±`context_lines` block around the match, newline-joined and
    /// truncated at `context_max_bytes`.
    pub text: String,
    /// **0-based byte** offset of the first match *within the matched line*
    /// (docs/search-cli.md §4).  Only the first match is recorded: the renderer windows
    /// around one span and quickfix wants one column, so per-match detail
    /// would be carried for nobody.  Editors want 1-based columns — the
    /// conversion belongs at the formatter, not here.
    pub col: usize,
    /// Byte offset one past that first match (exclusive).  `col_end == col`
    /// means the matcher declined to re-locate the match on the sunk line, so
    /// there is no span to highlight — never a reason to drop the hit.
    pub col_end: usize,
}

/// The full result of one search: hits plus whether the scan hit its cap.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SearchResults {
    pub hits: Vec<SearchHit>,
    /// True when the walk stopped at `max_hits_scanned` with files left.
    pub truncated: bool,
}

/// Knobs for one search run.  Sourced from `filter_config::GrepConfig`.
///
/// Not `Copy`: `types`/`overrides` own compiled glob sets.  `None` for either
/// is an exact no-op — the corresponding `WalkBuilder` call is skipped
/// entirely, so a filterless run walks precisely the tree it always did.
#[derive(Debug, Clone)]
pub struct SearchOptions {
    /// Treat the pattern as a regex rather than a literal string.
    pub regex: bool,
    /// Context lines on each side of a match.
    pub context_lines: usize,
    /// Byte budget for one rendered context block.
    pub context_max_bytes: usize,
    /// Skip files larger than this.
    pub max_file_bytes: u64,
    /// Stop the walk after this many hits.
    pub max_hits: usize,
    /// ripgrep-style file-type selection (`-t rust`, `-T js`).
    pub types: Option<Types>,
    /// ripgrep-style glob include/exclude (`-g '!target/**'`).
    pub overrides: Option<Override>,
}

impl Default for SearchOptions {
    /// Only the filters default — every other knob is a config value with no
    /// meaningful zero, so callers build the struct explicitly.
    fn default() -> Self {
        SearchOptions {
            regex: false,
            context_lines: 2,
            context_max_bytes: 2000,
            max_file_bytes: 1 << 20,
            max_hits: 1000,
            types: None,
            overrides: None,
        }
    }
}

// ── Filter construction ───────────────────────────────────────────────

/// Build a `Types` matcher from ripgrep's built-in type list.
///
/// `select` names are `-t`, `negate` names are `-T`.  An empty pair yields
/// `None` so the walk is left untouched.  An unknown type name is an error
/// with the caller-facing text scout surfaces verbatim.
pub fn build_types(select: &[String], negate: &[String]) -> Result<Option<Types>, String> {
    if select.is_empty() && negate.is_empty() {
        return Ok(None);
    }
    let mut b = TypesBuilder::new();
    b.add_defaults();
    for name in select {
        b.select(name);
    }
    for name in negate {
        b.negate(name);
    }
    b.build().map(Some).map_err(|e| format!("invalid file type: {e}"))
}

/// Build an `Override` (glob include/exclude) rooted at `root`.
///
/// rg-compatible: a leading `!` negates.  Empty input yields `None`.
pub fn build_overrides(root: &Path, globs: &[String]) -> Result<Option<Override>, String> {
    if globs.is_empty() {
        return Ok(None);
    }
    let mut b = OverrideBuilder::new(root);
    for g in globs {
        b.add(g).map_err(|e| format!("invalid glob {g:?}: {e}"))?;
    }
    b.build().map(Some).map_err(|e| format!("invalid glob set: {e}"))
}

/// Every file type ripgrep knows, as `(name, globs)`, sorted by name.
/// Backs `scout grep --type-list`.
pub fn type_definitions() -> Vec<(String, Vec<String>)> {
    let mut b = TypesBuilder::new();
    b.add_defaults();
    let mut defs: Vec<(String, Vec<String>)> = b
        .definitions()
        .iter()
        .map(|d| (d.name().to_string(), d.globs().iter().map(|g| g.to_string()).collect()))
        .collect();
    defs.sort_by(|a, b| a.0.cmp(&b.0));
    defs
}

/// Walk `root` and return the project-relative path of every file, in sorted
/// order — the paths-only tree sketch `find` shows the pattern-synthesis
/// preset (docs/search-cli.md §5, §9).
///
/// Same walk `search` performs (`.gitignore`, `.ignore`, hidden files, and the
/// caller's `types`/`overrides`), so a `-t rust` run is described the way it
/// will be searched.  `max_entries` bounds the walk itself: the caller trims to
/// a *byte* budget afterwards, but a monorepo should not be fully enumerated
/// into memory first.  Nothing is read — this is names, never contents.
pub fn list_paths(root: &Path, opts: &SearchOptions, max_entries: usize) -> Vec<String> {
    let mut builder = WalkBuilder::new(root);
    builder
        .standard_filters(true)
        .parents(true)
        .require_git(false)
        .sort_by_file_path(|a, b| a.cmp(b));
    if let Some(types) = &opts.types {
        builder.types(types.clone());
    }
    if let Some(ov) = &opts.overrides {
        builder.overrides(ov.clone());
    }

    let mut paths = Vec::new();
    for entry in builder.build() {
        if paths.len() >= max_entries {
            break;
        }
        let Ok(entry) = entry else { continue };
        if !entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
            continue;
        }
        paths.push(display_path(root, entry.path()));
    }
    paths
}

/// Walk `root` and return every hit for `pattern`, with context.
///
/// Files are visited in sorted path order so a given tree always produces the
/// same hit ids — the reranker's ids are positional, and a nondeterministic
/// walk would make them meaningless across calls.
pub fn search(root: &Path, pattern: &str, opts: &SearchOptions) -> Result<SearchResults, String> {
    let matcher = RegexMatcherBuilder::new()
        .line_terminator(Some(b'\n'))
        .fixed_strings(!opts.regex)
        .build(pattern)
        .map_err(|e| format!("invalid pattern {pattern:?}: {e}"))?;

    let mut searcher = SearcherBuilder::new()
        .line_number(true)
        .multi_line(false)
        // A binary file that slipped past the NUL heuristic still stops here.
        .binary_detection(BinaryDetection::quit(0))
        .build();

    let mut out = SearchResults::default();

    let mut builder = WalkBuilder::new(root);
    builder
        .standard_filters(true) // .gitignore + .ignore + global ignores + hidden
        .parents(true)
        // Apply .gitignore even outside a git checkout — an exported tree or a
        // vendored directory still means what its ignore file says.
        .require_git(false)
        .sort_by_file_path(|a, b| a.cmp(b));
    // Only touched when the caller actually asked for a filter, so the
    // no-filter walk is bit-for-bit what it was before these existed.
    if let Some(types) = &opts.types {
        builder.types(types.clone());
    }
    if let Some(ov) = &opts.overrides {
        builder.overrides(ov.clone());
    }
    let walker = builder.build();

    for entry in walker {
        if out.hits.len() >= opts.max_hits {
            out.truncated = true;
            break;
        }
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue, // unreadable dir/symlink loop — skip, never abort
        };
        if !entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
            continue;
        }
        let path = entry.path();
        match entry.metadata() {
            Ok(m) if m.len() > opts.max_file_bytes => continue,
            Ok(_) => {}
            Err(_) => continue,
        }
        let Ok(bytes) = std::fs::read(path) else { continue };
        if is_binary(&bytes) {
            continue;
        }
        let Ok(text) = std::str::from_utf8(&bytes) else { continue };

        // Collect matching line numbers first, then render context from the
        // same buffer — one read per file, and the context block is built
        // exactly the way the daemon used to build it.
        //
        // The sink hands back the matched line itself but not where in it the
        // match sat, so re-run the matcher over that one line to recover the
        // offset (the standard grep-searcher idiom).  It is a single `find`
        // over one line, not a second pass over the file.
        let mut line_hits: Vec<(usize, usize, usize)> = Vec::new();
        let sink = UTF8(|line_number, line| {
            let (col, col_end) = match matcher.find(line.as_bytes()) {
                Ok(Some(m)) => (m.start(), m.end()),
                // No re-match (or a matcher error) is not a reason to lose a
                // hit the searcher already confirmed: record an empty span.
                _ => (0, 0),
            };
            line_hits.push((line_number as usize, col, col_end));
            Ok(true)
        });
        if searcher.search_slice(&matcher, &bytes, sink).is_err() {
            continue; // unreadable/binary mid-file — skip the file, not the run
        }
        if line_hits.is_empty() {
            continue;
        }

        let lines: Vec<&str> = text.split('\n').map(|l| l.trim_end_matches('\r')).collect();
        let rel = display_path(root, path);
        for (line, col, col_end) in line_hits {
            if out.hits.len() >= opts.max_hits {
                out.truncated = true;
                break;
            }
            out.hits.push(SearchHit {
                file: rel.clone(),
                line,
                text: extract_context(&lines, line - 1, opts.context_lines, opts.context_max_bytes),
                col,
                col_end,
            });
        }
    }

    Ok(out)
}

/// Render `lines[i-n ..= i+n]`, newline-joined, capped at `max_bytes`.
///
/// The byte budget is measured from the block's *start*, which is why
/// `grep::matched_line` has to
/// admit that a long preceding line can cut the block before the matched line.
pub fn extract_context(lines: &[&str], line_idx: usize, n: usize, max_bytes: usize) -> String {
    let start = line_idx.saturating_sub(n);
    let end = (line_idx + n + 1).min(lines.len());
    if start >= end {
        return String::new();
    }
    let ctx = lines[start..end].join("\n");
    if ctx.len() > max_bytes {
        let mut cut = max_bytes;
        while cut > 0 && !ctx.is_char_boundary(cut) {
            cut -= 1;
        }
        format!("{}\n... (truncated)", &ctx[..cut])
    } else {
        ctx
    }
}

#[cfg(test)]
mod tests;
