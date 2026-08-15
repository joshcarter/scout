//! The ephemeral live channel (docs/dashboard.md §2, P3).
//!
//! Short-lived scout processes send `call.start` / `call.end` over a
//! non-blocking `AF_UNIX` **stream** socket, one length-prefixed frame per
//! event. The dashboard daemon is the only listener.
//!
//! The invariant that outranks everything else here: **telemetry never blocks
//! a tool call, and every failure drops the event.** Nobody listening, a stale
//! socket, a full send buffer, a write that only got half a frame out — all of
//! them end the same way, with the event gone and the caller unaffected. That
//! matters most for `shell_safety`, which sits in the critical path of every
//! Bash call the agent makes.
//!
//! It was a datagram channel until it met macOS. `net.local.dgram.maxdgram`
//! caps a unix datagram at 2048 bytes there whatever `SO_SNDBUF` says, so
//! every event past 2 KiB — which is nearly all of them — failed `EMSGSIZE`
//! and the dashboard stayed empty. `SOCK_SEQPACKET` would have kept message
//! boundaries, but macOS does not support it for `AF_UNIX` either. So:
//! `SOCK_STREAM` on every platform, with the message boundary the transport no
//! longer provides carried explicitly, as a 4-byte little-endian length in
//! front of each event's JSON.
//!
//! Framing is what the stream costs, and it is paid on both sides: the writer
//! serializes header and payload into one contiguous buffer so a successful
//! `write` is a whole frame, and drops the connection on any short or failed
//! write rather than leaving the reader stranded mid-frame. The reader
//! discards a frame torn by a writer that died mid-payload — the stream
//! world's lost datagram.
//!
//! Bodies and in-flight rows live only in the daemon's memory. A restart
//! loses them; that is accepted.

// The synthetic rows this module hands the history overlay are the same
// `record::Row` the log parses into.  They used to be a `LiveRow` copy of it,
// declared here "without creating a module cycle" — the cycle was real, the
// copy was not the way out of it, and `record` is the leaf both sides share.
use crate::record::Row;
use crate::stats::{self, CallRecord, Outcome};
use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::io::{self, ErrorKind, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{self, SyncSender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

/// Hard cap on a serialized event, and so on a frame either side will accept.
///
/// The stream itself imposes no limit — that was the point of leaving
/// datagrams — but a length prefix a reader trusts unconditionally is a
/// 4 GiB allocation waiting for one corrupt byte. So the bound is declared
/// here, `fit_event` shrinks anything larger before it reaches the wire, and
/// the reader treats a longer declared length as a stream it can no longer
/// follow.
pub const MAX_EVENT: usize = 64 * 1024;
/// In-memory body cache. A daemon restart empties it.
pub const MAX_BODIES: usize = 500;
/// In-memory `find` round cache, keyed by `op`. Smaller than `MAX_BODIES`:
/// only `find` writes here, and it is the rarest of the tools.
pub const MAX_FINDS: usize = 200;
/// Concurrent `/api/stream` connections. Each is a thread that lives as
/// long as the browser tab.
pub const MAX_STREAMS: usize = 8;
/// How long past scout's own HTTP timeout an in-flight row waits for a
/// `call.end` before the daemon gives up on it.
///
/// A live call cannot outrun `[llm] timeout_seconds` by more than the cost of
/// getting the request out and the outcome back, so a row still "running"
/// beyond that never reported and never will.
pub const ABANDON_GRACE_SECS: u64 = 30;
/// Fallback until the reachability thread has read a config: the shipped
/// default `timeout_seconds` plus the grace.
pub const ABANDON_AFTER_DEFAULT_SECS: u64 = 120 + ABANDON_GRACE_SECS;
/// Abandoned rows are evidence the log will never hold — a process that died
/// before reporting wrote no line — so enough of them are kept to see a
/// pattern. Bounded by count, newest first.
pub const MAX_ABANDONED: usize = 50;
/// …and by age, so the count the header reports means "lately" rather than
/// "since this daemon started". Without it a burst of kills would leave the
/// warning lit for as long as the daemon ran, including long after whatever
/// caused it was fixed.
pub const ABANDONED_RETAIN_SECS: f64 = 3600.0;
/// The synthesized outcome for a row the daemon gave up on. Not a
/// `stats::Outcome`: no scout process ever writes it, because a process that
/// could write it would have written its real outcome instead.
pub const ABANDONED: &str = "abandoned";
const SUB_CAP: usize = 32;
const RCVBUF: libc::c_int = 4 * 1024 * 1024;
/// Writer-side send buffer. A stream write that does not fit drops the event
/// and the connection with it (see `emit_bytes`), so the buffer is what keeps
/// that rare rather than merely handled. Best-effort: a host that refuses the
/// size just keeps its default.
const SNDBUF: libc::c_int = 1024 * 1024;
/// Concurrent connection threads the daemon will keep. Writers are
/// short-lived scout processes plus one long-lived MCP server, so this is a
/// backstop against something pathological, not a scheduler.
const MAX_CONNS: usize = 64;

// ── Path ────────────────────────────────────────────────────────────────────

/// Where a *writer* connects: the socket of the daemon on the configured
/// default port, which is the only one a short-lived scout process knows how
/// to look for.
pub fn socket_path() -> Option<PathBuf> {
    resolve_socket_path(DEFAULT_SOCKET_NAME)
}

/// Where the daemon on `port` binds.
///
/// Port-qualified for every port but the configured default, for precisely the
/// reason `dashboard::pid_path_for` already writes down about the *pidfile*:
/// the SIGTERM handler unlinks this path unconditionally — it may only make
/// async-signal-safe calls, so it cannot read the file back to check whose it
/// is — and `bind_socket` unlinks it a second time before binding.  With one
/// shared path a one-off `scout dashboard --port N` alongside the real one
/// steals the primary's socket on the way up (the primary's fd stays open and
/// valid, but nothing on disk points at it any more, so every writer's
/// `connect(2)` reaches the interloper) and deletes it on the way down,
/// leaving neither daemon reachable.  The reasoning was already correct one
/// field over; it simply had not been applied here.
///
/// The default port keeps §5's unqualified `live.sock` so `socket_path` —
/// which knows nothing about ports — still finds the real daemon.
pub fn socket_path_for(port: u16) -> Option<PathBuf> {
    let default = crate::filter_config::load_dashboard().port;
    let name =
        if port == default { DEFAULT_SOCKET_NAME.to_string() } else { format!("live-{port}.sock") };
    resolve_socket_path(&name)
}

const DEFAULT_SOCKET_NAME: &str = "live.sock";

/// 1. `$SCOUT_LIVE_SOCK` — tests and custom layouts, same idea as
///    `$SCOUT_CALLS_LOG`.  An explicit path is an explicit choice, so it is
///    taken verbatim and is *not* port-qualified.
/// 2. `$XDG_RUNTIME_DIR/scout/<name>`
/// 3. the state dir (`$XDG_STATE_HOME/scout` or `~/.local/state/scout`)
fn resolve_socket_path(name: &str) -> Option<PathBuf> {
    if let Ok(p) = std::env::var("SCOUT_LIVE_SOCK") {
        if !p.is_empty() {
            return Some(PathBuf::from(p));
        }
    }
    if let Ok(runtime) = std::env::var("XDG_RUNTIME_DIR") {
        if !runtime.is_empty() {
            return Some(PathBuf::from(runtime).join("scout").join(name));
        }
    }
    state_dir().map(|d| d.join(name))
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
    Ready(UnixStream),
}

fn sender_slot() -> std::sync::MutexGuard<'static, Option<Sock>> {
    static SLOT: Mutex<Option<Sock>> = Mutex::new(None);
    SLOT.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// The slot's next state.
///
/// `None` is "nothing to cache, ask again on the next event". It is what a
/// connect that could not complete *right now* deserves — a daemon whose
/// listen backlog is momentarily full is behind, not absent, and writing
/// telemetry off for the life of an MCP server over one stutter would be the
/// wrong trade. Everything else, `ENOENT` above all, is the permanent answer
/// and caches as `Missing`.
fn resolve() -> Option<Sock> {
    let Some(path) = socket_path() else {
        return Some(Sock::Missing);
    };
    match connect_nonblocking(&path) {
        Ok(s) => Some(Sock::Ready(s)),
        Err(e) if matches!(e.raw_os_error(), Some(libc::EAGAIN | libc::EINPROGRESS)) => None,
        Err(_) => Some(Sock::Missing),
    }
}

/// `connect(2)` to the daemon on a socket that is non-blocking *before* the
/// connect, not after.
///
/// Hand-rolled rather than `UnixStream::connect` + `set_nonblocking` because
/// that order has a hole in it: `connect(2)` on a unix stream whose listener
/// has a full backlog parks the caller until the daemon accepts, and parking a
/// tool call inside telemetry is the one thing this module may not do. A
/// wedged daemon is exactly the case the invariant exists for.
///
/// `SOCK_NONBLOCK` would fold this into the `socket(2)` call, but it is a
/// Linux extension; `fcntl` is portable, which is the whole reason this
/// transport exists.
///
/// Every failure — `ENOENT` for no daemon, `ECONNREFUSED` for a stale socket
/// or (on the BSDs) a full backlog, `EINPROGRESS`/`EAGAIN` for a connect that
/// has not finished — is one thing to the caller: not connected, try again on
/// the next event. Nothing here waits.
fn connect_nonblocking(path: &Path) -> io::Result<UnixStream> {
    use std::os::unix::io::FromRawFd;
    let (addr, addr_len) = sockaddr_un(path)?;
    // SAFETY: a plain `socket(2)`; the fd is adopted below before any other
    // fallible step, so no early return can leak it.
    let fd = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `fd` is a fresh socket owned by nobody else.
    let sock = unsafe { UnixStream::from_raw_fd(fd) };
    sock.set_nonblocking(true)?;
    // SAFETY: `addr` is a zeroed `sockaddr_un` this process filled in, and
    // `addr_len` is within it.
    let rc = unsafe { libc::connect(fd, (&raw const addr).cast::<libc::sockaddr>(), addr_len) };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    raise_buf(fd, libc::SO_SNDBUF, SNDBUF);
    Ok(sock)
}

/// A zeroed `sockaddr_un` naming `path`, and the length to pass with it.
///
/// The length is the prefix actually in use — everything up to `sun_path`,
/// plus the path, plus its NUL — rather than the whole struct. Both kernels
/// accept either for a pathname socket, but this is the form the platforms
/// agree on most narrowly, and it is what `std` itself sends.
///
/// `sun_path` is 108 bytes on Linux and 104 on macOS, and a path that does not
/// fit is a configuration error rather than a transport one — reported as
/// `InvalidInput` so `resolve` files it under "no daemon" like every other
/// failure.
fn sockaddr_un(path: &Path) -> io::Result<(libc::sockaddr_un, libc::socklen_t)> {
    use std::os::unix::ffi::OsStrExt;
    let bytes = path.as_os_str().as_bytes();
    // SAFETY: `sockaddr_un` is plain data; all-zero is a valid value for it,
    // and zeroing is what leaves `sun_path` NUL-terminated below.
    let mut addr: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    addr.sun_family = libc::AF_UNIX as libc::sa_family_t;
    if bytes.len() >= addr.sun_path.len() {
        return Err(io::Error::new(ErrorKind::InvalidInput, "live socket path is too long"));
    }
    for (dst, src) in addr.sun_path.iter_mut().zip(bytes) {
        *dst = *src as libc::c_char;
    }
    let len = std::mem::offset_of!(libc::sockaddr_un, sun_path) + bytes.len() + 1;
    Ok((addr, len as libc::socklen_t))
}

/// One event, framed: a 4-byte little-endian length, then the JSON.
///
/// Built into a single contiguous buffer on purpose. The length and the
/// payload have to reach the kernel together or not at all — a writer that
/// managed the header and then died would leave the reader waiting on bytes
/// nobody will send.
fn frame(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(4 + bytes.len());
    out.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
    out.extend_from_slice(bytes);
    out
}

fn emit_bytes(bytes: &[u8]) {
    let mut slot = sender_slot();
    if slot.is_none() {
        *slot = resolve();
    }
    let Some(Sock::Ready(sock)) = slot.as_mut() else { return };
    let buf = frame(bytes);
    // Any write that did not put the whole frame on the wire ends the
    // connection: a stream has no message boundaries to resynchronize on, so a
    // reader left holding half a frame would misread the next one as a length.
    // `WouldBlock` on a full send buffer is that same case — this is telemetry,
    // there is no pending-buffer machinery, and the event is what gets dropped.
    // Clearing the slot is also the reconnect path that `ConnectionRefused`
    // used to be: the next emit re-resolves, and finds `Missing` if the daemon
    // really is gone.
    match sock.write(&buf) {
        Ok(n) if n == buf.len() => {}
        _ => *slot = None,
    }
}

/// `call.start` — resolved prompts. No-op for a silent record.
///
/// `"v"` versions *this envelope* — the framed event on the socket — and has
/// nothing to do with `stats.rs`'s `"v":2`, which versions a `calls.jsonl`
/// record. Two formats, two things being versioned.
///
/// Changing the envelope's shape means restarting the dashboard: a freshly
/// built scout emits to whatever daemon is already running, and there is no
/// reader here that accepts both an old shape and a new one. That is a
/// deliberate omission, not an oversight — scout has one user, who can
/// relaunch the daemon, and a tolerant reader would be permanent weight
/// carried for a transient.
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
        // `endpoint` was read by `row_from_start` and never written here, so
        // every live row carried `None` for it and the UI showed an em dash
        // until the log poll replaced the row with the real one. Both the
        // operation detail panel and the SSE path in `dashboard.html` render
        // it, so the fix was to emit it, not to stop reading it.
        "endpoint": rec.endpoint,
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
///
/// Answering "yes" now leaves a connection open, because on a stream socket
/// that is what asking means. It is the same connection the emit would have
/// used, held for the same lifetime, and the daemon caps how many of them it
/// will service (`MAX_CONNS`).
pub fn is_listening() -> bool {
    let mut slot = sender_slot();
    if slot.is_none() {
        *slot = resolve();
    }
    matches!(slot.as_ref(), Some(Sock::Ready(_)))
}

// ── call.token (P5) ─────────────────────────────────────────────────────────

/// Coalescing window for `call.token` (docs/dashboard.md §2).
///
/// ~1 content delta per token was measured (§5.5); one event per delta is
/// 86 syscalls for a short reply where a 50 ms timer is ~40, and a browser
/// cannot paint faster than a frame anyway.
pub const TOKEN_COALESCE_MS: u64 = 50;

/// Flush early once the buffer reaches this, whatever the timer says.
///
/// The fail-open ladder in `fit_event` has to be re-checked against every new
/// *shape* of payload (§7.6) — and a token event's shape is one long string
/// with no structure to shrink intelligently. Bounding the buffer well under
/// `MAX_EVENT` means the elision path is unreachable in practice rather than
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
        Coalescer { buf: String::new(), index: 0, last: std::time::Instant::now(), interval, emit }
    }

    /// Append one delta, flushing if the window has closed or the buffer is
    /// full. Never sends an event per token, never blocks.
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

/// `find.*` — one round's internals (docs/dashboard.md §2, P4).
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
pub fn emit_find(op: &str, round: u64, kind: &str, fields: &Value) {
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

pub(crate) fn now_ts() -> f64 {
    SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map_or(0.0, |d| d.as_secs_f64())
}

/// Shrink body fields until the serialized event fits in `MAX_EVENT`.
fn fit_event(mut v: Value) -> Vec<u8> {
    let mut bytes = v.to_string().into_bytes();
    if bytes.len() <= MAX_EVENT {
        return bytes;
    }
    for key in ["system", "user", "response", "text"] {
        if let Some(Value::String(s)) = v.get_mut(key) {
            *s = elide(s, 8 * 1024);
        }
    }
    bytes = v.to_string().into_bytes();
    if bytes.len() <= MAX_EVENT {
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
        if bytes.len() <= MAX_EVENT {
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
        if bytes.len() <= MAX_EVENT {
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
    if bytes.len() > MAX_EVENT {
        bytes.truncate(MAX_EVENT);
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

/// Bind the live socket for the daemon on `port`. Not fatal for the daemon if
/// this fails.
///
/// Owner-only, both the socket and any directory created for it.  A listening
/// socket at the process umask is world-writable enough for any other local
/// process to connect and send it forged `call.start`/`call.end` events and
/// spoof rows in somebody else's dashboard — integrity rather than
/// confidentiality, and low stakes on a single-user box, but it costs two
/// lines.
///
/// The directory mode is set only when we create it (`DirBuilder::mode`), not
/// stamped onto one that already exists: `$XDG_RUNTIME_DIR` is already 0700 by
/// construction, and silently tightening a state directory the user made is
/// not this function's business.  The socket's own mode is set right after
/// `bind(2)` rather than by juggling the umask, which is process-global and
/// would race any other thread creating a file.
pub fn bind_socket(port: u16) -> io::Result<UnixListener> {
    let path = socket_path_for(port).ok_or_else(|| io::Error::other("no live socket path"))?;
    if let Some(parent) = path.parent() {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new().recursive(true).mode(0o700).create(parent)?;
    }
    let _ = std::fs::remove_file(&path);
    let sock = UnixListener::bind(&path)?;
    // The window between `bind` and this chmod is a few microseconds wide and,
    // for the `$XDG_RUNTIME_DIR` layout, sits behind a 0700 directory nothing
    // else can traverse. Binding elsewhere and `rename`ing into place would
    // close it outright, at the cost of a second path the SIGTERM handler
    // would also have to know about — not worth it for a forged-telemetry
    // window that short.
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    // Accepted connections inherit the listener's buffer sizes, so raising it
    // here raises it for every writer that arrives. Blocking accept and
    // blocking reads are both fine on this side: the daemon's whole job is
    // waiting, and it does it on threads of its own.
    {
        use std::os::unix::io::AsRawFd;
        raise_buf(sock.as_raw_fd(), libc::SO_RCVBUF, RCVBUF);
    }
    Ok(sock)
}

/// Best-effort `SO_SNDBUF`/`SO_RCVBUF`. A host that refuses the size keeps its
/// default, which costs throughput and nothing else — the failure mode on both
/// sides is a dropped event, never a stall.
fn raise_buf(fd: std::os::unix::io::RawFd, opt: libc::c_int, bytes: libc::c_int) {
    // SAFETY: `fd` is an open socket owned by the caller, and the value
    // pointed at is a live `c_int` of the length declared.
    unsafe {
        libc::setsockopt(
            fd,
            libc::SOL_SOCKET,
            opt,
            (&raw const bytes).cast::<libc::c_void>(),
            std::mem::size_of_val(&bytes) as libc::socklen_t,
        );
    }
}

/// Async-signal-safe path for the SIGTERM handler.
///
/// Takes the port for the same reason `bind_socket` does: the handler unlinks
/// whatever this returns, unconditionally, so it had better be this daemon's
/// own socket and not the one next door.
pub fn socket_cstring(port: u16) -> Option<std::ffi::CString> {
    let path = socket_path_for(port)?;
    std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).ok()
}

// ── Store ───────────────────────────────────────────────────────────────────

#[derive(Clone, Debug, Default)]
pub struct Bodies {
    pub system: Option<String>,
    pub user: Option<String>,
    pub response: Option<String>,
}

pub struct LiveStore {
    inner: Mutex<Inner>,
    streams: AtomicUsize,
    bound: AtomicBool,
    /// Seconds a row may sit at `running` before `sweep` abandons it. Kept
    /// here rather than passed per call so the reachability thread — which
    /// already re-reads `config.toml` every cycle — is the only writer.
    abandon_after: AtomicU64,
}

struct Inner {
    inflight: HashMap<String, Row>,
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
            abandon_after: AtomicU64::new(ABANDON_AFTER_DEFAULT_SECS),
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

    /// Attached SSE subscribers. Should track `stream_count` exactly — the two
    /// diverging means a handler is holding a slot for a stream the fan-out
    /// has stopped feeding, which is the shape of the bug the `Full`/
    /// `Disconnected` split in `apply_json` fixes.
    pub fn subscriber_count(&self) -> usize {
        self.lock().subs.len()
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
        self.streams
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| Some(n.saturating_sub(1)))
            .ok();
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
            // `Full` is not `Disconnected`, and treating them alike cost a
            // subscriber its whole stream for one stutter. The channel holds
            // `SUB_CAP` events and `call.token` coalesces at 50 ms, so a
            // streaming reply is ~20 events a second — 32 slots is under two
            // seconds of backlog, and a backgrounded tab or a briefly-closed
            // TCP window reaches it easily. This is telemetry (see the module
            // header): the *event* is what gets dropped, never the reader.
            //
            // `Disconnected` is the one fatal error, and it can only mean the
            // handler thread has already gone — `handle_stream` returning is
            // what drops the `Receiver`. Removing the `SyncSender` here is
            // also what makes the converse hold: a subscriber the store lets
            // go has its `recv_timeout` return `Disconnected` at once rather
            // than at the next keepalive, so the handler exits and gives back
            // its `MAX_STREAMS` slot instead of looping forever on a stream
            // nothing will ever feed again.
            let mut i = 0;
            while i < inner.subs.len() {
                match inner.subs[i].try_send(raw.to_string()) {
                    Ok(()) | Err(mpsc::TrySendError::Full(_)) => i += 1,
                    // `swap_remove` moved the last element into `i`, so do not
                    // advance past it.
                    Err(mpsc::TrySendError::Disconnected(_)) => {
                        inner.subs.swap_remove(i);
                    }
                }
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

    pub fn set_abandon_after_secs(&self, secs: u64) {
        self.abandon_after.store(secs.max(1), Ordering::Relaxed);
    }

    pub fn abandon_after_secs(&self) -> u64 {
        self.abandon_after.load(Ordering::Relaxed)
    }

    /// Give up on in-flight rows that will never report, and bound how many
    /// of the corpses we keep. Returns the number newly abandoned.
    ///
    /// `reap` alone cannot clear these. It drops ids the log has absorbed, and
    /// a process killed mid-call writes no log line: the shell-safety hook
    /// wraps scout in `timeout`, so SIGTERM lands between `emit_start` and the
    /// `emit_end`/`log()` pair and the id never appears anywhere again. Without
    /// a sweep the row sits at `running` forever and the dashboard reads as
    /// busy when nothing is running at all.
    ///
    /// Abandoning is a downgrade, not a delete, and it is reversible: a late
    /// `call.end` still matches on `id` and `apply_end` overwrites the kind
    /// with the real outcome, as does the log line if one ever lands.
    ///
    /// Abandoned rows then leave on either bound, whichever comes first: age
    /// (so the count means "lately") or count (so a bad afternoon cannot grow
    /// the store without limit).
    pub fn sweep(&self, now: f64) -> usize {
        let cutoff = self.abandon_after_secs() as f64;
        let mut inner = self.lock();
        let mut newly = 0;
        for row in inner.inflight.values_mut() {
            if row.kind == "running" && now - row.ts > cutoff {
                row.kind = ABANDONED.into();
                row.ok = false;
                row.ms = ((now - row.ts) * 1000.0).max(0.0) as u64;
                row.summary = Some(format!(
                    "no completion after {}s — the scout process was killed or died before reporting",
                    cutoff as u64
                ));
                newly += 1;
            }
        }
        inner.inflight.retain(|_, r| r.kind != ABANDONED || now - r.ts <= ABANDONED_RETAIN_SECS);
        let mut dead: Vec<(f64, String)> = inner
            .inflight
            .values()
            .filter(|r| r.kind == ABANDONED)
            .map(|r| (r.ts, r.id.clone()))
            .collect();
        if dead.len() > MAX_ABANDONED {
            let excess = dead.len() - MAX_ABANDONED;
            dead.sort_by(|a, b| a.0.total_cmp(&b.0));
            for (_, id) in dead.into_iter().take(excess) {
                inner.inflight.remove(&id);
            }
        }
        newly
    }

    /// How many inflight rows are still genuinely running vs. abandoned.
    pub fn inflight_split(&self) -> (usize, usize) {
        let inner = self.lock();
        let abandoned = inner.inflight.values().filter(|r| r.kind == ABANDONED).count();
        (inner.inflight.len() - abandoned, abandoned)
    }

    pub fn inflight_rows(&self) -> Vec<Row> {
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
        self.inner.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
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

fn row_from_start(v: &Value, id: &str) -> Row {
    let op = opt_s(v, "op").unwrap_or_else(|| id.to_string());
    Row {
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
        raw_path: opt_s(v, "raw_path"),
        tokens_in: 0,
        tokens_out: 0,
        ms: 0,
        ok: true,
    }
}

fn row_from_end(v: &Value, id: &str) -> Row {
    let mut row = row_from_start(v, id);
    apply_end(&mut row, v);
    row
}

fn apply_end(row: &mut Row, v: &Value) {
    if let Some(kind) = v["outcome"]["kind"].as_str() {
        row.kind = kind.to_string();
        // Ask `Outcome`, never restate it.  This line used to spell out
        // `kind == "ok" || "bypassed" || "none_relevant"` — a hand-copy of
        // `Outcome::is_ok`'s list that a ninth variant would have silently
        // desynchronized, and `SubprocessTimeout` was a ninth variant.
        //
        // A kind that is not an `Outcome` is not ok: `ABANDONED` is the one
        // the daemon itself synthesizes, and anything else is a string no
        // build of scout writes.
        row.ok = matches!(kind.parse::<Outcome>(), Ok(o) if o.is_ok());
    }
    row.summary = v["outcome"]["summary"].as_str().map(str::to_string);
    row.ms = v.get("ms").and_then(Value::as_u64).unwrap_or(row.ms);
    if let Some(u) = v.get("usage") {
        row.tokens_in = u["prompt_tokens"].as_u64().unwrap_or(row.tokens_in);
        row.tokens_out = u["completion_tokens"].as_u64().unwrap_or(row.tokens_out);
    }
}

/// Accept writers and drain their frames into `store` until the process exits.
///
/// One thread per connection, each of them blocking on `read`. That is the
/// shape a stream wants and it is strictly cheaper than what the datagram
/// version did, which was a non-blocking `recv` that slept 8 ms every time it
/// found nothing — a permanent 125 Hz wakeup and up to 8 ms of latency on
/// every event, in exchange for nothing.
///
/// Connections are capped at `MAX_CONNS`. Past that an accepted writer is
/// closed immediately: its next emit reconnects, and if the cap is still full
/// then, its events are dropped like any other failure here.
pub fn accept_loop(listener: &UnixListener, store: &Arc<LiveStore>) {
    let live = Arc::new(AtomicUsize::new(0));
    loop {
        match listener.accept() {
            Ok((stream, _)) => {
                if live.load(Ordering::Relaxed) >= MAX_CONNS {
                    drop(stream);
                    continue;
                }
                live.fetch_add(1, Ordering::Relaxed);
                let store = Arc::clone(store);
                let done = Arc::clone(&live);
                let spawned = std::thread::Builder::new()
                    .name("scout-live".into())
                    .spawn(move || {
                        conn_loop(stream, &store);
                        done.fetch_sub(1, Ordering::Relaxed);
                    })
                    .is_ok();
                if !spawned {
                    // Nobody will run the decrement above, so do it here or the
                    // cap ratchets shut.
                    live.fetch_sub(1, Ordering::Relaxed);
                }
            }
            Err(e) if e.kind() == ErrorKind::Interrupted => {}
            Err(_) => std::thread::sleep(Duration::from_millis(50)),
        }
    }
}

/// Read frames off one writer until it goes away.
///
/// Every exit from here is the same exit: EOF, a short read, a length past
/// `MAX_EVENT`, a payload that is not UTF-8. A frame torn off mid-payload —
/// the writer died between the header and the body, or dropped the connection
/// on a short write — is silently discarded, which is this transport's version
/// of a lost datagram and just as unremarkable.
fn conn_loop(mut stream: UnixStream, store: &LiveStore) {
    // macOS hands `accept(2)` a socket that inherits the listener's
    // `O_NONBLOCK`; Linux does not. Say which one we want rather than
    // depending on the difference.
    let _ = stream.set_nonblocking(false);
    let mut buf = Vec::new();
    while read_frame(&mut stream, &mut buf).is_ok() {
        if let Ok(text) = std::str::from_utf8(&buf) {
            store.apply_json(text);
        }
    }
}

/// One frame into `buf`: a 4-byte little-endian length, then that many bytes.
///
/// A declared length of zero or one past `MAX_EVENT` is not a big event, it is
/// a stream this reader can no longer follow — nothing writes either — so it
/// ends the connection rather than allocating whatever the bytes asked for.
fn read_frame(stream: &mut UnixStream, buf: &mut Vec<u8>) -> io::Result<()> {
    let mut header = [0u8; 4];
    stream.read_exact(&mut header)?;
    let len = u32::from_le_bytes(header) as usize;
    if len == 0 || len > MAX_EVENT {
        return Err(io::Error::new(ErrorKind::InvalidData, "live frame length out of bounds"));
    }
    buf.clear();
    buf.resize(len, 0);
    stream.read_exact(buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Env vars and the process-global sender are shared; serialise tests
    // that touch either.
    static TEST_LOCK: Mutex<()> = Mutex::new(());

    fn lock() -> std::sync::MutexGuard<'static, ()> {
        TEST_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
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

    /// Run `f` with the whole runtime layout redirected into `dir`, and with
    /// `$SCOUT_LIVE_SOCK` out of the way so the port-qualified name is what
    /// actually gets exercised.
    fn with_runtime_dir<T>(dir: &Path, f: impl FnOnce() -> T) -> T {
        let saved_runtime = std::env::var("XDG_RUNTIME_DIR").ok();
        let saved_explicit = std::env::var("SCOUT_LIVE_SOCK").ok();
        std::env::remove_var("SCOUT_LIVE_SOCK");
        std::env::set_var("XDG_RUNTIME_DIR", dir);
        reset_sender();
        let out = f();
        match saved_runtime {
            Some(v) => std::env::set_var("XDG_RUNTIME_DIR", v),
            None => std::env::remove_var("XDG_RUNTIME_DIR"),
        }
        if let Some(v) = saved_explicit {
            std::env::set_var("SCOUT_LIVE_SOCK", v);
        }
        reset_sender();
        out
    }

    /// A stand-in daemon: the socket a writer will find, with a non-blocking
    /// `accept` so a test that asserts *nothing* arrived says so instead of
    /// hanging.
    ///
    /// Every writer in these tests emits on the test's own thread, so by the
    /// time an assertion runs the `connect(2)` and the `write` behind it have
    /// both already happened.
    fn listen(path: &Path) -> UnixListener {
        let l = UnixListener::bind(path).expect("bind the test daemon's socket");
        l.set_nonblocking(true).expect("non-blocking accept");
        l
    }

    /// Take the one connection a writer made, ready for blocking frame reads.
    fn accept_one(listener: &UnixListener) -> UnixStream {
        let (conn, _) = listener.accept().expect("a writer connected");
        // macOS accepts with the listener's `O_NONBLOCK`; Linux does not.
        conn.set_nonblocking(false).unwrap();
        conn.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        conn
    }

    /// One event off the wire: accept the writer, read its first frame, and
    /// hand back the payload — the daemon's read path, minus the store.
    fn recv_frame(listener: &UnixListener) -> Vec<u8> {
        let mut conn = accept_one(listener);
        next_frame(&mut conn)
    }

    fn next_frame(conn: &mut UnixStream) -> Vec<u8> {
        let mut buf = Vec::new();
        read_frame(conn, &mut buf).expect("a whole frame");
        buf
    }

    fn recv_event(listener: &UnixListener) -> Value {
        serde_json::from_slice(&recv_frame(listener)).expect("the frame is JSON")
    }

    /// Did anything at all reach the socket? Distinguishes "no writer
    /// connected" from "a writer connected and sent nothing", because with a
    /// stream those are two different silences and only the first is what an
    /// `is_listening` probe leaves behind.
    fn nothing_arrived(listener: &UnixListener) -> bool {
        match listener.accept() {
            Err(e) if e.kind() == ErrorKind::WouldBlock => true,
            Ok((mut conn, _)) => {
                conn.set_nonblocking(true).unwrap();
                let mut byte = [0u8; 1];
                matches!(conn.read(&mut byte), Err(e) if e.kind() == ErrorKind::WouldBlock)
            }
            Err(e) => panic!("unexpected accept error: {e}"),
        }
    }

    /// The configured default port, and some other port that is not it.
    fn two_ports() -> (u16, u16) {
        let default = crate::filter_config::load_dashboard().port;
        (default, if default == u16::MAX { default - 1 } else { default + 1 })
    }

    fn rec(tool: &str) -> CallRecord {
        CallRecord::new(tool, tool)
    }

    /// The rule for "did scout answer the caller?" lives on `Outcome` and
    /// must live nowhere else.
    ///
    /// `apply_end` used to restate `Outcome::is_ok`'s list as a string
    /// comparison, so the two agreed only for as long as nobody added a
    /// variant — and `SubprocessTimeout` was added.  This sweeps `Outcome::ALL`
    /// (which `stats::all_lists_every_outcome` keeps complete, by making the
    /// crate fail to build when a variant is missing) and demands the two
    /// answers match for every one of them.
    ///
    /// Confirmed to catch the drift: with `apply_end` restored to the old
    /// hand-written comparison and a tenth, successful variant added to
    /// `Outcome`, this fails on that variant — `apply_end` reports `false`
    /// where `is_ok` reports `true`.  Against the current code it passes.
    #[test]
    fn apply_end_agrees_with_outcome_is_ok() {
        for o in Outcome::ALL {
            let mut row = row_from_start(&json!({}), "x-1");
            apply_end(&mut row, &json!({ "outcome": { "kind": o.as_str() } }));
            assert_eq!(row.kind, o.as_str());
            assert_eq!(row.ok, o.is_ok(), "apply_end disagrees about {}", o.as_str());
        }
    }

    /// A kind no `Outcome` produces is not a success.  `ABANDONED` is the
    /// daemon's own synthesis, so it is the case that actually occurs.
    #[test]
    fn a_kind_that_is_not_an_outcome_is_not_ok() {
        for kind in [ABANDONED, "something_a_later_build_invented"] {
            let mut row = row_from_start(&json!({}), "x-1");
            apply_end(&mut row, &json!({ "outcome": { "kind": kind } }));
            assert!(!row.ok, "{kind} must not read as a success");
            assert_eq!(row.kind, kind);
        }
    }

    /// `endpoint` was in the row and in the UI but never on the wire, so it
    /// was `None` in every live row ever built.
    #[test]
    fn call_start_carries_the_endpoint_it_used() {
        let _g = lock();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sock");
        let listener = listen(&path);
        with_sock_env(&path, || {
            emit_start(&rec("grep").endpoint("qwen3:27b", "http://localhost:11434/v1"), "s", "u");
        });
        let ev = recv_event(&listener);
        assert_eq!(ev["endpoint"], "http://localhost:11434/v1");
        assert_eq!(ev["model"], "qwen3:27b");

        let row = row_from_start(&ev, "x-1");
        assert_eq!(row.endpoint.as_deref(), Some("http://localhost:11434/v1"));
    }

    /// Two daemons, two sockets. With one shared path the interloper's
    /// `bind_socket` unlinked the primary's socket and rebound the name, so
    /// every writer's `connect(2)` reached the wrong daemon — and stopping the
    /// interloper then unlinked the name for good, leaving neither reachable.
    #[test]
    fn a_second_daemon_on_another_port_does_not_steal_the_first_socket() {
        let _g = lock();
        let dir = tempfile::tempdir().unwrap();
        with_runtime_dir(dir.path(), || {
            let (default, other) = two_ports();

            let primary = bind_socket(default).expect("primary binds");
            let primary_path = socket_path_for(default).unwrap();
            assert_eq!(
                primary_path,
                socket_path().unwrap(),
                "the default port keeps the name writers look for"
            );

            // `scout dashboard --port <other>` alongside the real one.
            let interloper = bind_socket(other).expect("interloper binds");
            let interloper_path = socket_path_for(other).unwrap();
            assert!(primary_path.exists(), "the primary's socket was unlinked under it");

            // A writer knows only the default port; it must still land on the
            // primary, and the interloper must hear nothing.
            let mut w = connect_nonblocking(&socket_path().unwrap()).unwrap();
            w.write_all(&frame(start_ev(1).as_bytes())).unwrap();

            primary.set_nonblocking(true).unwrap();
            interloper.set_nonblocking(true).unwrap();
            let got = recv_frame(&primary);
            assert!(String::from_utf8_lossy(&got).contains("call.start"));
            assert!(nothing_arrived(&interloper), "the interloper intercepted the primary's traffic");

            // And stopping it — `on_terminate` unlinks its socket path
            // unconditionally — takes only its own name with it.
            assert_ne!(interloper_path, primary_path, "one name, two daemons");
            let _ = std::fs::remove_file(&interloper_path);
            drop(interloper);
            assert!(primary_path.exists(), "the primary outlives its neighbour");
        });
    }

    #[test]
    fn the_live_socket_and_the_directory_it_makes_are_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let _g = lock();
        let dir = tempfile::tempdir().unwrap();
        with_runtime_dir(dir.path(), || {
            let (default, _) = two_ports();
            let sock = bind_socket(default).unwrap();
            let path = socket_path_for(default).unwrap();
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "0{mode:o}: any local process could forge events at us");
            let parent = std::fs::metadata(path.parent().unwrap()).unwrap();
            assert_eq!(parent.permissions().mode() & 0o777, 0o700);
            drop(sock);
        });
    }

    /// An explicit `$SCOUT_LIVE_SOCK` is an explicit choice and stays verbatim
    /// — the port qualification is for the paths scout picks for itself.
    #[test]
    fn an_explicit_socket_path_is_not_port_qualified() {
        let _g = lock();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("chosen.sock");
        with_sock_env(&path, || {
            assert_eq!(socket_path_for(13001).unwrap(), path);
            assert_eq!(socket_path_for(13002).unwrap(), path);
            assert_eq!(socket_path().unwrap(), path);
        });
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
        let listener = listen(&path);
        with_sock_env(&path, || {
            let mut r = rec("grep");
            r.silent = true;
            emit_start(&r, "sys", "user");
            assert!(nothing_arrived(&listener), "a silent record reached the socket");
        });
    }

    #[test]
    fn a_connected_send_carries_id_op_and_bodies() {
        let _g = lock();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("live.sock");
        let listener = listen(&path);
        with_sock_env(&path, || {
            let r = rec("check_output");
            emit_start(&r, "SYSTEM", "USER PROMPT");
            let v = recv_event(&listener);
            assert_eq!(v["kind"], "call.start");
            assert_eq!(v["id"], r.id);
            assert_eq!(v["op"], r.op);
            assert_eq!(v["system"], "SYSTEM");
            assert_eq!(v["user"], "USER PROMPT");
            assert_eq!(v["tool"], "check_output");
            assert_eq!(v["v"], 1);
        });
    }

    /// The cached-slot design, restated for a connection-oriented transport:
    /// one `connect(2)` per process, then every event of every call down the
    /// same stream, in the order they were emitted. A reconnect per event
    /// would be a syscall pair in `shell_safety`'s critical path.
    #[test]
    fn one_connection_carries_every_event_of_a_call_in_order() {
        let _g = lock();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("live.sock");
        let listener = listen(&path);
        with_sock_env(&path, || {
            let r = rec("check_output");
            emit_start(&r, "SYSTEM", "USER");
            emit_end(&r, Some("REPLY"));

            let mut conn = accept_one(&listener);
            let first: Value = serde_json::from_slice(&next_frame(&mut conn)).unwrap();
            let second: Value = serde_json::from_slice(&next_frame(&mut conn)).unwrap();
            assert_eq!(first["kind"], "call.start");
            assert_eq!(second["kind"], "call.end");
            assert_eq!(second["response"], "REPLY");
            assert_eq!(first["id"], second["id"], "both halves of one call");
            assert!(
                first["seq"].as_u64() < second["seq"].as_u64(),
                "the stream preserves the order `seq` records"
            );
            assert!(
                matches!(listener.accept(), Err(e) if e.kind() == ErrorKind::WouldBlock),
                "a second connection: the sender is reconnecting per event"
            );
        });
    }

    /// The reader's half of the framing contract: whole frames become events,
    /// and one that stops mid-payload — a writer killed between the header and
    /// the body — takes the connection with it and leaves nothing behind. That
    /// is this transport's lost datagram.
    #[test]
    fn a_torn_frame_is_discarded_and_the_whole_ones_before_it_are_not() {
        let (mut w, r) = UnixStream::pair().unwrap();
        let store = LiveStore::new();
        std::thread::scope(|s| {
            s.spawn(|| conn_loop(r, &store));
            w.write_all(&frame(start_ev(1).as_bytes())).unwrap();
            w.write_all(&frame(start_ev(2).as_bytes())).unwrap();
            // A header promising 64 bytes, and five of them.
            w.write_all(&64u32.to_le_bytes()).unwrap();
            w.write_all(b"{\"id\"").unwrap();
            drop(w);
        });
        let ids: Vec<String> = store.inflight_rows().into_iter().map(|r| r.id).collect();
        assert_eq!(ids.len(), 2, "the two whole frames landed: {ids:?}");
        assert!(ids.contains(&"id-1".to_string()) && ids.contains(&"id-2".to_string()));
    }

    /// A length past `MAX_EVENT` is not a large event — nothing writes one —
    /// it is a stream that has lost its place, and trusting it would mean
    /// allocating whatever the four bytes happened to say.
    #[test]
    fn a_frame_longer_than_the_bound_ends_the_connection() {
        let (mut w, r) = UnixStream::pair().unwrap();
        let store = LiveStore::new();
        std::thread::scope(|s| {
            s.spawn(|| conn_loop(r, &store));
            w.write_all(&((MAX_EVENT + 1) as u32).to_le_bytes()).unwrap();
            // Never read: the reader gave up on the header above.
            let _ = w.write_all(&frame(start_ev(1).as_bytes()));
            drop(w);
        });
        assert!(store.inflight_rows().is_empty(), "the corrupt stream was followed anyway");
    }

    /// A daemon that never reads is the shape of every failure this module
    /// exists to survive — a wedged dashboard, a socket whose accept loop died.
    /// The writer's send buffer fills, `write` comes back `WouldBlock`, and the
    /// event is dropped: several megabytes of telemetry aimed at nobody must
    /// cost the caller no more than the syscalls it took to find that out.
    #[test]
    fn a_daemon_that_never_reads_does_not_block_the_sender() {
        let _g = lock();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("live.sock");
        // Bound, never accepted, never read.
        let _listener = listen(&path);
        with_sock_env(&path, || {
            let r = rec("grep");
            let fat = "x".repeat(4000);
            let start = std::time::Instant::now();
            // Comfortably past `SNDBUF`, so this really does run out of buffer
            // rather than merely fitting.
            for _ in 0..512 {
                emit_start(&r, &fat, &fat);
            }
            assert!(start.elapsed() < Duration::from_secs(2), "sender blocked on a full buffer");
        });
    }

    #[test]
    fn a_huge_prompt_is_elided_under_the_event_cap() {
        let _g = lock();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("live.sock");
        let listener = listen(&path);
        with_sock_env(&path, || {
            let r = rec("check_output");
            let huge = "α".repeat(80_000); // 200 KiB of UTF-8, not ASCII
            emit_start(&r, "sys", &huge);
            let payload = recv_frame(&listener);
            assert!(payload.len() <= MAX_EVENT, "frame payload was {} bytes", payload.len());
            let v: Value = serde_json::from_slice(&payload).unwrap();
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

    /// A `call.start` with no id in `starts` and no `call.end` — a process
    /// killed mid-call.
    fn started(store: &LiveStore, id: &str, ts: f64) {
        let ev = json!({
            "v": 1, "id": id, "run": "r", "op": format!("{id}-op"),
            "kind": "call.start", "ts": ts, "tool": "shell_safety",
            "preset": "shell_safety", "via": "hook",
        });
        assert!(store.apply_json(&ev.to_string()));
    }

    #[test]
    fn a_call_that_never_reports_is_abandoned_not_left_running_forever() {
        let store = LiveStore::new();
        store.set_abandon_after_secs(150);
        started(&store, "k-1", 1000.0);

        // Still inside the window: a slow call is not a dead one.
        assert_eq!(store.sweep(1100.0), 0);
        assert_eq!(store.inflight_rows()[0].kind, "running");

        assert_eq!(store.sweep(1200.0), 1, "past the bound, give up on it");
        let row = &store.inflight_rows()[0];
        assert_eq!(row.kind, ABANDONED);
        assert!(!row.ok, "an abandoned call is not a success");
        assert_eq!(row.ms, 200_000, "elapsed stands in for the ms it never sent");
        assert!(row.summary.as_deref().unwrap().contains("150s"), "{:?}", row.summary);

        // Idempotent: sweeping again abandons nothing new.
        assert_eq!(store.sweep(1300.0), 0);
        assert_eq!(store.inflight_rows().len(), 1, "abandoning is a downgrade, not a delete");
    }

    /// The whole point of the two-threshold design: `reap` cannot clear a row
    /// the log will never mention, which is exactly the killed-process case.
    #[test]
    fn reap_alone_cannot_clear_a_row_the_log_never_receives() {
        let store = LiveStore::new();
        store.set_abandon_after_secs(150);
        started(&store, "k-1", 1000.0);
        store.reap(["some-other-id", "and-another"]);
        assert_eq!(store.inflight_rows().len(), 1, "reap only knows ids the log has");
        store.sweep(1200.0);
        assert_eq!(store.inflight_rows()[0].kind, ABANDONED);
    }

    #[test]
    fn a_late_call_end_overrides_an_abandoned_row() {
        let store = LiveStore::new();
        store.set_abandon_after_secs(150);
        started(&store, "k-1", 1000.0);
        store.sweep(1200.0);
        assert_eq!(store.inflight_rows()[0].kind, ABANDONED);

        let end = json!({
            "v": 1, "id": "k-1", "run": "r", "op": "k-1-op",
            "kind": "call.end", "ts": 1201.0, "ms": 900,
            "outcome": {"kind": "ok", "summary": "done"},
            "usage": {"prompt_tokens": 3, "completion_tokens": 5},
        });
        assert!(store.apply_json(&end.to_string()));
        let row = &store.inflight_rows()[0];
        assert_eq!(row.kind, "ok", "the real outcome wins over the guess");
        assert!(row.ok);
        assert_eq!(row.ms, 900);
    }

    #[test]
    fn a_log_line_still_reaps_an_abandoned_row() {
        let store = LiveStore::new();
        store.set_abandon_after_secs(150);
        started(&store, "k-1", 1000.0);
        store.sweep(1200.0);
        store.reap(["k-1"]);
        assert!(store.inflight_rows().is_empty(), "the log is still authoritative");
    }

    #[test]
    fn abandoned_rows_are_capped_and_the_newest_survive() {
        let store = LiveStore::new();
        store.set_abandon_after_secs(10);
        for i in 0..(MAX_ABANDONED + 5) {
            started(&store, &format!("k-{i}"), 1000.0 + i as f64);
        }
        // Past every row's abandon bound, but well inside the retention
        // window — this is the count cap under test, not the age cap.
        store.sweep(1100.0);
        let rows = store.inflight_rows();
        assert_eq!(rows.len(), MAX_ABANDONED, "unbounded growth is the bug being fixed");
        assert!(rows.iter().any(|r| r.id == format!("k-{}", MAX_ABANDONED + 4)), "newest kept");
        assert!(!rows.iter().any(|r| r.id == "k-0"), "oldest evicted");
    }

    /// The header warning counts these, so "since the daemon started" would
    /// leave it lit long after the cause was fixed.
    #[test]
    fn abandoned_rows_stop_being_counted_once_they_are_stale() {
        let store = LiveStore::new();
        store.set_abandon_after_secs(150);
        started(&store, "k-1", 1000.0);
        store.sweep(1200.0);
        assert_eq!(store.inflight_split(), (0, 1));

        // Retention runs from when the call started, not from when we gave up
        // on it — the row's `ts` is the only timestamp it ever had.
        store.sweep(1000.0 + ABANDONED_RETAIN_SECS);
        assert_eq!(store.inflight_split(), (0, 1), "still inside the retention window");

        store.sweep(1000.0 + ABANDONED_RETAIN_SECS + 1.0);
        assert_eq!(store.inflight_split(), (0, 0), "aged out");
    }

    #[test]
    fn inflight_split_separates_the_live_from_the_lost() {
        let store = LiveStore::new();
        store.set_abandon_after_secs(150);
        started(&store, "dead", 1000.0);
        started(&store, "live", 1190.0);
        store.sweep(1200.0);
        assert_eq!(store.inflight_split(), (1, 1));
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

    fn start_ev(n: usize) -> String {
        json!({
            "v": 1, "id": format!("id-{n}"), "run": "r", "op": format!("op-{n}"),
            "kind": "call.start", "ts": 1.0, "tool": "t", "preset": "t",
        })
        .to_string()
    }

    /// The core regression: a reader that falls behind for a moment is *not* a
    /// reader that has gone away. 32 slots is under two seconds of a streaming
    /// reply, so this is the ordinary fate of a backgrounded tab.
    #[test]
    fn a_subscriber_that_stutters_once_survives_and_keeps_receiving() {
        let store = LiveStore::new();
        let rx = store.subscribe();

        // Nobody draining: the channel fills, and every event past `SUB_CAP`
        // comes back `Full`.
        for n in 0..(SUB_CAP + 16) {
            assert!(store.apply_json(&start_ev(n)));
        }
        assert_eq!(store.subscriber_count(), 1, "a full channel is backpressure, not death");

        // The overflow is dropped — this is telemetry, and there is no replay
        // buffer — but the buffered prefix is intact and in order.
        let drained: Vec<String> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
        assert_eq!(drained.len(), SUB_CAP, "the channel held exactly its capacity");
        let first: Value = serde_json::from_str(&drained[0]).unwrap();
        assert_eq!(first["id"], "id-0");

        // And the tab is still wired up once it has caught up, which is the
        // whole point: it used to be gone for good.
        assert!(store.apply_json(&start_ev(9_000)));
        let got: Value = serde_json::from_str(&rx.try_recv().expect("still subscribed")).unwrap();
        assert_eq!(got["id"], "id-9000");
    }

    /// The other half of the split: removing a subscriber must close its
    /// channel *now*, so `handle_stream` breaks out and releases its
    /// `MAX_STREAMS` slot instead of sitting on a 15 s keepalive.
    #[test]
    fn a_removed_subscriber_sees_its_channel_close_at_once() {
        let store = LiveStore::new();
        let rx = store.subscribe();
        store.lock().subs.clear(); // what the `Disconnected` arm does
        let t = std::time::Instant::now();
        assert!(matches!(
            rx.recv_timeout(Duration::from_secs(15)),
            Err(mpsc::RecvTimeoutError::Disconnected)
        ));
        assert!(t.elapsed() < Duration::from_secs(1), "the handler would have blocked");
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
        // more text in one window than one event may carry.
        let (mut co, out) = collect(Duration::from_secs(3600));
        let block = "x".repeat(1024);
        for _ in 0..12 {
            co.push(&block);
        }
        let got = out.lock().unwrap().clone();
        assert!(!got.is_empty(), "the size cap never fired");
        for (text, _) in &got {
            assert!(text.len() < MAX_EVENT, "a chunk would not fit one event");
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
        let listener = listen(&path);
        with_sock_env(&path, || {
            let mut r = rec("grep");
            r.silent = true;
            with_token_stream(&r, |sink| {
                for _ in 0..200 {
                    sink("token ");
                }
            });
            assert!(nothing_arrived(&listener), "a silent record streamed tokens");
        });
    }

    #[test]
    fn a_streamed_call_lands_as_call_token_events() {
        let _g = lock();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("live.sock");
        let listener = listen(&path);
        with_sock_env(&path, || {
            let r = rec("check_output");
            with_token_stream(&r, |sink| {
                sink("Hello, ");
                sink("world");
            });
            let v = recv_event(&listener);
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

    fn find_ev(op: &str, round: u64, kind: &str, extra: &Value) -> String {
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
            emit_find("op-1", 1, "patterns", &json!({"patterns": []}));
            emit_find("op-1", 1, "hits", &json!({"candidates": []}));
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
        let listener = listen(&path);
        with_sock_env(&path, || {
            assert!(is_listening());
            emit_find("op-7", 2, "reflect", &json!({"answered": false, "patterns": ["draw"]}));
            let v = recv_event(&listener);
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
        let listener = listen(&path);
        with_sock_env(&path, || {
            // A pathological keep list: far past MAX_EVENT in one array.
            let keeps: Vec<Value> = (0..4000)
                .map(|i| json!({"file": format!("src/{i}/{}.rs", "long".repeat(20)), "line": i, "why": "y".repeat(120)}))
                .collect();
            emit_find("op-big", 1, "rerank", &json!({"keeps": keeps}));
            let payload = recv_frame(&listener);
            assert!(payload.len() <= MAX_EVENT, "frame payload was {} bytes", payload.len());
            let v: Value = serde_json::from_slice(&payload).expect("still valid JSON");
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
            assert!(store.apply_json(&find_ev("op-a", round, kind, &json!({"n": round}))));
        }
        // The same part of the same round arriving twice is one round, updated.
        assert!(store.apply_json(&find_ev("op-a", 1, "patterns", &json!({"n": 99}))));

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
        store.apply_json(&find_ev("op-a", 1, "patterns", &json!({"patterns": ["a1"]})));
        store.apply_json(&find_ev("op-b", 1, "patterns", &json!({"patterns": ["b1"]})));
        store.apply_json(&find_ev("op-b", 2, "patterns", &json!({"patterns": ["b2"]})));
        store.apply_json(&find_ev("op-a", 1, "hits", &json!({"union": 3})));
        store.apply_json(&find_ev("op-b", 2, "hits", &json!({"union": 9})));

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
        store.apply_json(&find_ev("op-late", 3, "hits", &json!({"union": 12})));
        let rounds = store.find_rounds("op-late").unwrap();
        let rounds = rounds.as_array().unwrap();
        assert_eq!(rounds.len(), 1);
        assert_eq!(rounds[0]["round"], 3);
        assert!(rounds[0]["patterns"].is_null(), "an unseen part is simply absent");
    }

    #[test]
    fn an_unknown_find_part_is_rejected() {
        let store = LiveStore::new();
        assert!(!store.apply_json(&find_ev("op-a", 1, "tokens", &json!({}))));
        assert!(store.find_rounds("op-a").is_none());
    }

    #[test]
    fn find_rounds_are_lru_capped_by_op() {
        let store = LiveStore::new();
        for i in 0..(MAX_FINDS + 2) {
            store.apply_json(&find_ev(&format!("op-{i}"), 1, "patterns", &json!({})));
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
        store.apply_json(&find_ev("op-a", 1, "rerank", &json!({"keeps": []})));
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
