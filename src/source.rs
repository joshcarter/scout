// Filesystem backend for the read-side filters.
//
// ct reached its indexed copy of the tree through the daemon socket
// (`ct::Response`); scout owns no index, so this module is the whole of the
// "where does code come from" layer (PLAN §2):
//
// * `read_file` — `std::fs::read_to_string` + line splitting, replacing the
//   daemon `read` call `local_extract.rs:76` used to make.  That call only
//   ever fetched file content, so this is lossless.
// * `search` — gitignore-aware repo walk (`ignore`) plus ripgrep's match
//   engine (`grep-regex` + `grep-searcher`), replacing the daemon `grep` call.
//   No dependency on an installed `rg`/`grep`; identical on macOS and Linux.
//
// Search behaviour (PLAN §2 defaults, all tunable via `filter_config`):
// respects `.gitignore`/`.ignore` and the global gitignore, skips hidden
// directories and binary files, renders ±`context_lines` around each match,
// skips files over `max_file_bytes`, and stops after `max_hits_scanned` hits
// (reporting the truncation rather than silently dropping it).

use std::path::{Path, PathBuf};

use grep_regex::RegexMatcherBuilder;
use grep_searcher::sinks::UTF8;
use grep_searcher::{BinaryDetection, SearcherBuilder};
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
pub fn read_file(project: &Path, file: &str, max_bytes: u64) -> Result<FileContent, String> {
    let raw = Path::new(file);
    let abs: PathBuf = if raw.is_absolute() { raw.to_path_buf() } else { project.join(raw) };

    let meta = std::fs::metadata(&abs).map_err(|e| format!("{}: {e}", abs.display()))?;
    if meta.is_dir() {
        return Err(format!("{} is a directory, not a file", abs.display()));
    }
    if meta.len() > max_bytes {
        return Err(format!(
            "{} is {} bytes, past the {max_bytes}-byte read cap",
            abs.display(),
            meta.len()
        ));
    }

    let bytes = std::fs::read(&abs).map_err(|e| format!("{}: {e}", abs.display()))?;
    if is_binary(&bytes) {
        return Err(format!("{} looks like a binary file", abs.display()));
    }
    let text = String::from_utf8(bytes)
        .map_err(|_| format!("{} is not valid UTF-8 text", abs.display()))?;

    Ok(FileContent { path: display_path(project, &abs), lines: split_lines(&text) })
}

/// Split file text into lines, dropping the terminators.  A trailing newline
/// does not produce a final empty line, so the count matches what an editor
/// (and `ct read`) reports.
pub fn split_lines(text: &str) -> Vec<String> {
    let mut lines: Vec<String> = text.split('\n').map(|l| l.trim_end_matches('\r').to_string()).collect();
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
}

/// The full result of one search: hits plus whether the scan hit its cap.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SearchResults {
    pub hits: Vec<SearchHit>,
    /// True when the walk stopped at `max_hits_scanned` with files left.
    pub truncated: bool,
}

/// Knobs for one search run.  Sourced from `filter_config::GrepConfig`.
#[derive(Debug, Clone, Copy)]
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

    let walker = WalkBuilder::new(root)
        .standard_filters(true) // .gitignore + .ignore + global ignores + hidden
        .parents(true)
        // Apply .gitignore even outside a git checkout — an exported tree or a
        // vendored directory still means what its ignore file says.
        .require_git(false)
        .sort_by_file_path(|a, b| a.cmp(b))
        .build();

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
        let mut line_numbers: Vec<usize> = Vec::new();
        let sink = UTF8(|line_number, _line| {
            line_numbers.push(line_number as usize);
            Ok(true)
        });
        if searcher.search_slice(&matcher, &bytes, sink).is_err() {
            continue; // unreadable/binary mid-file — skip the file, not the run
        }
        if line_numbers.is_empty() {
            continue;
        }

        let lines: Vec<&str> = text.split('\n').map(|l| l.trim_end_matches('\r')).collect();
        let rel = display_path(root, path);
        for line in line_numbers {
            if out.hits.len() >= opts.max_hits {
                out.truncated = true;
                break;
            }
            out.hits.push(SearchHit {
                file: rel.clone(),
                line,
                text: extract_context(&lines, line - 1, opts.context_lines, opts.context_max_bytes),
            });
        }
    }

    Ok(out)
}

/// Render `lines[i-n ..= i+n]`, newline-joined, capped at `max_bytes`.
///
/// Ported from ct's `handlers::extract::extract_context` — the byte budget is
/// measured from the block's *start*, which is why `grep::matched_line` has to
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
