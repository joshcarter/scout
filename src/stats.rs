// Call log: write path (used by every LLM-calling command) and the `scout
// stats` report (read path), in one module.
//
// One JSONL line per LLM round-trip, appended by every scout process — there
// is no daemon, so the log is the only thing every entry point shares
// (docs/dashboard.md §1).  The record is `v: 2` (§3): everything past the
// original six fields is optional, so a v1 line still parses, and `ts` is the
// one non-additive change — it is a float now, and readers must take both.

use serde_json::{Map, Value};
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::{Instant, SystemTime};

/// Path to the call log.
///
/// Resolution order:
///   1. `$SCOUT_CALLS_LOG` env var (tests and custom layouts)
///   2. `$XDG_STATE_HOME/scout/calls.jsonl`
///   3. `~/.local/state/scout/calls.jsonl`
pub fn log_path() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("SCOUT_CALLS_LOG") {
        return Some(PathBuf::from(p));
    }
    let state_dir = std::env::var("XDG_STATE_HOME")
        .ok()
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var("HOME")
                .ok()
                .map(|h| PathBuf::from(h).join(".local").join("state"))
        })?;
    Some(state_dir.join("scout").join("calls.jsonl"))
}

// ── Identity ────────────────────────────────────────────────────────────────

/// This process's `run` id, minted once.
///
/// A pid is unique among *live* processes and the millisecond separates the
/// reuses, which is all this has to do: it names the process every row of the
/// log was written by.  No uuid dependency, and no way for two concurrent
/// scout processes to collide.
///
/// Deliberately *not* the grouping key for a user-facing operation — `scout
/// mcp` is one process for a whole agent session (docs/dashboard.md §1), so
/// a whole session shares one `run`.  That is `op`; see `Ledger`.
pub fn run_id() -> &'static str {
    static RUN: OnceLock<String> = OnceLock::new();
    RUN.get_or_init(|| {
        let ms = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        format!("{ms:x}-{}", std::process::id())
    })
}

/// `run` plus a per-process counter: monotonic within a run, so a `find`'s
/// rounds sort into the order they were actually made even when two land in
/// the same millisecond.
///
/// The one source of ids in this module — a row's `id` and an operation's `op`
/// both come from here, so no two of either can collide inside a process.
fn next_id() -> String {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    format!("{}-{}", run_id(), SEQ.fetch_add(1, Ordering::Relaxed) + 1)
}

// ── The record ──────────────────────────────────────────────────────────────

/// How the call was reached.
///
/// Set at the entry point — the MCP server, the CLI dispatcher, `run_cmd` —
/// and never derived inside the logger, so it cannot drift away from the
/// process that actually made the call.
pub const VIA_MCP: &str = "mcp";
pub const VIA_CLI: &str = "cli";
pub const VIA_RUN: &str = "run";
pub const VIA_HOOK: &str = "hook";

const KNOWN_VIA: &[&str] = &[VIA_MCP, VIA_CLI, VIA_RUN, VIA_HOOK];

/// `$SCOUT_VIA`, when it names one of the four known values, else `default`.
///
/// `scout run` is reached both from a shell and from `hooks/shell-safety.sh`,
/// and only the caller knows which — so the caller says so.  Unknown values
/// are ignored rather than logged: an open-ended field would make `via` a
/// dumping ground the dashboard cannot group on.
pub fn via_from_env(default: &str) -> String {
    match std::env::var("SCOUT_VIA") {
        Ok(v) if KNOWN_VIA.contains(&v.as_str()) => v,
        _ => default.to_string(),
    }
}

/// What became of one round-trip.
///
/// This is the field that replaces `ok:false`, under which endpoint-down,
/// timeout, empty reply and unparseable JSON were indistinguishable —
/// endpoint-down being both the most common real failure and the one worth
/// telling apart from the rest.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// The model answered and scout used the answer.
    Ok,
    /// scout served the request itself — no model involved (`bypass_max_lines`,
    /// `bypass_max_hits`, a grep with no intent).
    Bypassed,
    /// The model read the candidates and judged none of them relevant — a
    /// verdict, not a failure.
    NoneRelevant,
    EmptyResponse,
    ParseFailure,
    EndpointUnreachable,
    /// The *model* did not answer in time.  Strictly an LLM round-trip failure
    /// (`client.rs`); see `SubprocessTimeout` for the other kind.
    Timeout,
    HttpError,
    /// `check_output`'s command was killed for exceeding a deadline — either it
    /// went silent long enough to look wedged or it blew the outer wall-clock
    /// cap.  No model was called, so this is not `Timeout`: nothing failed on
    /// the LLM side, and rolling the two together hid the one failure mode the
    /// tool most needs to make visible ("the build hung") inside a bucket that
    /// means "the endpoint was slow".
    SubprocessTimeout,
}

impl Outcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Outcome::Ok => "ok",
            Outcome::Bypassed => "bypassed",
            Outcome::NoneRelevant => "none_relevant",
            Outcome::EmptyResponse => "empty_response",
            Outcome::ParseFailure => "parse_failure",
            Outcome::EndpointUnreachable => "endpoint_unreachable",
            Outcome::Timeout => "timeout",
            Outcome::HttpError => "http_error",
            Outcome::SubprocessTimeout => "subprocess_timeout",
        }
    }

    /// The v1 `ok` boolean, still written so a reader that predates `outcome`
    /// keeps working: did scout answer the caller?  A bypass and a
    /// "none relevant" verdict both did.
    ///
    /// The single home of that rule.  `live::apply_end` used to carry its own
    /// copy of the list, which is a bug waiting for the next variant — it
    /// parses the kind back into an `Outcome` and calls this instead.
    pub fn is_ok(self) -> bool {
        matches!(self, Outcome::Ok | Outcome::Bypassed | Outcome::NoneRelevant)
    }

    /// Every variant.  Exists so a test can sweep all of them rather than
    /// restate a list that would drift the same way `is_ok`'s did; see
    /// `all_lists_every_outcome`, which is what keeps this honest.
    pub const ALL: &'static [Outcome] = &[
        Outcome::Ok,
        Outcome::Bypassed,
        Outcome::NoneRelevant,
        Outcome::EmptyResponse,
        Outcome::ParseFailure,
        Outcome::EndpointUnreachable,
        Outcome::Timeout,
        Outcome::HttpError,
        Outcome::SubprocessTimeout,
    ];
}

/// The inverse of `as_str`, for the one reader that gets an outcome back as a
/// string: the live channel puts `as_str` on the wire, and the daemon has to
/// return to the value to ask it anything.
///
/// `Err(())` for a string no `Outcome` produced — `live::ABANDONED`, or a kind
/// from some future build.  There is nothing to say about such a value beyond
/// "not one of ours", so the error carries nothing.
impl std::str::FromStr for Outcome {
    type Err = ();

    fn from_str(s: &str) -> Result<Outcome, ()> {
        Outcome::ALL.iter().copied().find(|o| o.as_str() == s).ok_or(())
    }
}

/// Cap for any string inside `input`.  A `check_output` command can carry a
/// whole shell pipeline and a `task` prompt is unbounded; neither belongs in a
/// log line at full length.
const MAX_INPUT_CHARS: usize = 300;

/// One string field of `input`, trimmed and capped with an explicit elision
/// marker so a truncated value can never read as the whole value.
fn clip(s: &str) -> Value {
    let s = s.trim();
    if s.chars().count() <= MAX_INPUT_CHARS {
        return Value::String(s.to_string());
    }
    let head: String = s.chars().take(MAX_INPUT_CHARS).collect();
    Value::String(format!("{head}…"))
}

/// The `input` object for one round-trip: a few named fields per preset, never
/// a blob (§3).  The preset is the key rather than the tool because the record
/// already says which template was sent, and `input` describes *that* call —
/// a `find`'s rerank round is a grep call, and its pattern is the interesting
/// thing about it.
///
/// An unknown preset (a user override, `quality_review`) falls back to
/// whichever of the common argument names it happens to carry.
pub fn input_summary(preset: &str, args: &Value) -> Value {
    let mut m = Map::new();
    let put_str = |m: &mut Map<String, Value>, out: &str, key: &str| {
        if let Some(s) = args.get(key).and_then(Value::as_str).filter(|s| !s.trim().is_empty()) {
            m.insert(out.to_string(), clip(s));
        }
    };
    let put_num = |m: &mut Map<String, Value>, out: &str, key: &str| {
        if let Some(n) = args.get(key).and_then(Value::as_u64) {
            m.insert(out.to_string(), Value::from(n));
        }
    };

    match preset {
        "check_output" | "shell_safety" => put_str(&mut m, "command", "command"),
        "extract" => {
            put_str(&mut m, "file", "file");
            put_str(&mut m, "question", "question");
            put_num(&mut m, "lines", "file_lines");
        }
        "grep" => {
            put_str(&mut m, "pattern", "pattern");
            put_str(&mut m, "intent", "intent");
            put_num(&mut m, "hits_scanned", "hits_considered");
        }
        "find" | "find_patterns" | "find_reflect" => put_str(&mut m, "question", "question"),
        "task" => put_str(&mut m, "prompt", "prompt"),
        _ => {
            for key in ["command", "question", "pattern", "prompt"] {
                put_str(&mut m, key, key);
            }
        }
    }
    Value::Object(m)
}

/// One call-log row, built up as the call proceeds and written once.
///
/// Everything but `tool`/`preset` is optional: a record with nothing else set
/// still writes a line a v1 reader understands.
#[derive(Debug, Clone)]
pub struct CallRecord {
    pub tool: String,
    pub preset: String,
    /// Minted at construction, not at write time: `call.start` has to carry
    /// the same `id` the log line will, or the daemon cannot reconcile the
    /// two arrivals (docs/dashboard.md P3).
    pub id: String,
    /// The user-facing operation this row belongs to — the grouping key the
    /// dashboard reads.  A record built on its own is its own operation; a
    /// record parked with a `Ledger` takes the ledger's.
    pub op: String,
    pub via: String,
    pub attempt: u64,
    pub project: Option<String>,
    pub model: Option<String>,
    pub endpoint: Option<String>,
    pub input: Value,
    pub outcome: Outcome,
    pub summary: Option<String>,
    pub raw_bytes: Option<u64>,
    pub returned_bytes: Option<u64>,
    pub tokens_in: u64,
    pub tokens_out: u64,
    pub ms: u64,
    /// Set when the record is built from a silent ledger so the live channel
    /// does not fire from unit tests into a developer dashboard.
    pub(crate) silent: bool,
}

impl CallRecord {
    /// A record for `tool`'s round-trip through `preset`.  `tool` is the
    /// user-facing operation (`find`), `preset` the template actually sent
    /// (`find_patterns`) — they differ for exactly the reason the dashboard
    /// needs both.
    pub fn new(tool: &str, preset: &str) -> Self {
        CallRecord {
            tool: tool.to_string(),
            preset: preset.to_string(),
            id: next_id(),
            op: next_id(),
            via: VIA_CLI.to_string(),
            attempt: 1,
            project: None,
            model: None,
            endpoint: None,
            input: Value::Object(Map::new()),
            outcome: Outcome::Ok,
            summary: None,
            raw_bytes: None,
            returned_bytes: None,
            tokens_in: 0,
            tokens_out: 0,
            ms: 0,
            silent: false,
        }
    }

    pub fn via(mut self, via: &str) -> Self {
        self.via = via.to_string();
        self
    }

    /// `find`'s round counter; 1 everywhere else.
    pub fn attempt(mut self, attempt: u64) -> Self {
        self.attempt = attempt.max(1);
        self
    }

    pub fn project(mut self, project: &str) -> Self {
        if !project.is_empty() {
            self.project = Some(project.to_string());
        }
        self
    }

    pub fn endpoint(mut self, model: &str, endpoint: &str) -> Self {
        self.model = Some(model.to_string());
        self.endpoint = Some(endpoint.to_string());
        self
    }

    pub fn input(mut self, input: Value) -> Self {
        self.input = input;
        self
    }

    pub fn outcome(mut self, outcome: Outcome) -> Self {
        self.outcome = outcome;
        self
    }

    /// A one-line note about the outcome — the model's verdict, or the error
    /// as the caller saw it.  Capped like `input`.
    pub fn summary(mut self, summary: impl AsRef<str>) -> Self {
        let s = summary.as_ref().trim();
        if !s.is_empty() {
            self.summary = clip(s).as_str().map(str::to_string);
        }
        self
    }

    /// Token counts from an OpenAI-shaped `usage` object.
    pub fn usage(mut self, usage: &Value) -> Self {
        self.tokens_in = usage["prompt_tokens"].as_u64().unwrap_or(0);
        self.tokens_out = usage["completion_tokens"].as_u64().unwrap_or(0);
        self
    }

    pub fn ms(mut self, ms: u64) -> Self {
        self.ms = ms;
        self
    }

    /// What scout digested on the caller's behalf: captured build output, file
    /// bytes read, the pre-rerank hit list.
    pub fn raw_bytes(mut self, bytes: u64) -> Self {
        self.raw_bytes = Some(bytes);
        self
    }

    /// What scout handed back — the serialized payload.  With `raw_bytes`,
    /// this is the context-saved metric.
    pub fn returned_bytes(mut self, bytes: u64) -> Self {
        self.returned_bytes = Some(bytes);
        self
    }

    /// Serialize to the on-disk shape.  Absent fields are omitted rather than
    /// written as null: the log is read far more often than it is written, and
    /// an absent field is unambiguous.
    pub fn to_json(&self) -> Value {
        let ts = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.as_secs_f64())
            .unwrap_or(0.0);
        let mut m = Map::new();
        m.insert("v".into(), Value::from(2));
        m.insert("id".into(), Value::from(self.id.clone()));
        m.insert("run".into(), Value::from(run_id()));
        m.insert("op".into(), Value::from(self.op.clone()));
        m.insert("ts".into(), Value::from(ts));
        m.insert("via".into(), Value::from(self.via.clone()));
        m.insert("tool".into(), Value::from(self.tool.clone()));
        m.insert("preset".into(), Value::from(self.preset.clone()));
        m.insert("attempt".into(), Value::from(self.attempt));
        for (key, value) in [
            ("project", &self.project),
            ("model", &self.model),
            ("endpoint", &self.endpoint),
        ] {
            if let Some(v) = value {
                m.insert(key.into(), Value::from(v.clone()));
            }
        }
        if self.input.as_object().is_some_and(|o| !o.is_empty()) {
            m.insert("input".into(), self.input.clone());
        }
        let mut outcome = Map::new();
        outcome.insert("kind".into(), Value::from(self.outcome.as_str()));
        if let Some(s) = &self.summary {
            outcome.insert("summary".into(), Value::from(s.clone()));
        }
        m.insert("outcome".into(), Value::Object(outcome));
        for (key, value) in [("raw_bytes", self.raw_bytes), ("returned_bytes", self.returned_bytes)] {
            if let Some(n) = value {
                m.insert(key.into(), Value::from(n));
            }
        }
        m.insert("tokens_in".into(), Value::from(self.tokens_in));
        m.insert("tokens_out".into(), Value::from(self.tokens_out));
        m.insert("ms".into(), Value::from(self.ms));
        m.insert("ok".into(), Value::from(self.outcome.is_ok()));
        Value::Object(m)
    }

    /// Append this record to the call log.  Silently ignored on any I/O error
    /// (including a missing parent dir) so a full disk or unset $HOME never
    /// breaks a scout command.
    pub fn log(self) {
        if let Some(path) = log_path() {
            append_line(&path, &self.to_json().to_string());
        }
    }
}

// ── The ledger: what an operation only knows at its ends ────────────────────

/// Identity, byte accounting and end-of-operation verdicts, held across an
/// operation's round-trips.
///
/// A ledger *is* the operation: one is constructed per MCP dispatch and per CLI
/// invocation, which is precisely the span a human calls one `scout find` or
/// one `grep`.  So it mints the `op` id every row it parks carries, and the
/// dashboard groups on ground truth rather than guessing from timestamps.
/// `run` cannot do this job — `scout mcp` is one process, and one `run`, for a
/// whole agent session (docs/dashboard.md §1).
///
/// The log's unit is one LLM call, but two of §3's fields are not: `raw_bytes`
/// is known before the *first* call of an operation and `returned_bytes` only
/// after the *last* one, and `none_relevant` is a verdict the rerank reaches
/// after its final batch.  The ledger holds that gap open — a filter deposits
/// the raw size up front, `call_preset` attaches it to the first row it
/// writes and parks the newest row here, and the entry point stamps the
/// payload size onto that parked row as it flushes it.
///
/// Attributing each number to exactly one row is the point: a three-chunk
/// `extract` that counted its file three times would inflate the one metric
/// the whole thing exists to report.
pub struct Ledger {
    op: String,
    started: Instant,
    raw: Cell<Option<u64>>,
    pending: RefCell<Option<CallRecord>>,
    silent: bool,
}

impl Default for Ledger {
    fn default() -> Self {
        Ledger {
            op: next_id(),
            started: Instant::now(),
            raw: Cell::new(None),
            pending: RefCell::new(None),
            silent: false,
        }
    }
}

impl std::fmt::Debug for Ledger {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Ledger")
    }
}

impl Ledger {
    /// A ledger that records nothing.
    ///
    /// For test fixtures: they build a real `Ctx` and run the real filters, and
    /// the real filters log — so without this, every test run would append rows
    /// to the developer's own `calls.jsonl`.  Deliberately not the `Default`,
    /// which is what the entry points get — and `cfg(test)`, so no production
    /// path can reach for it.
    #[cfg(test)]
    pub fn silent() -> Self {
        // Spelled out rather than `..Default::default()`: struct update syntax
        // cannot move fields out of a type that implements `Drop`.
        Ledger {
            op: next_id(),
            started: Instant::now(),
            raw: Cell::new(None),
            pending: RefCell::new(None),
            silent: true,
        }
    }

    fn emit(&self, rec: CallRecord) {
        if !self.silent {
            rec.log();
        }
    }

    /// The operation id every row of this ledger carries.  Exposed so
    /// `call.start` can stamp it before `record` parks the row.
    pub fn op(&self) -> &str {
        &self.op
    }

    pub(crate) fn is_silent(&self) -> bool {
        self.silent
    }

    /// The outcome of the row currently parked.
    ///
    /// Test-only, for the same reason `silent` is: production code has no
    /// business reading a row back out of the ledger, but a test that claims a
    /// path logs `subprocess_timeout` has to be able to prove it — and a silent
    /// ledger writes no file to inspect instead.
    #[cfg(test)]
    pub fn pending_outcome(&self) -> Option<Outcome> {
        self.pending.borrow().as_ref().map(|r| r.outcome)
    }

    /// Milliseconds since the operation began.  This is what a bypassed row
    /// reports: no model ran, but the file read or the search did.
    pub fn elapsed_ms(&self) -> u64 {
        self.started.elapsed().as_millis() as u64
    }

    /// Deposit the bytes scout digested for this operation.  Claimed by the
    /// next record written; a deposit nobody claims is simply dropped.
    pub fn raw_bytes(&self, bytes: u64) {
        self.raw.set(Some(bytes));
    }

    /// Park a record: writes out whatever was parked before it, so only the
    /// newest row is ever waiting.
    ///
    /// Claiming the record for this operation happens here rather than at
    /// `CallRecord::new`, so a row can only be grouped with its siblings by
    /// going through the ledger that actually delimits them.
    pub fn record(&self, mut rec: CallRecord) {
        rec.op = self.op.clone();
        if rec.raw_bytes.is_none() {
            rec.raw_bytes = self.raw.take();
        }
        if let Some(previous) = self.pending.replace(Some(rec)) {
            self.emit(previous);
        }
    }

    /// Stamp the finished operation's payload onto the parked row and write it.
    pub fn finish(&self, payload: &Value) {
        let Some(mut rec) = self.pending.take() else { return };
        if rec.returned_bytes.is_none() {
            let bytes = serde_json::to_string(payload).map(|s| s.len() as u64).unwrap_or(0);
            rec.returned_bytes = Some(bytes);
        }
        // The rerank's verdict is only known once every batch is in, which is
        // after the last of its rows was built.
        if rec.outcome == Outcome::Ok
            && payload.get("none_relevant").and_then(Value::as_bool).unwrap_or(false)
        {
            rec.outcome = Outcome::NoneRelevant;
        }
        self.emit(rec);
    }

    /// The operation failed: write the parked row, naming the failure.
    ///
    /// A row whose own round-trip already failed (endpoint down, timeout)
    /// keeps that kind — it is the more specific answer.  A row that succeeded
    /// under an operation that did not is the "model replied, scout could not
    /// use the reply" case: an unparseable selector, no usable line ranges,
    /// hit ids that were all hallucinated.
    pub fn fail(&self, reason: &str) {
        let Some(mut rec) = self.pending.take() else { return };
        if rec.outcome.is_ok() {
            rec = rec.outcome(Outcome::ParseFailure).summary(reason);
        }
        self.emit(rec);
    }
}

impl Drop for Ledger {
    /// A parked row must reach the log even when the operation ends by a path
    /// that never calls `finish` — an early return, a `?`, a panic unwinding
    /// past the entry point.  Losing the payload size is a missing field;
    /// losing the row is a missing call.
    fn drop(&mut self) {
        if let Some(rec) = self.pending.take() {
            self.emit(rec);
        }
    }
}

// ── Write path ──────────────────────────────────────────────────────────────

/// Rotate the log past this size, keeping one previous generation.
///
/// `shell-safety.sh` fires on every Bash tool call, so this file grows faster
/// than anything else scout writes and `print_report` reads all of it every
/// time.  A v2 record runs ~500 bytes against v1's ~90, so 8 MB is ~17k rows —
/// around seven months at the observed ~80 calls/day, and two generations of
/// that is the whole history a dashboard can show.
const MAX_LOG_BYTES: u64 = 8 * 1024 * 1024;

/// `calls.jsonl` → `calls.jsonl.1`.  Not `with_extension`, which would eat the
/// `.jsonl`.
fn rotated_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(".1");
    PathBuf::from(name)
}

/// Create `dir` with mode `0700` if it does not already exist. Only `dir`
/// itself is tightened — any missing ancestors (`~/.local/state`, say) are
/// created at the process umask, because they are shared system directories
/// scout has no business narrowing. A `dir` that already exists is left
/// alone: `calls.jsonl` can carry full command strings and project paths
/// (§ security note in the module doc), so a freshly-created state dir
/// should not be world-readable, but a mode the user set on purpose —
/// deliberately or not — is not ours to override after the fact.
#[cfg(unix)]
fn ensure_private_dir(dir: &Path) {
    if dir.exists() {
        return;
    }
    if let Some(parent) = dir.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    use std::os::unix::fs::DirBuilderExt;
    let _ = std::fs::DirBuilder::new().mode(0o700).create(dir);
}

#[cfg(not(unix))]
fn ensure_private_dir(dir: &Path) {
    let _ = std::fs::create_dir_all(dir);
}

/// Open `path` for appending, creating it with mode `0600` if it does not
/// already exist. `calls.jsonl` persists full command strings and project
/// paths, so a freshly-created log must not land at the process umask
/// (typically `0644`). `create_new` is what makes this creation-time-only —
/// on the common "already there" path it falls back to a plain append-open
/// that touches no permission bit, matching `ensure_private_dir`'s rule that
/// an existing file's mode is never ours to change.
#[cfg(unix)]
fn open_for_append(path: &Path) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    match std::fs::OpenOptions::new().create_new(true).append(true).mode(0o600).open(path) {
        Ok(f) => Ok(f),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            std::fs::OpenOptions::new().append(true).open(path)
        }
        Err(e) => Err(e),
    }
}

#[cfg(not(unix))]
fn open_for_append(path: &Path) -> std::io::Result<std::fs::File> {
    std::fs::OpenOptions::new().create(true).append(true).open(path)
}

/// Append one line, rotating first if the file is over the cap.
///
/// Fail-open is the whole contract here: every step is best-effort and any
/// failure — unwritable dir, full disk, a lost rotation race — degrades to
/// "no log line" and never reaches the caller.  The rotation check costs one
/// `fstat` on the handle we just opened, which is why it lives in the writer:
/// every entry point writes, and most of them have no dashboard running to do
/// it for them.
///
/// Two writers racing on the rename has two outcomes, and only one is free.
/// If the loser's rename lands between the winner's rename and its reopen,
/// the path is gone, `rename` fails with `ENOENT`, and nothing is lost.  If it
/// lands after the reopen, it renames the winner's *fresh* file over the
/// generation the winner just rotated, and 8 MB of history goes with it.
///
/// Left as-is deliberately: the window is a few microseconds against ~80 calls
/// a day, both writers must also arrive exactly at the cap, and the cost is one
/// lost generation of a diagnostic log — no corruption, no effect on the
/// command. Closing it properly needs an O_EXCL lockfile, which is more
/// machinery than this earns.
fn append_line(path: &Path, line: &str) {
    if let Some(parent) = path.parent() {
        ensure_private_dir(parent);
    }
    let Ok(mut f) = open_for_append(path) else { return };
    if f.metadata().map(|m| m.len() >= MAX_LOG_BYTES).unwrap_or(false) {
        drop(f);
        let _ = std::fs::rename(path, rotated_path(path));
        // The rotated-away name is gone from `path`, so this is a fresh
        // creation too — `open_for_append` gives the reopened file the same
        // `0600` the original got, rather than whatever the rotation's
        // `rename`+reopen would land at under the umask.
        let Ok(reopened) = open_for_append(path) else { return };
        f = reopened;
    }
    // One write_all, not writeln!.  `writeln!` reaches the fd twice — once for
    // the record, once for the newline — and O_APPEND only makes each of those
    // atomic on its own, not the pair.  Two writers interleaving between them
    // produce `{a}{b}\n\n`: one line serde_json rejects outright and one blank,
    // so a collision destroys both records rather than neither.
    //
    // This is not a rare race.  shell-safety.sh runs `scout run` on every Bash
    // command containing an expansion, while `scout mcp` dispatches tool calls
    // on spawn_blocking, so two parallel calls are two threads here on their
    // own fds.  It was invisible by construction: the wreckage lands in the
    // parse_errors counter, reported as a parenthetical nobody reads, which
    // means the context-saved numbers under-report by an unknowable amount.
    let _ = f.write_all(format!("{line}\n").as_bytes());
}

// ── `scout stats` report ────────────────────────────────────────────────────

struct PresetStats {
    calls: u64,
    ok: u64,
    tokens_in: u64,
    tokens_out: u64,
    // Only summed over successful calls so avg_ms(ok) isn't diluted by
    // error paths that log ms=0.
    ok_total_ms: u64,
}

struct Report {
    rows: Vec<(String, PresetStats)>,
    parse_errors: u64,
    /// Rows scout served with no model at all.  Deliberately kept out of the
    /// per-preset table: that table counts LLM round-trips, and a bypass is
    /// the absence of one.
    bypassed: u64,
    raw_bytes: u64,
    returned_bytes: u64,
    /// Failed rows by `outcome.kind`, most frequent first.  A v1 row that
    /// failed has no kind and counts as `unknown`.
    failures: Vec<(String, u64)>,
    span_secs: f64,
}

/// One record's timestamp, in seconds.
///
/// v1 wrote an integer, v2 writes a float; `as_f64` takes both, where the
/// `as_u64` this replaced would silently return 0 for every v2 row.
fn record_ts(v: &Value) -> Option<f64> {
    v.get("ts").and_then(Value::as_f64).filter(|t| *t > 0.0)
}

#[derive(Default)]
struct Accumulator {
    by_preset: HashMap<String, PresetStats>,
    by_failure: HashMap<String, u64>,
    parse_errors: u64,
    bypassed: u64,
    raw_bytes: u64,
    returned_bytes: u64,
    first_ts: Option<f64>,
    last_ts: Option<f64>,
}

/// Fold one log file into the accumulator.  A missing file is not an error —
/// `calls.jsonl.1` only exists after the first rotation.
fn fold_file(path: &Path, acc: &mut Accumulator) -> std::io::Result<()> {
    let f = std::fs::File::open(path)?;
    for line in BufReader::new(f).lines().map_while(Result::ok) {
        if line.trim().is_empty() {
            continue;
        }
        let v: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => {
                acc.parse_errors += 1;
                continue;
            }
        };

        if let Some(ts) = record_ts(&v) {
            acc.first_ts = Some(acc.first_ts.map_or(ts, |f: f64| f.min(ts)));
            acc.last_ts = Some(acc.last_ts.map_or(ts, |l: f64| l.max(ts)));
        }
        acc.raw_bytes += v["raw_bytes"].as_u64().unwrap_or(0);
        acc.returned_bytes += v["returned_bytes"].as_u64().unwrap_or(0);

        let kind = v["outcome"]["kind"].as_str();
        if kind == Some(Outcome::Bypassed.as_str()) {
            acc.bypassed += 1;
            continue;
        }

        let call_ok = v["ok"].as_bool().unwrap_or(false);
        if !call_ok {
            *acc.by_failure.entry(kind.unwrap_or("unknown").to_string()).or_insert(0) += 1;
        }

        let preset = v["preset"].as_str().unwrap_or("unknown").to_string();
        let entry = acc.by_preset.entry(preset).or_insert(PresetStats {
            calls: 0,
            ok: 0,
            tokens_in: 0,
            tokens_out: 0,
            ok_total_ms: 0,
        });
        entry.calls += 1;
        if call_ok {
            entry.ok += 1;
            entry.ok_total_ms += v["ms"].as_u64().unwrap_or(0);
        }
        entry.tokens_in += v["tokens_in"].as_u64().unwrap_or(0);
        entry.tokens_out += v["tokens_out"].as_u64().unwrap_or(0);
    }
    Ok(())
}

/// Read the whole history: the rotated generation first, then the live file,
/// so the report spans a rotation instead of restarting at it.
fn parse_log(path: &Path) -> std::io::Result<Report> {
    let mut acc = Accumulator::default();
    let _ = fold_file(&rotated_path(path), &mut acc);
    fold_file(path, &mut acc)?;

    let mut rows: Vec<(String, PresetStats)> = acc.by_preset.into_iter().collect();
    rows.sort_by_key(|r| std::cmp::Reverse(r.1.calls));
    let mut failures: Vec<(String, u64)> = acc.by_failure.into_iter().collect();
    failures.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));

    Ok(Report {
        rows,
        parse_errors: acc.parse_errors,
        bypassed: acc.bypassed,
        raw_bytes: acc.raw_bytes,
        returned_bytes: acc.returned_bytes,
        failures,
        span_secs: match (acc.first_ts, acc.last_ts) {
            (Some(f), Some(l)) => l - f,
            _ => 0.0,
        },
    })
}

/// Byte counts a human reads at a glance — the context-saved line is the whole
/// point of the report and `18874368 → 219136` is not a number anyone parses.
fn human_bytes(n: u64) -> String {
    const UNITS: [(u64, &str); 3] = [(1 << 30, "GB"), (1 << 20, "MB"), (1 << 10, "KB")];
    for (scale, unit) in UNITS {
        if n >= scale {
            return format!("{:.1} {unit}", n as f64 / scale as f64);
        }
    }
    format!("{n} B")
}

/// Print the `scout stats` report: per-preset call counts, pass rate, token
/// totals, and average latency of successful calls, plus an overall total —
/// then the three things the record now carries that the table cannot show:
/// context saved, calls served without the model, and failures by kind.
pub fn print_report() -> anyhow::Result<()> {
    let path = match log_path() {
        Some(p) => p,
        None => {
            println!("scout stats: $HOME not set and $SCOUT_CALLS_LOG not set; no log to read");
            return Ok(());
        }
    };

    if !path.exists() {
        println!("No calls recorded yet.");
        println!("Run `scout run --preset <name> ...` or `scout task \"...\"` to populate data.");
        return Ok(());
    }

    let report = parse_log(&path)?;

    if report.rows.is_empty() && report.bypassed == 0 {
        println!("No calls recorded yet.");
        return Ok(());
    }

    // A log of nothing but bypasses is a real state — `scout grep` with no
    // intent never calls a model — and an empty table with a 0.0% TOTAL under
    // it would read as a failure rather than as an absence.
    if report.rows.is_empty() {
        println!("No model calls recorded yet.");
        print_summary(&report);
        return Ok(());
    }

    println!(
        "{:<25} {:>6} {:>7} {:>10} {:>10} {:>11}",
        "preset", "calls", "pass%", "tok_in", "tok_out", "avg_ms(ok)"
    );
    println!("{}", "-".repeat(75));
    for (name, s) in &report.rows {
        let pass_pct = if s.calls > 0 {
            s.ok as f64 / s.calls as f64 * 100.0
        } else {
            0.0
        };
        let avg_ms = match s.ok_total_ms.checked_div(s.ok) {
            Some(v) => v.to_string(),
            None => "-".to_string(),
        };
        println!(
            "{:<25} {:>6} {:>6.1}% {:>10} {:>10} {:>11}",
            name, s.calls, pass_pct, s.tokens_in, s.tokens_out, avg_ms
        );
    }

    let total_calls: u64 = report.rows.iter().map(|(_, s)| s.calls).sum();
    let total_ok: u64 = report.rows.iter().map(|(_, s)| s.ok).sum();
    let overall_pass = if total_calls > 0 {
        total_ok as f64 / total_calls as f64 * 100.0
    } else {
        0.0
    };
    println!("{}", "-".repeat(75));
    println!(
        "{:<25} {:>6} {:>6.1}%  (overall)",
        "TOTAL", total_calls, overall_pass
    );

    print_summary(&report);

    if report.parse_errors > 0 {
        eprintln!("  ({} unreadable log lines skipped)", report.parse_errors);
    }

    Ok(())
}

/// The lines below the table: everything the v2 record added.  Each one is
/// skipped when the log has nothing to say about it, so a report over old
/// v1-only history looks exactly as it did before.
fn print_summary(report: &Report) {
    let mut printed_header = false;
    let mut section = |line: String| {
        if !printed_header {
            println!();
            printed_header = true;
        }
        println!("{line}");
    };

    if report.raw_bytes > 0 {
        let ratio = if report.returned_bytes > 0 {
            format!(" ({:.0}×)", report.raw_bytes as f64 / report.returned_bytes as f64)
        } else {
            String::new()
        };
        section(format!(
            "context saved:  {} → {}{ratio}",
            human_bytes(report.raw_bytes),
            human_bytes(report.returned_bytes)
        ));
    }
    if report.bypassed > 0 {
        section(format!(
            "bypassed:       {} call(s) served without the model",
            report.bypassed
        ));
    }
    if !report.failures.is_empty() {
        let parts: Vec<String> =
            report.failures.iter().map(|(kind, n)| format!("{kind} {n}")).collect();
        section(format!("failures:       {}", parts.join(" · ")));
    }
    // Below an hour the span says nothing but "you just started"; the number
    // is only interesting as a denominator for the counts above it.
    if report.span_secs >= 3600.0 {
        let span = if report.span_secs >= 86_400.0 {
            format!("{:.1} days", report.span_secs / 86_400.0)
        } else {
            format!("{:.1} hours", report.span_secs / 3600.0)
        };
        section(format!("history spans:  {span}"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::NamedTempFile;

    // ── log_path / the write path ────────────────────────────────────────

    // Env vars are process-global and tests run in parallel: every test that
    // sets SCOUT_CALLS_LOG — or asserts on a log_path() derived without it —
    // must hold this lock.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn env_lock() -> std::sync::MutexGuard<'static, ()> {
        ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn lines_of(f: &NamedTempFile) -> Vec<Value> {
        BufReader::new(f.reopen().unwrap())
            .lines()
            .map_while(Result::ok)
            .filter(|l| !l.trim().is_empty())
            .map(|l| serde_json::from_str(&l).expect("every written line must be JSON"))
            .collect()
    }

    fn write(f: &NamedTempFile, rec: CallRecord) {
        append_line(f.path(), &rec.to_json().to_string());
    }

    #[test]
    fn log_path_contains_state_dir() {
        let _g = env_lock();
        if let Some(p) = log_path() {
            let s = p.to_string_lossy();
            assert!(s.contains("scout"), "expected 'scout' in path: {s}");
            assert!(s.ends_with("calls.jsonl"));
        }
    }

    #[test]
    fn log_path_honours_scout_calls_log_override() {
        let _g = env_lock();
        std::env::set_var("SCOUT_CALLS_LOG", "/tmp/scout-stats-test.jsonl");
        let p = log_path().unwrap();
        std::env::remove_var("SCOUT_CALLS_LOG");
        assert_eq!(p, PathBuf::from("/tmp/scout-stats-test.jsonl"));
    }

    #[test]
    fn a_record_writes_every_field_the_dashboard_reads() {
        let tmp = NamedTempFile::new().unwrap();
        write(
            &tmp,
            CallRecord::new("find", "find_patterns")
                .via(VIA_CLI)
                .attempt(2)
                .project("/home/josh/Projects/scout")
                .endpoint("qwen3:27b", "http://localhost:11434/v1")
                .input(input_summary("find_patterns", &json!({"question": "where is the port bound"})))
                .outcome(Outcome::Ok)
                .summary("8 patterns, 3 non-degenerate")
                .raw_bytes(184_320)
                .returned_bytes(1180)
                .usage(&json!({"prompt_tokens": 1840, "completion_tokens": 210}))
                .ms(3100),
        );

        let v = &lines_of(&tmp)[0];
        assert_eq!(v["v"], 2);
        assert_eq!(v["via"], "cli");
        assert_eq!(v["tool"], "find");
        assert_eq!(v["preset"], "find_patterns");
        assert_eq!(v["attempt"], 2);
        assert_eq!(v["project"], "/home/josh/Projects/scout");
        assert_eq!(v["model"], "qwen3:27b");
        assert_eq!(v["endpoint"], "http://localhost:11434/v1");
        assert_eq!(v["input"]["question"], "where is the port bound");
        assert_eq!(v["outcome"]["kind"], "ok");
        assert_eq!(v["outcome"]["summary"], "8 patterns, 3 non-degenerate");
        assert_eq!(v["raw_bytes"], 184_320);
        assert_eq!(v["returned_bytes"], 1180);
        assert_eq!(v["tokens_in"], 1840);
        assert_eq!(v["tokens_out"], 210);
        assert_eq!(v["ms"], 3100);
        assert_eq!(v["ok"], true, "the v1 boolean is still written");
        assert!(v["ts"].as_f64().unwrap() > 1_700_000_000.0, "ts is float seconds");
        assert!(v["id"].as_str().unwrap().starts_with(run_id()), "id extends run");
        assert!(v["op"].as_str().unwrap().starts_with(run_id()), "so does op");
    }

    #[test]
    fn a_record_written_on_its_own_is_its_own_operation() {
        let tmp = NamedTempFile::new().unwrap();
        write(&tmp, CallRecord::new("task", "task"));
        write(&tmp, CallRecord::new("task", "task"));
        let rows = lines_of(&tmp);
        assert_eq!(rows[0]["run"], rows[1]["run"], "one process is one run");
        assert_ne!(rows[0]["op"], rows[1]["op"], "...and two operations");
    }

    #[test]
    fn ids_are_monotonic_within_a_run() {
        let tmp = NamedTempFile::new().unwrap();
        write(&tmp, CallRecord::new("grep", "grep"));
        write(&tmp, CallRecord::new("grep", "grep"));
        let rows = lines_of(&tmp);
        assert_eq!(rows[0]["run"], rows[1]["run"], "one process is one run");
        assert_ne!(rows[0]["id"], rows[1]["id"]);
        let seq = |v: &Value| {
            v["id"].as_str().unwrap().rsplit('-').next().unwrap().parse::<u64>().unwrap()
        };
        assert!(seq(&rows[1]) > seq(&rows[0]), "the counter only goes up");
    }

    #[test]
    fn a_records_id_is_stable_across_serialisations() {
        let rec = CallRecord::new("grep", "grep");
        let a = rec.to_json();
        let b = rec.to_json();
        assert_eq!(a["id"], b["id"], "id is minted once, at construction");
        assert_eq!(a["id"], rec.id);
        assert_ne!(a["id"], a["op"], "id and op are distinct next_id() calls");
    }

    #[test]
    fn all_lists_every_outcome() {
        // The compiler is the enforcement, not the loop.  Adding a tenth
        // variant makes this `match` non-exhaustive and the crate stops
        // building here, which is the moment to add it to `ALL` too — and
        // `ALL` is what the sweep tests (`FromStr` below, and
        // `live::apply_end_agrees_with_outcome_is_ok`) iterate.
        for o in Outcome::ALL {
            match o {
                Outcome::Ok
                | Outcome::Bypassed
                | Outcome::NoneRelevant
                | Outcome::EmptyResponse
                | Outcome::ParseFailure
                | Outcome::EndpointUnreachable
                | Outcome::Timeout
                | Outcome::HttpError
                | Outcome::SubprocessTimeout => {}
            }
        }
        assert_eq!(Outcome::ALL.len(), 9);
    }

    #[test]
    fn every_outcome_survives_the_round_trip_through_its_string() {
        // `live` puts `as_str` on the wire and parses it back to ask
        // `is_ok`; a variant that did not round-trip would read as a failure.
        for o in Outcome::ALL {
            assert_eq!(o.as_str().parse::<Outcome>(), Ok(*o), "{} lost", o.as_str());
        }
        assert_eq!("abandoned".parse::<Outcome>(), Err(()), "a daemon-synthesized kind");
        assert_eq!("".parse::<Outcome>(), Err(()));
    }

    #[test]
    fn a_failure_records_its_kind_and_is_not_ok() {
        let tmp = NamedTempFile::new().unwrap();
        write(
            &tmp,
            CallRecord::new("check_output", "check_output")
                .outcome(Outcome::EndpointUnreachable)
                .summary("local LLM endpoint http://localhost:11434/v1 is not responding"),
        );
        let v = &lines_of(&tmp)[0];
        assert_eq!(v["outcome"]["kind"], "endpoint_unreachable");
        assert_eq!(v["ok"], false);
        assert!(v["outcome"]["summary"].as_str().unwrap().contains("not responding"));
    }

    #[test]
    fn a_subprocess_timeout_is_its_own_kind_and_is_not_a_success() {
        // The taxonomy's whole job here: "the build hung" must not land in the
        // same bucket as "the model was slow", and must never read as ok.
        assert_eq!(Outcome::SubprocessTimeout.as_str(), "subprocess_timeout");
        assert_ne!(Outcome::SubprocessTimeout.as_str(), Outcome::Timeout.as_str());
        assert!(!Outcome::SubprocessTimeout.is_ok());

        let tmp = NamedTempFile::new().unwrap();
        write(
            &tmp,
            CallRecord::new("check_output", "check_output")
                .outcome(Outcome::SubprocessTimeout)
                .summary("the command printed nothing for 120s and was killed after 121s"),
        );
        let v = &lines_of(&tmp)[0];
        assert_eq!(v["outcome"]["kind"], "subprocess_timeout");
        assert_eq!(v["ok"], false);

        // ...and it reaches the report as a failure of its own kind.
        let r = parse_log(tmp.path()).unwrap();
        assert_eq!(r.failures, vec![("subprocess_timeout".to_string(), 1)]);
    }

    #[test]
    fn a_bypassed_record_round_trips_as_a_success_with_no_tokens() {
        let tmp = NamedTempFile::new().unwrap();
        write(
            &tmp,
            CallRecord::new("extract", "extract")
                .outcome(Outcome::Bypassed)
                .summary("file is small enough to return whole")
                .raw_bytes(4096)
                .returned_bytes(4400)
                .ms(7),
        );
        let v = &lines_of(&tmp)[0];
        assert_eq!(v["outcome"]["kind"], "bypassed");
        assert_eq!(v["ok"], true, "scout answered the caller — no model needed");
        assert_eq!(v["tokens_in"], 0);
        assert_eq!(v["tokens_out"], 0);
        assert_eq!(v["ms"], 7, "a bypass still did real work");

        // ...and it stays out of the per-preset table, which counts LLM calls.
        let r = parse_log(tmp.path()).unwrap();
        assert!(r.rows.is_empty());
        assert_eq!(r.bypassed, 1);
        assert_eq!(r.raw_bytes, 4096);
        assert_eq!(r.returned_bytes, 4400);
    }

    #[test]
    fn absent_fields_are_omitted_rather_than_written_null() {
        let tmp = NamedTempFile::new().unwrap();
        write(&tmp, CallRecord::new("task", "task"));
        let v = &lines_of(&tmp)[0];
        for key in ["project", "model", "endpoint", "input", "raw_bytes", "returned_bytes"] {
            assert!(v.get(key).is_none(), "{key} should be absent, got {v}");
        }
        assert!(v["outcome"].get("summary").is_none());
    }

    #[test]
    fn input_strings_are_capped_with_an_elision_marker() {
        let long = "x".repeat(1000);
        let input = input_summary("check_output", &json!({"command": long}));
        let command = input["command"].as_str().unwrap();
        assert_eq!(command.chars().count(), MAX_INPUT_CHARS + 1);
        assert!(command.ends_with('…'), "truncation must be visible: {command}");
    }

    #[test]
    fn input_summary_picks_the_fields_that_belong_to_each_preset() {
        let extract = input_summary(
            "extract",
            &json!({"file": "src/a.rs", "question": "where", "file_lines": 900, "output": "huge"}),
        );
        assert_eq!(extract, json!({"file": "src/a.rs", "question": "where", "lines": 900}));

        let grep = input_summary(
            "grep",
            &json!({"pattern": "TcpListener", "intent": "bind sites", "hits_considered": 40}),
        );
        assert_eq!(
            grep,
            json!({"pattern": "TcpListener", "intent": "bind sites", "hits_scanned": 40})
        );

        // An unknown preset still says something useful rather than nothing.
        assert_eq!(
            input_summary("quality_review", &json!({"command": "git diff", "junk": "ignored"})),
            json!({"command": "git diff"})
        );
        // ...and an empty object is what "nothing to say" looks like.
        assert_eq!(input_summary("quality_review", &json!({})), json!({}));
    }

    #[test]
    fn via_from_env_only_accepts_the_known_values() {
        let _g = env_lock();
        std::env::set_var("SCOUT_VIA", "hook");
        assert_eq!(via_from_env(VIA_RUN), "hook");
        std::env::set_var("SCOUT_VIA", "nonsense");
        assert_eq!(via_from_env(VIA_RUN), "run", "an unknown value must not reach the log");
        std::env::remove_var("SCOUT_VIA");
        assert_eq!(via_from_env(VIA_RUN), "run");
    }

    #[test]
    fn logging_to_an_unwritable_path_does_not_panic() {
        let _g = env_lock();
        // Ensure fail-open: an uncreatable log path must not panic. Pointing
        // at /dev/null/... also keeps the test from appending a synthetic row
        // to the developer's real calls.jsonl.
        std::env::set_var("SCOUT_CALLS_LOG", "/dev/null/calls.jsonl");
        CallRecord::new("task", "task").ms(200).log();
        // The ledger's paths must be as fail-open as the direct one.
        let ledger = Ledger::default();
        ledger.raw_bytes(10);
        ledger.record(CallRecord::new("task", "task"));
        ledger.record(CallRecord::new("task", "task"));
        ledger.finish(&json!({"hits": []}));
        ledger.fail("nothing parked, nothing written");
        drop(ledger);
        std::env::remove_var("SCOUT_CALLS_LOG");
    }

    #[test]
    fn rotation_happens_at_the_cap_and_the_report_spans_both_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("calls.jsonl");
        // One oversized generation's worth of real records, cheaply: a padded
        // line per call would take 90k writes.
        let fat = json!({"ts": 1_770_000_000u64, "preset": "grep", "tokens_in": 1,
                         "tokens_out": 1, "ms": 10, "ok": true, "pad": "x".repeat(4096)});
        while std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0) < MAX_LOG_BYTES {
            append_line(&path, &fat.to_string());
        }
        let before = std::fs::metadata(&path).unwrap().len();

        append_line(&path, &json!({"preset": "extract", "ok": true, "ts": 1_770_000_100.5}).to_string());

        assert!(rotated_path(&path).exists(), "the previous generation must be kept");
        assert_eq!(std::fs::metadata(rotated_path(&path)).unwrap().len(), before);
        assert!(
            std::fs::metadata(&path).unwrap().len() < 1024,
            "the live file restarts at the record that triggered the rotation"
        );

        // History spans the rotation: both presets are still in the report.
        let r = parse_log(&path).unwrap();
        let presets: Vec<&str> = r.rows.iter().map(|(n, _)| n.as_str()).collect();
        assert!(presets.contains(&"grep") && presets.contains(&"extract"), "{presets:?}");
    }

    // ── file/dir permissions (security cleanup ahead of going public) ─────

    #[cfg(unix)]
    fn mode_of(path: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[cfg(unix)]
    #[test]
    fn a_freshly_created_log_file_is_0600() {
        let dir = tempfile::tempdir().unwrap();
        // A nested path so the parent state dir also has to be created —
        // exercising both halves of the fix in one call.
        let path = dir.path().join("scout").join("calls.jsonl");
        append_line(&path, &json!({"preset": "grep", "ok": true}).to_string());
        assert_eq!(mode_of(&path), 0o600, "calls.jsonl can carry full command strings");
        assert_eq!(mode_of(path.parent().unwrap()), 0o700, "the state dir must not be group/other readable");
    }

    #[cfg(unix)]
    #[test]
    fn an_existing_log_files_mode_is_never_widened_by_a_later_append() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("calls.jsonl");
        std::fs::write(&path, "").unwrap();
        // Simulate a file that predates this fix, or that the user
        // deliberately loosened — append must not touch its mode either way.
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        append_line(&path, &json!({"preset": "grep", "ok": true}).to_string());
        assert_eq!(mode_of(&path), 0o644, "an existing file's mode is not ours to change");
    }

    #[cfg(unix)]
    #[test]
    fn an_existing_state_dirs_mode_is_never_widened() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let state_dir = dir.path().join("scout");
        std::fs::create_dir(&state_dir).unwrap();
        std::fs::set_permissions(&state_dir, std::fs::Permissions::from_mode(0o775)).unwrap();
        let path = state_dir.join("calls.jsonl");
        append_line(&path, &json!({"preset": "grep", "ok": true}).to_string());
        assert_eq!(mode_of(&state_dir), 0o775, "a pre-existing dir's mode is not ours to change");
    }

    #[cfg(unix)]
    #[test]
    fn the_reopened_file_after_rotation_is_still_0600() {
        // The case flagged as most likely to regress: a rotation that
        // renames-and-reopens must not land the new generation back at the
        // process umask.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("calls.jsonl");
        let fat = json!({"ts": 1_770_000_000u64, "preset": "grep", "ok": true, "pad": "x".repeat(4096)});
        while std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0) < MAX_LOG_BYTES {
            append_line(&path, &fat.to_string());
        }
        assert_eq!(mode_of(&path), 0o600, "pre-rotation generation is 0600");

        append_line(&path, &json!({"preset": "extract", "ok": true}).to_string());
        assert!(rotated_path(&path).exists());
        assert_eq!(mode_of(&path), 0o600, "the reopened post-rotation file must also be 0600");
        assert_eq!(mode_of(&rotated_path(&path)), 0o600, "and the renamed-away generation keeps its mode");
    }

    // ── parse_log / report aggregation ───────────────────────────────────

    fn write_log(lines: &[&str]) -> NamedTempFile {
        let mut f = NamedTempFile::new().unwrap();
        for line in lines {
            writeln!(f, "{line}").unwrap();
        }
        f
    }

    #[test]
    fn empty_file_produces_empty_report() {
        let f = write_log(&[]);
        let r = parse_log(f.path()).unwrap();
        assert!(r.rows.is_empty());
        assert_eq!(r.parse_errors, 0);
    }

    #[test]
    fn malformed_lines_counted_as_parse_errors() {
        let f = write_log(&[
            r#"{"preset":"quality_review","tokens_in":100,"tokens_out":50,"ms":200,"ok":true}"#,
            "not json at all",
            r#"{"preset":"quality_review","tokens_in":10,"tokens_out":5,"ms":100,"ok":false}"#,
        ]);
        let r = parse_log(f.path()).unwrap();
        assert_eq!(r.parse_errors, 1);
        assert_eq!(r.rows.len(), 1);
        assert_eq!(r.rows[0].1.calls, 2);
    }

    #[test]
    fn v1_lines_still_parse_after_the_schema_change() {
        // The exact six-field shape scout wrote before v2, integer ts and all.
        let f = write_log(&[
            r#"{"ts":1770000000,"preset":"grep","tokens_in":1840,"tokens_out":210,"ms":3100,"ok":true}"#,
            r#"{"ts":1770000060,"preset":"grep","tokens_in":0,"tokens_out":0,"ms":0,"ok":false}"#,
        ]);
        let r = parse_log(f.path()).unwrap();
        assert_eq!(r.rows.len(), 1);
        assert_eq!(r.rows[0].1.calls, 2);
        assert_eq!(r.rows[0].1.ok, 1);
        assert_eq!(r.rows[0].1.tokens_in, 1840);
        assert_eq!(r.span_secs, 60.0, "an integer ts must not read as 0");
        // A v1 failure has no kind, and is counted rather than dropped.
        assert_eq!(r.failures, vec![("unknown".to_string(), 1)]);
    }

    #[test]
    fn float_and_integer_timestamps_both_read() {
        let f = write_log(&[
            r#"{"ts":1770000000,"preset":"a","ok":true}"#,
            r#"{"ts":1770000010.482,"preset":"a","ok":true}"#,
        ]);
        let r = parse_log(f.path()).unwrap();
        assert!((r.span_secs - 10.482).abs() < 0.001, "span was {}", r.span_secs);
    }

    #[test]
    fn failures_are_grouped_by_outcome_kind() {
        let f = write_log(&[
            r#"{"preset":"grep","ok":false,"outcome":{"kind":"endpoint_unreachable"}}"#,
            r#"{"preset":"grep","ok":false,"outcome":{"kind":"endpoint_unreachable"}}"#,
            r#"{"preset":"grep","ok":false,"outcome":{"kind":"timeout"}}"#,
            r#"{"preset":"grep","ok":true,"outcome":{"kind":"ok"}}"#,
        ]);
        let r = parse_log(f.path()).unwrap();
        assert_eq!(
            r.failures,
            vec![("endpoint_unreachable".to_string(), 2), ("timeout".to_string(), 1)],
            "most frequent first"
        );
        assert_eq!(r.rows[0].1.calls, 4, "failures still count as calls");
    }

    #[test]
    fn context_saved_sums_across_every_row() {
        let f = write_log(&[
            r#"{"preset":"extract","ok":true,"raw_bytes":100000,"returned_bytes":1000}"#,
            r#"{"preset":"grep","ok":true,"raw_bytes":50000,"returned_bytes":500}"#,
            r#"{"preset":"grep","ok":true}"#,
        ]);
        let r = parse_log(f.path()).unwrap();
        assert_eq!(r.raw_bytes, 150_000);
        assert_eq!(r.returned_bytes, 1500);
    }

    #[test]
    fn single_successful_call_aggregates_correctly() {
        let f = write_log(&[
            r#"{"preset":"quality_review","tokens_in":100,"tokens_out":50,"ms":200,"ok":true}"#,
        ]);
        let r = parse_log(f.path()).unwrap();
        assert_eq!(r.rows.len(), 1);
        let s = &r.rows[0].1;
        assert_eq!(s.calls, 1);
        assert_eq!(s.ok, 1);
        assert_eq!(s.tokens_in, 100);
        assert_eq!(s.tokens_out, 50);
        assert_eq!(s.ok_total_ms, 200);
    }

    #[test]
    fn failed_calls_do_not_count_toward_ok_ms() {
        // Failures log ms=0 — avg_ms(ok) should only cover successful calls.
        let f = write_log(&[
            r#"{"preset":"test_review","tokens_in":200,"tokens_out":80,"ms":500,"ok":true}"#,
            r#"{"preset":"test_review","tokens_in":0,"tokens_out":0,"ms":0,"ok":false}"#,
            r#"{"preset":"test_review","tokens_in":0,"tokens_out":0,"ms":0,"ok":false}"#,
        ]);
        let r = parse_log(f.path()).unwrap();
        let s = &r.rows[0].1;
        assert_eq!(s.calls, 3);
        assert_eq!(s.ok, 1);
        assert_eq!(s.ok_total_ms, 500); // only the 500ms success counts
    }

    #[test]
    fn multiple_presets_sorted_by_calls_descending() {
        let f = write_log(&[
            r#"{"preset":"a","tokens_in":1,"tokens_out":1,"ms":10,"ok":true}"#,
            r#"{"preset":"b","tokens_in":1,"tokens_out":1,"ms":10,"ok":true}"#,
            r#"{"preset":"b","tokens_in":1,"tokens_out":1,"ms":10,"ok":true}"#,
            r#"{"preset":"b","tokens_in":1,"tokens_out":1,"ms":10,"ok":true}"#,
        ]);
        let r = parse_log(f.path()).unwrap();
        assert_eq!(r.rows[0].0, "b"); // 3 calls first
        assert_eq!(r.rows[1].0, "a"); // 1 call second
    }

    // ── Ledger ───────────────────────────────────────────────────────────

    #[test]
    fn the_ledger_attributes_raw_bytes_to_one_row_and_payload_size_to_the_last() {
        let _g = env_lock();
        let tmp = NamedTempFile::new().unwrap();
        std::env::set_var("SCOUT_CALLS_LOG", tmp.path());

        let payload = json!({"mode": "extract", "snippets": []});
        let ledger = Ledger::default();
        ledger.raw_bytes(100_000);
        ledger.record(CallRecord::new("extract", "extract").ms(1));
        ledger.record(CallRecord::new("extract", "extract").ms(2));
        ledger.finish(&payload);
        drop(ledger);
        std::env::remove_var("SCOUT_CALLS_LOG");

        let rows = lines_of(&tmp);
        assert_eq!(rows.len(), 2, "every round-trip is a row");
        assert_eq!(rows[0]["raw_bytes"], 100_000, "the first row claims the input");
        assert!(rows[1].get("raw_bytes").is_none(), "and no other row counts it again");
        assert!(rows[0].get("returned_bytes").is_none());
        assert_eq!(
            rows[1]["returned_bytes"],
            payload.to_string().len(),
            "the last row carries the payload size"
        );
    }

    #[test]
    fn two_ledgers_of_one_process_stamp_two_operations() {
        // The case the dashboard exists for: `scout mcp` is one process for a
        // whole Claude Code session, so `run` is shared and `op` is not.
        let _g = env_lock();
        let tmp = NamedTempFile::new().unwrap();
        std::env::set_var("SCOUT_CALLS_LOG", tmp.path());

        for _ in 0..2 {
            let ledger = Ledger::default();
            ledger.record(CallRecord::new("grep", "grep"));
            ledger.record(CallRecord::new("grep", "grep"));
            ledger.finish(&json!({"hits": []}));
        }
        std::env::remove_var("SCOUT_CALLS_LOG");

        let rows = lines_of(&tmp);
        assert_eq!(rows.len(), 4);
        assert_eq!(rows[0]["op"], rows[1]["op"], "one dispatch is one operation");
        assert_eq!(rows[2]["op"], rows[3]["op"]);
        assert_ne!(rows[1]["op"], rows[2]["op"], "the next dispatch is another");
        assert_eq!(rows[0]["run"], rows[3]["run"], "...all in the one process");
    }

    #[test]
    fn the_ledger_promotes_a_none_relevant_verdict_onto_the_last_row() {
        let _g = env_lock();
        let tmp = NamedTempFile::new().unwrap();
        std::env::set_var("SCOUT_CALLS_LOG", tmp.path());
        let ledger = Ledger::default();
        ledger.record(CallRecord::new("grep", "grep"));
        ledger.finish(&json!({"mode": "rerank", "none_relevant": true, "hits": []}));
        std::env::remove_var("SCOUT_CALLS_LOG");

        let v = &lines_of(&tmp)[0];
        assert_eq!(v["outcome"]["kind"], "none_relevant");
        assert_eq!(v["ok"], true, "a verdict is not a failure");
    }

    #[test]
    fn a_failed_operation_names_the_failure_on_the_row_that_replied() {
        let _g = env_lock();
        let tmp = NamedTempFile::new().unwrap();
        std::env::set_var("SCOUT_CALLS_LOG", tmp.path());
        let ledger = Ledger::default();
        // The round-trip itself succeeded; the reply was unusable.
        ledger.record(CallRecord::new("extract", "extract").ms(900));
        ledger.fail("scout extract: local LLM returned unparsable output");

        // A round-trip that failed on its own keeps the more specific kind.
        let ledger2 = Ledger::default();
        ledger2.record(CallRecord::new("extract", "extract").outcome(Outcome::EndpointUnreachable));
        ledger2.fail("scout extract: local LLM call failed");
        std::env::remove_var("SCOUT_CALLS_LOG");

        let rows = lines_of(&tmp);
        assert_eq!(rows[0]["outcome"]["kind"], "parse_failure");
        assert_eq!(rows[0]["ok"], false);
        assert!(rows[0]["outcome"]["summary"].as_str().unwrap().contains("unparsable"));
        assert_eq!(rows[1]["outcome"]["kind"], "endpoint_unreachable");
    }

    #[test]
    fn a_dropped_ledger_still_writes_its_parked_row() {
        let _g = env_lock();
        let tmp = NamedTempFile::new().unwrap();
        std::env::set_var("SCOUT_CALLS_LOG", tmp.path());
        {
            let ledger = Ledger::default();
            ledger.record(CallRecord::new("grep", "grep").outcome(Outcome::Timeout));
            // No finish(): the operation failed and returned early.
        }
        std::env::remove_var("SCOUT_CALLS_LOG");

        let rows = lines_of(&tmp);
        assert_eq!(rows.len(), 1, "losing the payload size must not lose the call");
        assert_eq!(rows[0]["outcome"]["kind"], "timeout");
    }

    #[test]
    fn human_bytes_reads_like_a_size() {
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(2048), "2.0 KB");
        assert_eq!(human_bytes(18 * (1 << 20)), "18.0 MB");
    }
}
