//! `scout edit` — pipeline results straight into `$EDITOR` (docs/search-cli.md §6).
//!
//! A separate verb rather than a flag on `grep`, because the contract differs:
//! it must *end* in an editor, and interactivity is acceptable.  It front-ends
//! **both** search pipelines, disambiguated by arity alone — one positional is
//! a question (`find`), two are a pattern and an intent (`grep`), and `-p` is a
//! pattern with no rerank.  No sentence-detection heuristic decides anything.
//!
//! The module is deliberately split into a pure half and an effectful one:
//!
//! * pure — `dispatch` (arity), `classify`/`open_args`/`quickfix_args` (the
//!   per-editor invocation table), `parse_choice` (the picker's input),
//!   `split_words` (`$EDITOR` may carry arguments), `hits` (payload → targets).
//! * effectful — `run`, which prints, prompts, and hands the terminal over.
//!
//! Everything in the first half is unit-tested; the second half is covered by
//! a fake-editor smoke test.
//!
//! ## Columns
//!
//! The payload's `col` is a **0-based byte offset** and nullable.  Every editor
//! in the table below counts columns from 1, as does the quickfix format, so
//! this module converts once — in `hits` — and works in 1-based columns
//! thereafter.  A null column (the context budget cut the matched line away
//! before the match was reached) falls back to column 1: the line number is
//! still exact, which is all the vi/vim family uses anyway.
//!
//! ## Handing over the terminal
//!
//! The editor replaces scout via `execvp` wherever it can: the editor then owns
//! the tty and the process group outright, so job control, `SIGWINCH` and
//! `SIGTSTP` behave exactly as if the user had typed the editor's name.  The
//! one exception is the vim quickfix path, which has a temp file to delete
//! afterwards — `exec` never returns, so nothing would ever run the cleanup.
//! That path spawns, waits, unlinks, and forwards the editor's exit status.

use crate::render::{self, RenderOpts};
use serde_json::Value;
use std::io::{IsTerminal, Write};
use std::path::PathBuf;
use std::process::Command;

// ── Arity dispatch ───────────────────────────────────────────────────

/// Which pipeline the positionals selected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Pipeline {
    /// One positional: the local model guesses the patterns.
    Find { question: String, attempts: Option<u64> },
    /// Two positionals, or `-p`: an explicit pattern, reranked only when an
    /// intent came with it.
    Grep { pattern: String, intent: Option<String>, regex: bool },
}

/// Resolve `scout edit`'s positionals into a pipeline choice.
///
/// Kept out of clap on purpose.  Expressing "one positional means one thing and
/// two mean another" in clap's conflict grammar is possible but unreadable, and
/// the interesting cases — `-p` next to a positional, `--attempts` on the grep
/// path — deserve a sentence rather than clap's generic conflict text.  The
/// upside is that the whole rule set is one pure function with a table test.
pub fn dispatch(
    query: Option<String>,
    intent: Option<String>,
    pattern: Option<String>,
    regex: bool,
    attempts: Option<u64>,
) -> Result<Pipeline, String> {
    // `--attempts` budgets *pattern guessing*; only `find` guesses.  Silently
    // ignoring it would let a caller believe a retry loop ran.
    let no_attempts = |what: &str| -> Result<(), String> {
        match attempts {
            Some(_) => Err(format!("--attempts belongs to the find pipeline ({what})")),
            None => Ok(()),
        }
    };

    if let Some(pattern) = pattern {
        if query.is_some() || intent.is_some() {
            return Err("-p already carries the pattern — for a reranked search drop it and \
                 pass both positionals: scout edit <pattern> <intent>"
                .into());
        }
        no_attempts("-p never guesses patterns")?;
        return Ok(Pipeline::Grep { pattern, intent: None, regex });
    }

    let Some(query) = query else {
        return Err("nothing to search for — scout edit <question>, \
                    scout edit <pattern> <intent>, or scout edit -p <pattern>"
            .into());
    };

    // Two positionals: pattern + intent, the reranked grep.
    if let Some(intent) = intent {
        no_attempts("this is an explicit pattern")?;
        return Ok(Pipeline::Grep { pattern: query, intent: Some(intent), regex });
    }

    // One positional: a question for the find pipeline.  `--regex` is rejected
    // here for the same reason `scout find` has no such flag — the model
    // decides per candidate whether its pattern is a regex, so a global
    // override would countermand a decision the caller never made.
    if regex {
        return Err("--regex applies to an explicit pattern; this is a question \
                    for the find pipeline (use -p, or add an intent)"
            .into());
    }
    Ok(Pipeline::Find { question: query, attempts })
}

// ── The editor and its invocation ────────────────────────────────────

/// The editor families scout knows how to position (docs/search-cli.md §6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditorKind {
    /// vi/vim/nvim: `+<line>`, and `-q <file>` for a quickfix list.
    Vi,
    /// emacs/emacsclient: `+<line>:<col>`.
    Emacs,
    /// helix: `<file>:<line>:<col>`.
    Helix,
    /// VS Code and its forks: `-g <file>:<line>:<col>`.
    VsCode,
    /// zed: `<file>:<line>:<col>`.
    Zed,
    /// Anything else: just the file, with the line printed beforehand.
    Unknown,
}

/// Classify an editor by the **basename** of its program word.
///
/// A basename, because `$EDITOR` is routinely an absolute path
/// (`/usr/local/bin/nvim`) and just as routinely a bare name.  The spec's table
/// is extended with the obvious siblings of each entry — `gvim`, `code-insiders`
/// — since they take the same flags as the name they were forked from and
/// falling through to `Unknown` would silently lose the line number.
pub fn classify(program: &str) -> EditorKind {
    let base = program.rsplit('/').next().unwrap_or(program);
    match base {
        "vi" | "vim" | "vimx" | "nvim" | "gvim" | "mvim" | "view" => EditorKind::Vi,
        "emacs" | "emacsclient" => EditorKind::Emacs,
        "hx" | "helix" => EditorKind::Helix,
        "code" | "code-insiders" | "codium" | "vscodium" | "cursor" | "windsurf" => {
            EditorKind::VsCode
        }
        "zed" | "zeditor" => EditorKind::Zed,
        _ => EditorKind::Unknown,
    }
}

/// The argv tail that opens `files` with the cursor on `line`:`col`.
///
/// `line` and `col` are 1-based (see the module note on columns) and describe
/// the *first* file; any others are opened as plain arguments, which every
/// editor here reads as "also load these buffers".  That is exactly docs/search-cli.md §6's
/// `a` behaviour for the non-vim families: all the files, positioned at the
/// first hit.
pub fn open_args(kind: EditorKind, files: &[String], line: usize, col: usize) -> Vec<String> {
    let Some((first, rest)) = files.split_first() else { return Vec::new() };
    let mut argv = match kind {
        EditorKind::Vi => vec![format!("+{line}"), first.clone()],
        EditorKind::Emacs => vec![format!("+{line}:{col}"), first.clone()],
        EditorKind::Helix | EditorKind::Zed => vec![format!("{first}:{line}:{col}")],
        EditorKind::VsCode => vec!["-g".to_string(), format!("{first}:{line}:{col}")],
        EditorKind::Unknown => vec![first.clone()],
    };
    argv.extend(rest.iter().cloned());
    argv
}

/// The argv tail that loads a quickfix list into the vi family.
///
/// Precedent: `git jump`, and `rg --vimgrep | vim -q -`.  The file it names is
/// `--format vimgrep` output, produced by the very same formatter (§9), so the
/// list `vim` navigates is byte-identical to what a shell pipeline would build.
pub fn quickfix_args(path: &str) -> Vec<String> {
    vec!["-q".to_string(), path.to_string()]
}

/// Split `$EDITOR` into a program and its arguments.
///
/// `EDITOR="code -w"` and `EDITOR="emacsclient -a ''"` are both common, so the
/// value cannot be treated as a bare program name.  This is a shell-*word*
/// split, not a shell: single quotes, double quotes and backslash escapes are
/// honoured (enough for every real `$EDITOR` anyone writes), while `$VAR`, `~`,
/// globs, pipes and `&&` are not — they would need a shell, and running
/// `$EDITOR` through one would hand arbitrary strings to `sh -c`.
pub fn split_words(s: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut cur = String::new();
    let mut started = false;
    let mut quote: Option<char> = None;
    let mut chars = s.chars();

    while let Some(c) = chars.next() {
        match (quote, c) {
            // A backslash escapes the next character outside quotes and inside
            // double quotes; inside single quotes it is literal, as in sh.
            (None | Some('"'), '\\') => {
                started = true;
                if let Some(next) = chars.next() {
                    cur.push(next);
                }
            }
            (None, '\'' | '"') => {
                started = true;
                quote = Some(c);
            }
            (Some(q), c) if c == q => quote = None,
            (None, c) if c.is_whitespace() => {
                if started {
                    words.push(std::mem::take(&mut cur));
                    started = false;
                }
            }
            (_, c) => {
                started = true;
                cur.push(c);
            }
        }
    }
    if started {
        words.push(cur);
    }
    words
}

/// Read `$EDITOR` and split it, or say what is missing.
///
/// Called *before* the search runs: a missing `$EDITOR` makes the whole verb
/// impossible, and discovering that after a multi-second rerank would be rude.
pub fn editor_words() -> Result<Vec<String>, String> {
    let raw = std::env::var("EDITOR").unwrap_or_default();
    let words = split_words(&raw);
    if words.is_empty() {
        return Err("$EDITOR is not set — export it (e.g. EDITOR=vim), or use \
                    scout grep / scout find to just look at the results"
            .into());
    }
    Ok(words)
}

// ── The picker ───────────────────────────────────────────────────────

/// What the caller typed at the `edit which?` prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Choice {
    /// A 1-based hit number, already range-checked.
    One(usize),
    All,
    Quit,
    /// Anything else — the prompt re-asks rather than guessing.
    Invalid,
}

/// Parse one line of picker input against a list of `n` hits.
///
/// Case-insensitive, whitespace-tolerant, and strict about range: `0` and
/// `n + 1` are `Invalid`, not clamped, because silently opening the wrong hit
/// is worse than asking again.
pub fn parse_choice(input: &str, n: usize) -> Choice {
    let s = input.trim();
    if s.is_empty() {
        return Choice::Invalid;
    }
    match s.to_ascii_lowercase().as_str() {
        "a" | "all" => Choice::All,
        "q" | "quit" => Choice::Quit,
        _ => match s.parse::<usize>() {
            Ok(i) if (1..=n).contains(&i) => Choice::One(i),
            _ => Choice::Invalid,
        },
    }
}

// ── Payload → editor targets ─────────────────────────────────────────

/// One hit, reduced to what an editor needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hit {
    /// Path as the payload carries it: relative to the project root.  The
    /// editor is launched with that root as its working directory, so it
    /// resolves — and the paths the user just read in the listing are the ones
    /// the editor shows.
    pub file: String,
    /// 1-based, as every editor counts lines.
    pub line: usize,
    /// 1-based byte column, converted from the payload's 0-based offset; 1 when
    /// the payload has none.
    pub col: usize,
}

/// Pull the editable hits out of a grep/find payload.
///
/// Hits with no `file` are dropped: there is nothing to open, and passing an
/// empty string to an editor creates a stray unnamed buffer.
pub fn hits(payload: &Value) -> Vec<Hit> {
    let Some(hits) = payload.get("hits").and_then(Value::as_array) else {
        return Vec::new();
    };
    hits.iter()
        .filter_map(|h| {
            let file = h.get("file").and_then(Value::as_str).filter(|f| !f.is_empty())?;
            Some(Hit {
                file: file.to_string(),
                line: h.get("line").and_then(Value::as_u64).unwrap_or(1).max(1) as usize,
                // 0-based byte offset → 1-based column, the editors' convention.
                col: h.get("col").and_then(Value::as_u64).map_or(1, |c| c as usize + 1),
            })
        })
        .collect()
}

/// The distinct files of a hit list, in first-seen order.
///
/// `a` opens *files*, not hits — three hits in one file are one buffer, and
/// naming it three times makes vim's argument list lie about how much is open.
pub fn distinct_files(hits: &[Hit]) -> Vec<String> {
    let mut files: Vec<String> = Vec::new();
    for h in hits {
        if !files.contains(&h.file) {
            files.push(h.file.clone());
        }
    }
    files
}

// ── The flow ─────────────────────────────────────────────────────────

/// Print, pick, and hand over to the editor.  Never returns (docs/search-cli.md §6: when the
/// editor exits, scout exits — there is no re-prompt loop; rerunning is cheap).
///
/// `status` is the caller's already-computed stderr status lines, so `edit`
/// says exactly what `grep`/`find` would have said about the same result — the
/// zero-hit case in particular is required to be indistinguishable.
pub fn run(
    payload: &Value,
    opts: &RenderOpts,
    status: &[String],
    project: &str,
    editor: &[String],
) -> ! {
    let hits = hits(payload);

    // Zero hits: the underlying pipeline's own message, and its exit code.
    if hits.is_empty() {
        emit(status);
        std::process::exit(1);
    }

    // Exactly one: nothing to pick between, so skip the listing entirely and
    // go straight to the file.  This is the case the verb exists for.
    if hits.len() == 1 {
        emit(status);
        launch(editor, &hits, Selection::One(0), payload, project);
    }

    print!("{}", render::render_human(payload, &RenderOpts { numbered: true, ..*opts }));
    let _ = std::io::stdout().flush();
    emit(status);

    // The picker is a terminal conversation.  With stdin redirected there is no
    // one to ask, and inventing an answer would open a file the caller never
    // chose — so say what to use instead and fail with the error code.
    if !std::io::stdin().is_terminal() {
        eprintln!(
            "scout edit: {} hits to choose from but stdin is not a terminal — \
             for scripting use `scout grep --format vimgrep` (or `scout find …`) \
             and feed it to your editor's quickfix list",
            hits.len()
        );
        std::process::exit(2);
    }

    match prompt(hits.len()) {
        Choice::Quit | Choice::Invalid => std::process::exit(0),
        Choice::One(i) => launch(editor, &hits, Selection::One(i - 1), payload, project),
        Choice::All => launch(editor, &hits, Selection::All, payload, project),
    }
}

fn emit(status: &[String]) {
    for line in status {
        eprintln!("{line}");
    }
}

/// Ask until the answer parses.  EOF (`^D`, or a closed stdin) is a quit —
/// looping on it would spin forever.
fn prompt(n: usize) -> Choice {
    let mut line = String::new();
    loop {
        eprint!("edit which? [1-{n}, a=all, q=quit] ");
        let _ = std::io::stderr().flush();
        line.clear();
        match std::io::stdin().read_line(&mut line) {
            Ok(0) | Err(_) => return Choice::Quit,
            Ok(_) => {}
        }
        match parse_choice(&line, n) {
            // Anything unparseable just re-prompts.
            Choice::Invalid => {}
            choice => return choice,
        }
    }
}

/// Which hits the caller picked.
#[derive(Clone, Copy)]
enum Selection {
    /// A 0-based index into the hit list.
    One(usize),
    All,
}

fn launch(editor: &[String], hits: &[Hit], sel: Selection, payload: &Value, project: &str) -> ! {
    let kind = classify(&editor[0]);

    let (files, target) = match sel {
        Selection::One(i) => (vec![hits[i].file.clone()], &hits[i]),
        Selection::All => {
            // The vi family gets a real quickfix list instead of a pile of
            // buffers: `:cn` walks every hit, including several in one file,
            // which the argument list cannot express.
            if kind == EditorKind::Vi {
                quickfix(editor, payload, project);
            }
            (distinct_files(hits), &hits[0])
        }
    };

    // An unknown editor gets no position, so the position is printed instead —
    // otherwise the line number scout worked out is simply lost.
    if kind == EditorKind::Unknown {
        eprintln!("{}: at line {}", target.file, target.line);
    }
    exec_editor(editor, &open_args(kind, &files, target.line, target.col), project, None)
}

/// Write the hit list as `--format vimgrep` to a private temp file and return
/// its path.
///
/// `NamedTempFile` rather than a `scout-quickfix-<pid>.txt` path of our own:
/// a pid-based name in shared `/tmp` is predictable, and `std::fs::write`
/// follows symlinks — another local user can pre-create or race a symlink at
/// that path and get `scout edit -a` to overwrite whatever the invoking user
/// can write. `NamedTempFile` opens with `O_EXCL` and mode `0600`, closing
/// both holes. `.keep()` disarms its own delete-on-drop so the file's
/// lifetime keeps matching what `exec_editor` already does for the vim
/// quickfix path: it must still exist when the editor opens it, and
/// `exec_editor`'s `cleanup` unlinks it once the editor exits.
///
/// Split out from `quickfix` (which never returns) so this fallible half —
/// the part worth unit-testing — is a plain function a test can call and get
/// a `Result` back from, instead of a process exit.
fn write_quickfix_file(payload: &Value) -> std::io::Result<PathBuf> {
    let mut tmp = tempfile::Builder::new().prefix("scout-quickfix-").suffix(".txt").tempfile()?;
    tmp.write_all(render::render_vimgrep(payload).as_bytes())?;
    let (_file, path) = tmp.keep().map_err(|e| e.error)?;
    Ok(path)
}

/// Write the hit list as `--format vimgrep` and open it with `-q`.
fn quickfix(editor: &[String], payload: &Value, project: &str) -> ! {
    let path = match write_quickfix_file(payload) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("scout edit: cannot create the quickfix temp file: {e}");
            std::process::exit(2);
        }
    };
    let args = quickfix_args(&path.to_string_lossy());
    exec_editor(editor, &args, project, Some(path))
}

/// Hand the terminal to the editor.
///
/// With nothing to clean up this is `execvp`: the editor *becomes* this process,
/// so it inherits the tty and the process group and scout leaves no wrapper
/// behind to confuse `^Z` or a window resize.  A quickfix temp file forces the
/// other shape — `exec` never returns, so the unlink would never run — and there
/// the child is spawned, waited on, and its exit status forwarded.
fn exec_editor(editor: &[String], args: &[String], project: &str, cleanup: Option<PathBuf>) -> ! {
    let mut cmd = Command::new(&editor[0]);
    cmd.args(&editor[1..]).args(args).current_dir(project);

    #[cfg(unix)]
    if cleanup.is_none() {
        use std::os::unix::process::CommandExt;
        // Only ever returns on failure to launch.
        let err = cmd.exec();
        eprintln!("scout edit: cannot run {}: {err}", editor[0]);
        std::process::exit(2);
    }

    let status = cmd.status();
    if let Some(path) = cleanup {
        let _ = std::fs::remove_file(path);
    }
    match status {
        Ok(s) => std::process::exit(s.code().unwrap_or(0)),
        Err(e) => {
            eprintln!("scout edit: cannot run {}: {e}", editor[0]);
            std::process::exit(2);
        }
    }
}

#[cfg(test)]
mod tests;
