// The dashboard: a detached daemon that tails the call log and serves it over
// loopback HTTP (docs/dashboard.md §4–§5).
//
// scout has no long-lived process — every entry point is a short-lived
// invocation — so there is no in-process state to serve.  The one thing every
// path touches is `calls.jsonl`, which makes the log the API and this module a
// reader: it holds the parsed records in memory and touches the file only for
// the tail delta.  A pleasant consequence is that the view works retroactively
// over calls made before it was started, and survives its own restart.
//
// P3 adds the live channel (§2.5): a unix datagram the writers sendto, an
// in-memory body cache, in-flight rows overlaid on the log, and `/api/stream`
// as SSE. Token streams (P5) and `find` internals (P4) reuse the same pipe.

use crate::stats::{self, Outcome};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::ffi::CString;
use std::io::{Read, Seek, SeekFrom, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, SystemTime};

/// The marker `/api/status` carries so a liveness probe can tell scout's
/// dashboard from whatever else might have grabbed the port.
const SERVICE: &str = "scout-dashboard";

/// The daemon log is diagnostics, not history: bind errors, panics, rotation
/// notices.  Truncated at start once it passes this.
const MAX_DAEMON_LOG_BYTES: u64 = 1024 * 1024;

const DASHBOARD_HTML: &str = include_str!("../dashboard.html");

/// `scout dashboard` — start, stop, inspect, or be the daemon.
#[derive(clap::Args, Debug, Default)]
pub struct Args {
    /// Run the server in this process instead of detaching. For debugging,
    /// and the trampoline the detached start re-execs into.
    #[arg(long)]
    pub foreground: bool,
    /// SIGTERM the running daemon and remove its pidfile.
    #[arg(long, conflicts_with_all = ["foreground", "status", "restart"])]
    pub stop: bool,
    /// Report whether a daemon is running; exit 0 if it is, 1 if not.
    #[arg(long, conflicts_with_all = ["foreground", "restart"])]
    pub status: bool,
    /// Stop the running daemon, then start a new one.
    #[arg(long, conflicts_with = "foreground")]
    pub restart: bool,
    /// Port to bind (default: `[dashboard] port`, else 13001). The *address*
    /// is always 127.0.0.1 and cannot be changed.
    #[arg(long)]
    pub port: Option<u16>,
    /// Also open the dashboard in a browser.
    #[arg(long)]
    pub open: bool,
}

// ── Paths ───────────────────────────────────────────────────────────────────

/// `$XDG_STATE_HOME/scout`, the directory the call log already lives in.
///
/// Deliberately derived the same way `stats::log_path` derives its own, and
/// deliberately *not* from `$SCOUT_CALLS_LOG`: that variable moves one file for
/// tests, while `$XDG_STATE_HOME` moves the whole state directory, which is
/// what an isolated daemon needs.
fn state_dir() -> Option<PathBuf> {
    let base = std::env::var("XDG_STATE_HOME")
        .ok()
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var("HOME")
                .ok()
                .filter(|v| !v.is_empty())
                .map(|h| PathBuf::from(h).join(".local").join("state"))
        })?;
    Some(base.join("scout"))
}

/// `$XDG_STATE_HOME/scout/dashboard.pid` for the configured port — §5's path,
/// for the only configuration that is actually supported — and
/// `dashboard-<port>.pid` for anything else.
///
/// The suffix exists because the daemon's SIGTERM handler unlinks its pidfile
/// path *unconditionally*: it may only make async-signal-safe calls, so it
/// cannot read the file back to check whose it is.  With one shared path, a
/// one-off `scout dashboard --port N` alongside the real one would leave the
/// real one's pidfile deleted by whichever exited first.  A path per port makes
/// each daemon's cleanup its own business.
fn pid_path_for(port: u16) -> Option<PathBuf> {
    let default = crate::filter_config::load_dashboard().port;
    let name =
        if port == default { "dashboard.pid".to_string() } else { format!("dashboard-{port}.pid") };
    state_dir().map(|d| d.join(name))
}

fn daemon_log_path() -> Option<PathBuf> {
    state_dir().map(|d| d.join("dashboard.log"))
}

/// `calls.jsonl` → `calls.jsonl.1`.  Mirrors `stats::rotated_path`, which is
/// private to the writer.
fn rotated_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(".1");
    PathBuf::from(name)
}

/// The port, in precedence order: `--port`, `[dashboard] port`, 13001.
fn resolve_port(flag: Option<u16>) -> u16 {
    flag.unwrap_or_else(|| crate::filter_config::load_dashboard().port)
}

// ── One log row ─────────────────────────────────────────────────────────────

/// One parsed call-log line, always a current-schema (`"v":2`) one — see
/// `Row::parse`, which drops anything older.
#[derive(Debug, Clone)]
struct Row {
    id: String,
    /// The operation this row belongs to — the grouping key, stamped by the
    /// writer's ledger.  A row from before `op` was recorded falls back to its
    /// own `id`, which makes it an operation of one; see `group_ops`.
    op: String,
    run: String,
    ts: f64,
    via: String,
    tool: String,
    preset: String,
    attempt: u64,
    project: Option<String>,
    model: Option<String>,
    endpoint: Option<String>,
    input: Value,
    kind: String,
    summary: Option<String>,
    raw_bytes: u64,
    returned_bytes: u64,
    tokens_in: u64,
    tokens_out: u64,
    ms: u64,
    ok: bool,
}

impl Row {
    /// Parse one line, or `None` if it is not a current-schema record.
    ///
    /// The dashboard reads only lines carrying `"v":2` — the shape that has
    /// `id`, `op`, `via` and `input`.  Older lines are skipped rather than
    /// padded out with synthesized identity and an empty `input`, which is
    /// what made a pre-`input` record indistinguishable in the UI from a call
    /// that genuinely had no arguments.  They stay in the log; `scout stats`
    /// still counts them, and the dashboard simply has nothing to show for a
    /// row whose arguments, prompt and response were never recorded.
    fn parse(line: &str) -> Option<Row> {
        let v: Value = serde_json::from_str(line).ok()?;
        if v.get("v").and_then(Value::as_u64) != Some(2) {
            return None;
        }
        let s = |k: &str| v.get(k).and_then(Value::as_str).map(str::to_string);
        let n = |k: &str| v.get(k).and_then(Value::as_u64).unwrap_or(0);
        let ok = v.get("ok").and_then(Value::as_bool).unwrap_or(false);
        let preset = s("preset").unwrap_or_else(|| "unknown".to_string());
        let id = s("id")?;
        let kind = v["outcome"]["kind"]
            .as_str()
            .map(str::to_string)
            .unwrap_or_else(|| if ok { "ok" } else { "unknown" }.to_string());
        Some(Row {
            op: s("op").unwrap_or_else(|| id.clone()),
            run: s("run").unwrap_or_else(|| id.clone()),
            id,
            ts: v.get("ts").and_then(Value::as_f64).unwrap_or(0.0),
            via: s("via").unwrap_or_default(),
            tool: s("tool").unwrap_or_else(|| preset.clone()),
            preset,
            attempt: v.get("attempt").and_then(Value::as_u64).unwrap_or(1),
            project: s("project"),
            model: s("model"),
            endpoint: s("endpoint"),
            input: v.get("input").cloned().unwrap_or_else(|| json!({})),
            kind,
            summary: v["outcome"]["summary"].as_str().map(str::to_string),
            raw_bytes: n("raw_bytes"),
            returned_bytes: n("returned_bytes"),
            tokens_in: n("tokens_in"),
            tokens_out: n("tokens_out"),
            ms: n("ms"),
            ok,
        })
    }

    fn bypassed(&self) -> bool {
        self.kind == Outcome::Bypassed.as_str()
    }

    fn to_json(&self) -> Value {
        json!({
            "id": self.id,
            "op": self.op,
            "run": self.run,
            "ts": self.ts,
            "via": self.via,
            "tool": self.tool,
            "preset": self.preset,
            "attempt": self.attempt,
            "project": self.project,
            "model": self.model,
            "endpoint": self.endpoint,
            "input": self.input,
            "kind": self.kind,
            "summary": self.summary,
            "raw_bytes": self.raw_bytes,
            "returned_bytes": self.returned_bytes,
            "tokens_in": self.tokens_in,
            "tokens_out": self.tokens_out,
            "ms": self.ms,
            "ok": self.ok,
        })
    }
}

// ── The tailing reader ──────────────────────────────────────────────────────

/// The parsed log, plus enough of the file's identity to read only the delta.
///
/// The daemon is long-lived and the writers rotate underneath it (§3), so this
/// detects rotation rather than assuming it: a changed inode, or a length below
/// the last offset, forces a reload spanning `calls.jsonl.1` + `calls.jsonl`.
/// Getting that wrong is the most likely way for a dashboard to silently stop
/// updating after a few days.
struct Tail {
    path: PathBuf,
    rows: Vec<Row>,
    /// Bytes of the *live* file already consumed.  Only complete lines count,
    /// so a half-written record is re-read rather than parsed as garbage.
    offset: u64,
    ident: Option<(u64, u64)>,
    /// Reload count — surfaced on `/api/status` because "did the reader notice
    /// the rotation" is otherwise unobservable.
    reloads: u64,
    /// Rows dropped because they would not parse.
    parse_errors: u64,
    /// Rows skipped because they predate the current record schema.  Counted
    /// separately from `parse_errors` so a log with a long tail of old history
    /// does not look corrupt on `/api/status`.
    skipped_legacy: u64,
}

/// A line that parses as JSON but is not a current (`"v":2`) record.
///
/// Distinguishes "deliberately skipped" from "malformed" for the two counters
/// above; `Row::parse` returns `None` for both.
fn is_legacy_record(line: &str) -> bool {
    serde_json::from_str::<Value>(line)
        .map(|v| v.get("v").and_then(Value::as_u64) != Some(2))
        .unwrap_or(false)
}

/// `(dev, ino)` — the pair that identifies a file across a rename.
#[cfg(unix)]
fn file_ident(meta: &std::fs::Metadata) -> (u64, u64) {
    use std::os::unix::fs::MetadataExt;
    (meta.dev(), meta.ino())
}

impl Tail {
    fn new(path: PathBuf) -> Tail {
        Tail {
            path,
            rows: Vec::new(),
            offset: 0,
            ident: None,
            reloads: 0,
            parse_errors: 0,
            skipped_legacy: 0,
        }
    }

    /// Bring the in-memory rows up to date.  One `stat` when nothing changed,
    /// which is the overwhelmingly common case.
    fn refresh(&mut self) {
        let Ok(meta) = std::fs::metadata(&self.path) else {
            // The log has not been created yet, or vanished.  Either way there
            // is nothing to read; a later `refresh` picks it up as a reload
            // because `ident` will not match.
            return;
        };
        let ident = file_ident(&meta);
        if self.ident != Some(ident) || meta.len() < self.offset {
            self.reload();
            return;
        }
        if meta.len() == self.offset {
            return;
        }
        let from = self.offset;
        let consumed = self.append_from(&self.path.clone(), from);
        self.offset += consumed;
    }

    /// Re-read both generations from scratch.
    ///
    /// The rotated file first, then the live one, so history spans the rotation
    /// instead of restarting at it — the same order `stats::parse_log` reads in.
    fn reload(&mut self) {
        self.rows.clear();
        self.offset = 0;
        self.parse_errors = 0;
        self.reloads += 1;
        self.skipped_legacy = 0;
        let rotated = rotated_path(&self.path);
        self.append_from(&rotated, 0);
        let consumed = self.append_from(&self.path.clone(), 0);
        self.offset = consumed;
        self.ident = std::fs::metadata(&self.path).ok().map(|m| file_ident(&m));
    }

    /// Parse from `from` to EOF, returning the bytes of *complete* lines
    /// consumed.
    fn append_from(&mut self, path: &Path, from: u64) -> u64 {
        let Ok(mut f) = std::fs::File::open(path) else { return 0 };
        if from > 0 && f.seek(SeekFrom::Start(from)).is_err() {
            return 0;
        }
        let mut buf = Vec::new();
        if f.read_to_end(&mut buf).is_err() {
            return 0;
        }
        // Stop at the last newline: a writer mid-`writeln!` leaves a partial
        // record, and re-reading it next poll is free.
        let Some(end) = buf.iter().rposition(|b| *b == b'\n') else { return 0 };
        let text = String::from_utf8_lossy(&buf[..=end]);
        for line in text.lines() {
            if line.trim().is_empty() {
                continue;
            }
            match Row::parse(line) {
                Some(row) => self.rows.push(row),
                // An older record is skipped on purpose, not broken — keep it
                // out of the malformed count the status endpoint reports.
                None if is_legacy_record(line) => self.skipped_legacy += 1,
                None => self.parse_errors += 1,
            }
        }
        end as u64 + 1
    }

    fn log_bytes(&self) -> u64 {
        std::fs::metadata(&self.path).map(|m| m.len()).unwrap_or(0)
    }
}

// ── Operations ──────────────────────────────────────────────────────────────

/// Row indices grouped by `op`, one entry per user-facing operation, ordered by
/// where each operation first appears in the log.
///
/// `op` is ground truth, not an inference: the writer's `Ledger` is constructed
/// once per MCP dispatch and once per CLI invocation, and stamps its id on
/// every row it parks — so the three or four rows one `scout find` writes carry
/// the same `op`, and two tool calls of one long-lived `scout mcp` do not.
/// `run` cannot stand in for it; it names the *process*, and an MCP server's
/// process is a whole Claude Code session (§1's own table says so).
///
/// Deliberately not "consecutive rows of one `op`": `mcp_server` dispatches on
/// `spawn_blocking`, so two parallel tool calls interleave their rows in the
/// log, and an operation that straddles a rotation is read from two files.
/// Identity does not care about position, so neither does this.
///
/// A row written before `op` was recorded falls back to its own `id` in
/// `Row::parse`, so it is an operation of one — which is also what a v1 row,
/// carrying no identity at all, degrades to.
fn group_ops(rows: &[Row]) -> Vec<Vec<usize>> {
    let mut ops: Vec<Vec<usize>> = Vec::new();
    let mut slot: HashMap<&str, usize> = HashMap::new();
    for (i, row) in rows.iter().enumerate() {
        match slot.get(row.op.as_str()) {
            Some(&at) => ops[at].push(i),
            None => {
                slot.insert(row.op.as_str(), ops.len());
                ops.push(vec![i]);
            }
        }
    }
    ops
}

/// One operation as the history pane reads it: the summed byte and token
/// accounting, one outcome, and the constituent rows behind a `⋮n` marker.
///
/// Summing is the whole point.  P1's ledger parks the newest record and lets
/// the *first* row of an operation claim `raw_bytes` while the *last* one
/// carries `returned_bytes` — so a per-row ratio is meaningless and a per-`op`
/// ratio is the metric.
fn op_json(rows: &[Row], op: &[usize]) -> Value {
    let slice: Vec<&Row> = op.iter().map(|i| &rows[*i]).collect();
    let first = slice[0];
    let last = slice[slice.len() - 1];
    let sum = |f: fn(&Row) -> u64| slice.iter().map(|r| f(r)).sum::<u64>();

    // A failure is the interesting thing about an operation that had one;
    // otherwise the last row's verdict wins, because `none_relevant` and the
    // payload size are both stamped there.
    let failed = slice.iter().find(|r| !r.ok);
    let (kind, summary) = match failed {
        Some(r) => (r.kind.clone(), r.summary.clone()),
        None => (last.kind.clone(), last.summary.clone().or_else(|| first.summary.clone())),
    };
    let input = slice
        .iter()
        .map(|r| &r.input)
        .find(|i| i.as_object().is_some_and(|o| !o.is_empty()))
        .cloned()
        .unwrap_or_else(|| json!({}));

    json!({
        "id": first.id,
        "last_id": last.id,
        "op": first.op,
        "run": first.run,
        "ts": first.ts,
        "end_ts": last.ts,
        "via": first.via,
        "tool": first.tool,
        "project": slice.iter().find_map(|r| r.project.clone()),
        "model": slice.iter().find_map(|r| r.model.clone()),
        "endpoint": slice.iter().find_map(|r| r.endpoint.clone()),
        "input": input,
        "kind": kind,
        "summary": summary,
        "ok": failed.is_none(),
        "n": slice.len(),
        "ms": sum(|r| r.ms),
        "tokens_in": sum(|r| r.tokens_in),
        "tokens_out": sum(|r| r.tokens_out),
        "raw_bytes": sum(|r| r.raw_bytes),
        "returned_bytes": sum(|r| r.returned_bytes),
        "rows": slice.iter().map(|r| r.to_json()).collect::<Vec<_>>(),
    })
}

// ── Aggregates ──────────────────────────────────────────────────────────────

/// Nearest-rank percentile of an already-sorted slice.
fn percentile(sorted: &[u64], p: f64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let rank = ((p / 100.0) * sorted.len() as f64).ceil() as usize;
    sorted[rank.saturating_sub(1).min(sorted.len() - 1)]
}

/// The overview and failures panes (§4), computed from the in-memory rows.
///
/// Byte totals come from the grouped operations rather than the raw rows: the
/// sum is the same either way, but going through `op_json` keeps one definition
/// of "an operation's context saved" instead of two that can drift.
fn overview(tail: &Tail) -> Value {
    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0);
    let rows = &tail.rows;

    let mut bypassed = 0u64;
    let mut calls_1h = 0u64;
    let mut calls_24h = 0u64;
    let mut ok = 0u64;
    let mut counted = 0u64;
    let mut tokens_in = 0u64;
    let mut tokens_out = 0u64;
    let mut raw_bytes = 0u64;
    let mut returned_bytes = 0u64;
    let mut failures: HashMap<&str, u64> = HashMap::new();
    let mut latencies: Vec<u64> = Vec::new();

    for r in rows {
        raw_bytes += r.raw_bytes;
        returned_bytes += r.returned_bytes;
        if r.ts > 0.0 && now - r.ts <= 3600.0 {
            calls_1h += 1;
        }
        if r.ts > 0.0 && now - r.ts <= 86_400.0 {
            calls_24h += 1;
        }
        if r.bypassed() {
            // Kept out of the round-trip counts for the same reason `scout
            // stats` keeps it out of the per-preset table: a bypass is the
            // absence of a round-trip, and it gets its own row in §4.
            bypassed += 1;
            continue;
        }
        counted += 1;
        if r.ok {
            ok += 1;
            if r.ms > 0 {
                latencies.push(r.ms);
            }
        } else {
            *failures.entry(r.kind.as_str()).or_insert(0) += 1;
        }
        tokens_in += r.tokens_in;
        tokens_out += r.tokens_out;
    }
    latencies.sort_unstable();

    let mut failures: Vec<(&str, u64)> = failures.into_iter().collect();
    failures.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));

    let span = match (rows.first(), rows.last()) {
        (Some(f), Some(l)) if f.ts > 0.0 && l.ts > 0.0 => l.ts - f.ts,
        _ => 0.0,
    };

    json!({
        "rows": rows.len(),
        "operations": group_ops(rows).len(),
        "calls_1h": calls_1h,
        "calls_24h": calls_24h,
        "calls": counted,
        "ok": ok,
        "bypassed": bypassed,
        "tokens_in": tokens_in,
        "tokens_out": tokens_out,
        "raw_bytes": raw_bytes,
        "returned_bytes": returned_bytes,
        // p95 matters far more than the mean for a local model: one 40s stall
        // is what you notice, and the mean hides it.
        "p50_ms": percentile(&latencies, 50.0),
        "p95_ms": percentile(&latencies, 95.0),
        "failures": failures.iter().map(|(k, n)| json!({"kind": k, "count": n})).collect::<Vec<_>>(),
        "span_secs": span,
        "log_bytes": tail.log_bytes(),
        "log_max_bytes": 8 * 1024 * 1024,
        "log_rotated": rotated_path(&tail.path).exists(),
        "parse_errors": tail.parse_errors,
        "skipped_legacy": tail.skipped_legacy,
        "reloads": tail.reloads,
    })
}

/// `/api/stats` — the `scout stats` table, as JSON.
///
/// Recomputed from the in-memory rows rather than calling into `stats.rs`:
/// that module's reader is file-based and prints, and re-reading 8 MB per poll
/// to render a table the daemon already holds would be the one expensive thing
/// in here.
fn stats_json(tail: &Tail) -> Value {
    struct Agg {
        calls: u64,
        ok: u64,
        tokens_in: u64,
        tokens_out: u64,
        ok_ms: u64,
    }
    let mut by_preset: HashMap<&str, Agg> = HashMap::new();
    for r in tail.rows.iter().filter(|r| !r.bypassed()) {
        let e = by_preset
            .entry(r.preset.as_str())
            .or_insert(Agg { calls: 0, ok: 0, tokens_in: 0, tokens_out: 0, ok_ms: 0 });
        e.calls += 1;
        if r.ok {
            e.ok += 1;
            e.ok_ms += r.ms;
        }
        e.tokens_in += r.tokens_in;
        e.tokens_out += r.tokens_out;
    }
    let mut rows: Vec<(&str, Agg)> = by_preset.into_iter().collect();
    rows.sort_by(|a, b| b.1.calls.cmp(&a.1.calls).then(a.0.cmp(b.0)));
    let presets: Vec<Value> = rows
        .iter()
        .map(|(name, a)| {
            json!({
                "preset": name,
                "calls": a.calls,
                "ok": a.ok,
                "pass_pct": if a.calls > 0 { a.ok as f64 / a.calls as f64 * 100.0 } else { 0.0 },
                "tokens_in": a.tokens_in,
                "tokens_out": a.tokens_out,
                "avg_ms_ok": a.ok_ms.checked_div(a.ok),
            })
        })
        .collect();
    json!({
        "presets": presets,
        "total_calls": rows.iter().map(|(_, a)| a.calls).sum::<u64>(),
        "total_ok": rows.iter().map(|(_, a)| a.ok).sum::<u64>(),
    })
}

// ── Endpoint reachability ───────────────────────────────────────────────────

/// The one thing the log cannot tell you: is the local model up right now.
///
/// Polled on its own thread every 15s rather than on request, so a 5s connect
/// timeout to a dead host never becomes a 5s `/api/status`.  The config is
/// re-read each cycle so editing `config.toml` takes effect without restarting
/// the daemon.
#[derive(Default)]
struct Reach {
    model: Option<String>,
    endpoint: Option<String>,
    reachable: bool,
    ms: u64,
    error: Option<String>,
    checked: f64,
    /// `[llm] timeout_seconds`, carried out of the same config load so the
    /// in-flight sweep can be bounded by what scout itself would wait.
    timeout_secs: u64,
}

fn check_reachability() -> Reach {
    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0);
    match crate::config::load_config(&crate::config::config_path()) {
        Ok(cfg) => {
            let timeout_secs = cfg.timeout.as_secs();
            let client = crate::client::LlmClient::new(cfg);
            let (reachable, ms) = client.check_endpoint();
            Reach {
                model: Some(client.model().to_string()),
                endpoint: Some(client.endpoint().to_string()),
                reachable,
                ms,
                error: None,
                checked: now,
                timeout_secs,
            }
        }
        Err(e) => Reach { error: Some(e), checked: now, ..Default::default() },
    }
}

// ── Server ──────────────────────────────────────────────────────────────────

struct State {
    tail: Mutex<Tail>,
    live: Arc<crate::live::LiveStore>,
    reach: Mutex<Reach>,
    started: SystemTime,
    port: u16,
    /// The live socket's path — `None` when the bind failed, or when this is
    /// not a real daemon.  Re-checked on the reachability timer; see
    /// `socket_still_bound`.
    live_socket: Option<PathBuf>,
}

impl State {
    fn status_json(&self) -> Value {
        let mut tail = self.tail.lock().unwrap_or_else(|e| e.into_inner());
        tail.refresh();
        let overview = overview(&tail);
        drop(tail);
        let reach = self.reach.lock().unwrap_or_else(|e| e.into_inner());
        let (inflight, bodies, finds, streams) = self.live.snapshot();
        let (running, abandoned) = self.live.inflight_split();
        json!({
            // The marker a liveness probe looks for (§5): a pidfile surviving
            // `kill -9` is common, so the port answering *as scout* is what
            // decides whether a daemon is running.
            "service": SERVICE,
            "version": env!("CARGO_PKG_VERSION"),
            "pid": std::process::id(),
            "port": self.port,
            "started": unix_secs(self.started),
            "uptime_secs": self.started.elapsed().map(|d| d.as_secs()).unwrap_or(0),
            "log_path": stats::log_path().map(|p| p.display().to_string()),
            "llm": {
                "model": reach.model,
                "endpoint": reach.endpoint,
                "reachable": reach.reachable,
                "ms": reach.ms,
                "error": reach.error,
                "checked": reach.checked,
            },
            "overview": overview,
            "live": {
                "bound": self.live.bound(),
                "inflight": inflight,
                "running": running,
                "abandoned": abandoned,
                "abandon_after_secs": self.live.abandon_after_secs(),
                "bodies": bodies,
                "finds": finds,
                "streams": streams,
                // Should equal `streams`. A gap means a handler is holding a
                // `MAX_STREAMS` slot for a stream the fan-out has stopped
                // feeding, which used to be the fate of any tab that fell one
                // window behind — see `LiveStore::apply_json`.
                "subscribers": self.live.subscriber_count(),
            },
        })
    }
}

/// Is there still a socket at `path` for writers to find?
///
/// `bound` used to be a startup snapshot — stamped once and never revisited —
/// so a daemon whose socket name had been taken away went on reporting
/// `"bound": true` while receiving nothing.  The fd stays open and valid
/// whatever happens on disk; what a writer has is the *name*, so the name is
/// what `/api/status` should be reporting on.
///
/// Deliberately "a socket is there", not "*our* socket is there".  A path stat
/// cannot answer the stronger question: `fstat` on an `AF_UNIX` socket reports
/// its sockfs inode rather than the directory entry `bind(2)` created, so
/// there is nothing to compare the fd against, and `(dev, ino)` taken from the
/// path at bind time is no good either — an unlink-and-rebind reuses the inode
/// often enough that the check would silently pass.  Port-qualifying the
/// socket path (see `live::socket_path_for`) is what makes the stronger
/// question moot: no other daemon binds this name any more.  What is left, and
/// what this catches, is the name going away.
fn socket_still_bound(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::FileTypeExt;
        std::fs::metadata(path).map(|m| m.file_type().is_socket()).unwrap_or(false)
    }
}

fn unix_secs(t: SystemTime) -> u64 {
    t.duration_since(SystemTime::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// Bind with `SO_REUSEADDR` set before bind, so a well-known port can be
/// reclaimed immediately after a restart instead of waiting out `TIME_WAIT`.
///
/// The address is `127.0.0.1` and there is no override — not a flag, not an
/// env var.  scout's payloads carry file contents from every repo the user
/// works in, so there is no other bind address worth supporting.
#[cfg(unix)]
fn bind_tcp_reuse(port: u16) -> std::io::Result<TcpListener> {
    use std::os::unix::io::FromRawFd;
    unsafe {
        let fd = libc::socket(libc::AF_INET, libc::SOCK_STREAM, 0);
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        let optval: libc::c_int = 1;
        libc::setsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_REUSEADDR,
            &optval as *const _ as *const libc::c_void,
            std::mem::size_of::<libc::c_int>() as libc::socklen_t,
        );
        let mut sa: libc::sockaddr_in = std::mem::zeroed();
        sa.sin_family = libc::AF_INET as libc::sa_family_t;
        sa.sin_port = port.to_be();
        sa.sin_addr = libc::in_addr {
            s_addr: u32::from_ne_bytes(std::net::Ipv4Addr::LOCALHOST.octets()),
        };
        let bound = libc::bind(
            fd,
            &sa as *const _ as *const libc::sockaddr,
            std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t,
        );
        if bound < 0 {
            let err = std::io::Error::last_os_error();
            libc::close(fd);
            return Err(err);
        }
        if libc::listen(fd, 128) < 0 {
            let err = std::io::Error::last_os_error();
            libc::close(fd);
            return Err(err);
        }
        Ok(TcpListener::from_raw_fd(fd))
    }
}

/// Bind, retrying twice.  `--restart` races its own predecessor's socket
/// teardown, which is precisely the window this covers.
fn bind_with_retry(port: u16) -> std::io::Result<TcpListener> {
    let mut last = None;
    for attempt in 1..=3u8 {
        match bind_tcp_reuse(port) {
            Ok(l) => return Ok(l),
            Err(e) => {
                if attempt < 3 {
                    eprintln!("dashboard: bind 127.0.0.1:{port} failed ({e}), retrying in 1s");
                    std::thread::sleep(Duration::from_secs(1));
                }
                last = Some(e);
            }
        }
    }
    Err(last.unwrap_or_else(|| std::io::Error::other("bind failed")))
}

/// Percent-decode a query value.
///
/// Works on `bytes` throughout and never slices `s` as a `&str`: `%` is
/// itself ASCII, but the two bytes after it are arbitrary and can land
/// mid-codepoint when the input isn't well-formed percent-encoding (e.g. a
/// literal multi-byte UTF-8 character right after a stray `%`, as in `%€`).
/// `&s[i+1..i+3]` would panic on that char boundary; `bytes.get(i+1..i+3)`
/// plus a byte-level hex parse cannot, because it never asks `str` to agree
/// the slice is valid UTF-8. See `live.rs`'s and `source.rs`'s boundary
/// handling for the same rule applied elsewhere in this codebase.
fn url_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if matches!(bytes.get(i + 1..i + 3), Some(pair) if pair.iter().all(u8::is_ascii_hexdigit)) =>
            {
                let hi = (bytes[i + 1] as char).to_digit(16).unwrap();
                let lo = (bytes[i + 2] as char).to_digit(16).unwrap();
                out.push((hi * 16 + lo) as u8);
                i += 3;
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8(out).unwrap_or_else(|_| s.to_string())
}

fn parse_query(query: &str) -> HashMap<String, String> {
    let mut params = HashMap::new();
    for pair in query.split('&') {
        if let Some((k, v)) = pair.split_once('=') {
            params.insert(k.to_string(), url_decode(v));
        }
    }
    params
}

/// `/api/history` — operations, newest first.
///
/// `since` is the opaque `last_id` of the newest operation the caller already
/// has.  An id that is no longer in the log — the caller slept through a
/// rotation — falls back to a full page rather than an error, which is the only
/// behavior that lets a browser tab recover on its own.
fn row_from_live(r: &crate::live::LiveRow) -> Row {
    Row {
        id: r.id.clone(),
        op: r.op.clone(),
        run: r.run.clone(),
        ts: r.ts,
        via: r.via.clone(),
        tool: r.tool.clone(),
        preset: r.preset.clone(),
        attempt: r.attempt,
        project: r.project.clone(),
        model: r.model.clone(),
        endpoint: r.endpoint.clone(),
        input: r.input.clone(),
        kind: r.kind.clone(),
        summary: r.summary.clone(),
        raw_bytes: r.raw_bytes,
        returned_bytes: r.returned_bytes,
        tokens_in: r.tokens_in,
        tokens_out: r.tokens_out,
        ms: r.ms,
        ok: r.ok,
    }
}

/// Log rows plus any inflight rows the log has not yet absorbed.
///
/// Reaping happens here because it is driven by what the log now contains.
/// The other cleanup — `LiveStore::sweep`, for rows the log will *never*
/// contain — is on the daemon's timer instead: it is driven by elapsed time,
/// and a read path that mutated rows on the wall clock would make every
/// history response depend on when it was asked.
fn merged_rows(tail: &Tail, live: &crate::live::LiveStore) -> Vec<Row> {
    live.reap(tail.rows.iter().map(|r| r.id.as_str()));
    let mut rows = tail.rows.clone();
    let have: std::collections::HashSet<String> = rows.iter().map(|r| r.id.clone()).collect();
    for r in live.inflight_rows() {
        if !have.contains(&r.id) {
            rows.push(row_from_live(&r));
        }
    }
    rows
}

fn attach_bodies(op: &mut Value, live: &crate::live::LiveStore) {
    let Some(rows) = op.get_mut("rows").and_then(Value::as_array_mut) else { return };
    for row in rows {
        let Some(id) = row.get("id").and_then(Value::as_str).map(str::to_string) else {
            continue;
        };
        if let Some(b) = live.bodies_of(&id) {
            if let Some(s) = b.system {
                row["system"] = Value::from(s);
            }
            if let Some(s) = b.user {
                row["user"] = Value::from(s);
            }
            if let Some(s) = b.response {
                row["response"] = Value::from(s);
            }
        }
    }
}

/// `find`'s per-round internals (§2.5, P4), for the detail pane's tab strip.
///
/// Only on `/api/call/<id>`, never on `/api/history`: a page of 300 operations
/// carrying every round's pattern and keep list would dwarf the summary rows it
/// exists to deliver, and the pane fetches the one operation it is painting.
/// The live stream carries the same events as they happen, so this path serves
/// a reload, a deep link, and `curl`.
fn attach_find_rounds(op: &mut Value, live: &crate::live::LiveStore) {
    let Some(id) = op.get("op").and_then(Value::as_str).map(str::to_string) else { return };
    if let Some(rounds) = live.find_rounds(&id) {
        op["find_rounds"] = rounds;
    }
}

#[cfg(test)]
fn history_json(tail: &Tail, params: &HashMap<String, String>) -> Value {
    history_with_live(tail, None, params)
}

fn history_with_live(
    tail: &Tail,
    live: Option<&crate::live::LiveStore>,
    params: &HashMap<String, String>,
) -> Value {
    let overlay;
    let rows: &[Row] = match live {
        Some(live) => {
            overlay = merged_rows(tail, live);
            &overlay
        }
        None => &tail.rows,
    };
    let ops = group_ops(rows);
    let limit: usize = params
        .get("limit")
        .and_then(|v| v.parse().ok())
        .unwrap_or(300)
        .clamp(1, 5000);

    let since_idx = params
        .get("since")
        .filter(|s| !s.is_empty())
        .and_then(|id| rows.iter().position(|r| &r.id == id));
    let known = params.get("since").is_some_and(|s| !s.is_empty());

    let want = |op: &Vec<usize>| -> bool {
        if let Some(cut) = since_idx {
            if op.iter().all(|i| *i <= cut) {
                return false;
            }
        }
        let matches = |key: &str, value: &str| {
            params.get(key).is_none_or(|want| want.is_empty() || want == value)
        };
        let first = &rows[op[0]];
        if !matches("tool", &first.tool) || !matches("via", &first.via) {
            return false;
        }
        if let Some(want) = params.get("project").filter(|p| !p.is_empty()) {
            if !op.iter().any(|i| rows[*i].project.as_deref() == Some(want.as_str())) {
                return false;
            }
        }
        if params.get("failed").is_some_and(|v| v == "1" || v == "true") {
            return op.iter().any(|i| !rows[*i].ok);
        }
        true
    };

    let selected: Vec<&Vec<usize>> = ops.iter().filter(|op| want(op)).collect();
    let page: Vec<Value> = selected
        .iter()
        .rev()
        .take(limit)
        .map(|op| op_json(rows, op))
        .collect();

    json!({
        "ops": page,
        // The cursor to pass back as `since`: the newest row in the *log*,
        // never an inflight id. An inflight cursor that then vanishes would
        // `resynced: true` and wipe the tab.
        "cursor": tail.rows.last().map(|r| r.id.clone()),
        "total_ops": ops.len(),
        "matched": selected.len(),
        "resynced": known && since_idx.is_none(),
    })
}

/// `/api/call/<id>` — one operation, by the id of any row in it.
#[cfg(test)]
fn call_json(tail: &Tail, id: &str) -> Option<Value> {
    call_with_live(tail, None, id)
}

fn call_with_live(tail: &Tail, live: Option<&crate::live::LiveStore>, id: &str) -> Option<Value> {
    let overlay;
    let rows: &[Row] = match live {
        Some(live) => {
            overlay = merged_rows(tail, live);
            &overlay
        }
        None => &tail.rows,
    };
    let idx = rows.iter().position(|r| r.id == id || r.op == id)?;
    let op = group_ops(rows).into_iter().find(|op| op.contains(&idx))?;
    let mut v = op_json(rows, &op);
    if let Some(live) = live {
        attach_bodies(&mut v, live);
        attach_find_rounds(&mut v, live);
    }
    Some(v)
}

fn respond(stream: &mut TcpStream, status: u16, content_type: &str, body: &[u8]) {
    let text = match status {
        200 => "OK",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        431 => "Request Header Fields Too Large",
        501 => "Not Implemented",
        503 => "Service Unavailable",
        _ => "Error",
    };
    let header = format!(
        "HTTP/1.0 {status} {text}\r\nContent-Type: {content_type}\r\n\
         Content-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let _ = stream.write_all(header.as_bytes());
    let _ = stream.write_all(body);
}

fn respond_json(stream: &mut TcpStream, status: u16, body: &Value) {
    respond(stream, status, "application/json", body.to_string().as_bytes());
}

/// SSE: hold the connection, `data: {...}\n\n` per event, comment keepalives.
///
/// This is the one route that must not go through `respond` / `HTTP/1.0
/// Connection: close`. The handler thread lives as long as the tab.
fn handle_stream(state: &Arc<State>, mut stream: TcpStream) {
    if !state.live.try_acquire_stream() {
        respond_json(&mut stream, 503, &json!({"error": "too many live streams"}));
        return;
    }
    let rx = state.live.subscribe();
    let header = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\
                  Cache-Control: no-cache\r\nConnection: keep-alive\r\n\r\n";
    if stream.write_all(header.as_bytes()).is_err() || stream.flush().is_err() {
        state.live.release_stream();
        return;
    }
    loop {
        match rx.recv_timeout(Duration::from_secs(15)) {
            Ok(ev) => {
                let line = format!("data: {ev}\n\n");
                if stream.write_all(line.as_bytes()).is_err() || stream.flush().is_err() {
                    break;
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                if stream.write_all(b": keepalive\n\n").is_err() || stream.flush().is_err() {
                    break;
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    state.live.release_stream();
}

/// One line off the header phase (§ below `read_header_line`).
enum LineOutcome {
    /// A complete line, `\r\n`- or `\n`-terminated, terminator stripped.
    Line(String),
    /// The peer closed (or the socket errored) before a newline arrived.
    Eof,
    /// The line grew past `MAX_HEADER_LINE_BYTES` without a newline.
    TooLong,
    /// `HEADER_PHASE_DEADLINE` elapsed before a newline arrived.
    TimedOut,
}

/// Hard cap on one header-phase line (the request line, or one header),
/// enforced byte-by-byte so a line that never sends `\n` cannot grow the
/// buffer without bound.
const MAX_HEADER_LINE_BYTES: usize = 8 * 1024;

/// Hard cap on the number of header lines. A per-line byte budget alone does
/// not stop a client sending thousands of short header lines.
const MAX_HEADER_COUNT: usize = 64;

/// Wall-clock budget for the *whole* header phase (request line + every
/// header), independent of how many bytes have arrived.
///
/// `set_read_timeout` bounds one syscall, not the phase: a client that
/// trickles a single byte in just under that window, forever, keeps every
/// individual read succeeding while never finishing a request — 17 GB of RSS
/// in 12 seconds was reproduced this way against the old per-syscall-only
/// timeout. This deadline is what actually closes such a connection.
const HEADER_PHASE_DEADLINE: Duration = Duration::from_secs(10);

/// Read one `\n`-terminated line directly off `stream`, bounded by
/// `max_bytes` and by `deadline`.
///
/// Byte-at-a-time on purpose: the alternative (`BufReader::read_line`) has no
/// hook to recheck a wall-clock deadline between the syscalls it makes while
/// hunting for the delimiter, which is exactly the gap this function exists
/// to close. Traffic on this socket is a handful of short header lines per
/// request on loopback, so the extra syscalls are not a real cost.
fn read_header_line(stream: &mut TcpStream, max_bytes: usize, deadline: std::time::Instant) -> LineOutcome {
    let mut line: Vec<u8> = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        let remaining = match deadline.checked_duration_since(std::time::Instant::now()) {
            Some(d) if !d.is_zero() => d,
            _ => return LineOutcome::TimedOut,
        };
        // Never let one syscall block past what remains of the deadline,
        // even though the connection's own read timeout (set once, in
        // `handle`) is longer.
        if stream.set_read_timeout(Some(remaining)).is_err() {
            return LineOutcome::Eof;
        }
        match stream.read(&mut byte) {
            Ok(0) => return LineOutcome::Eof, // peer closed
            Ok(_) => {
                if byte[0] == b'\n' {
                    if line.last() == Some(&b'\r') {
                        line.pop();
                    }
                    return LineOutcome::Line(String::from_utf8_lossy(&line).into_owned());
                }
                line.push(byte[0]);
                if line.len() > max_bytes {
                    return LineOutcome::TooLong;
                }
            }
            Err(e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut =>
            {
                return LineOutcome::TimedOut;
            }
            Err(_) => return LineOutcome::Eof,
        }
    }
}

/// The authorities this daemon answers to: its own loopback address, or
/// `localhost`, on its own port.
///
/// The bind is 127.0.0.1 and both `bind_tcp_reuse` and `filter_config` explain
/// at length why — scout's payloads carry file contents from every repo the
/// user works in, so there is no other bind address worth supporting.  That
/// reasoning is right and the bind is right, but loopback binding does not
/// stop a page the user merely *visits*.  DNS rebinding was invented for
/// exactly this: `attacker.example` resolves to its own IP, serves a page,
/// re-resolves to 127.0.0.1, and from then on the page's `fetch` and
/// `EventSource` calls are same-origin as far as the browser is concerned —
/// at which point `/api/history` and `/api/call/<id>` hand over the full
/// `system` / `user` / `response` text of every call the daemon holds.
///
/// The one field that still carries the attacker's name through all of that is
/// `Host:`, which this server was reading off the wire and dropping with the
/// rest of the headers.  Now it is the gate.
///
/// A missing `Host` is rejected too: HTTP/1.1 requires one, and a client that
/// omits it is not a browser that could have been rebound — but it is also not
/// something this daemon needs to serve.
///
/// `[::1]` is deliberately absent: the listener is bound to the IPv4 loopback
/// only, so nothing can reach it over v6 to send that authority in the first
/// place.
fn authority_is_ours(host: Option<&str>, port: u16) -> bool {
    let Some(host) = host else { return false };
    let host = host.trim().to_ascii_lowercase();
    // Split from the right, then check: an IPv6 literal has colons of its own,
    // and a trailing group that is not all digits is not a port.
    let (name, given) = match host.rsplit_once(':') {
        Some((n, p)) if !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()) => (n, Some(p)),
        _ => (host.as_str(), None),
    };
    let port_ok = match given {
        Some(p) => p.parse::<u16>() == Ok(port),
        // A browser omits the port when it is the scheme's default, so a
        // daemon on 80 legitimately sees a bare authority.  Every other port
        // must be spelled out, which is what makes this check worth anything.
        None => port == 80,
    };
    port_ok && matches!(name, "127.0.0.1" | "localhost")
}

/// Reject an `Origin` that is present and foreign.
///
/// Belt and braces rather than the load-bearing check: a genuinely
/// cross-origin `fetch` is already useless to an attacker, because this server
/// has never sent CORS response headers and the browser will not let the page
/// read the body.  And the rebinding case sends no `Origin` at all — the page
/// believes it *is* same-origin.  So an `Origin` that is present and not ours
/// can only be a cross-origin attempt, and refusing it costs one comparison.
/// Absent is accepted: that is what every same-origin GET, every `EventSource`
/// and every `curl` looks like.
fn origin_is_ours(origin: &str, port: u16) -> bool {
    let origin = origin.trim();
    // `Origin: null` is what a sandboxed iframe or a `file://` page sends.
    match origin.split_once("://") {
        Some((scheme, authority)) if scheme.eq_ignore_ascii_case("http") => {
            authority_is_ours(Some(authority), port)
        }
        _ => false,
    }
}

/// One connection: request line, headers, route, close.
///
/// `HTTP/1.0 Connection: close` throughout — everything here is a GET of a
/// small body, and there is no state to mutate.
fn handle(state: &Arc<State>, mut stream: TcpStream) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
    let deadline = std::time::Instant::now() + HEADER_PHASE_DEADLINE;

    let request_line = match read_header_line(&mut stream, MAX_HEADER_LINE_BYTES, deadline) {
        LineOutcome::Line(l) => l,
        LineOutcome::TooLong => {
            respond(&mut stream, 431, "text/plain", b"request line too large");
            return;
        }
        // A client that vanished, or one that went silent past the
        // deadline, gets nothing back — there is no well-formed request to
        // answer, and a still-open socket is not implied by either case.
        LineOutcome::Eof | LineOutcome::TimedOut => return,
    };

    // Headers are read for `Host` and `Origin` and otherwise dropped: no route
    // takes a body. Bounded the same way as the request line, plus a count cap
    // of its own.
    let mut header_count = 0usize;
    let mut host: Option<String> = None;
    let mut origin: Option<String> = None;
    loop {
        match read_header_line(&mut stream, MAX_HEADER_LINE_BYTES, deadline) {
            LineOutcome::Line(l) if l.trim().is_empty() => break, // end of headers
            LineOutcome::Line(l) => {
                header_count += 1;
                if header_count > MAX_HEADER_COUNT {
                    respond(&mut stream, 431, "text/plain", b"too many headers");
                    return;
                }
                let Some((name, value)) = l.split_once(':') else { continue };
                match name.trim().to_ascii_lowercase().as_str() {
                    "host" => {
                        // Two `Host`s is ambiguous by construction, and picking
                        // either one is how a check like this gets walked past.
                        if host.is_some() {
                            respond(&mut stream, 403, "text/plain", b"ambiguous Host header\n");
                            return;
                        }
                        host = Some(value.trim().to_string());
                    }
                    "origin" => origin = Some(value.trim().to_string()),
                    _ => {}
                }
            }
            LineOutcome::TooLong => {
                respond(&mut stream, 431, "text/plain", b"header too large");
                return;
            }
            LineOutcome::Eof | LineOutcome::TimedOut => return,
        }
    }

    // Before the route, before the method: a rebound page must not reach any
    // of them. See `authority_is_ours`.
    if !authority_is_ours(host.as_deref(), state.port) {
        respond(&mut stream, 403, "text/plain", b"forbidden: Host is not this dashboard\n");
        return;
    }
    if origin.as_deref().is_some_and(|o| !origin_is_ours(o, state.port)) {
        respond(&mut stream, 403, "text/plain", b"forbidden: cross-origin request\n");
        return;
    }

    let mut parts = request_line.split_whitespace();
    let (Some(method), Some(target)) = (parts.next(), parts.next()) else { return };
    if method != "GET" {
        respond_json(&mut stream, 405, &json!({"error": "only GET is served"}));
        return;
    }
    let (path, query) = match target.split_once('?') {
        Some((p, q)) => (p, q),
        None => (target, ""),
    };
    let params = parse_query(query);

    match path {
        "/" | "/index.html" => {
            respond(&mut stream, 200, "text/html; charset=utf-8", DASHBOARD_HTML.as_bytes())
        }
        "/api/status" => respond_json(&mut stream, 200, &state.status_json()),
        "/api/history" => {
            let mut tail = state.tail.lock().unwrap_or_else(|e| e.into_inner());
            tail.refresh();
            let body = history_with_live(&tail, Some(state.live.as_ref()), &params);
            drop(tail);
            respond_json(&mut stream, 200, &body);
        }
        "/api/stats" => {
            let mut tail = state.tail.lock().unwrap_or_else(|e| e.into_inner());
            tail.refresh();
            let body = stats_json(&tail);
            drop(tail);
            respond_json(&mut stream, 200, &body);
        }
        "/api/stream" => {
            handle_stream(state, stream);
            return;
        }
        p if p.starts_with("/api/call/") => {
            let id = url_decode(&p["/api/call/".len()..]);
            let mut tail = state.tail.lock().unwrap_or_else(|e| e.into_inner());
            tail.refresh();
            let found = call_with_live(&tail, Some(state.live.as_ref()), &id);
            drop(tail);
            match found {
                Some(v) => respond_json(&mut stream, 200, &v),
                None => respond_json(&mut stream, 404, &json!({"error": "no such call", "id": id})),
            }
        }
        _ => respond_json(&mut stream, 404, &json!({"error": "not found", "path": path})),
    }
}

// ── The daemon ──────────────────────────────────────────────────────────────

/// The pidfile path, as a `CString`, so the signal handler can `unlink` it
/// without allocating.  See `on_terminate`.
static PIDFILE_C: OnceLock<CString> = OnceLock::new();
static SOCKET_C: OnceLock<CString> = OnceLock::new();

/// SIGTERM/SIGINT: remove the pidfile and go.
///
/// Only async-signal-safe calls, which is why the path was turned into a
/// `CString` at startup: `unlink(2)` and `_exit(2)` are on the list, and
/// `std::fs::remove_file` — which allocates — is not.  There is nothing to
/// flush; the daemon only reads.
extern "C" fn on_terminate(_sig: libc::c_int) {
    if let Some(p) = PIDFILE_C.get() {
        unsafe { libc::unlink(p.as_ptr()) };
    }
    if let Some(p) = SOCKET_C.get() {
        unsafe { libc::unlink(p.as_ptr()) };
    }
    unsafe { libc::_exit(0) };
}

fn install_signal_handlers(pidfile: &Path) {
    if let Ok(c) = CString::new(pidfile.as_os_str().as_encoded_bytes()) {
        let _ = PIDFILE_C.set(c);
    }
    // Through the fn *pointer* rather than the fn item: casting an item
    // straight to an integer is a lint, and `sighandler_t` is one.
    let handler = on_terminate as extern "C" fn(libc::c_int);
    unsafe {
        libc::signal(libc::SIGTERM, handler as libc::sighandler_t);
        libc::signal(libc::SIGINT, handler as libc::sighandler_t);
    }
}

/// Run the server in this process.  Returns only on a bind failure.
fn run_foreground(port: u16) -> anyhow::Result<()> {
    let listener = bind_with_retry(port)
        .map_err(|e| anyhow::anyhow!("cannot bind 127.0.0.1:{port}: {e}"))?;

    let log = stats::log_path()
        .ok_or_else(|| anyhow::anyhow!("no call log path: $HOME and $XDG_STATE_HOME are both unset"))?;
    let mut tail = Tail::new(log);
    tail.reload();

    let live = Arc::new(crate::live::LiveStore::new());
    let mut live_socket = None;
    let live_sock = match crate::live::bind_socket(port) {
        Ok(sock) => {
            live.set_bound(true);
            if let Some(c) = crate::live::socket_cstring(port) {
                let _ = SOCKET_C.set(c);
            }
            // Remember the name, so `bound` can stay a fact rather than a
            // startup snapshot.  See `socket_still_bound`.
            live_socket = crate::live::socket_path_for(port);
            Some(sock)
        }
        Err(e) => {
            eprintln!("scout dashboard: live socket not bound: {e}");
            None
        }
    };

    let state = Arc::new(State {
        tail: Mutex::new(tail),
        live: Arc::clone(&live),
        reach: Mutex::new(Reach::default()),
        started: SystemTime::now(),
        port,
        live_socket,
    });

    if let Some(sock) = live_sock {
        std::thread::spawn(move || crate::live::recv_loop(sock, live));
    }

    if let Some(pidfile) = pid_path_for(port) {
        install_signal_handlers(&pidfile);
        if let Some(parent) = pidfile.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let body = json!({
            "pid": std::process::id(),
            "port": port,
            "started": unix_secs(state.started),
        });
        let _ = std::fs::write(&pidfile, body.to_string());
    }

    {
        let state = Arc::clone(&state);
        std::thread::spawn(move || loop {
            let fresh = check_reachability();
            // A config read that failed leaves `timeout_secs` at 0; keep the
            // store's existing bound rather than collapsing it to the grace
            // period and abandoning calls that are merely slow.
            if fresh.timeout_secs > 0 {
                state
                    .live
                    .set_abandon_after_secs(fresh.timeout_secs + crate::live::ABANDON_GRACE_SECS);
            }
            *state.reach.lock().unwrap_or_else(|e| e.into_inner()) = fresh;
            // `bound` is re-derived here rather than left as the startup
            // snapshot it used to be; see `socket_still_bound`.
            if let Some(path) = &state.live_socket {
                state.live.set_bound(socket_still_bound(path));
            }
            // Same cadence, and it wants the bound this cycle just read: an
            // in-flight row whose process was killed reports nothing and lands
            // in no log, so nothing but elapsed time can retire it.
            state.live.sweep(crate::live::now_ts());
            std::thread::sleep(Duration::from_secs(15));
        });
    }

    eprintln!("scout dashboard listening on http://localhost:{port}/ (pid {})", std::process::id());
    for stream in listener.incoming().flatten() {
        let state = Arc::clone(&state);
        std::thread::spawn(move || handle(&state, stream));
    }
    Ok(())
}

// ── Lifecycle ───────────────────────────────────────────────────────────────

/// Is a scout dashboard answering on `port`?
///
/// The liveness test, deliberately not a pid check: a pidfile surviving
/// `kill -9` is common, a recycled pid is not impossible, and pid-liveness is
/// awkward to check portably (`/proc` is Linux-only).  The pidfile's job is to
/// hold the pid for `--stop`; this decides whether anything is there.
fn probe(port: u16) -> Option<Value> {
    let url = format!("http://127.0.0.1:{port}/api/status");
    let resp = ureq::get(&url).timeout(Duration::from_millis(1500)).call().ok()?;
    let v: Value = serde_json::from_str(&resp.into_string().ok()?).ok()?;
    (v.get("service").and_then(Value::as_str) == Some(SERVICE)).then_some(v)
}

/// The pid this port's pidfile records.
///
/// The recorded port is checked as well as the path: a hand-edited or
/// half-written file must not send SIGTERM to a pid that was never a dashboard.
fn pid_for_port(port: u16) -> Option<u64> {
    let path = pid_path_for(port)?;
    let v: Value = serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()?;
    (v.get("port").and_then(Value::as_u64) == Some(port as u64))
        .then(|| v.get("pid").and_then(Value::as_u64))
        .flatten()
}

fn clear_pidfile(port: u16) {
    if let Some(p) = pid_path_for(port) {
        let _ = std::fs::remove_file(p);
    }
}

/// Is anything at all listening on the port?  Used only to tell "something
/// that is not scout has it" from "the port is free".
fn port_is_free(port: u16) -> bool {
    match bind_tcp_reuse(port) {
        Ok(l) => {
            drop(l);
            true
        }
        Err(_) => false,
    }
}

fn url_for(port: u16) -> String {
    format!("http://localhost:{port}/")
}

/// Truncate the daemon log if it has grown past the cap.  Diagnostics, not
/// history — there is no rotation here on purpose.
fn trim_daemon_log(path: &Path) {
    if std::fs::metadata(path).map(|m| m.len() > MAX_DAEMON_LOG_BYTES).unwrap_or(false) {
        let _ = std::fs::write(path, b"");
    }
}

/// Start a detached daemon: re-exec self with `--foreground`, stdin from
/// `/dev/null`, stdout/stderr to the daemon log, in a new session.
///
/// `setsid` is the load-bearing part.  Without it a later `^C` in the launching
/// shell kills the dashboard and closing the terminal SIGHUPs it — both look
/// like random crashes.
fn spawn_daemon(port: u16) -> anyhow::Result<()> {
    use std::os::unix::process::CommandExt;

    let exe = std::env::current_exe()?;
    let log_path = daemon_log_path()
        .ok_or_else(|| anyhow::anyhow!("no state directory: $HOME and $XDG_STATE_HOME are unset"))?;
    if let Some(parent) = log_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    trim_daemon_log(&log_path);
    let log = std::fs::OpenOptions::new().create(true).append(true).open(&log_path)?;

    let mut cmd = std::process::Command::new(exe);
    cmd.args(["dashboard", "--foreground", "--port"])
        .arg(port.to_string())
        .stdin(std::process::Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log);
    unsafe {
        cmd.pre_exec(|| {
            // EPERM here means we are already a process-group leader, which is
            // the state setsid was trying to reach; either way the child is out
            // of the launching shell's foreground group.
            libc::setsid();
            Ok(())
        });
    }
    cmd.spawn()?;

    // Wait for the port to answer as scout rather than trusting the fork: a
    // daemon that dies on a bind error must not be reported as started.
    for _ in 0..40 {
        if probe(port).is_some() {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(125));
    }
    Err(anyhow::anyhow!(
        "daemon did not come up on port {port} within 5s — see {}",
        log_path.display()
    ))
}

fn open_browser(url: &str) {
    let opener = if cfg!(target_os = "macos") { "open" } else { "xdg-open" };
    let _ = std::process::Command::new(opener)
        .arg(url)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

/// `scout dashboard` — start, idempotently.
///
/// Idempotence is what makes this safe in a shell profile or a SessionStart
/// hook, which is what turns P7 from a feature into a config flip.
fn start(port: u16, open: bool) -> anyhow::Result<()> {
    let had_pidfile = pid_for_port(port).is_some();
    if let Some(status) = probe(port) {
        let pid = status.get("pid").and_then(Value::as_u64).unwrap_or(0);
        println!("scout dashboard already running (pid {pid}) — {}", url_for(port));
        if open {
            open_browser(&url_for(port));
        }
        return Ok(());
    }
    if had_pidfile {
        println!("scout dashboard: stale pidfile (port {port} not answering) — cleared, restarting");
        clear_pidfile(port);
    } else if !port_is_free(port) {
        anyhow::bail!(
            "port {port} is in use by something that is not scout — \
             free it, or pick another with --port / [dashboard] port"
        );
    }

    spawn_daemon(port)?;
    println!("scout dashboard started — {}", url_for(port));
    if open {
        open_browser(&url_for(port));
    }
    Ok(())
}

/// `--stop`: SIGTERM the recorded pid, wait up to 2s, remove the pidfile.
///
/// The pid comes from the pidfile when there is one and from the daemon's own
/// `/api/status` when there is not, so a dashboard whose pidfile was deleted is
/// still stoppable.
fn stop(port: u16) -> anyhow::Result<bool> {
    let live = probe(port);
    let pid = pid_for_port(port)
        .or_else(|| live.as_ref().and_then(|v| v.get("pid").and_then(Value::as_u64)));

    let Some(pid) = pid else {
        clear_pidfile(port);
        println!("scout dashboard: not running");
        return Ok(false);
    };
    if live.is_none() {
        clear_pidfile(port);
        println!("scout dashboard: not running (stale pidfile for pid {pid} cleared)");
        return Ok(false);
    }

    unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) };
    for _ in 0..20 {
        if probe(port).is_none() {
            clear_pidfile(port);
            println!("scout dashboard stopped (pid {pid})");
            return Ok(true);
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    clear_pidfile(port);
    println!("scout dashboard: pid {pid} did not exit within 2s; pidfile removed");
    Ok(true)
}

fn human_secs(secs: u64) -> String {
    match secs {
        s if s < 60 => format!("{s}s"),
        s if s < 3600 => format!("{}m{}s", s / 60, s % 60),
        s if s < 86_400 => format!("{}h{}m", s / 3600, (s % 3600) / 60),
        s => format!("{}d{}h", s / 86_400, (s % 86_400) / 3600),
    }
}

/// `--status`: running/not, pid, port, uptime, log path. Exit 0/1.
fn status(port: u16) -> anyhow::Result<bool> {
    let log = daemon_log_path().map(|p| p.display().to_string()).unwrap_or_default();
    match probe(port) {
        Some(v) => {
            let get = |k: &str| v.get(k).and_then(Value::as_u64).unwrap_or(0);
            println!("scout dashboard: running");
            println!("  url      {}", url_for(port));
            println!("  pid      {}", get("pid"));
            println!("  port     {}", get("port"));
            println!("  uptime   {}", human_secs(get("uptime_secs")));
            println!("  version  {}", v.get("version").and_then(Value::as_str).unwrap_or("?"));
            println!("  calls    {} rows", v["overview"]["rows"].as_u64().unwrap_or(0));
            println!("  log      {log}");
            Ok(true)
        }
        None => {
            if pid_for_port(port).is_some() {
                clear_pidfile(port);
                println!("scout dashboard: not running (stale pidfile cleared)");
            } else {
                println!("scout dashboard: not running");
            }
            println!("  port     {port}");
            println!("  log      {log}");
            Ok(false)
        }
    }
}

/// Entry point for `scout dashboard`.
pub fn run(args: Args) -> anyhow::Result<()> {
    let port = resolve_port(args.port);

    if args.foreground {
        // Truncate here too: `--foreground` writes to the terminal, but the
        // detached start re-execs into this path with stderr on the log file.
        if let Some(p) = daemon_log_path() {
            trim_daemon_log(&p);
        }
        return run_foreground(port);
    }
    if args.status {
        return match status(port)? {
            true => Ok(()),
            false => std::process::exit(1),
        };
    }
    if args.stop {
        stop(port)?;
        return Ok(());
    }
    if args.restart {
        stop(port)?;
        return start(port, args.open);
    }
    start(port, args.open)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(json: &str) -> Row {
        Row::parse(json).expect("fixture must parse")
    }

    fn v2(id: &str, run: &str, ts: f64, extra: &str) -> String {
        format!(
            r#"{{"v":2,"id":"{id}","run":"{run}","ts":{ts},"via":"cli","tool":"find",
                "preset":"find_patterns","outcome":{{"kind":"ok"}},"ok":true,"ms":0{extra}}}"#
        )
        .replace('\n', "")
    }

    // ── Row parsing ──────────────────────────────────────────────────────

    #[test]
    fn a_pre_v2_row_is_skipped_not_synthesized() {
        // The six-field shape scout wrote before v2. It carries no `input`,
        // no `id` and no `via`, and the panes have nothing to show for it —
        // so the reader drops it rather than padding it out into a row whose
        // empty `input` is indistinguishable from a call that had none.
        let line =
            r#"{"ts":1770000000,"preset":"grep","tokens_in":1840,"tokens_out":210,"ms":3100,"ok":true}"#;
        assert!(Row::parse(line).is_none());
        assert!(is_legacy_record(line), "skipped on purpose, not malformed");
    }

    #[test]
    fn a_malformed_line_is_not_mistaken_for_old_history() {
        assert!(Row::parse("{not json").is_none());
        assert!(!is_legacy_record("{not json"), "this one really is broken");
    }

    #[test]
    fn skipped_and_malformed_lines_are_counted_apart() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("calls.jsonl");
        std::fs::write(
            &path,
            format!(
                "{}\n{}\n{}\n",
                r#"{"ts":1770000000,"preset":"grep","ok":true}"#,
                "{not json",
                v2("a-1", "a", 100.0, ""),
            ),
        )
        .expect("write fixture");
        let mut t = Tail::new(path);
        t.reload();
        assert_eq!(t.rows.len(), 1, "only the current-schema row is read");
        assert_eq!(t.skipped_legacy, 1);
        assert_eq!(t.parse_errors, 1);
    }

    #[test]
    fn a_v2_row_keeps_every_field_the_panes_read() {
        let r = row(
            r#"{"v":2,"id":"abc-1","run":"abc","op":"abc-op","ts":1770000000.482,"via":"mcp","tool":"find",
                "preset":"find_patterns","attempt":2,"project":"/p","model":"m","endpoint":"e",
                "input":{"question":"where"},"outcome":{"kind":"ok","summary":"8 patterns"},
                "raw_bytes":184320,"returned_bytes":1180,"tokens_in":1840,"tokens_out":210,
                "ms":3100,"ok":true}"#,
        );
        assert_eq!(r.id, "abc-1");
        assert_eq!(r.op, "abc-op");
        assert_eq!(r.run, "abc");
        assert_eq!(r.via, "mcp");
        assert_eq!(r.tool, "find");
        assert_eq!(r.attempt, 2);
        assert_eq!(r.input["question"], "where");
        assert_eq!(r.summary.as_deref(), Some("8 patterns"));
        assert_eq!(r.raw_bytes, 184_320);
        assert!((r.ts - 1_770_000_000.482).abs() < 0.001, "float ts survives");
    }

    #[test]
    fn a_bypassed_row_is_recognised_as_one() {
        let r = row(
            r#"{"v":2,"id":"x-1","preset":"extract","outcome":{"kind":"bypassed"},"ok":true,"ms":7}"#,
        );
        assert!(r.bypassed());
        assert!(r.ok, "scout answered the caller — no model needed");
    }

    // ── Grouping ─────────────────────────────────────────────────────────

    #[test]
    fn rows_of_one_op_collapse_into_one_operation() {
        // A `find` is three preset calls and one operation to a human.
        let rows: Vec<Row> = [
            v2("r-1", "r", 100.0, r#","op":"r-op","preset":"find_patterns""#),
            v2("r-2", "r", 101.0, r#","op":"r-op","preset":"grep""#),
            v2("r-3", "r", 102.0, r#","op":"r-op","preset":"find_reflect""#),
        ]
        .iter()
        .map(|s| row(s))
        .collect();
        let ops = group_ops(&rows);
        assert_eq!(ops, vec![vec![0, 1, 2]]);
        let v = op_json(&rows, &ops[0]);
        assert_eq!(v["n"], 3, "the ⋮n marker");
        assert_eq!(v["op"], "r-op");
        assert_eq!(v["rows"].as_array().unwrap().len(), 3);
    }

    #[test]
    fn two_operations_of_one_process_stay_apart() {
        // `scout mcp` is one process — one `run` — for a whole Claude Code
        // session, so `run` alone would collapse a session's every tool call
        // into one history row.  `op` is what keeps them apart.
        let rows: Vec<Row> = [
            v2("s-1", "s", 100.0, r#","op":"s-op1""#),
            v2("s-2", "s", 100.2, r#","op":"s-op2""#),
        ]
        .iter()
        .map(|s| row(s))
        .collect();
        assert_eq!(group_ops(&rows), vec![vec![0], vec![1]]);
    }

    #[test]
    fn one_operation_holds_together_across_any_wall_clock_gap() {
        // The record says these rows are one operation, so no elapsed time
        // between them — a slow model, a long local walk — can split them.
        let rows: Vec<Row> = [
            v2("s-1", "s", 100.0, r#","op":"s-op""#),
            v2("s-2", "s", 4000.0, r#","op":"s-op","ms":40000"#),
        ]
        .iter()
        .map(|s| row(s))
        .collect();
        assert_eq!(group_ops(&rows), vec![vec![0, 1]]);
    }

    #[test]
    fn interleaved_rows_of_two_concurrent_operations_still_sort_themselves_out() {
        // `mcp_server` dispatches on `spawn_blocking`, so two parallel tool
        // calls write into the log turn by turn.  Identity does not care.
        let rows: Vec<Row> = [
            v2("x-1", "s", 100.0, r#","op":"x""#),
            v2("y-1", "s", 100.1, r#","op":"y""#),
            v2("x-2", "s", 100.2, r#","op":"x""#),
            v2("y-2", "s", 100.3, r#","op":"y""#),
        ]
        .iter()
        .map(|s| row(s))
        .collect();
        assert_eq!(group_ops(&rows), vec![vec![0, 2], vec![1, 3]], "first seen, first listed");
    }

    #[test]
    fn a_row_with_no_op_is_an_operation_of_one() {
        // History written before `op` existed.  Each stands alone rather than
        // being guessed at: `op` falls back to the row's own id.
        let rows: Vec<Row> = [
            v2("a-1", "a", 100.0, ""),
            v2("a-2", "a", 100.1, ""),
            r#"{"v":2,"id":"lone-1","ts":1770000000,"preset":"grep","ok":true}"#.to_string(),
        ]
        .iter()
        .map(|s| row(s))
        .collect();
        assert_eq!(group_ops(&rows), vec![vec![0], vec![1], vec![2]]);
    }

    #[test]
    fn different_ops_never_merge() {
        let rows: Vec<Row> = [v2("a-1", "a", 100.0, ""), v2("b-1", "b", 100.5, "")]
            .iter()
            .map(|s| row(s))
            .collect();
        assert_eq!(group_ops(&rows), vec![vec![0], vec![1]]);
    }

    #[test]
    fn an_operation_sums_its_bytes_across_rows_rather_than_per_row() {
        // The whole reason the reader groups: P1's ledger lets the *first* row
        // claim `raw_bytes` and stamps `returned_bytes` on the *last*, so a
        // per-row ratio is meaningless and a per-run ratio is the metric.
        let rows: Vec<Row> = [
            v2("r-1", "r", 100.0, r#","op":"r-op","raw_bytes":100000,"tokens_in":10,"ms":900"#),
            v2("r-2", "r", 101.0, r#","op":"r-op","returned_bytes":1000,"tokens_in":5,"ms":800"#),
        ]
        .iter()
        .map(|s| row(s))
        .collect();
        let v = op_json(&rows, &group_ops(&rows)[0]);
        assert_eq!(v["raw_bytes"], 100_000);
        assert_eq!(v["returned_bytes"], 1000);
        assert_eq!(v["tokens_in"], 15);
        assert_eq!(v["ms"], 1700, "elapsed is the operation's, not one round's");
    }

    #[test]
    fn a_failed_row_names_the_operations_outcome() {
        let rows: Vec<Row> = [
            v2("r-1", "r", 100.0, r#","op":"r-op""#),
            r#"{"v":2,"id":"r-2","run":"r","op":"r-op","ts":101,"preset":"grep","ok":false,
                "outcome":{"kind":"endpoint_unreachable","summary":"down"}}"#
                .replace('\n', ""),
        ]
        .iter()
        .map(|s| row(s))
        .collect();
        let v = op_json(&rows, &group_ops(&rows)[0]);
        assert_eq!(v["ok"], false);
        assert_eq!(v["kind"], "endpoint_unreachable");
        assert_eq!(v["summary"], "down");
    }

    #[test]
    fn an_operations_input_comes_from_whichever_row_carries_one() {
        let rows: Vec<Row> = [
            v2("r-1", "r", 100.0, r#","op":"r-op","input":{}"#),
            v2("r-2", "r", 101.0, r#","op":"r-op","input":{"question":"where does it bind"}"#),
        ]
        .iter()
        .map(|s| row(s))
        .collect();
        let v = op_json(&rows, &group_ops(&rows)[0]);
        assert_eq!(v["input"]["question"], "where does it bind");
    }

    // ── Aggregates ───────────────────────────────────────────────────────

    fn tail_of(lines: &[String]) -> Tail {
        let mut t = Tail::new(PathBuf::from("/nonexistent"));
        t.rows = lines.iter().map(|s| row(s)).collect();
        t
    }

    #[test]
    fn percentiles_use_nearest_rank_and_survive_an_empty_set() {
        let v: Vec<u64> = (1..=100).collect();
        assert_eq!(percentile(&v, 50.0), 50);
        assert_eq!(percentile(&v, 95.0), 95);
        assert_eq!(percentile(&[], 95.0), 0);
        assert_eq!(percentile(&[42], 95.0), 42);
    }

    #[test]
    fn the_overview_keeps_bypasses_out_of_the_round_trip_counts() {
        let t = tail_of(&[
            v2("a-1", "a", 100.0, r#","tokens_in":10"#),
            r#"{"v":2,"id":"b-1","run":"b","ts":101,"preset":"extract","ok":true,"ms":7,
                "outcome":{"kind":"bypassed"},"raw_bytes":4096,"returned_bytes":4400}"#
                .replace('\n', ""),
        ]);
        let o = overview(&t);
        assert_eq!(o["bypassed"], 1);
        assert_eq!(o["calls"], 1, "a bypass is the absence of a round-trip");
        assert_eq!(o["ok"], 1);
        assert_eq!(o["raw_bytes"], 4096, "...but its bytes still count as context saved");
        assert_eq!(o["rows"], 2);
    }

    #[test]
    fn failures_are_grouped_by_outcome_kind_most_frequent_first() {
        let fail = |id: &str, kind: &str| {
            format!(
                r#"{{"v":2,"id":"{id}","run":"{id}","ts":100,"preset":"grep","ok":false,"outcome":{{"kind":"{kind}"}}}}"#
            )
        };
        let t = tail_of(&[
            fail("a", "endpoint_unreachable"),
            fail("b", "endpoint_unreachable"),
            fail("c", "timeout"),
            v2("d-1", "d", 100.0, ""),
        ]);
        let o = overview(&t);
        assert_eq!(o["failures"][0]["kind"], "endpoint_unreachable");
        assert_eq!(o["failures"][0]["count"], 2);
        assert_eq!(o["failures"][1]["kind"], "timeout");
        assert_eq!(o["calls"], 4, "failures still count as calls");
        assert_eq!(o["ok"], 1);
    }

    #[test]
    fn latency_percentiles_ignore_the_zero_ms_failure_rows() {
        let mut lines = vec![];
        for i in 1..=10 {
            lines.push(v2(&format!("a{i}-1"), &format!("a{i}"), 100.0, &format!(r#","ms":{}"#, i * 100)));
        }
        lines.push(
            r#"{"v":2,"id":"z-1","run":"z","ts":100,"preset":"grep","ok":false,"ms":0,"outcome":{"kind":"timeout"}}"#
                .to_string(),
        );
        let o = overview(&tail_of(&lines));
        assert_eq!(o["p50_ms"], 500);
        assert_eq!(o["p95_ms"], 1000);
    }

    #[test]
    fn the_stats_table_matches_scout_stats_shape() {
        let t = tail_of(&[
            v2("a-1", "a", 100.0, r#","preset":"grep","ms":100"#),
            v2("b-1", "b", 100.0, r#","preset":"grep","ms":300"#),
            v2("c-1", "c", 100.0, r#","preset":"extract","ms":50"#),
        ]);
        let s = stats_json(&t);
        assert_eq!(s["presets"][0]["preset"], "grep", "most calls first");
        assert_eq!(s["presets"][0]["calls"], 2);
        assert_eq!(s["presets"][0]["avg_ms_ok"], 200);
        assert_eq!(s["total_calls"], 3);
    }

    // ── History paging ───────────────────────────────────────────────────

    fn q(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn history_returns_operations_newest_first() {
        let t = tail_of(&[v2("a-1", "a", 100.0, ""), v2("b-1", "b", 200.0, "")]);
        let h = history_json(&t, &q(&[]));
        assert_eq!(h["ops"][0]["id"], "b-1");
        assert_eq!(h["ops"][1]["id"], "a-1");
        assert_eq!(h["cursor"], "b-1");
        assert_eq!(h["total_ops"], 2);
    }

    #[test]
    fn since_returns_only_what_the_caller_has_not_seen() {
        let t = tail_of(&[v2("a-1", "a", 100.0, ""), v2("b-1", "b", 200.0, "")]);
        let h = history_json(&t, &q(&[("since", "a-1")]));
        assert_eq!(h["ops"].as_array().unwrap().len(), 1);
        assert_eq!(h["ops"][0]["id"], "b-1");
        assert_eq!(h["resynced"], false);
    }

    #[test]
    fn an_unknown_cursor_resyncs_rather_than_erroring() {
        // The caller slept through a rotation.  A full page is the only
        // behavior that lets a browser tab recover on its own.
        let t = tail_of(&[v2("a-1", "a", 100.0, "")]);
        let h = history_json(&t, &q(&[("since", "gone-9")]));
        assert_eq!(h["ops"].as_array().unwrap().len(), 1);
        assert_eq!(h["resynced"], true);
    }

    #[test]
    fn history_filters_by_tool_via_project_and_failure() {
        let t = tail_of(&[
            v2("a-1", "a", 100.0, r#","via":"hook","tool":"shell_safety","project":"/one""#),
            v2("b-1", "b", 200.0, r#","via":"mcp","tool":"grep","project":"/two""#),
            r#"{"v":2,"id":"c-1","run":"c","ts":300,"via":"mcp","tool":"grep","preset":"grep",
                "ok":false,"outcome":{"kind":"timeout"}}"#
                .replace('\n', ""),
        ]);
        let ids = |params: &[(&str, &str)]| -> Vec<String> {
            history_json(&t, &q(params))["ops"]
                .as_array()
                .unwrap()
                .iter()
                .map(|o| o["id"].as_str().unwrap().to_string())
                .collect()
        };
        assert_eq!(ids(&[("via", "hook")]), vec!["a-1"]);
        assert_eq!(ids(&[("tool", "grep")]), vec!["c-1", "b-1"]);
        assert_eq!(ids(&[("project", "/two")]), vec!["b-1"]);
        assert_eq!(ids(&[("failed", "1")]), vec!["c-1"]);
        // An empty value is "no filter", not "match the empty string" — a
        // browser sending `&tool=` must not blank the view.
        assert_eq!(ids(&[("tool", "")]).len(), 3);
    }

    #[test]
    fn limit_caps_the_page_at_the_newest_operations() {
        let lines: Vec<String> = (1..=10)
            .map(|i| v2(&format!("a{i}-1"), &format!("a{i}"), 100.0 + i as f64 * 10.0, ""))
            .collect();
        let h = history_json(&tail_of(&lines), &q(&[("limit", "3")]));
        let ops = h["ops"].as_array().unwrap();
        assert_eq!(ops.len(), 3);
        assert_eq!(ops[0]["id"], "a10-1", "newest first");
    }

    #[test]
    fn a_call_lookup_finds_the_operation_from_any_of_its_rows() {
        let t = tail_of(&[
            v2("r-1", "r", 100.0, r#","op":"r-op""#),
            v2("r-2", "r", 101.0, r#","op":"r-op""#),
        ]);
        for id in ["r-1", "r-2"] {
            let v = call_json(&t, id).expect("both rows reach the same operation");
            assert_eq!(v["id"], "r-1");
            assert_eq!(v["n"], 2);
            assert!(v["rows"][0].get("system").is_none(), "no live cache in this test");
        }
        assert!(call_json(&t, "nope").is_none());
    }

    #[test]
    fn inflight_rows_overlay_the_history_and_do_not_become_the_cursor() {
        let t = tail_of(&[v2("a-1", "a", 100.0, r#","op":"a-op""#)]);
        let live = crate::live::LiveStore::new();
        live.apply_json(
            &serde_json::json!({
                "v": 1, "id": "b-1", "run": "b", "op": "b-op",
                "kind": "call.start", "ts": 200.0, "tool": "grep", "preset": "grep",
                "via": "mcp", "system": "S", "user": "U",
            })
            .to_string(),
        );
        let h = history_with_live(&t, Some(&live), &q(&[]));
        let ops = h["ops"].as_array().unwrap();
        assert_eq!(ops.len(), 2);
        assert_eq!(h["cursor"], "a-1", "cursor stays on the log, never inflight");
        assert!(ops.iter().any(|o| o["id"] == "b-1" && o["kind"] == "running"));
    }

    #[test]
    fn a_live_round_joins_its_logged_siblings_by_op() {
        let t = tail_of(&[v2("r-1", "r", 100.0, r#","op":"r-op","tool":"find""#)]);
        let live = crate::live::LiveStore::new();
        live.apply_json(
            &serde_json::json!({
                "v": 1, "id": "r-2", "run": "r", "op": "r-op",
                "kind": "call.start", "ts": 101.0, "tool": "find", "preset": "grep",
            })
            .to_string(),
        );
        let h = history_with_live(&t, Some(&live), &q(&[]));
        let ops = h["ops"].as_array().unwrap();
        assert_eq!(ops.len(), 1, "same op is one history row");
        assert_eq!(ops[0]["n"], 2);
    }

    #[test]
    fn a_logged_id_reaps_inflight_and_keeps_bodies() {
        let t = tail_of(&[v2("r-1", "r", 100.0, r#","op":"r-op""#)]);
        let live = crate::live::LiveStore::new();
        live.apply_json(
            &serde_json::json!({
                "v": 1, "id": "r-1", "run": "r", "op": "r-op",
                "kind": "call.start", "ts": 99.0, "tool": "find", "preset": "find_patterns",
                "system": "SYS", "user": "USR",
            })
            .to_string(),
        );
        live.apply_json(
            &serde_json::json!({
                "v": 1, "id": "r-1", "run": "r", "op": "r-op",
                "kind": "call.end", "response": "OK",
                "outcome": {"kind": "ok"},
            })
            .to_string(),
        );
        let h = history_with_live(&t, Some(&live), &q(&[]));
        assert_eq!(h["ops"].as_array().unwrap().len(), 1);
        let v = call_with_live(&t, Some(&live), "r-1").unwrap();
        assert_eq!(v["rows"][0]["system"], "SYS");
        assert_eq!(v["rows"][0]["user"], "USR");
        assert_eq!(v["rows"][0]["response"], "OK");
        assert!(live.inflight_rows().is_empty(), "reaped once the log has the id");
    }

    #[test]
    fn find_rounds_ride_on_the_call_route_but_not_on_history() {
        let t = tail_of(&[v2("r-1", "r", 100.0, r#","op":"r-op""#)]);
        let live = crate::live::LiveStore::new();
        for (round, kind) in [(1, "patterns"), (1, "hits"), (2, "patterns")] {
            live.apply_json(
                &serde_json::json!({
                    "v": 1, "id": "r-op", "run": "r", "op": "r-op", "ts": 100.0,
                    "kind": format!("find.{kind}"), "round": round, "union": 4,
                })
                .to_string(),
            );
        }
        let v = call_with_live(&t, Some(&live), "r-1").unwrap();
        let rounds = v["find_rounds"].as_array().unwrap();
        assert_eq!(rounds.len(), 2);
        assert_eq!(rounds[0]["round"], 1);
        assert_eq!(rounds[0]["hits"]["union"], 4);
        assert!(rounds[1]["hits"].is_null());

        // History stays a summary: 300 operations' worth of round internals
        // would dwarf the rows they belong to.
        let h = history_with_live(&t, Some(&live), &q(&[]));
        assert!(h["ops"][0]["find_rounds"].is_null());
    }

    #[test]
    fn an_operation_with_no_captured_rounds_says_nothing_about_them() {
        let t = tail_of(&[v2("r-1", "r", 100.0, r#","op":"r-op""#)]);
        let live = crate::live::LiveStore::new();
        let v = call_with_live(&t, Some(&live), "r-1").unwrap();
        assert!(v.get("find_rounds").is_none(), "absent, not an empty array");
    }

    // ── The tailing reader ───────────────────────────────────────────────

    fn append(path: &Path, line: &str) {
        let mut f = std::fs::OpenOptions::new().create(true).append(true).open(path).unwrap();
        writeln!(f, "{line}").unwrap();
    }

    #[test]
    fn the_reader_reads_only_the_tail_delta() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("calls.jsonl");
        append(&path, &v2("a-1", "a", 100.0, ""));
        let mut t = Tail::new(path.clone());
        t.refresh();
        assert_eq!(t.rows.len(), 1);
        let after_first = t.offset;

        // An unchanged file costs one stat and adds nothing.
        t.refresh();
        assert_eq!(t.rows.len(), 1);
        assert_eq!(t.offset, after_first);
        assert_eq!(t.reloads, 1, "only the initial load");

        append(&path, &v2("b-1", "b", 200.0, ""));
        t.refresh();
        assert_eq!(t.rows.len(), 2);
        assert_eq!(t.rows[1].id, "b-1");
        assert_eq!(t.reloads, 1, "an append is not a rotation");
    }

    #[test]
    fn a_half_written_line_is_re_read_rather_than_parsed_as_garbage() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("calls.jsonl");
        append(&path, &v2("a-1", "a", 100.0, ""));
        let mut t = Tail::new(path.clone());
        t.refresh();

        // A writer caught mid-`writeln!`.
        let partial = v2("b-1", "b", 200.0, "");
        let (head, tail_str) = partial.split_at(20);
        let mut f = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
        f.write_all(head.as_bytes()).unwrap();
        drop(f);
        t.refresh();
        assert_eq!(t.rows.len(), 1, "the partial record must not be parsed");
        assert_eq!(t.parse_errors, 0, "...nor counted as malformed");

        let mut f = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
        writeln!(f, "{tail_str}").unwrap();
        drop(f);
        t.refresh();
        assert_eq!(t.rows.len(), 2, "the completed record lands on the next poll");
        assert_eq!(t.rows[1].id, "b-1");
    }

    #[test]
    fn the_reader_survives_a_rotation_mid_read() {
        // The most likely source of a dashboard that silently stops updating
        // after a few days: the writers rotate underneath a long-lived reader.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("calls.jsonl");
        let rotated = rotated_path(&path);

        for i in 1..=3 {
            append(&path, &v2(&format!("old{i}-1"), &format!("old{i}"), 100.0 + i as f64, ""));
        }
        let mut t = Tail::new(path.clone());
        t.refresh();
        assert_eq!(t.rows.len(), 3);
        let offset_before = t.offset;

        // Exactly what `stats::append_line` does at the cap: rename, reopen,
        // write the record that triggered it.  The new file is shorter than
        // the old offset *and* has a different inode.
        std::fs::rename(&path, &rotated).unwrap();
        append(&path, &v2("new1-1", "new1", 200.0, ""));
        assert!(std::fs::metadata(&path).unwrap().len() < offset_before);

        t.refresh();
        assert_eq!(t.reloads, 2, "the rotation forced a reload");
        assert_eq!(t.rows.len(), 4, "history spans the rotation, not restarts at it");
        let ids: Vec<&str> = t.rows.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(ids, vec!["old1-1", "old2-1", "old3-1", "new1-1"]);

        // ...and the reader keeps tailing the new generation afterwards.
        append(&path, &v2("new2-1", "new2", 201.0, ""));
        t.refresh();
        assert_eq!(t.rows.len(), 5);
        assert_eq!(t.reloads, 2, "one rotation, one reload");
    }

    #[test]
    fn a_truncated_log_is_treated_as_a_rotation() {
        // Same inode, shorter than the last offset: someone truncated the file
        // in place.  Re-reading is the only safe answer.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("calls.jsonl");
        append(&path, &v2("a-1", "a", 100.0, ""));
        append(&path, &v2("b-1", "b", 101.0, ""));
        let mut t = Tail::new(path.clone());
        t.refresh();
        assert_eq!(t.rows.len(), 2);

        std::fs::write(&path, format!("{}\n", v2("c-1", "c", 102.0, ""))).unwrap();
        t.refresh();
        assert_eq!(t.rows.len(), 1);
        assert_eq!(t.rows[0].id, "c-1");
    }

    #[test]
    fn a_missing_log_is_not_an_error_and_is_picked_up_when_it_appears() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("calls.jsonl");
        let mut t = Tail::new(path.clone());
        t.refresh();
        assert!(t.rows.is_empty());
        append(&path, &v2("a-1", "a", 100.0, ""));
        t.refresh();
        assert_eq!(t.rows.len(), 1);
    }

    #[test]
    fn unreadable_lines_are_counted_not_fatal() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("calls.jsonl");
        append(&path, &v2("a-1", "a", 100.0, ""));
        append(&path, "not json at all");
        append(&path, &v2("b-1", "b", 101.0, ""));
        let mut t = Tail::new(path);
        t.refresh();
        assert_eq!(t.rows.len(), 2);
        assert_eq!(t.parse_errors, 1);
    }

    // ── HTTP plumbing ────────────────────────────────────────────────────

    #[test]
    fn query_values_are_percent_decoded() {
        let p = parse_query("tool=check_output&project=%2Fhome%2Fjosh%2FProjects%2Fscout&failed=1");
        assert_eq!(p["tool"], "check_output");
        assert_eq!(p["project"], "/home/josh/Projects/scout");
        assert_eq!(p["failed"], "1");
        assert_eq!(url_decode("a+b%20c"), "a b c");
        // A stray '%' must not panic or eat the rest of the value.
        assert_eq!(url_decode("100%"), "100%");
    }

    #[test]
    fn a_percent_before_a_multibyte_char_does_not_panic() {
        // `%` followed by `€` (a 3-byte UTF-8 character) used to slice
        // `&s[i+1..i+3]` as a `&str` and land mid-codepoint: "byte index 3 is
        // not a char boundary". Neither byte of the pair is a hex digit, so
        // this is not valid percent-encoding and must be passed through.
        assert_eq!(url_decode("%€"), "%€");
        // Reachable end to end via `parse_query`, e.g.
        // `GET /api/history?since=%€`.
        let p = parse_query("since=%€");
        assert_eq!(p["since"], "%€");
    }

    /// A `State` with nothing behind it: no log, no live socket, no daemon.
    fn test_state(port: u16, live: Arc<crate::live::LiveStore>) -> State {
        State {
            tail: Mutex::new(Tail::new(PathBuf::from("/nonexistent"))),
            live,
            reach: Mutex::new(Reach::default()),
            started: SystemTime::now(),
            port,
            live_socket: None,
        }
    }

    #[test]
    fn the_status_marker_is_what_a_probe_looks_for() {
        let state = test_state(13001, Arc::new(crate::live::LiveStore::new()));
        let v = state.status_json();
        assert_eq!(v["service"], SERVICE, "liveness is decided by this field, not the pid");
        assert_eq!(v["version"], env!("CARGO_PKG_VERSION"));
        assert!(v["overview"].is_object());
        assert!(v["llm"].is_object());
        assert_eq!(v["live"]["bound"], false);
        assert_eq!(v["live"]["streams"], 0);
    }

    /// Serve exactly one connection on an ephemeral port and hand the caller
    /// the client end, so a test can drive `handle` the way a browser does.
    ///
    /// The port is only known after the bind, and `Host:` has to carry it —
    /// hence the closure rather than a literal request.
    fn serve_one(
        live: Arc<crate::live::LiveStore>,
        build: impl FnOnce(u16) -> Vec<u8>,
    ) -> (TcpStream, u16) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let state = Arc::new(test_state(addr.port(), live));
        std::thread::spawn(move || {
            let (s, _) = listener.accept().unwrap();
            handle(&state, s);
        });
        let mut c = TcpStream::connect(addr).unwrap();
        c.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        c.write_all(&build(addr.port())).unwrap();
        (c, addr.port())
    }

    #[test]
    fn stream_is_sse_not_501() {
        let (mut c, _) = serve_one(Arc::new(crate::live::LiveStore::new()), |port| {
            format!("GET /api/stream HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\r\n").into_bytes()
        });
        c.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let mut buf = [0u8; 256];
        let n = c.read(&mut buf).unwrap_or(0);
        let text = String::from_utf8_lossy(&buf[..n]);
        assert!(text.contains("text/event-stream"), "{text}");
        assert!(!text.contains("501"), "{text}");
        drop(c);
    }

    /// Spin up a real listener on `handle`, send a request over a real socket,
    /// and return whatever comes back before the peer closes.
    ///
    /// Exercises the bounded header reader (`read_header_line`) exactly as a
    /// real client would drive it — through `TcpStream`, not by calling
    /// internal parsing functions directly — since the bug this guards
    /// against (unbounded growth reading the request line and headers off
    /// the wire) only shows up at that layer.  The `Host` gate lives at the
    /// same layer, and is driven the same way.
    fn send_built(build: impl FnOnce(u16) -> Vec<u8>) -> String {
        let (mut c, _) = serve_one(Arc::new(crate::live::LiveStore::new()), build);
        let mut buf = Vec::new();
        c.read_to_end(&mut buf).unwrap_or(0);
        String::from_utf8_lossy(&buf).into_owned()
    }

    fn send_raw(request: &[u8]) -> String {
        let owned = request.to_vec();
        send_built(move |_| owned)
    }

    /// `GET <target>` with one extra header line (already `\r\n`-terminated,
    /// or empty), carrying whatever `Host` the caller wants.
    fn get_with_host(target: &str, host: &str, extra: &str) -> String {
        send_built(|port| {
            let host = host.replace("{port}", &port.to_string());
            format!("GET {target} HTTP/1.1\r\n{host}{extra}\r\n").into_bytes()
        })
    }

    #[test]
    fn an_over_long_request_line_is_rejected_without_unbounded_growth() {
        // Comfortably past `MAX_HEADER_LINE_BYTES` (8 KiB), but small enough
        // to fit in one write without needing a concurrent reader — this is
        // about proving the *server* stops growing its buffer at the cap,
        // not about proving TCP flow control.  The request line is rejected
        // before any header is read, so `Host` never comes into it.
        let path = "/".to_string() + &"a".repeat(MAX_HEADER_LINE_BYTES + 1000);
        let request = format!("GET {path} HTTP/1.1\r\nHost: x\r\n\r\n");
        let resp = send_raw(request.as_bytes());
        assert!(resp.starts_with("HTTP/1.0 431"), "{resp}");
        assert!(resp.contains("too large"), "{resp}");
    }

    #[test]
    fn too_many_headers_is_rejected() {
        let mut request = String::from("GET /api/status HTTP/1.1\r\n");
        for i in 0..(MAX_HEADER_COUNT + 10) {
            request.push_str(&format!("X-Custom-{i}: v\r\n"));
        }
        request.push_str("\r\n");
        let resp = send_raw(request.as_bytes());
        assert!(resp.starts_with("HTTP/1.0 431"), "{resp}");
        assert!(resp.contains("too many headers"), "{resp}");
    }

    #[test]
    fn a_well_formed_request_still_routes_normally_through_the_bounded_reader() {
        let resp = get_with_host("/api/status", "Host: 127.0.0.1:{port}\r\n", "Accept: */*\r\n");
        assert!(resp.starts_with("HTTP/1.0 200"), "{resp}");
        let body = resp.split("\r\n\r\n").nth(1).expect("a body after the headers");
        let v: Value = serde_json::from_str(body).expect("a JSON status body");
        assert_eq!(v["service"], SERVICE);
    }

    // ── DNS rebinding (`authority_is_ours`) ──────────────────────────────

    /// The attack, end to end: a page served from `attacker.example`, whose
    /// name re-resolves to 127.0.0.1, asking for every prompt body the daemon
    /// holds.  Before the gate this answered 200 with the lot.
    #[test]
    fn a_rebound_page_cannot_read_the_history() {
        for target in ["/", "/api/status", "/api/history", "/api/stats", "/api/call/x-1"] {
            let resp = get_with_host(target, "Host: attacker.example\r\n", "");
            assert!(resp.starts_with("HTTP/1.0 403"), "{target}: {resp}");
            assert!(resp.contains("not this dashboard"), "{target}: {resp}");
        }
        // Right name, wrong port: an attacker who guessed 13001 while this
        // daemon is elsewhere is still an attacker.
        let resp = get_with_host("/api/history", "Host: 127.0.0.1:1\r\n", "");
        assert!(resp.starts_with("HTTP/1.0 403"), "{resp}");
        // …and a name that merely ends in ours.
        let resp = get_with_host("/api/history", "Host: not-localhost:{port}\r\n", "");
        assert!(resp.starts_with("HTTP/1.0 403"), "{resp}");
    }

    /// HTTP/1.1 requires `Host`; a request without one has nothing to check.
    #[test]
    fn a_request_with_no_host_is_rejected() {
        let resp = get_with_host("/api/status", "", "");
        assert!(resp.starts_with("HTTP/1.0 403"), "{resp}");
        // Two of them is ambiguous, which is how such a gate gets walked past.
        let resp =
            get_with_host("/api/status", "Host: 127.0.0.1:{port}\r\n", "Host: attacker.example\r\n");
        assert!(resp.starts_with("HTTP/1.0 403"), "{resp}");
        assert!(resp.contains("ambiguous"), "{resp}");
    }

    /// Both authorities a browser can actually produce for this daemon.
    /// `dashboard.html` fetches its own origin, so breaking either breaks the
    /// page — `url_for` hands out `localhost`, and `probe` uses `127.0.0.1`.
    #[test]
    fn the_dashboards_own_origins_are_accepted() {
        for host in ["Host: 127.0.0.1:{port}\r\n", "Host: localhost:{port}\r\n", "Host: LocalHost:{port}\r\n"] {
            let resp = get_with_host("/api/status", host, "");
            assert!(resp.starts_with("HTTP/1.0 200"), "{host}: {resp}");
        }
        // The page itself, which is the one route a browser asks for by hand.
        let resp = get_with_host("/", "Host: localhost:{port}\r\n", "");
        assert!(resp.starts_with("HTTP/1.0 200"), "{resp}");
        assert!(resp.contains("text/html"), "{resp}");
    }

    /// A same-origin `fetch`/`EventSource` sends no `Origin` at all — and so
    /// does a rebound one, which is why `Host` is the load-bearing check. A
    /// present-and-foreign `Origin` can only be a real cross-origin attempt.
    #[test]
    fn a_foreign_origin_is_refused_and_our_own_is_not() {
        let resp = get_with_host(
            "/api/history",
            "Host: 127.0.0.1:{port}\r\n",
            "Origin: https://attacker.example\r\n",
        );
        assert!(resp.starts_with("HTTP/1.0 403"), "{resp}");
        assert!(resp.contains("cross-origin"), "{resp}");

        let resp =
            get_with_host("/api/status", "Host: 127.0.0.1:{port}\r\n", "Origin: null\r\n");
        assert!(resp.starts_with("HTTP/1.0 403"), "{resp}");

        let resp = send_built(|port| {
            format!(
                "GET /api/status HTTP/1.1\r\nHost: localhost:{port}\r\n\
                 Origin: http://localhost:{port}\r\n\r\n"
            )
            .into_bytes()
        });
        assert!(resp.starts_with("HTTP/1.0 200"), "{resp}");
    }

    #[test]
    fn the_authority_gate_reads_the_way_a_browser_writes_it() {
        assert!(authority_is_ours(Some("127.0.0.1:13001"), 13001));
        assert!(authority_is_ours(Some("localhost:13001"), 13001));
        assert!(authority_is_ours(Some(" LOCALHOST:13001 "), 13001));
        assert!(!authority_is_ours(None, 13001), "HTTP/1.1 requires a Host");
        assert!(!authority_is_ours(Some(""), 13001));
        assert!(!authority_is_ours(Some("localhost"), 13001), "the port is half the check");
        assert!(!authority_is_ours(Some("localhost:13002"), 13001));
        assert!(!authority_is_ours(Some("attacker.example"), 13001));
        assert!(!authority_is_ours(Some("localhost.attacker.example:13001"), 13001));
        assert!(!authority_is_ours(Some("127.0.0.2:13001"), 13001), "not the bound address");
        assert!(!authority_is_ours(Some("[::1]:13001"), 13001), "the listener is v4-only");
        // Only on 80 may a browser leave the port off.
        assert!(authority_is_ours(Some("localhost"), 80));
        assert!(!authority_is_ours(Some("attacker.example"), 80));
    }

    /// `bound` was stamped once at startup and never revisited, so a daemon
    /// whose socket had been unlinked went on claiming `"bound": true` while
    /// receiving nothing — the one fact `/api/status` most needs to get right.
    #[test]
    fn bound_stops_being_true_once_the_socket_name_is_gone() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("live.sock");
        assert!(!socket_still_bound(&path), "nothing bound yet");

        let sock = std::os::unix::net::UnixDatagram::bind(&path).unwrap();
        assert!(socket_still_bound(&path));

        // What `on_terminate` and `bind_socket` both do, unconditionally.
        std::fs::remove_file(&path).unwrap();
        assert!(!socket_still_bound(&path), "the fd is fine; the name is what writers have");
        drop(sock);

        // A plain file at the name is not a socket either.
        std::fs::write(&path, b"").unwrap();
        assert!(!socket_still_bound(&path));
    }

    /// A stream slot is held for as long as the handler thread runs, so the
    /// handler has to notice the client leaving.  Eight of these is the whole
    /// `MAX_STREAMS` budget, and nobody closes dashboard tabs.
    #[test]
    fn a_stream_releases_its_slot_when_the_client_goes_away() {
        let live = Arc::new(crate::live::LiveStore::new());
        let (mut c, _) = serve_one(Arc::clone(&live), |port| {
            format!("GET /api/stream HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\r\n").into_bytes()
        });
        c.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let mut buf = [0u8; 256];
        assert!(c.read(&mut buf).unwrap_or(0) > 0, "SSE headers");
        assert_eq!(live.stream_count(), 1);
        assert_eq!(live.subscriber_count(), 1, "a slot and a subscription, in step");

        drop(c);
        // Writes to a closed peer succeed once (they are buffered) and fail on
        // the RST that comes back, so push until the handler notices.
        let mut released = false;
        for n in 0..200 {
            let ev = json!({
                "v": 1, "id": format!("s-{n}"), "run": "r", "op": format!("o-{n}"),
                "kind": "call.start", "ts": 1.0, "tool": "t", "preset": "t",
            });
            live.apply_json(&ev.to_string());
            if live.stream_count() == 0 {
                released = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(released, "the handler thread kept a MAX_STREAMS slot forever");
        assert_eq!(live.subscriber_count(), 0, "and the fan-out stopped feeding it");
    }

    #[test]
    fn human_secs_reads_like_an_uptime() {
        assert_eq!(human_secs(42), "42s");
        assert_eq!(human_secs(90), "1m30s");
        assert_eq!(human_secs(3700), "1h1m");
        assert_eq!(human_secs(90_000), "1d1h");
    }
}
