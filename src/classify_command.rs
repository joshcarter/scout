// `scout classify-command` subcommand.
//
// Decides whether a Bash command string is a build/test invocation that
// hooks/prefer-local-llm.sh should redirect to `check_output`, and whether it
// carries the `# raw-output` escape marker.
//
//   printf '%s' "$COMMAND" | scout classify-command
//   → {"intercept":true,"escape":false}
//
// The command arrives on **stdin**, never argv: commands contain quotes,
// newlines and heredoc bodies, and stdin sidesteps every quoting hazard.
// Output is one JSON object on stdout, exit 0. Any other exit code, or output
// the hook cannot parse, means "I don't know" — the hook fails open.
//
// This is hook-internal plumbing. It is deliberately NOT exposed as an MCP
// tool: it is not a question the model should be asking, and adding it to the
// tool surface would just cost context.
//
// Why lexing and not a regex (see docs/command-matching.md): the property that
// separates `cd foo && cargo test` (intercept) from a commit message that
// merely mentions `cargo test` (don't) is whether the verb sits in *command
// position* — the head of a simple command the shell will actually execute.
// That is a lexical question about shell structure: it needs quote, heredoc,
// comment and command-substitution state, none of which a regex over the raw
// string can track.
//
// Deliberate non-goals:
//   * Heredoc bodies are pure data, even with an unquoted delimiter where the
//     shell would expand `$(...)` inside them. The symptom that motivated this
//     work was a commit message passed by heredoc; treating bodies as data is
//     the whole point.
//   * Indirection through an interpreter — `bash -c "cargo test"` — is a MISS.
//     The payload is a single data word to the outer shell; chasing it would
//     mean interpreting arbitrary nested languages.

use std::io::Read;

/// The verdict for one command string.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Classification {
    /// A build/test verb appears in command position.
    pub intercept: bool,
    /// A `# raw-output` marker appears in a real shell comment.
    pub escape: bool,
}

/// `scout classify-command` — read a command from stdin, print the verdict.
///
/// Reads bytes and converts lossily rather than requiring UTF-8: a stray
/// invalid byte in a command should still get classified, not fail the hook
/// into its fail-open path.
pub fn run_subcommand() -> anyhow::Result<()> {
    let mut raw = Vec::new();
    std::io::stdin().read_to_end(&mut raw)?;
    let command = String::from_utf8_lossy(&raw);
    let verdict = classify(&command);
    println!("{}", serde_json::json!({ "intercept": verdict.intercept, "escape": verdict.escape }));
    Ok(())
}

/// Classify a raw Bash command string.
pub(crate) fn classify(command: &str) -> Classification {
    let (segments, comments) = Lexer::new(command).run();
    Classification {
        intercept: segments.iter().any(|words| segment_matches(words)),
        escape: comments.iter().any(|c| has_raw_output_marker(c)),
    }
}

// ── Escape marker ─────────────────────────────────────────────────────────────

/// The `ESCAPE_RE` the hook used to run over the raw string — `#[[:space:]]*raw-output`
/// — applied to comment text only, so the marker counts when the shell would
/// actually treat it as a comment and not when it is quoted or heredoc data.
fn has_raw_output_marker(comment: &str) -> bool {
    let bytes: Vec<char> = comment.chars().collect();
    for (i, c) in bytes.iter().enumerate() {
        if *c != '#' {
            continue;
        }
        let mut j = i + 1;
        while j < bytes.len() && bytes[j].is_whitespace() {
            j += 1;
        }
        if bytes[j..].starts_with(&['r', 'a', 'w', '-', 'o', 'u', 't', 'p', 'u', 't']) {
            return true;
        }
    }
    false
}

// ── Verb table ────────────────────────────────────────────────────────────────

/// The intercept table, matched against a segment's head words.
///
/// Verb-level, exactly as the old `BUILD_RE` was: `cargo add` and `go fmt` and
/// `npm install` are not build/test commands and must keep running normally.
/// Kept literally in sync with the hook's header comment.
///
/// Note `python -m pytest` matches `python` only, not `python3` — the same
/// table the old regex encoded. Widening it is a separate decision.
fn matches_verb(words: &[&str]) -> bool {
    let w = |n: usize| words.get(n).copied().unwrap_or("");
    match w(0) {
        "cargo" => matches!(w(1), "build" | "test" | "check" | "clippy"),
        "go" => matches!(w(1), "build" | "test" | "vet"),
        "npx" => w(1) == "tsc",
        "tsc" => w(1).starts_with("--"),
        "npm" => {
            if w(1) == "run" {
                matches!(w(2), "build" | "test")
            } else {
                matches!(w(1), "build" | "test")
            }
        }
        "python" => w(1) == "-m" && w(2) == "pytest",
        "pytest" => true,
        _ => false,
    }
}

// ── Segment head normalization ────────────────────────────────────────────────

/// Does this segment's head, once the noise is stripped, name a build verb?
fn segment_matches(words: &[String]) -> bool {
    matches_verb(&head_words(words))
}

fn is_assignment(word: &str) -> bool {
    let mut chars = word.char_indices();
    match chars.next() {
        Some((_, c)) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    for (i, c) in chars {
        if c == '=' {
            return i > 0;
        }
        if c == '+' {
            // `arr+=(...)`
            return word[i..].starts_with("+=");
        }
        if !c.is_ascii_alphanumeric() && c != '_' {
            return false;
        }
    }
    false
}

/// `>f`, `2>&1`, `</dev/null`, `&>log`, or a bare `>` whose target is the next
/// word. Returns `Some(words_to_skip)`.
fn redirect_span(word: &str) -> Option<usize> {
    let rest = word.trim_start_matches(|c: char| c.is_ascii_digit());
    let rest = rest.strip_prefix('&').unwrap_or(rest);
    if !rest.starts_with('>') && !rest.starts_with('<') {
        return None;
    }
    let target = rest.trim_start_matches(|c| matches!(c, '<' | '>' | '&' | '|'));
    // Operator with no filename glued on → the target is the following word.
    Some(if target.is_empty() { 2 } else { 1 })
}

/// A `timeout`/`sleep`-style duration argument: `60`, `1.5`, `30s`, `2m`.
fn is_duration(word: &str) -> bool {
    let core = word.trim_end_matches(|c| matches!(c, 's' | 'm' | 'h' | 'd'));
    !core.is_empty() && core.chars().all(|c| c.is_ascii_digit() || c == '.')
}

/// Strip everything that stands between the start of a segment and the command
/// the shell will actually run: grouping (`(`, `{`, `!`), redirections,
/// leading `VAR=value` assignments, and a small allowlist of transparent
/// wrappers.  The allowlist is deliberately short — a wrapper that is not on it
/// simply becomes the head and fails to match, which errs toward allowing.
fn head_words(words: &[String]) -> Vec<&str> {
    let mut out: Vec<&str> = words.iter().map(String::as_str).collect();
    let mut i = 0;
    while i < out.len() {
        let word = out[i];

        // Grouping tokens: `(`, `{`, `!`, and anything glued to them.
        let stripped = word.trim_start_matches(|c| matches!(c, '(' | '{' | '!'));
        if stripped != word {
            if stripped.is_empty() {
                i += 1;
            } else {
                out[i] = stripped;
            }
            continue;
        }
        if word == ")" || word == "}" {
            i += 1;
            continue;
        }

        if let Some(span) = redirect_span(word) {
            i += span;
            continue;
        }
        if is_assignment(word) {
            i += 1;
            continue;
        }

        match word {
            "env" => {
                i += 1;
                while i < out.len() && (out[i].starts_with('-') || is_assignment(out[i])) {
                    i += 1;
                }
            }
            "time" => {
                i += 1;
                while i < out.len() && out[i].starts_with('-') {
                    i += 1;
                }
            }
            "timeout" => {
                i += 1;
                while i < out.len() && out[i].starts_with('-') {
                    i += 1;
                }
                if i < out.len() && is_duration(out[i]) {
                    i += 1;
                }
            }
            "nice" => {
                i += 1;
                while i < out.len() && out[i].starts_with('-') {
                    i += 1;
                    // `nice -n 10 cargo test`
                    if i < out.len() && is_duration(out[i]) {
                        i += 1;
                    }
                }
            }
            _ => break,
        }
    }
    out.split_off(i)
}

// ── Lexer ─────────────────────────────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq, Eq)]
enum Quote {
    None,
    Single,
    Double,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum FrameKind {
    /// The command string itself.
    Top,
    /// `$( … )`.
    Subst,
    /// `` ` … ` ``.
    Backtick,
}

/// One command-position context. Command substitutions get their own frame
/// because they reset quoting: `echo "$(cargo test)"` runs `cargo test` even
/// though the `$(` sits inside double quotes.
struct Frame {
    kind: FrameKind,
    quote: Quote,
    /// Nesting of plain `(` inside this frame, so the `)` that closes a `$(`
    /// can be told apart from the one that closes a subshell.
    paren_depth: usize,
    words: Vec<String>,
    word: String,
    in_word: bool,
}

impl Frame {
    fn new(kind: FrameKind) -> Self {
        Frame {
            kind,
            quote: Quote::None,
            paren_depth: 0,
            words: Vec::new(),
            word: String::new(),
            in_word: false,
        }
    }
}

struct Heredoc {
    delim: String,
    /// `<<-`: leading tabs are stripped before comparing the delimiter line.
    strip_tabs: bool,
}

struct Lexer {
    src: Vec<char>,
    i: usize,
    frames: Vec<Frame>,
    /// Command-position words, one Vec per simple command.
    segments: Vec<Vec<String>>,
    /// Real shell comments, `#` included, one per comment.
    comments: Vec<String>,
    /// Heredocs whose bodies start at the next newline, in declaration order.
    heredocs: Vec<Heredoc>,
}

impl Lexer {
    fn new(src: &str) -> Self {
        Lexer {
            src: src.chars().collect(),
            i: 0,
            frames: vec![Frame::new(FrameKind::Top)],
            segments: Vec::new(),
            comments: Vec::new(),
            heredocs: Vec::new(),
        }
    }

    fn run(mut self) -> (Vec<Vec<String>>, Vec<String>) {
        while self.i < self.src.len() {
            let c = self.src[self.i];
            match self.top().quote {
                Quote::Single => self.step_single(c),
                Quote::Double => self.step_double(c),
                Quote::None => self.step_bare(c),
            }
        }
        // Unterminated `$(` / backtick: flush every frame rather than dropping
        // the words in it.
        while self.frames.len() > 1 {
            self.end_segment();
            self.frames.pop();
        }
        self.end_segment();
        (self.segments, self.comments)
    }

    // ── Frame plumbing ────────────────────────────────────────────────────────

    fn top(&mut self) -> &mut Frame {
        self.frames.last_mut().expect("frame stack is never empty")
    }

    fn peek(&self, off: usize) -> Option<char> {
        self.src.get(self.i + off).copied()
    }

    fn prev(&self) -> Option<char> {
        if self.i == 0 {
            None
        } else {
            self.src.get(self.i - 1).copied()
        }
    }

    fn push_char(&mut self, c: char) {
        let f = self.top();
        f.word.push(c);
        f.in_word = true;
    }

    fn end_word(&mut self) {
        let f = self.top();
        if f.in_word {
            let w = std::mem::take(&mut f.word);
            f.words.push(w);
            f.in_word = false;
        }
    }

    /// Emit a standalone token (`(`, `)`) that is a word of its own regardless
    /// of surrounding whitespace.
    fn push_token(&mut self, tok: &str) {
        self.end_word();
        self.top().words.push(tok.to_string());
    }

    fn end_segment(&mut self) {
        self.end_word();
        let words = std::mem::take(&mut self.top().words);
        if !words.is_empty() {
            self.segments.push(words);
        }
    }

    fn push_frame(&mut self, kind: FrameKind) {
        self.frames.push(Frame::new(kind));
    }

    fn pop_frame(&mut self) {
        if self.frames.len() > 1 {
            self.end_segment();
            self.frames.pop();
        }
    }

    /// A backtick opens a command-position context, or closes the one it opened.
    fn toggle_backtick(&mut self) {
        if self.top().kind == FrameKind::Backtick {
            self.pop_frame();
        } else {
            self.push_frame(FrameKind::Backtick);
        }
    }

    // ── Per-state stepping ────────────────────────────────────────────────────

    /// Single quotes: nothing is special, not even a backslash.
    fn step_single(&mut self, c: char) {
        self.i += 1;
        if c == '\'' {
            self.top().quote = Quote::None;
        } else {
            self.push_char(c);
        }
    }

    /// Double quotes: `$(`, backticks and a short escape set stay live.
    fn step_double(&mut self, c: char) {
        match c {
            '\\' => match self.peek(1) {
                Some(x) if matches!(x, '$' | '`' | '"' | '\\') => {
                    self.i += 2;
                    self.push_char(x);
                }
                Some('\n') => self.i += 2,
                _ => {
                    self.i += 1;
                    self.push_char('\\');
                }
            },
            '"' => {
                self.i += 1;
                self.top().quote = Quote::None;
            }
            '`' => {
                self.i += 1;
                self.toggle_backtick();
            }
            '$' if self.peek(1) == Some('(') => {
                self.i += 2;
                self.push_frame(FrameKind::Subst);
            }
            _ => {
                self.i += 1;
                self.push_char(c);
            }
        }
    }

    /// Unquoted: the only state where operators segment and `#` comments.
    fn step_bare(&mut self, c: char) {
        match c {
            '\\' => match self.peek(1) {
                // Line continuation: the newline is not a segment boundary.
                Some('\n') => self.i += 2,
                Some(x) => {
                    self.i += 2;
                    self.push_char(x);
                }
                None => {
                    self.i += 1;
                    self.push_char('\\');
                }
            },
            '\'' | '"' => {
                self.i += 1;
                let f = self.top();
                f.quote = if c == '\'' { Quote::Single } else { Quote::Double };
                // `''` and `""` are real (empty) words.
                f.in_word = true;
            }
            '`' => {
                self.i += 1;
                self.toggle_backtick();
            }
            '$' if self.peek(1) == Some('(') => {
                self.i += 2;
                self.push_frame(FrameKind::Subst);
            }
            // `#` comments only at the start of a word; `echo a#b` does not.
            '#' if !self.top().in_word => self.consume_comment(),
            ' ' | '\t' | '\r' => {
                self.i += 1;
                self.end_word();
            }
            '\n' => {
                self.i += 1;
                self.end_segment();
                self.consume_heredocs();
            }
            ';' => {
                self.i += 1;
                self.end_segment();
            }
            '&' => self.step_amp(),
            '|' => self.step_pipe(),
            '(' => {
                self.i += 1;
                self.top().paren_depth += 1;
                self.push_token("(");
            }
            ')' => self.step_close_paren(),
            // `<<<` here-string: an ordinary redirect whose data is the next
            // word. Consumed whole — eating only the first `<` would leave the
            // remaining `<<` to be misread as a heredoc opener, whose phantom
            // body then swallows everything after the next newline.
            '<' if self.peek(1) == Some('<') && self.peek(2) == Some('<') => {
                self.i += 3;
                self.push_char('<');
                self.push_char('<');
                self.push_char('<');
            }
            '<' if self.peek(1) == Some('<') => self.start_heredoc(),
            _ => {
                self.i += 1;
                self.push_char(c);
            }
        }
    }

    /// `&&` separates; the `&` of `2>&1` and `&>log` does not.
    fn step_amp(&mut self) {
        if self.peek(1) == Some('&') {
            self.i += 2;
            self.end_segment();
        } else if matches!(self.prev(), Some('>') | Some('<')) || self.peek(1) == Some('>') {
            self.i += 1;
            self.push_char('&');
        } else {
            self.i += 1;
            self.end_segment();
        }
    }

    /// `|` and `||` separate; the `|` of `>|file` does not.
    fn step_pipe(&mut self) {
        if self.prev() == Some('>') {
            self.i += 1;
            self.push_char('|');
        } else if self.peek(1) == Some('|') {
            self.i += 2;
            self.end_segment();
        } else {
            self.i += 1;
            self.end_segment();
        }
    }

    fn step_close_paren(&mut self) {
        self.i += 1;
        let closes_subst = {
            let f = self.top();
            f.kind == FrameKind::Subst && f.paren_depth == 0
        };
        if closes_subst {
            self.pop_frame();
        } else {
            let f = self.top();
            if f.paren_depth > 0 {
                f.paren_depth -= 1;
            }
            self.push_token(")");
        }
    }

    fn consume_comment(&mut self) {
        let start = self.i;
        while self.i < self.src.len() && self.src[self.i] != '\n' {
            self.i += 1;
        }
        self.comments.push(self.src[start..self.i].iter().collect());
    }

    /// Queue a `<<` / `<<-` heredoc. `<<<` is a here-string — an ordinary word,
    /// handled by the caller's lookahead.
    fn start_heredoc(&mut self) {
        self.end_word();
        self.i += 2;
        let strip_tabs = self.peek(0) == Some('-');
        if strip_tabs {
            self.i += 1;
        }
        while matches!(self.peek(0), Some(' ') | Some('\t')) {
            self.i += 1;
        }

        // The delimiter may be bare, quoted, or a mix (`<<E"O"F`). Whether it
        // was quoted only changes expansion inside the body, and bodies are
        // data either way, so we keep just the literal text.
        let mut delim = String::new();
        while let Some(c) = self.peek(0) {
            match c {
                '\'' | '"' => {
                    self.i += 1;
                    while let Some(x) = self.peek(0) {
                        self.i += 1;
                        if x == c {
                            break;
                        }
                        delim.push(x);
                    }
                }
                '\\' => {
                    self.i += 1;
                    if let Some(x) = self.peek(0) {
                        delim.push(x);
                        self.i += 1;
                    }
                }
                c if c.is_whitespace() || "|&;<>()#".contains(c) => break,
                c => {
                    delim.push(c);
                    self.i += 1;
                }
            }
        }
        self.heredocs.push(Heredoc { delim, strip_tabs });
    }

    /// Skip every pending heredoc body. Bodies are data: they are never scanned
    /// for verbs or for the escape marker, and their quotes cannot desync the
    /// lexer because nothing in them is lexed at all.
    fn consume_heredocs(&mut self) {
        if self.heredocs.is_empty() {
            return;
        }
        for hd in std::mem::take(&mut self.heredocs) {
            loop {
                if self.i >= self.src.len() {
                    return; // unterminated heredoc; nothing left to classify
                }
                let start = self.i;
                while self.i < self.src.len() && self.src[self.i] != '\n' {
                    self.i += 1;
                }
                let line: String = self.src[start..self.i].iter().collect();
                if self.i < self.src.len() {
                    self.i += 1;
                }
                let cmp = if hd.strip_tabs { line.trim_start_matches('\t') } else { line.as_str() };
                if cmp == hd.delim {
                    break;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests;
