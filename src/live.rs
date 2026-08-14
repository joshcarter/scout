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
use std::collections::{HashMap, VecDeque};
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
    for key in ["system", "user", "response"] {
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
        let longest = ["system", "user", "response"]
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
    // Last resort: drop the bodies entirely. Metadata still has to land.
    for key in ["system", "user", "response"] {
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

    /// `(inflight, bodies, streams)` for `/api/status`.
    pub fn snapshot(&self) -> (usize, usize, usize) {
        let inner = self.lock();
        (inner.inflight.len(), inner.bodies.len(), self.stream_count())
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
        let (_, n, _) = store.snapshot();
        assert_eq!(n, MAX_BODIES);
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
