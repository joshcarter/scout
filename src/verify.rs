//! Command execution for `check_output`.
//!
//! Deliberately small: `run_command_capture`, `truncate_diagnostic` and
//! `detect_language`, and nothing else.  An earlier design had a
//! place→build→restore→retry repair loop here, driven by `write_tests` and
//! `refactor` presets.  Those presets were cut — a local model is not good at
//! composing net-new code, and the loop and its whole support cast went with
//! them.
//!
//! What is left is one job: run a command the caller named, capture what it
//! printed — capped as the bytes arrive, never buffered whole — and decide
//! whether it is still making progress or has wedged.

// See `render.rs` for why this writes into the buffer instead of pushing a
// freshly-formatted `String`, and why the infallible `Result` is discarded.
use std::fmt::Write as _;

use std::collections::VecDeque;
use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

// ── Language detection ────────────────────────────────────────────────

/// Project language inferred from manifest files in the project root.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Language {
    Rust,
    Go,
    Python,
    Node,
}

impl Language {
    /// Lower-case name, as injected into the `check_output` preset args.
    pub fn as_str(self) -> &'static str {
        match self {
            Language::Rust => "rust",
            Language::Go => "go",
            Language::Python => "python",
            Language::Node => "node",
        }
    }
}

/// Detect the primary language of a project by inspecting manifest files.
///
/// Checks in order: Cargo.toml → go.mod → pyproject.toml / setup.py → package.json.
/// Returns `None` if no known manifest is found.
pub fn detect_language(dir: &Path) -> Option<Language> {
    if dir.join("Cargo.toml").exists() {
        Some(Language::Rust)
    } else if dir.join("go.mod").exists() {
        Some(Language::Go)
    } else if dir.join("pyproject.toml").exists() || dir.join("setup.py").exists() {
        Some(Language::Python)
    } else if dir.join("package.json").exists() {
        Some(Language::Node)
    } else {
        None
    }
}

// ── Command capture ───────────────────────────────────────────────────

/// Default output cap for captured build/test output.  16 KB keeps both the
/// first actionable error (near the top) and the summary line (at the bottom)
/// via head+tail elision.
pub const MAX_OUTPUT_BYTES: usize = 16 * 1024;

/// How long a command may print *nothing* before it is treated as wedged.
///
/// Deliberately generous, and it must stay that way — the obvious "optimise
/// this to 30s" is wrong twice over:
///
/// * Silence is normal in a healthy build.  A linker, an LTO pass, a `cargo
///   test` sitting inside one slow test, a `docker build` layer: all of them
///   are legitimately quiet for minutes while doing exactly what was asked.
/// * Because we hand the child a *pipe* rather than a TTY, libc switches from
///   line buffering to block buffering at 4–8 KB.  Some toolchains (CPython,
///   Node) therefore emit nothing at all for long stretches even when they are
///   printing steadily, because the block has not filled yet.  Shortening this
///   would kill healthy processes for a property of stdio, not of the build.
///
/// Silence alone is never sufficient evidence of a hang, which is why the poll
/// loop checks `try_wait()` alongside this: a process that has already exited
/// is reaped on the next tick regardless of how long it was quiet.
pub const IDLE_TIMEOUT: Duration = Duration::from_secs(120);

/// How often the supervising thread wakes to poll `try_wait()` and the two
/// deadlines.  Short enough that a 200 ms test timeout is still honoured,
/// long enough that a ten-minute build costs a few thousand wakeups.
const POLL_INTERVAL: Duration = Duration::from_millis(50);

/// How long to wait for a reader thread to finish after the child is gone.
///
/// Normally instant — the pipe hits EOF the moment the last writer closes it.
/// The deadline is there for the case where something the command left behind
/// still holds the write end: better to return with a partial capture than to
/// pin a thread for the lifetime of the process.  `scout mcp` is one process
/// per agent session, so a thread leaked here is leaked for hours.
const READER_DRAIN: Duration = Duration::from_secs(2);

/// Per-stream bytes held back from the retention budget so that a capped
/// stream plus its elision marker still fits inside `max_output_bytes` — and
/// `truncate_diagnostic` therefore stays a no-op rather than eliding an
/// already-elided string and nesting two markers.
const ELISION_RESERVE: usize = 64;

/// Which deadline ran out.  The distinction is the whole point: a wall-clock
/// stop means "still working, just slow — give it longer", and an idle stop
/// means "printed nothing for two minutes — it is stuck, a longer timeout
/// will not help".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimeoutKind {
    /// No output for `IDLE_TIMEOUT` while the process was still alive.
    Idle,
    /// The caller's outer wall-clock cap, the last-resort circuit breaker.
    WallClock,
}

impl TimeoutKind {
    pub fn as_str(self) -> &'static str {
        match self {
            TimeoutKind::Idle => "idle",
            TimeoutKind::WallClock => "wall_clock",
        }
    }
}

/// What one command run produced.
///
/// `timed_out` is `Some` exactly when the process was killed rather than
/// allowed to finish, which is a different thing from `exit_ok == false` and
/// has to be reported as such — see `check_output`, which short-circuits the
/// classifier on it.
#[derive(Debug, Clone)]
pub struct Capture {
    pub exit_ok: bool,
    pub output: String,
    pub timed_out: Option<TimeoutKind>,
    pub elapsed: Duration,
}

/// Run a shell command string via `sh -c` under the default idle deadline.
///
/// Output is captured from both stdout and stderr and capped at
/// `max_output_bytes` via head+tail elision.  Always captures output so the
/// classifier receives stdout from passing builds (e.g. "test result: ok. N
/// passed") as well as failure output.
pub fn run_command_capture(
    cmd: &str,
    dir: &Path,
    timeout: Duration,
    max_output_bytes: usize,
) -> Capture {
    capture_with_deadlines(cmd, dir, timeout, IDLE_TIMEOUT, max_output_bytes)
}

/// The real thing, with both deadlines spelled out.
///
/// Separate from `run_command_capture` so tests can use an idle deadline they
/// do not have to wait two minutes for — and so a future caller with a
/// different notion of "wedged" (the `git()` provider's timeout, say) can pick
/// its own without a second implementation.
///
/// Structure, and why it is not the obvious one: the `Child` stays in *this*
/// thread.  The previous version moved it into a thread running
/// `wait_with_output()` and blocked the caller on a channel, which cost three
/// bugs at once — `try_wait()` was unreachable so the only stop signal was the
/// wall clock; the kill went to `sh`'s pid, which for anything compound (`a &&
/// b`, a pipeline) leaves the real work running detached; and the reader never
/// saw EOF afterwards because the orphan still held the pipe, so the thread and
/// its unbounded buffer leaked for the life of the process.
///
/// So: one reader thread per pipe, each appending into a bounded buffer and
/// stamping a shared "last byte seen" clock, while this thread polls
/// `try_wait()` and the two deadlines.
// One honest sequence, and deliberately so: the doc comment above is a record
// of what splitting it cost last time.  The child, its two reader threads and
// the two deadlines are a single piece of state; handing any part of it to a
// helper means handing over the rest as well.
#[allow(clippy::too_many_lines)]
pub fn capture_with_deadlines(
    cmd: &str,
    dir: &Path,
    wall_clock: Duration,
    idle: Duration,
    max_output_bytes: usize,
) -> Capture {
    let started = Instant::now();

    let mut command = Command::new("sh");
    command
        .args(["-c", cmd])
        .current_dir(dir)
        // Never the parent's stdin: under `scout mcp` that is the JSON-RPC
        // channel, and a command that reads it would eat the protocol.  It also
        // turns "waiting on a prompt forever" into an immediate EOF, which is
        // the honest answer for a non-interactive build runner.
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // Put the child in a session (and so a process group) of its own, so
        // the timeout path can signal the whole tree with one `kill(-pgid)`.
        // The child loses its controlling terminal, which changes nothing here:
        // its stdout was already a pipe, so anything that probes for a TTY
        // already saw "no".
        //
        // SAFETY: between fork and exec only async-signal-safe work is legal.
        // `setsid` is exactly that — one syscall, no allocation, no locks, no
        // state shared with the parent.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }

    let mut child = match command.spawn() {
        Ok(c) => c,
        Err(e) => {
            return Capture {
                exit_ok: false,
                output: format!("sh: failed to spawn: {e}"),
                timed_out: None,
                elapsed: started.elapsed(),
            }
        }
    };

    let pid = child.id();
    // Milliseconds since `started` at which a reader last got bytes.  An atomic
    // rather than a `Mutex<Instant>`: two writers, one reader, no invariant
    // beyond "monotonic", and the poll loop reads it every 50 ms.
    let last_output_ms = Arc::new(AtomicU64::new(0));
    let out_buf = Arc::new(Mutex::new(BoundedBuffer::new(max_output_bytes)));
    let err_buf = Arc::new(Mutex::new(BoundedBuffer::new(max_output_bytes)));

    let mut readers = Vec::new();
    if let Some(pipe) = child.stdout.take() {
        readers.push(spawn_reader(
            pipe,
            Arc::clone(&out_buf),
            Arc::clone(&last_output_ms),
            started,
        ));
    }
    if let Some(pipe) = child.stderr.take() {
        readers.push(spawn_reader(
            pipe,
            Arc::clone(&err_buf),
            Arc::clone(&last_output_ms),
            started,
        ));
    }

    let mut exit_ok = false;
    let mut timed_out = None;
    let mut wait_error = None;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                exit_ok = status.success();
                break;
            }
            Ok(None) => {}
            Err(e) => {
                wait_error = Some(format!("sh: wait failed: {e}"));
                break;
            }
        }
        let elapsed = started.elapsed();
        if elapsed >= wall_clock {
            timed_out = Some(TimeoutKind::WallClock);
            break;
        }
        let quiet_for =
            elapsed.saturating_sub(Duration::from_millis(last_output_ms.load(Ordering::Relaxed)));
        if quiet_for >= idle {
            timed_out = Some(TimeoutKind::Idle);
            break;
        }
        thread::sleep(POLL_INTERVAL);
    }

    if timed_out.is_some() {
        kill_process_group(pid);
        // The group is dead; this only reaps the zombie `sh` left behind.
        let _ = child.wait();
    }

    // On a clean exit the readers see EOF at once.  If they do not, something
    // the command left running still holds the write end — kill the group and
    // give them one more chance rather than blocking here or detaching blind.
    if !join_within(&mut readers, READER_DRAIN) {
        kill_process_group(pid);
        let _ = join_within(&mut readers, READER_DRAIN);
    }

    if let Some(msg) = wait_error {
        return Capture {
            exit_ok: false,
            output: msg,
            timed_out: None,
            elapsed: started.elapsed(),
        };
    }

    // Combined in stream order, not chronological order — the two pipes were
    // never interleaved here and making them so would rewrite every caller's
    // expectations for no gain.
    let stdout = lock(&out_buf).render();
    let stderr = lock(&err_buf).render();
    let mut output = format!("{stdout}\n{stderr}").trim().to_string();
    truncate_diagnostic(&mut output, max_output_bytes);

    Capture { exit_ok, output, timed_out, elapsed: started.elapsed() }
}

/// Take a buffer lock, ignoring poisoning: a panicked reader loses its own
/// thread, and the bytes it already appended are still worth reporting.
fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Drain one pipe into `buf`, stamping `last_output_ms` on every read that
/// returned bytes.  That stamp is the liveness signal the poll loop watches.
fn spawn_reader<R: Read + Send + 'static>(
    mut pipe: R,
    buf: Arc<Mutex<BoundedBuffer>>,
    last_output_ms: Arc<AtomicU64>,
    started: Instant,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let mut chunk = [0u8; 8192];
        loop {
            match pipe.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => {
                    last_output_ms.store(started.elapsed().as_millis() as u64, Ordering::Relaxed);
                    lock(&buf).push(&chunk[..n]);
                }
                // EINTR is not a read failure: go round and read again.
                Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(_) => break,
            }
        }
    })
}

/// Join every handle, giving up after `deadline`.  Returns whether they all
/// finished; the ones that did not are dropped (detached) by the caller, which
/// is the least-bad outcome once the process group has already been signalled.
fn join_within(handles: &mut Vec<thread::JoinHandle<()>>, deadline: Duration) -> bool {
    let start = Instant::now();
    loop {
        if handles.iter().all(thread::JoinHandle::is_finished) {
            for h in handles.drain(..) {
                let _ = h.join();
            }
            return true;
        }
        if start.elapsed() >= deadline {
            return false;
        }
        thread::sleep(POLL_INTERVAL);
    }
}

/// Kill the child *and everything it forked*.
///
/// The child called `setsid`, so its pid is also its process-group id and one
/// negative-pid `kill` reaches the whole tree.  Killing the pid alone — what
/// this used to shell out to `/bin/kill -9` to do — is only correct when `sh`
/// exec'd its argument, which it does for `sleep 30` and does not for `cd x &&
/// cargo test`.  That is the orphan bug: `sh` died, `cargo` did not.
#[cfg(unix)]
fn kill_process_group(pid: u32) {
    // SAFETY: a plain syscall with a pid we own; an already-dead group is an
    // ignored ESRCH, not undefined behaviour.
    unsafe {
        let _ = libc::kill(-(pid as i32), libc::SIGKILL);
    }
}

/// Windows has no process groups in the POSIX sense; `/T` is the tree flag and
/// carries the same intent as the negative pid above.
#[cfg(windows)]
fn kill_process_group(pid: u32) {
    let _ = Command::new("taskkill").args(["/PID", &pid.to_string(), "/F", "/T"]).status();
}

/// One stream's capture, capped as the bytes arrive.
///
/// The first `head_cap` bytes are kept verbatim, the last `tail_cap` in a ring,
/// and everything squeezed out between them is counted.  Peak resident size is
/// therefore `head_cap + tail_cap` — bounded by `max_output_bytes` — no matter
/// how much the command prints, which is the point: the previous version read
/// both pipes to EOF and only then truncated, so a chatty ten-minute build was
/// gigabytes resident before anything capped it.
struct BoundedBuffer {
    head: Vec<u8>,
    tail: VecDeque<u8>,
    head_cap: usize,
    tail_cap: usize,
    dropped: usize,
}

impl BoundedBuffer {
    fn new(limit: usize) -> Self {
        // Same 75/25 split `truncate_diagnostic` uses, for the same reason:
        // the first actionable error is near the top and the verdict line
        // ("test result: FAILED") is at the very bottom.
        let budget = limit.saturating_sub(ELISION_RESERVE).max(2);
        let head_cap = budget * 3 / 4;
        BoundedBuffer {
            head: Vec::new(),
            tail: VecDeque::new(),
            head_cap,
            tail_cap: budget - head_cap,
            dropped: 0,
        }
    }

    fn push(&mut self, mut bytes: &[u8]) {
        if self.head.len() < self.head_cap {
            let n = (self.head_cap - self.head.len()).min(bytes.len());
            self.head.extend_from_slice(&bytes[..n]);
            bytes = &bytes[n..];
        }
        for &b in bytes {
            if self.tail.len() >= self.tail_cap {
                self.tail.pop_front();
                self.dropped += 1;
            }
            if self.tail_cap == 0 {
                self.dropped += 1;
            } else {
                self.tail.push_back(b);
            }
        }
    }

    fn render(&self) -> String {
        let tail: Vec<u8> = self.tail.iter().copied().collect();
        if self.dropped == 0 {
            // Nothing was squeezed out, so head and tail are contiguous: decode
            // them as one buffer or a multi-byte char straddling `head_cap`
            // would come back as two replacement characters.
            let mut whole = self.head.clone();
            whole.extend_from_slice(&tail);
            return String::from_utf8_lossy(&whole).into_owned();
        }
        let mut out = String::from_utf8_lossy(&self.head).into_owned();
        let _ = writeln!(out, "\n...[{} bytes elided]...", self.dropped);
        out.push_str(&String::from_utf8_lossy(&tail));
        out
    }
}

/// Keep the first 75% and last 25% of `limit` bytes, replacing the middle with
/// a `"\n...[N bytes elided]...\n"` marker.  No-op when `s` fits.
///
/// Compiler/test output puts its summary line (`test result: FAILED`, `aborting
/// due to N errors`) at the bottom — a head-only truncation drops it.
/// Head+tail preserves both the first actionable error and the outcome line.
///
/// Both split points are walked back to the nearest UTF-8 char boundary so the
/// result is always valid UTF-8 regardless of where the limit falls.
pub fn truncate_diagnostic(s: &mut String, limit: usize) {
    assert!(limit > 0, "truncate_diagnostic: limit must be > 0");
    if s.len() <= limit {
        return;
    }
    let head_limit = limit * 3 / 4;
    let tail_limit = limit - head_limit;

    // Head: walk back to char boundary.
    let head_end = (0..=head_limit).rev().find(|&i| s.is_char_boundary(i)).unwrap_or(0);

    // Tail: walk forward from `s.len() - tail_limit` to find a char boundary.
    let tail_start_raw = s.len() - tail_limit;
    let tail_start = (tail_start_raw..=s.len()).find(|&i| s.is_char_boundary(i)).unwrap_or(s.len());

    let elided = tail_start - head_end;
    let marker = format!("\n...[{elided} bytes elided]...\n");
    let tail = s[tail_start..].to_string();
    s.truncate(head_end);
    s.push_str(&marker);
    s.push_str(&tail);
}

// ── Tests ─────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn make_temp() -> TempDir {
        tempfile::tempdir().expect("tempdir")
    }

    // ── detect_language ───────────────────────────────────────────────

    #[test]
    fn detect_rust_from_cargo_toml() {
        let dir = make_temp();
        fs::write(dir.path().join("Cargo.toml"), "[package]\nname=\"x\"").unwrap();
        assert_eq!(detect_language(dir.path()), Some(Language::Rust));
    }

    #[test]
    fn detect_go_from_go_mod() {
        let dir = make_temp();
        fs::write(dir.path().join("go.mod"), "module example.com/x\ngo 1.21").unwrap();
        assert_eq!(detect_language(dir.path()), Some(Language::Go));
    }

    #[test]
    fn detect_python_from_pyproject_toml() {
        let dir = make_temp();
        fs::write(dir.path().join("pyproject.toml"), "[build-system]").unwrap();
        assert_eq!(detect_language(dir.path()), Some(Language::Python));
    }

    #[test]
    fn detect_python_from_setup_py() {
        let dir = make_temp();
        fs::write(dir.path().join("setup.py"), "from setuptools import setup").unwrap();
        assert_eq!(detect_language(dir.path()), Some(Language::Python));
    }

    #[test]
    fn detect_node_from_package_json() {
        let dir = make_temp();
        fs::write(dir.path().join("package.json"), "{}").unwrap();
        assert_eq!(detect_language(dir.path()), Some(Language::Node));
    }

    #[test]
    fn detect_none_when_no_manifest() {
        let dir = make_temp();
        assert_eq!(detect_language(dir.path()), None);
    }

    #[test]
    fn detect_rust_takes_precedence_over_go() {
        // Cargo.toml + go.mod in same dir → Rust wins (checked first)
        let dir = make_temp();
        fs::write(dir.path().join("Cargo.toml"), "").unwrap();
        fs::write(dir.path().join("go.mod"), "").unwrap();
        assert_eq!(detect_language(dir.path()), Some(Language::Rust));
    }

    #[test]
    fn language_names_are_stable() {
        assert_eq!(Language::Rust.as_str(), "rust");
        assert_eq!(Language::Node.as_str(), "node");
    }

    // ── truncate_diagnostic ───────────────────────────────────────────

    #[test]
    fn truncate_diagnostic_no_op_when_under_limit() {
        let mut s = "short".to_string();
        truncate_diagnostic(&mut s, 8192);
        assert_eq!(s, "short", "short string should be unchanged");
    }

    #[test]
    fn truncate_diagnostic_head_tail_elision() {
        // 20_000 'x' bytes with limit=8192: head=6144, tail=2048, elide middle.
        let limit = 8192usize;
        let head = limit * 3 / 4; // 6144
        let tail = limit - head; // 2048
        let mut s = "x".repeat(20_000);
        truncate_diagnostic(&mut s, limit);
        assert!(s.starts_with(&"x".repeat(head)), "head not preserved");
        assert!(s.ends_with(&"x".repeat(tail)), "tail not preserved");
        assert!(s.contains("bytes elided"), "elision marker missing");
        let x_count = s.chars().filter(|&c| c == 'x').count();
        assert_eq!(x_count, head + tail, "should keep exactly head+tail x bytes");
    }

    #[test]
    fn truncate_diagnostic_respects_utf8_char_boundary() {
        // "é" is 2 bytes (0xC3 0xA9).  limit=100: head=75, tail=25.  Place "é"
        // at byte offset 74 so it straddles the head split point.
        let limit = 100usize;
        let mut s = "a".repeat(74) + "é" + "b".repeat(200).as_str();
        truncate_diagnostic(&mut s, limit);
        assert!(std::str::from_utf8(s.as_bytes()).is_ok(), "result is not valid UTF-8");
        assert!(s.contains("bytes elided"), "elision marker missing");
    }

    #[test]
    fn truncate_diagnostic_no_op_at_exact_limit() {
        let mut s = "x".repeat(100);
        truncate_diagnostic(&mut s, 100);
        assert_eq!(s.len(), 100, "string at exact limit should not be modified");
        assert!(!s.contains("elided"), "no marker should be added");
    }

    #[test]
    fn truncate_diagnostic_utf8_boundary_in_tail() {
        // "é" is 2 bytes. Place it at the tail split point to verify we don't
        // panic and the result is valid UTF-8.
        let limit = 100usize;
        let mut s = "a".repeat(100) + &"b".repeat(23) + "é" + "c";
        assert!(s.len() > limit);
        truncate_diagnostic(&mut s, limit);
        assert!(std::str::from_utf8(s.as_bytes()).is_ok(), "result is not valid UTF-8");
        assert!(s.contains("bytes elided"), "elision marker missing");
    }

    #[test]
    fn truncate_diagnostic_empty_string_no_op() {
        let mut s = String::new();
        truncate_diagnostic(&mut s, 100);
        assert!(s.is_empty(), "empty string should remain empty");
    }

    // ── run_command_capture ───────────────────────────────────────────

    #[test]
    fn run_command_capture_success_captures_stdout() {
        let dir = make_temp();
        let c = run_command_capture(
            "echo hello-from-capture",
            dir.path(),
            Duration::from_secs(5),
            MAX_OUTPUT_BYTES,
        );
        assert!(c.exit_ok, "expected exit 0");
        assert!(c.timed_out.is_none(), "a fast command must not report a deadline");
        assert!(c.output.contains("hello-from-capture"), "stdout not captured: {}", c.output);
    }

    #[test]
    fn run_command_capture_failure_captures_output_and_returns_false() {
        let dir = make_temp();
        let c = run_command_capture(
            "echo error-text && exit 1",
            dir.path(),
            Duration::from_secs(5),
            MAX_OUTPUT_BYTES,
        );
        assert!(!c.exit_ok, "expected non-zero exit");
        assert!(c.timed_out.is_none(), "a non-zero exit is not a timeout");
        assert!(c.output.contains("error-text"), "stderr/stdout not captured: {}", c.output);
    }

    #[test]
    fn run_command_capture_captures_stderr_too() {
        let dir = make_temp();
        let c = run_command_capture(
            "echo to-stderr 1>&2",
            dir.path(),
            Duration::from_secs(5),
            MAX_OUTPUT_BYTES,
        );
        assert!(c.exit_ok);
        assert!(c.output.contains("to-stderr"), "stderr not captured: {}", c.output);
    }

    #[test]
    fn run_command_capture_timeout_returns_false() {
        let dir = make_temp();
        let c = run_command_capture(
            "sleep 30",
            dir.path(),
            Duration::from_millis(200),
            MAX_OUTPUT_BYTES,
        );
        assert!(!c.exit_ok, "expected timeout failure");
        assert_eq!(
            c.timed_out,
            Some(TimeoutKind::WallClock),
            "a silent-but-short command hits the outer cap, not the idle deadline"
        );
        assert!(c.elapsed < Duration::from_secs(2), "did not respect timeout: {:?}", c.elapsed);
    }

    #[test]
    fn a_quiet_command_hits_the_idle_deadline_before_the_outer_cap() {
        let dir = make_temp();
        let c = capture_with_deadlines(
            "sleep 30",
            dir.path(),
            Duration::from_secs(30),
            Duration::from_millis(200),
            MAX_OUTPUT_BYTES,
        );
        assert_eq!(c.timed_out, Some(TimeoutKind::Idle), "silence is what fired here");
        assert!(c.elapsed < Duration::from_secs(3), "idle deadline ignored: {:?}", c.elapsed);
    }

    #[test]
    fn a_command_that_keeps_printing_is_never_killed_for_being_slow() {
        // The whole reason the idle deadline exists rather than a shorter wall
        // clock: this runs for ~1s with a 250ms idle deadline and must survive,
        // because it is visibly making progress the entire time.
        let dir = make_temp();
        let c = capture_with_deadlines(
            "i=0; while [ $i -lt 10 ]; do echo tick-$i; sleep 0.1; i=$((i+1)); done",
            dir.path(),
            Duration::from_secs(30),
            Duration::from_millis(250),
            MAX_OUTPUT_BYTES,
        );
        assert_eq!(c.timed_out, None, "a chatty command must not be treated as wedged");
        assert!(c.exit_ok, "output: {}", c.output);
        assert!(c.elapsed > Duration::from_millis(500), "the test did not actually run long");
        assert!(c.output.contains("tick-9"), "output: {}", c.output);
    }

    /// Is `pid` still alive?  `kill(pid, 0)` is the portable liveness probe:
    /// it delivers no signal and fails with ESRCH once the pid is gone.
    #[cfg(unix)]
    fn alive(pid: i32) -> bool {
        unsafe { libc::kill(pid, 0) == 0 }
    }

    #[test]
    #[cfg(unix)]
    fn a_timeout_kills_the_whole_process_group_not_just_sh() {
        // The orphan bug, made falsifiable.  `sh -c 'sleep 300 & wait'` does not
        // exec, so `sh`'s pid is not `sleep`'s — killing the pid alone (what
        // this code used to do) leaves `sleep` running, reparented to init.
        let dir = make_temp();
        let pidfile = dir.path().join("grandchild.pid");
        let cmd = format!("sleep 300 & echo $! > {}; wait", pidfile.display());
        let c = capture_with_deadlines(
            &cmd,
            dir.path(),
            Duration::from_millis(400),
            Duration::from_secs(30),
            MAX_OUTPUT_BYTES,
        );
        assert_eq!(c.timed_out, Some(TimeoutKind::WallClock));

        let pid: i32 = fs::read_to_string(&pidfile)
            .expect("the shell should have recorded the grandchild pid")
            .trim()
            .parse()
            .expect("pid");
        // The grandchild is not our child, so its death is observed via
        // reparenting to init — poll briefly rather than assuming it is
        // instantaneous.
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        while alive(pid) && std::time::Instant::now() < deadline {
            thread::sleep(Duration::from_millis(20));
        }
        assert!(!alive(pid), "grandchild {pid} survived the timeout — the process group leaked");
    }

    #[test]
    fn run_command_capture_handles_special_chars_in_command() {
        // Commands with quotes, equals and spaces pass through `sh -c` intact.
        let dir = make_temp();
        let c = run_command_capture(
            "echo 'key=value with spaces'",
            dir.path(),
            Duration::from_secs(5),
            MAX_OUTPUT_BYTES,
        );
        assert!(c.exit_ok, "command with special chars should succeed");
        assert!(c.output.contains("key=value with spaces"), "output: {}", c.output);
    }

    #[test]
    fn run_command_capture_caps_a_flood_of_output() {
        let dir = make_temp();
        let c = run_command_capture(
            "for i in $(seq 1 5000); do echo 'noisy line of build output'; done",
            dir.path(),
            Duration::from_secs(20),
            2048,
        );
        assert!(c.output.len() < 4096, "output not capped: {} bytes", c.output.len());
        assert!(c.output.contains("bytes elided"), "elision marker missing");
    }

    #[test]
    fn output_is_capped_as_it_arrives_not_after_the_fact() {
        // ~2 MB through a 4 KB cap.  If this ever regresses to buffer-then-
        // truncate the assertion still passes, so the load-bearing part is the
        // size: the buffer never holds more than head+tail at any instant.
        let dir = make_temp();
        let c = run_command_capture(
            "for i in $(seq 1 20000); do echo '0123456789012345678901234567890123456789012345678901234567890123456789012345678901234567890123'; done",
            dir.path(),
            Duration::from_secs(60),
            4096,
        );
        assert!(c.exit_ok, "the command itself should succeed");
        assert!(c.output.len() <= 4096 + 128, "capped output was {} bytes", c.output.len());
        assert!(c.output.contains("bytes elided"), "elision marker missing");
    }

    #[test]
    fn bounded_buffer_keeps_head_and_tail_and_counts_the_middle() {
        let mut b = BoundedBuffer::new(200);
        // Feed in small chunks so the head/tail transition is exercised.
        for i in 0..500u32 {
            b.push(format!("{:03}\n", i % 1000).as_bytes());
        }
        let s = b.render();
        assert!(s.starts_with("000\n001\n"), "head not preserved: {:?}", &s[..16]);
        assert!(s.ends_with("499\n"), "tail not preserved");
        assert!(s.contains("bytes elided"), "elision marker missing");
        assert!(s.len() <= 200 + ELISION_RESERVE, "rendered {} bytes", s.len());
    }

    #[test]
    fn bounded_buffer_is_lossless_under_the_cap() {
        let mut b = BoundedBuffer::new(1024);
        b.push("héllo ".as_bytes());
        b.push("wörld".as_bytes());
        assert_eq!(b.render(), "héllo wörld", "nothing dropped means nothing changed");
    }

    #[test]
    fn run_command_capture_runs_in_the_given_directory() {
        let dir = make_temp();
        fs::write(dir.path().join("marker.txt"), "x").unwrap();
        let c = run_command_capture("ls", dir.path(), Duration::from_secs(5), MAX_OUTPUT_BYTES);
        assert!(c.exit_ok);
        assert!(c.output.contains("marker.txt"), "wrong cwd: {}", c.output);
    }
}
