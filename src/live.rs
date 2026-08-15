//! The ephemeral live channel (SPEC-dashboard §2.5, P3).
//!
//! Short-lived scout processes send `call.start` / `call.end` over a
//! non-blocking unix datagram. The dashboard daemon is the only listener.
//! Every failure — nobody listening, a stale socket, a full buffer, a
//! payload that will not fit — drops the event. Telemetry is never allowed
//! to slow a tool call, least of all `shell_safety`.
//!
//! Bodies and in-flight rows live only in the daemon's memory. A restart
//! loses them; that is accepted.

use crate::stats::{self, CallRecord};
use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::io::{self, ErrorKind};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{self, SyncSender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

/// Hard cap on a serialized event. Unix datagrams fail `EMSGSIZE` above
/// `SO_SNDBUF`, which is not a portable promise; 64 KiB is safely under
/// every host this has to run on.
pub const MAX_DGRAM: usize = 64 * 1024;
/// In-memory body cache. A daemon restart empties it.
pub const MAX_BODIES: usize = 500;
/// In-memory `find` round cache, keyed by `op`. Smaller than `MAX_BODIES`:
/// only `find` writes here, and it is the rarest of the tools.
pub const MAX_FINDS: usize = 200;
/// Concurrent `/api/stream` connections. Each is a thread that lives as
/// long as the browser tab.
pub const MAX_STREAMS: usize = 8;
const SUB_CAP: usize = 32;
const RCVBUF: libc::c_int = 4 * 1024 * 1024;

// ── Path ────────────────────────────────────────────────────────────────────

/// Where the daemon binds and writers connect.
///
/// 1. `$SCOUT_LIVE_SOCK` — tests and custom layouts, same idea as
///    `$SCOUT_CALLS_LOG`.
/// 2. `$XDG_RUNTIME_DIR/scout/live.sock`
/// 3. the state dir (`$XDG_STATE_HOME/scout` or `~/.local/state/scout`)
pub fn socket_path() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("SCOUT_LIVE_SOCK") {
        if !p.is_empty() {
            return Some(PathBuf::from(p));
        }
    }
    if let Ok(runtime) = std::env::var("XDG_RUNTIME_DIR") {
        if !runtime.is_empty() {
            return Some(PathBuf::from(runtime).join("scout").join("live.sock"));
        }
    }
    state_dir().map(|d| d.join("live.sock"))
}

fn state_dir() -> Option<PathBuf> {
    std::env::var("XDG_STATE_HOME")
        .ok()
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var("HOME")
                .ok()
                .filter(|v| !v.is_empty())
                .map(|h| PathBuf::from(h).join(".local").join("state"))
        })
        .map(|base| base.join("scout"))
}

// ── Sender ──────────────────────────────────────────────────────────────────

enum Sock {
    /// First resolve saw `ENOENT`. Cached so a long-lived MCP server does
    /// not `connect(2)` on every Bash intercept.
    Missing,
    Ready(std::os::unix::net::UnixDatagram),
}

fn sender_slot() -> std::sync::MutexGuard<'static, Option<Sock>> {
    static SLOT: Mutex<Option<Sock>> = Mutex::new(None);
    SLOT.lock().unwrap_or_else(|e| e.into_inner())
}

fn resolve() -> Sock {
    let Some(path) = socket_path() else {
        return Sock::Missing;
    };
    match connect_nonblocking(&path) {
        Ok(s) => Sock::Ready(s),
        Err(e) if e.kind() == ErrorKind::NotFound => Sock::Missing,
        Err(_) => Sock::Missing,
    }
}

fn connect_nonblocking(path: &Path) -> io::Result<std::os::unix::net::UnixDatagram> {
    let sock = std::os::unix::net::UnixDatagram::unbound()?;
    sock.set_nonblocking(true)?;
    sock.connect(path)?;
    Ok(sock)
}

fn emit_bytes(bytes: &[u8]) {
    let mut slot = sender_slot();
    if slot.is_none() {
        *slot = Some(resolve());
    }
    match slot.as_mut() {
        Some(Sock::Missing) | None => {}
        Some(Sock::Ready(sock)) => match sock.send(bytes) {
            Ok(_) => {}
            Err(e) if e.kind() == ErrorKind::ConnectionRefused => {
                // Daemon restarted and unlinked the socket. Forget so the
                // next emit re-resolves; this is the one permitted reconnect.
                *slot = None;
            }
            Err(_) => {
                // EAGAIN, EMSGSIZE, anything else: drop, stay connected.
            }
        },
    }
}

/// `call.start` — resolved prompts. No-op for a silent record.
pub fn emit_start(rec: &CallRecord, system: &str, user: &str) {
    if rec.silent {
        return;
    }
    let ev = json!({
        "v": 1,
        "id": rec.id,
        "run": stats::run_id(),
        "op": rec.op,
        "seq": next_seq(),
        "kind": "call.start",
        "ts": now_ts(),
        "tool": rec.tool,
        "preset": rec.preset,
        "via": rec.via,
        "project": rec.project,
        "model": rec.model,
        "attempt": rec.attempt,
        "input": rec.input,
        "system": system,
        "user": user,
    });
    emit_bytes(&fit_event(ev));
}

/// `call.end` — full response (or none, on failure).
pub fn emit_end(rec: &CallRecord, response: Option<&str>) {
    if rec.silent {
        return;
    }
    let mut outcome = Map::new();
    outcome.insert("kind".into(), Value::from(rec.outcome.as_str()));
    if let Some(s) = &rec.summary {
        outcome.insert("summary".into(), Value::from(s.clone()));
    }
    let mut ev = json!({
        "v": 1,
        "id": rec.id,
        "run": stats::run_id(),
        "op": rec.op,
        "seq": next_seq(),
        "kind": "call.end",
        "ts": now_ts(),
        "ms": rec.ms,
        "outcome": Value::Object(outcome),
        "usage": {
            "prompt_tokens": rec.tokens_in,
            "completion_tokens": rec.tokens_out,
        },
    });
    if let Some(text) = response {
        ev["response"] = Value::from(text);
    }
    emit_bytes(&fit_event(ev));
}

/// Is anyone on the other end?
///
/// Resolves and caches the socket exactly as an emit would, so a caller whose
/// payload costs something to build can skip building it. Answering "no" is
/// one mutex acquisition after the first call — the same cached `Missing` that
/// keeps a long-lived MCP server from re-`connect(2)`ing per event.
pub fn is_listening() -> bool {
    let mut slot = sender_slot();
    if slot.is_none() {
        *slot = Some(resolve());
    }
    matches!(slot.as_ref(), Some(Sock::Ready(_)))
}

// ── call.token (P5) ─────────────────────────────────────────────────────────

/// Coalescing window for `call.token` (SPEC-dashboard §2.5).
///
/// ~1 content delta per token was measured (§5.5); one datagram per delta is
/// 86 syscalls for a short reply where a 50 ms timer is ~40, and a browser
/// cannot paint faster than a frame anyway.
pub const TOKEN_COALESCE_MS: u64 = 50;

/// Flush early once the buffer reaches this, whatever the timer says.
///
/// The fail-open ladder in `fit_event` has to be re-checked against every new
/// *shape* of payload (§7.6) — and a token event's shape is one long string
/// with no structure to shrink intelligently. Bounding the buffer well under
/// `MAX_DGRAM` means the elision path is unreachable in practice rather than
/// merely handled.
pub const MAX_TOKEN_CHUNK: usize = 8 * 1024;

/// Buffer deltas, emit at most one event per window.
///
/// The timer lives here rather than in `client.rs` so it can be driven with a
/// collecting closure and no socket: the sink `client.rs` sees is just "here
/// is some text", and everything about batching is testable from a unit test.
pub struct Coalescer<E: FnMut(&str, u64)> {
    buf: String,
    index: u64,
    last: std::time::Instant,
    interval: Duration,
    emit: E,
}

impl<E: FnMut(&str, u64)> Coalescer<E> {
    pub fn new(interval: Duration, emit: E) -> Self {
        Coalescer {
            buf: String::new(),
            index: 0,
            last: std::time::Instant::now(),
            interval,
            emit,
        }
    }

    /// Append one delta, flushing if the window has closed or the buffer is
    /// full. Never allocates a datagram per token, never blocks.
    pub fn push(&mut self, delta: &str) {
        self.buf.push_str(delta);
        if self.buf.len() >= MAX_TOKEN_CHUNK || self.last.elapsed() >= self.interval {
            self.flush();
        }
    }

    /// Emit whatever is buffered. A no-op on an empty buffer, so the end of a
    /// call that landed exactly on a window boundary does not emit an empty
    /// event.
    pub fn flush(&mut self) {
        self.last = std::time::Instant::now();
        if self.buf.is_empty() {
            return;
        }
        (self.emit)(&self.buf, self.index);
        self.index += 1;
        self.buf.clear();
    }
}

/// Run `f` with a delta sink that streams `call.token` events for `rec`.
///
/// The sink is a no-op — and no coalescer is built at all — for a silent
/// record or when nobody is listening. `is_listening` is the same cached
/// lookup `emit_find`'s callers use, so the cost of the whole feature with no
/// dashboard running is one mutex acquisition per call.
///
/// The final partial buffer is flushed when `f` returns, which is why this is
/// a scope rather than a constructor: a call whose last window is 3 ms long
/// must still deliver its tail.
pub fn with_token_stream<T>(rec: &CallRecord, f: impl FnOnce(&mut dyn FnMut(&str)) -> T) -> T {
    if rec.silent || !is_listening() {
        return f(&mut |_| {});
    }
    let id = rec.id.clone();
    let op = rec.op.clone();
    let mut co = Coalescer::new(Duration::from_millis(TOKEN_COALESCE_MS), move |text, index| {
        emit_token(&id, &op, index, text);
    });
    let out = f(&mut |delta| co.push(delta));
    co.flush();
    out
}

/// `call.token` — one coalesced run of text from a reply still in flight.
///
/// Deliberately thin: no usage, no outcome, nothing the authoritative
/// `call.end` also carries. The daemon fans these out and forgets them.
fn emit_token(id: &str, op: &str, index: u64, text: &str) {
    let ev = json!({
        "v": 1,
        "id": id,
        "run": stats::run_id(),
        "op": op,
        "seq": next_seq(),
        "kind": "call.token",
        "ts": now_ts(),
        "index": index,
        "text": text,
    });
    emit_bytes(&fit_event(ev));
}

/// `find.*` — one round's internals (SPEC-dashboard §2.5, P4).
///
/// `kind` is the bare suffix (`patterns`, `hits`, `rerank`, `reflect`) and
/// `fields` is merged over the envelope, so the shape of a round lives in
/// `find.rs` next to the values and the transport lives here.
///
/// Unlike `call.*` these describe an operation rather than a single
/// round-trip, so there is no row id to carry: `id` is the operation's own id.
/// That is minted from the same counter as a row id (`stats::next_id`) and so
/// can never collide with one. The daemon keys on `op` regardless — two
/// concurrent `find`s interleave over this channel exactly as their log rows
/// interleave in the log.
pub fn emit_find(op: &str, round: u64, kind: &str, fields: Value) {
    let mut ev = json!({
        "v": 1,
        "id": op,
        "run": stats::run_id(),
        "op": op,
        "seq": next_seq(),
        "kind": format!("find.{kind}"),
        "ts": now_ts(),
        "round": round,
    });
    if let (Some(obj), Some(extra)) = (ev.as_object_mut(), fields.as_object()) {
        for (k, v) in extra {
            obj.insert(k.clone(), v.clone());
        }
    }
    emit_bytes(&fit_event(ev));
}

fn next_seq() -> u64 {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    SEQ.fetch_add(1, Ordering::Relaxed) + 1
}

fn now_ts() -> f64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// Shrink body fields until the serialized event fits in `MAX_DGRAM`.
fn fit_event(mut v: Value) -> Vec<u8> {
    let mut bytes = v.to_string().into_bytes();
    if bytes.len() <= MAX_DGRAM {
        return bytes;
    }
    for key in ["system", "user", "response", "text"] {
        if let Some(Value::String(s)) = v.get_mut(key) {
            *s = elide(s, 8 * 1024);
        }
    }
    bytes = v.to_string().into_bytes();
    if bytes.len() <= MAX_DGRAM {
        return bytes;
    }
    // Still too big: keep shrinking the longest body field.
    for _ in 0..8 {
        let longest = ["system", "user", "response", "text"]
            .into_iter()
            .filter_map(|k| v.get(k).and_then(Value::as_str).map(|s| (k, s.len())))
            .max_by_key(|(_, n)| *n);
        let Some((key, len)) = longest else { break };
        if len < 64 {
            break;
        }
        if let Some(Value::String(s)) = v.get_mut(key) {
            *s = elide(s, len / 2);
        }
        bytes = v.to_string().into_bytes();
        if bytes.len() <= MAX_DGRAM {
            return bytes;
        }
    }
    // Lists next. `find.*` carries pattern, candidate and keep arrays, and a
    // wide search can make any of them long enough to matter on its own. Halve
    // the longest repeatedly and say so: a partial list is worth more than a
    // dropped event, and a reader that cannot tell partial from complete would
    // be worse than either.
    for _ in 0..16 {
        let Some((key, len)) = longest_array(&v) else { break };
        if len < 2 {
            break;
        }
        if let Some(Value::Array(a)) = v.get_mut(&key) {
            a.truncate(len / 2);
        }
        if let Some(obj) = v.as_object_mut() {
            obj.insert("truncated".into(), Value::Bool(true));
        }
        bytes = v.to_string().into_bytes();
        if bytes.len() <= MAX_DGRAM {
            return bytes;
        }
    }

    // Last resort: drop the bodies entirely. Metadata still has to land.
    for key in ["system", "user", "response", "text"] {
        if let Some(obj) = v.as_object_mut() {
            obj.remove(key);
        }
    }
    let mut bytes = v.to_string().into_bytes();
    if bytes.len() > MAX_DGRAM {
        bytes.truncate(MAX_DGRAM);
    }
    bytes
}

/// The longest top-level array field, if any: `fit_event`'s next thing to trim.
fn longest_array(v: &Value) -> Option<(String, usize)> {
    v.as_object()?
        .iter()
        .filter_map(|(k, val)| val.as_array().map(|a| (k.clone(), a.len())))
        .filter(|(_, n)| *n > 0)
        .max_by_key(|(_, n)| *n)
}

fn elide(s: &str, budget: usize) -> String {
    if s.len() <= budget {
        return s.to_string();
    }
    let mark_budget = 40; // room for the marker with a large N
    let keep = budget.saturating_sub(mark_budget) / 2;
    if keep < 8 {
        return format!("…[{} bytes]…", s.len());
    }
    let head = floor_char(s, keep);
    let tail_start = s.len().saturating_sub(keep);
    let tail = ceil_char(s, tail_start);
    let skipped = s.len().saturating_sub(head.len() + tail.len());
    format!("{head}\n…[{skipped} bytes elided]…\n{tail}")
}

fn floor_char(s: &str, max: usize) -> &str {
    let mut end = max.min(s.len());
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

fn ceil_char(s: &str, mut start: usize) -> &str {
    if start > s.len() {
        start = s.len();
    }
    while start < s.len() && !s.is_char_boundary(start) {
        start += 1;
    }
    &s[start..]
}

// ── Bind / unlink (daemon side) ─────────────────────────────────────────────

/// Bind the live socket. Not fatal for the daemon if this fails.
pub fn bind_socket() -> io::Result<std::os::unix::net::UnixDatagram> {
    let path = socket_path().ok_or_else(|| io::Error::other("no live socket path"))?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let _ = std::fs::remove_file(&path);
    let sock = std::os::unix::net::UnixDatagram::bind(&path)?;
    let _ = sock.set_nonblocking(true);
    raise_rcvbuf(&sock);
    Ok(sock)
}

fn raise_rcvbuf(sock: &std::os::unix::net::UnixDatagram) {
    use std::os::unix::io::AsRawFd;
    let fd = sock.as_raw_fd();
    let buf = RCVBUF;
    unsafe {
        libc::setsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_RCVBUF,
            &buf as *const _ as *const libc::c_void,
            std::mem::size_of_val(&buf) as libc::socklen_t,
        );
    }
}

/// Async-signal-safe path for the SIGTERM handler.
pub fn socket_cstring() -> Option<std::ffi::CString> {
    let path = socket_path()?;
    std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).ok()
}



// ── Store ───────────────────────────────────────────────────────────────────

#[derive(Clone, Debug, Default)]
pub struct Bodies {
    pub system: Option<String>,
    pub user: Option<String>,
    pub response: Option<String>,
}

/// A synthetic row the history overlay can render. Mirrors `dashboard::Row`
/// without creating a module cycle.
#[derive(Clone, Debug)]
pub struct LiveRow {
    pub id: String,
    pub op: String,
    pub run: String,
    pub ts: f64,
    pub via: String,
    pub tool: String,
    pub preset: String,
    pub attempt: u64,
    pub project: Option<String>,
    pub model: Option<String>,
    pub endpoint: Option<String>,
    pub input: Value,
    pub kind: String,
    pub summary: Option<String>,
    pub raw_bytes: u64,
    pub returned_bytes: u64,
    pub tokens_in: u64,
    pub tokens_out: u64,
    pub ms: u64,
    pub ok: bool,
}

pub struct LiveStore {
    inner: Mutex<Inner>,
    streams: AtomicUsize,
    bound: AtomicBool,
}

struct Inner {
    inflight: HashMap<String, LiveRow>,
    body_order: VecDeque<String>,
    bodies: HashMap<String, Bodies>,
    /// `op` → round → part name (`patterns` … `reflect`) → the event.
    ///
    /// Keyed by `op` and by round number, never by arrival order: rounds of
    /// two concurrent `find`s interleave, and a dashboard that joined mid-run
    /// holds round 3 without ever having seen 1 or 2. A `BTreeMap` so the
    /// rounds come back ordered whatever order they landed in.
    finds: HashMap<String, BTreeMap<u64, Map<String, Value>>>,
    find_order: VecDeque<String>,
    subs: Vec<SyncSender<String>>,
}

impl Default for LiveStore {
    fn default() -> Self {
        Self::new()
    }
}

impl LiveStore {
    pub fn new() -> Self {
        LiveStore {
            inner: Mutex::new(Inner {
                inflight: HashMap::new(),
                body_order: VecDeque::new(),
                bodies: HashMap::new(),
                finds: HashMap::new(),
                find_order: VecDeque::new(),
                subs: Vec::new(),
            }),
            streams: AtomicUsize::new(0),
            bound: AtomicBool::new(false),
        }
    }

    pub fn set_bound(&self, bound: bool) {
        self.bound.store(bound, Ordering::Relaxed);
    }

    pub fn bound(&self) -> bool {
        self.bound.load(Ordering::Relaxed)
    }

    pub fn stream_count(&self) -> usize {
        self.streams.load(Ordering::Relaxed)
    }

    /// Reserve a stream slot. False if the cap is already hit.
    pub fn try_acquire_stream(&self) -> bool {
        let mut current = self.streams.load(Ordering::Relaxed);
        loop {
            if current >= MAX_STREAMS {
                return false;
            }
            match self.streams.compare_exchange_weak(
                current,
                current + 1,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => return true,
                Err(seen) => current = seen,
            }
        }
    }

    pub fn release_stream(&self) {
        self.streams.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| Some(n.saturating_sub(1))).ok();
    }

    pub fn subscribe(&self) -> mpsc::Receiver<String> {
        let (tx, rx) = mpsc::sync_channel(SUB_CAP);
        self.lock().subs.push(tx);
        rx
    }

    pub fn apply_json(&self, raw: &str) -> bool {
        let Ok(v) = serde_json::from_str::<Value>(raw) else {
            return false;
        };
        let Some(id) = v.get("id").and_then(Value::as_str).map(str::to_string) else {
            return false;
        };
        let kind = v.get("kind").and_then(Value::as_str).unwrap_or("");
        {
            let mut inner = self.lock();
            match kind {
                "call.start" => {
                    let row = row_from_start(&v, &id);
                    inner.inflight.insert(id.clone(), row);
                    let mut b = inner.bodies.get(&id).cloned().unwrap_or_default();
                    if let Some(s) = v.get("system").and_then(Value::as_str) {
                        b.system = Some(s.to_string());
                    }
                    if let Some(s) = v.get("user").and_then(Value::as_str) {
                        b.user = Some(s.to_string());
                    }
                    put_bodies(&mut inner, id, b);
                }
                "call.end" => {
                    if let Some(existing) = inner.inflight.get_mut(&id) {
                        apply_end(existing, &v);
                    } else {
                        inner.inflight.insert(id.clone(), row_from_end(&v, &id));
                    }
                    if let Some(s) = v.get("response").and_then(Value::as_str) {
                        let mut b = inner.bodies.get(&id).cloned().unwrap_or_default();
                        b.response = Some(s.to_string());
                        put_bodies(&mut inner, id, b);
                    }
                }
                // Fan out and forget. A token run is only meaningful in
                // motion (§2.5): the authoritative body arrives on `call.end`,
                // and storing partial text here would let a failed call leave
                // half a reply behind looking like a whole one.
                "call.token" => {
                    if v.get("text").and_then(Value::as_str).is_none() {
                        return false;
                    }
                }
                k if k.starts_with("find.") => {
                    let part = &k["find.".len()..];
                    if !matches!(part, "patterns" | "hits" | "rerank" | "reflect") {
                        return false;
                    }
                    // `op` is the grouping key. A find event minted before the
                    // ledger existed would have none; fall back to its own id
                    // so it lands somewhere rather than nowhere.
                    let op = opt_s(&v, "op").unwrap_or_else(|| id.clone());
                    let round = v.get("round").and_then(Value::as_u64).unwrap_or(0);
                    let part = part.to_string();
                    put_find(&mut inner, op, round, part, v.clone());
                }
                _ => return false,
            }
            let dead: Vec<usize> = inner
                .subs
                .iter()
                .enumerate()
                .filter_map(|(i, tx)| tx.try_send(raw.to_string()).err().map(|_| i))
                .collect();
            for i in dead.into_iter().rev() {
                inner.subs.swap_remove(i);
            }
        }
        true
    }

    /// Drop inflight rows whose id has landed in the log. Bodies stay.
    pub fn reap(&self, log_ids: impl IntoIterator<Item = impl AsRef<str>>) {
        let mut inner = self.lock();
        for id in log_ids {
            inner.inflight.remove(id.as_ref());
        }
    }

    pub fn inflight_rows(&self) -> Vec<LiveRow> {
        self.lock().inflight.values().cloned().collect()
    }

    pub fn bodies_of(&self, id: &str) -> Option<Bodies> {
        self.lock().bodies.get(id).cloned()
    }

    /// The `find` rounds captured for one operation, oldest round first.
    ///
    /// `None` when nothing was captured — which is the ordinary answer for
    /// every tool that is not `find`, for a `find` that ran before the daemon
    /// started, and for one whose rounds have since been evicted. There is no
    /// replay buffer (§2.5), so a partial answer here is a fact about what was
    /// witnessed, not an error.
    pub fn find_rounds(&self, op: &str) -> Option<Value> {
        let inner = self.lock();
        let rounds = inner.finds.get(op)?;
        if rounds.is_empty() {
            return None;
        }
        Some(Value::Array(
            rounds
                .iter()
                .map(|(round, parts)| {
                    let mut obj = Map::new();
                    obj.insert("round".into(), Value::from(*round));
                    for (k, v) in parts {
                        obj.insert(k.clone(), v.clone());
                    }
                    Value::Object(obj)
                })
                .collect(),
        ))
    }

    /// `(inflight, bodies, finds, streams)` for `/api/status`.
    pub fn snapshot(&self) -> (usize, usize, usize, usize) {
        let inner = self.lock();
        (inner.inflight.len(), inner.bodies.len(), inner.finds.len(), self.stream_count())
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }
}

fn put_bodies(inner: &mut Inner, id: String, bodies: Bodies) {
    if inner.bodies.contains_key(&id) {
        if let Some(pos) = inner.body_order.iter().position(|k| k == &id) {
            inner.body_order.remove(pos);
        }
    }
    inner.bodies.insert(id.clone(), bodies);
    inner.body_order.push_back(id);
    while inner.body_order.len() > MAX_BODIES {
        if let Some(old) = inner.body_order.pop_front() {
            inner.bodies.remove(&old);
        }
    }
}

/// Merge one `find.*` event into its operation's round, LRU-capped by `op`.
///
/// Re-applying the same part of the same round overwrites rather than appends:
/// an operation that somehow emits `find.patterns` twice for round 2 is one
/// round with a later reading, never two rounds.
fn put_find(inner: &mut Inner, op: String, round: u64, part: String, ev: Value) {
    if let Some(pos) = inner.find_order.iter().position(|k| k == &op) {
        inner.find_order.remove(pos);
    }
    inner.finds.entry(op.clone()).or_default().entry(round).or_default().insert(part, ev);
    inner.find_order.push_back(op);
    while inner.find_order.len() > MAX_FINDS {
        if let Some(old) = inner.find_order.pop_front() {
            inner.finds.remove(&old);
        }
    }
}

fn s(v: &Value, k: &str) -> String {
    v.get(k).and_then(Value::as_str).unwrap_or("").to_string()
}

fn opt_s(v: &Value, k: &str) -> Option<String> {
    v.get(k).and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_string)
}

fn row_from_start(v: &Value, id: &str) -> LiveRow {
    let op = opt_s(v, "op").unwrap_or_else(|| id.to_string());
    LiveRow {
        id: id.to_string(),
        op,
        run: s(v, "run"),
        ts: v.get("ts").and_then(Value::as_f64).unwrap_or(0.0),
        via: s(v, "via"),
        tool: s(v, "tool"),
        preset: s(v, "preset"),
        attempt: v.get("attempt").and_then(Value::as_u64).unwrap_or(1),
        project: opt_s(v, "project"),
        model: opt_s(v, "model"),
        endpoint: opt_s(v, "endpoint"),
        input: v.get("input").cloned().unwrap_or_else(|| json!({})),
        kind: "running".into(),
        summary: None,
        raw_bytes: 0,
        returned_bytes: 0,
        tokens_in: 0,
        tokens_out: 0,
        ms: 0,
        ok: true,
    }
}

fn row_from_end(v: &Value, id: &str) -> LiveRow {
    let mut row = row_from_start(v, id);
    apply_end(&mut row, v);
    row
}

fn apply_end(row: &mut LiveRow, v: &Value) {
    if let Some(kind) = v["outcome"]["kind"].as_str() {
        row.kind = kind.to_string();
        row.ok = kind == "ok" || kind == "bypassed" || kind == "none_relevant";
    }
    row.summary = v["outcome"]["summary"].as_str().map(str::to_string);
    row.ms = v.get("ms").and_then(Value::as_u64).unwrap_or(row.ms);
    if let Some(u) = v.get("usage") {
        row.tokens_in = u["prompt_tokens"].as_u64().unwrap_or(row.tokens_in);
        row.tokens_out = u["completion_tokens"].as_u64().unwrap_or(row.tokens_out);
    }
}

/// Drain datagrams into `store` until the process exits.
pub fn recv_loop(sock: std::os::unix::net::UnixDatagram, store: Arc<LiveStore>) {
    let mut buf = vec![0u8; MAX_DGRAM];
    loop {
        match sock.recv(&mut buf) {
            Ok(n) => {
                if let Ok(text) = std::str::from_utf8(&buf[..n]) {
                    store.apply_json(text);
                }
            }
            Err(e) if e.kind() == ErrorKind::WouldBlock || e.kind() == ErrorKind::Interrupted => {
                std::thread::sleep(Duration::from_millis(8));
            }
            Err(_) => std::thread::sleep(Duration::from_millis(50)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixDatagram;

    // Env vars and the process-global sender are shared; serialise tests
    // that touch either.
    static TEST_LOCK: Mutex<()> = Mutex::new(());

    fn lock() -> std::sync::MutexGuard<'static, ()> {
        TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn reset_sender() {
        *sender_slot() = None;
    }

    fn with_sock_env<T>(path: &Path, f: impl FnOnce() -> T) -> T {
        std::env::set_var("SCOUT_LIVE_SOCK", path);
        reset_sender();
        let out = f();
        std::env::remove_var("SCOUT_LIVE_SOCK");
        reset_sender();
        out
    }

    fn rec(tool: &str) -> CallRecord {
        CallRecord::new(tool, tool)
    }

    #[test]
    fn a_missing_socket_is_cached_and_does_not_panic() {
        let _g = lock();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("no-such.sock");
        with_sock_env(&path, || {
            let r = rec("grep");
            emit_start(&r, "sys", "user");
            emit_end(&r, Some("hi"));
            emit_start(&r, "sys", "user");
            match sender_slot().as_ref() {
                Some(Sock::Missing) => {}
                other => panic!("expected cached Missing, got {other:?}"),
            }
            assert!(!path.exists(), "a miss must not create the socket");
        });
    }

    #[test]
    fn a_silent_record_does_not_touch_the_socket() {
        let _g = lock();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("live.sock");
        let listener = UnixDatagram::bind(&path).unwrap();
        listener.set_nonblocking(true).unwrap();
        with_sock_env(&path, || {
            let mut r = rec("grep");
            r.silent = true;
            emit_start(&r, "sys", "user");
            let mut buf = [0u8; 256];
            match listener.recv(&mut buf) {
                Err(e) if e.kind() == ErrorKind::WouldBlock => {}
                Ok(n) => panic!("silent emit sent {n} bytes"),
                Err(e) => panic!("unexpected recv error: {e}"),
            }
        });
    }

    #[test]
    fn a_connected_send_carries_id_op_and_bodies() {
        let _g = lock();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("live.sock");
        let listener = UnixDatagram::bind(&path).unwrap();
        listener.set_nonblocking(false).unwrap();
        let _ = listener.set_read_timeout(Some(Duration::from_secs(2)));
        with_sock_env(&path, || {
            let r = rec("check_output");
            emit_start(&r, "SYSTEM", "USER PROMPT");
            let mut buf = vec![0u8; MAX_DGRAM];
            let n = listener.recv(&mut buf).expect("datagram");
            let v: Value = serde_json::from_slice(&buf[..n]).unwrap();
            assert_eq!(v["kind"], "call.start");
            assert_eq!(v["id"], r.id);
            assert_eq!(v["op"], r.op);
            assert_eq!(v["system"], "SYSTEM");
            assert_eq!(v["user"], "USER PROMPT");
            assert_eq!(v["tool"], "check_output");
            assert_eq!(v["v"], 1);
        });
    }

    #[test]
    fn a_full_receive_buffer_does_not_block_the_sender() {
        let _g = lock();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("live.sock");
        let listener = UnixDatagram::bind(&path).unwrap();
        // Tiny receive buffer so a handful of 8 KiB payloads fill it.
        {
            use std::os::unix::io::AsRawFd;
            let fd = listener.as_raw_fd();
            let buf: libc::c_int = 2048;
            unsafe {
                libc::setsockopt(
                    fd,
                    libc::SOL_SOCKET,
                    libc::SO_RCVBUF,
                    &buf as *const _ as *const libc::c_void,
                    std::mem::size_of_val(&buf) as libc::socklen_t,
                );
            }
        }
        listener.set_nonblocking(true).unwrap();
        with_sock_env(&path, || {
            let r = rec("grep");
            let fat = "x".repeat(4000);
            let start = std::time::Instant::now();
            for _ in 0..64 {
                emit_start(&r, &fat, &fat);
            }
            assert!(
                start.elapsed() < Duration::from_secs(2),
                "sender blocked on a full buffer"
            );
        });
    }

    #[test]
    fn a_huge_prompt_is_elided_under_the_datagram_cap() {
        let _g = lock();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("live.sock");
        let listener = UnixDatagram::bind(&path).unwrap();
        let _ = listener.set_read_timeout(Some(Duration::from_secs(2)));
        with_sock_env(&path, || {
            let r = rec("check_output");
            let huge = "α".repeat(80_000); // 200 KiB of UTF-8, not ASCII
            emit_start(&r, "sys", &huge);
            let mut buf = vec![0u8; MAX_DGRAM + 1024];
            let n = listener.recv(&mut buf).expect("datagram");
            assert!(n <= MAX_DGRAM, "datagram was {n} bytes");
            let v: Value = serde_json::from_slice(&buf[..n]).unwrap();
            let user = v["user"].as_str().unwrap();
            assert!(user.contains("elided") || user.contains("bytes"), "{user}");
            assert!(user.starts_with('α') || user.starts_with('…'), "head survived");
        });
    }

    #[test]
    fn reconcile_does_not_duplicate_a_call_that_lands_on_both_paths() {
        let store = LiveStore::new();
        let start = json!({
            "v": 1, "id": "r-1", "run": "r", "op": "r-op",
            "kind": "call.start", "ts": 1.0, "tool": "grep", "preset": "grep",
            "via": "mcp", "system": "S", "user": "U",
        });
        assert!(store.apply_json(&start.to_string()));
        assert_eq!(store.inflight_rows().len(), 1);
        assert_eq!(store.bodies_of("r-1").unwrap().system.as_deref(), Some("S"));
        store.reap(["r-1"]);
        assert!(store.inflight_rows().is_empty(), "log id wins");
        assert!(store.bodies_of("r-1").is_some(), "bodies survive reap");
    }

    #[test]
    fn call_end_without_start_is_an_ordinary_row() {
        let store = LiveStore::new();
        let end = json!({
            "v": 1, "id": "r-9", "run": "r", "op": "r-op",
            "kind": "call.end", "ts": 2.0, "ms": 40,
            "outcome": {"kind": "ok", "summary": "done"},
            "usage": {"prompt_tokens": 3, "completion_tokens": 5},
            "response": "hello",
        });
        assert!(store.apply_json(&end.to_string()));
        let rows = store.inflight_rows();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].kind, "ok");
        assert_eq!(rows[0].ms, 40);
        assert_eq!(store.bodies_of("r-9").unwrap().response.as_deref(), Some("hello"));
        assert!(store.bodies_of("r-9").unwrap().system.is_none());
    }

    #[test]
    fn concurrent_ops_stay_separate_and_rounds_share_an_op() {
        let store = LiveStore::new();
        let a = json!({"v":1,"id":"a-1","run":"r","op":"op-a","kind":"call.start","ts":1.0,"tool":"find","preset":"find_patterns"});
        let b = json!({"v":1,"id":"b-1","run":"r","op":"op-b","kind":"call.start","ts":1.1,"tool":"grep","preset":"grep"});
        let a2 = json!({"v":1,"id":"a-2","run":"r","op":"op-a","kind":"call.start","ts":1.2,"tool":"find","preset":"grep"});
        store.apply_json(&a.to_string());
        store.apply_json(&b.to_string());
        store.apply_json(&a2.to_string());
        let rows = store.inflight_rows();
        assert_eq!(rows.len(), 3);
        let a_rounds = rows.iter().filter(|r| r.op == "op-a").count();
        assert_eq!(a_rounds, 2);
        assert_eq!(rows.iter().filter(|r| r.op == "op-b").count(), 1);
    }

    #[test]
    fn stream_cap_rejects_the_ninth() {
        let store = LiveStore::new();
        for _ in 0..MAX_STREAMS {
            assert!(store.try_acquire_stream());
        }
        assert!(!store.try_acquire_stream());
        store.release_stream();
        assert!(store.try_acquire_stream());
    }

    #[test]
    fn subscribe_is_dropped_on_client_disconnect() {
        let store = LiveStore::new();
        let rx = store.subscribe();
        drop(rx);
        let ev = json!({"v":1,"id":"x","run":"r","op":"o","kind":"call.start","ts":1.0,"tool":"t","preset":"t"});
        store.apply_json(&ev.to_string());
        assert!(store.lock().subs.is_empty(), "a closed subscriber is pruned");
    }

    #[test]
    fn lru_evicts_the_oldest_body() {
        let store = LiveStore::new();
        for i in 0..(MAX_BODIES + 3) {
            let ev = json!({
                "v": 1, "id": format!("id-{i}"), "run": "r", "op": format!("op-{i}"),
                "kind": "call.start", "ts": 1.0, "tool": "t", "preset": "t",
                "system": "s", "user": "u",
            });
            store.apply_json(&ev.to_string());
        }
        assert!(store.bodies_of("id-0").is_none());
        assert!(store.bodies_of(&format!("id-{}", MAX_BODIES + 2)).is_some());
        let (_, n, _, _) = store.snapshot();
        assert_eq!(n, MAX_BODIES);
    }

    // ── call.token (P5) ──────────────────────────────────────────────────
    // The coalescer is driven with a collecting closure: no socket, no
    // daemon, and the timer is a constructor argument so a test can make a
    // window close without sleeping for the real 50 ms.

    /// Every `(text, index)` a coalescer emitted, in order.
    type Emitted = Arc<Mutex<Vec<(String, u64)>>>;

    fn collect(interval: Duration) -> (Coalescer<impl FnMut(&str, u64)>, Emitted) {
        let out = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&out);
        let co = Coalescer::new(interval, move |text: &str, index: u64| {
            sink.lock().unwrap().push((text.to_string(), index));
        });
        (co, out)
    }

    #[test]
    fn the_coalescer_batches_a_window_into_one_event() {
        let (mut co, out) = collect(Duration::from_millis(50));
        for d in ["Hel", "lo, ", "wor", "ld"] {
            co.push(d);
        }
        assert!(out.lock().unwrap().is_empty(), "four deltas are not four events");
        std::thread::sleep(Duration::from_millis(60));
        co.push("!");
        let got = out.lock().unwrap().clone();
        assert_eq!(got.len(), 1, "one closed window, one event: {got:?}");
        assert_eq!(got[0].0, "Hello, world!", "the whole window, in order");
        assert_eq!(got[0].1, 0, "events are indexed from zero");
    }

    #[test]
    fn a_partial_buffer_is_flushed_at_the_end_of_the_call() {
        let (mut co, out) = collect(Duration::from_millis(50));
        co.push("tail");
        assert!(out.lock().unwrap().is_empty());
        co.flush();
        assert_eq!(out.lock().unwrap().clone(), [("tail".to_string(), 0)]);
        // A second flush on an empty buffer must not emit an empty event: a
        // call that ended exactly on a window boundary is the common case.
        co.flush();
        assert_eq!(out.lock().unwrap().len(), 1);
    }

    #[test]
    fn a_run_of_deltas_larger_than_the_chunk_cap_flushes_early() {
        // The timer alone cannot bound the payload — a fast host can produce
        // more text in one window than a datagram can carry.
        let (mut co, out) = collect(Duration::from_secs(3600));
        let block = "x".repeat(1024);
        for _ in 0..12 {
            co.push(&block);
        }
        let got = out.lock().unwrap().clone();
        assert!(!got.is_empty(), "the size cap never fired");
        for (text, _) in &got {
            assert!(text.len() < MAX_DGRAM, "a chunk would not fit a datagram");
        }
        assert!(got.iter().map(|(t, _)| t.len()).sum::<usize>() >= 8 * 1024);
        assert_eq!(got[0].1, 0);
        assert!(got.iter().enumerate().all(|(i, (_, n))| *n == i as u64), "{got:?}");
    }

    #[test]
    fn a_silent_record_streams_no_tokens() {
        // `CallRecord::silent` is enforced here, where it already was —
        // `client.rs` never learns the concept, it just gets no sink.
        let _g = lock();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("live.sock");
        let listener = UnixDatagram::bind(&path).unwrap();
        listener.set_nonblocking(true).unwrap();
        with_sock_env(&path, || {
            let mut r = rec("grep");
            r.silent = true;
            with_token_stream(&r, |sink| {
                for _ in 0..200 {
                    sink("token ");
                }
            });
            let mut buf = vec![0u8; MAX_DGRAM];
            match listener.recv(&mut buf) {
                Err(e) if e.kind() == ErrorKind::WouldBlock => {}
                Ok(n) => panic!("a silent record sent {n} bytes"),
                Err(e) => panic!("unexpected recv error: {e}"),
            }
        });
    }

    #[test]
    fn a_streamed_call_lands_as_call_token_events() {
        let _g = lock();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("live.sock");
        let listener = UnixDatagram::bind(&path).unwrap();
        let _ = listener.set_read_timeout(Some(Duration::from_secs(2)));
        with_sock_env(&path, || {
            let r = rec("check_output");
            with_token_stream(&r, |sink| {
                sink("Hello, ");
                sink("world");
            });
            let mut buf = vec![0u8; MAX_DGRAM];
            let n = listener.recv(&mut buf).expect("datagram");
            let v: Value = serde_json::from_slice(&buf[..n]).unwrap();
            assert_eq!(v["kind"], "call.token");
            assert_eq!(v["id"], r.id, "the row id, so the pane knows where to append");
            assert_eq!(v["op"], r.op);
            assert_eq!(v["text"], "Hello, world");
            assert_eq!(v["index"], 0);
            // Usage has exactly one path to the dashboard, and it is not this
            // one (§5.5): `call.end` carries it, as it always did.
            assert!(v.get("usage").is_none(), "usage must not ride the token stream");
        });
    }

    #[test]
    fn streaming_costs_nothing_when_nobody_is_listening() {
        let _g = lock();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("no-such.sock");
        with_sock_env(&path, || {
            let r = rec("grep");
            let start = std::time::Instant::now();
            with_token_stream(&r, |sink| {
                for _ in 0..10_000 {
                    sink("tok");
                }
            });
            assert!(start.elapsed() < Duration::from_secs(1), "the sink cost real time");
            match sender_slot().as_ref() {
                Some(Sock::Missing) => {}
                other => panic!("expected cached Missing, got {other:?}"),
            }
            assert!(!path.exists());
        });
    }

    #[test]
    fn a_token_event_fans_out_without_becoming_a_row_or_a_body() {
        let store = LiveStore::new();
        let rx = store.subscribe();
        let ev = json!({
            "v": 1, "id": "r-1", "run": "r", "op": "op-a", "seq": 1,
            "kind": "call.token", "ts": 1.0, "index": 0, "text": "par",
        });
        assert!(store.apply_json(&ev.to_string()));
        let got: Value = serde_json::from_str(&rx.try_recv().expect("fanned out")).unwrap();
        assert_eq!(got["text"], "par");
        // Nothing is kept: the authoritative body arrives on `call.end`, and a
        // stored partial would outlive a failed call looking whole (§2.5).
        assert!(store.inflight_rows().is_empty());
        assert!(store.bodies_of("r-1").is_none());
        let (_, bodies, _, _) = store.snapshot();
        assert_eq!(bodies, 0);
    }

    #[test]
    fn a_token_event_without_text_is_rejected() {
        let store = LiveStore::new();
        let ev = json!({"v":1,"id":"r-1","run":"r","op":"op-a","kind":"call.token","ts":1.0});
        assert!(!store.apply_json(&ev.to_string()));
    }

    // ── find.* (P4) ──────────────────────────────────────────────────────

    fn find_ev(op: &str, round: u64, kind: &str, extra: Value) -> String {
        let mut v = json!({
            "v": 1, "id": op, "run": "r", "op": op, "seq": 1,
            "kind": format!("find.{kind}"), "ts": 1.0, "round": round,
        });
        for (k, val) in extra.as_object().unwrap() {
            v[k] = val.clone();
        }
        v.to_string()
    }

    #[test]
    fn a_find_event_costs_nothing_when_nobody_is_listening() {
        let _g = lock();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("no-such.sock");
        with_sock_env(&path, || {
            assert!(!is_listening(), "no socket, no listener");
            // The guard callers use is one cached lookup; emitting anyway must
            // still be silent and must not create the socket.
            emit_find("op-1", 1, "patterns", json!({"patterns": []}));
            emit_find("op-1", 1, "hits", json!({"candidates": []}));
            match sender_slot().as_ref() {
                Some(Sock::Missing) => {}
                other => panic!("expected cached Missing, got {other:?}"),
            }
            assert!(!path.exists(), "a miss must not create the socket");
        });
    }

    #[test]
    fn a_connected_find_event_carries_id_op_and_round() {
        let _g = lock();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("live.sock");
        let listener = UnixDatagram::bind(&path).unwrap();
        let _ = listener.set_read_timeout(Some(Duration::from_secs(2)));
        with_sock_env(&path, || {
            assert!(is_listening());
            emit_find("op-7", 2, "reflect", json!({"answered": false, "patterns": ["draw"]}));
            let mut buf = vec![0u8; MAX_DGRAM];
            let n = listener.recv(&mut buf).expect("datagram");
            let v: Value = serde_json::from_slice(&buf[..n]).unwrap();
            assert_eq!(v["kind"], "find.reflect");
            assert_eq!(v["op"], "op-7");
            assert_eq!(v["id"], "op-7", "no row id exists; the op stands in");
            assert_eq!(v["round"], 2);
            assert_eq!(v["run"], stats::run_id());
            assert_eq!(v["answered"], false);
            assert_eq!(v["patterns"][0], "draw");
        });
    }

    #[test]
    fn an_oversized_find_event_is_truncated_rather_than_dropped() {
        let _g = lock();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("live.sock");
        let listener = UnixDatagram::bind(&path).unwrap();
        let _ = listener.set_read_timeout(Some(Duration::from_secs(2)));
        with_sock_env(&path, || {
            // A pathological keep list: far past MAX_DGRAM in one array.
            let keeps: Vec<Value> = (0..4000)
                .map(|i| json!({"file": format!("src/{i}/{}.rs", "long".repeat(20)), "line": i, "why": "y".repeat(120)}))
                .collect();
            emit_find("op-big", 1, "rerank", json!({"keeps": keeps}));
            let mut buf = vec![0u8; MAX_DGRAM + 4096];
            let n = listener.recv(&mut buf).expect("datagram");
            assert!(n <= MAX_DGRAM, "datagram was {n} bytes");
            let v: Value = serde_json::from_slice(&buf[..n]).expect("still valid JSON");
            assert_eq!(v["kind"], "find.rerank");
            assert_eq!(v["truncated"], true, "a shortened list says so");
            let kept = v["keeps"].as_array().unwrap().len();
            assert!(kept > 0 && kept < 4000, "kept a prefix, not all and not none");
        });
    }

    #[test]
    fn find_rounds_reconcile_onto_one_op_without_duplicating() {
        let store = LiveStore::new();
        for (round, kind) in [
            (1, "patterns"),
            (1, "hits"),
            (1, "rerank"),
            (1, "reflect"),
            (2, "patterns"),
            (2, "hits"),
        ] {
            assert!(store.apply_json(&find_ev("op-a", round, kind, json!({"n": round}))));
        }
        // The same part of the same round arriving twice is one round, updated.
        assert!(store.apply_json(&find_ev("op-a", 1, "patterns", json!({"n": 99}))));

        let rounds = store.find_rounds("op-a").unwrap();
        let rounds = rounds.as_array().unwrap();
        assert_eq!(rounds.len(), 2, "two rounds, not seven events");
        assert_eq!(rounds[0]["round"], 1);
        assert_eq!(rounds[1]["round"], 2);
        for part in ["patterns", "hits", "rerank", "reflect"] {
            assert!(rounds[0][part].is_object(), "round 1 missing {part}");
        }
        assert_eq!(rounds[0]["patterns"]["n"], 99, "a repeat overwrites");
        assert!(rounds[1]["rerank"].is_null(), "round 2 never reranked");
        // Find events are not calls: no phantom history row, no bodies.
        assert!(store.inflight_rows().is_empty());
        assert!(store.bodies_of("op-a").is_none());
    }

    #[test]
    fn interleaved_finds_do_not_cross_contaminate() {
        let store = LiveStore::new();
        // Two concurrent `find`s, their events arriving alternately — which is
        // the ordinary case under `spawn_blocking` dispatch, not an edge one.
        store.apply_json(&find_ev("op-a", 1, "patterns", json!({"patterns": ["a1"]})));
        store.apply_json(&find_ev("op-b", 1, "patterns", json!({"patterns": ["b1"]})));
        store.apply_json(&find_ev("op-b", 2, "patterns", json!({"patterns": ["b2"]})));
        store.apply_json(&find_ev("op-a", 1, "hits", json!({"union": 3})));
        store.apply_json(&find_ev("op-b", 2, "hits", json!({"union": 9})));

        let a = store.find_rounds("op-a").unwrap();
        let b = store.find_rounds("op-b").unwrap();
        assert_eq!(a.as_array().unwrap().len(), 1);
        assert_eq!(b.as_array().unwrap().len(), 2);
        assert_eq!(a[0]["patterns"]["patterns"][0], "a1");
        assert_eq!(a[0]["hits"]["union"], 3);
        assert_eq!(b[1]["patterns"]["patterns"][0], "b2");
        assert_eq!(b[1]["hits"]["union"], 9);
        assert!(store.find_rounds("op-c").is_none());
    }

    #[test]
    fn joining_mid_find_yields_a_partial_round_list_not_an_orphan() {
        let store = LiveStore::new();
        // No replay buffer (§2.5): rounds 1 and 2 happened before the daemon
        // bound its socket, so all it ever sees is round 3.
        store.apply_json(&find_ev("op-late", 3, "hits", json!({"union": 12})));
        let rounds = store.find_rounds("op-late").unwrap();
        let rounds = rounds.as_array().unwrap();
        assert_eq!(rounds.len(), 1);
        assert_eq!(rounds[0]["round"], 3);
        assert!(rounds[0]["patterns"].is_null(), "an unseen part is simply absent");
    }

    #[test]
    fn an_unknown_find_part_is_rejected() {
        let store = LiveStore::new();
        assert!(!store.apply_json(&find_ev("op-a", 1, "tokens", json!({}))));
        assert!(store.find_rounds("op-a").is_none());
    }

    #[test]
    fn find_rounds_are_lru_capped_by_op() {
        let store = LiveStore::new();
        for i in 0..(MAX_FINDS + 2) {
            store.apply_json(&find_ev(&format!("op-{i}"), 1, "patterns", json!({})));
        }
        assert!(store.find_rounds("op-0").is_none());
        assert!(store.find_rounds(&format!("op-{}", MAX_FINDS + 1)).is_some());
        let (_, _, finds, _) = store.snapshot();
        assert_eq!(finds, MAX_FINDS);
    }

    #[test]
    fn a_find_event_reaches_subscribers() {
        let store = LiveStore::new();
        let rx = store.subscribe();
        store.apply_json(&find_ev("op-a", 1, "rerank", json!({"keeps": []})));
        let got = rx.try_recv().expect("the SSE fan-out carries find.* too");
        let v: Value = serde_json::from_str(&got).unwrap();
        assert_eq!(v["kind"], "find.rerank");
    }

    impl std::fmt::Debug for Sock {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            match self {
                Sock::Missing => write!(f, "Missing"),
                Sock::Ready(_) => write!(f, "Ready"),
            }
        }
    }
}
