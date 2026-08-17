//! Server-owned jobs: wrap deferred (`docs/wait.md`).
//!
//! The MCP server is the one long-lived scout process, so it owns children
//! that `wrap(..., detach: true)` starts. Each job is a process group (the
//! `setsid` discipline from `verify::capture_argv`), streams stdout+stderr
//! into a pinned spool blob, and is killed on cancel or when the server
//! exits. `watch` will share this module later; it is not a watch.

use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use crate::spool;
use crate::verify;

/// How often the supervisor wakes to poll `try_wait` and the two optional
/// deadlines. Same 50 ms as `verify`: short enough that a 200 ms test
/// timeout is still honoured.
const POLL_INTERVAL: Duration = Duration::from_millis(50);

/// How long to wait for reader threads after the child is gone.
const READER_DRAIN: Duration = Duration::from_secs(2);

/// Tunables the registry needs from `[wait]`. Defaults match `docs/wait.md`
/// §3.6. `idle` / `wall` of `None` mean "do not apply" — a quiet notebook
/// is normal, and the caller sets the deadline via `wait`, not here.
#[derive(Debug, Clone)]
pub struct JobConfig {
    pub max_jobs: usize,
    pub idle: Option<Duration>,
    pub wall: Option<Duration>,
}

impl Default for JobConfig {
    fn default() -> Self {
        JobConfig { max_jobs: 16, idle: None, wall: None }
    }
}

/// Why `start` refused. The caller turns these into fail-open tool errors.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StartError {
    NotADirectory,
    AtCapacity { max_jobs: usize },
    SpawnFailed(String),
}

/// What one job currently looks like, for `wait` / `jobs` to render.
#[derive(Debug, Clone)]
pub struct JobView {
    pub id: String,
    pub label: String,
    pub command: String,
    pub question: Option<String>,
    pub raw_path: Option<PathBuf>,
    pub started_at: SystemTime,
    pub elapsed: Duration,
    pub status: JobStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobStatus {
    Running,
    Exited { exit_code: Option<i32> },
}

/// The session-scoped table of detached wrap jobs.
///
/// `Scout` holds one of these behind an `Arc`. Tests construct their own
/// against a tempdir so nothing here needs an env var to be testable.
pub struct JobRegistry {
    inner: Mutex<Inner>,
    spool_base: PathBuf,
    cfg: JobConfig,
}

struct Inner {
    jobs: HashMap<String, Job>,
    next: u64,
}

struct Job {
    id: String,
    label: String,
    command: String,
    question: Option<String>,
    raw_path: Option<PathBuf>,
    started_at: SystemTime,
    started: Instant,
    pid: u32,
    status: Arc<Mutex<JobStatus>>,
}

impl JobRegistry {
    pub fn new(spool_base: PathBuf, cfg: JobConfig) -> Self {
        JobRegistry { inner: Mutex::new(Inner { jobs: HashMap::new(), next: 1 }), spool_base, cfg }
    }

    pub fn with_defaults() -> Self {
        Self::new(spool::cache_dir(), JobConfig::default())
    }

    /// Spawn `command` in `cwd` and return immediately. The child is in its
    /// own process group; output is appended to a pinned spool blob.
    pub fn start(
        &self,
        command: &str,
        cwd: &Path,
        question: Option<&str>,
        wall_override: Option<Duration>,
    ) -> Result<JobView, StartError> {
        if !cwd.is_dir() {
            return Err(StartError::NotADirectory);
        }

        let mut inner = lock(&self.inner);
        let live =
            inner.jobs.values().filter(|j| matches!(*lock(&j.status), JobStatus::Running)).count();
        if live >= self.cfg.max_jobs {
            return Err(StartError::AtCapacity { max_jobs: self.cfg.max_jobs });
        }

        let seq = inner.next;
        inner.next += 1;
        let id = format!("j{seq:04}");
        let label = label_for(command);
        let call_id = format!("{id}-{}", std::process::id());
        let raw_path = spool::create_in(&self.spool_base, "wrap", &call_id);
        if let Some(path) = &raw_path {
            spool::pin(path);
        }

        let mut cmd = Command::new("sh");
        cmd.args(["-c", command])
            .current_dir(cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        verify::apply_session(&mut cmd);

        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                if let Some(path) = &raw_path {
                    spool::unpin(path);
                }
                return Err(StartError::SpawnFailed(format!("failed to spawn: {e}")));
            }
        };

        let pid = child.id();
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let status = Arc::new(Mutex::new(JobStatus::Running));
        let started_at = SystemTime::now();
        let started = Instant::now();

        let job = Job {
            id: id.clone(),
            label: label.clone(),
            command: command.to_string(),
            question: question.map(str::to_string),
            raw_path: raw_path.clone(),
            started_at,
            started,
            pid,
            status: Arc::clone(&status),
        };
        inner.jobs.insert(id.clone(), job);
        drop(inner);

        let idle = self.cfg.idle;
        let wall = wall_override.or(self.cfg.wall);
        let supervise_path = raw_path.clone();
        thread::spawn(move || {
            supervise(Supervise {
                child,
                stdout,
                stderr,
                raw_path: supervise_path,
                idle,
                wall,
                started,
                status,
            });
        });

        Ok(JobView {
            id,
            label,
            command: command.to_string(),
            question: question.map(str::to_string),
            raw_path,
            started_at,
            elapsed: Duration::ZERO,
            status: JobStatus::Running,
        })
    }

    /// Snapshot the requested jobs (or all of them). Does not reap.
    pub fn snapshot(&self, ids: Option<&[String]>) -> Vec<JobView> {
        let inner = lock(&self.inner);
        let mut views: Vec<JobView> = inner
            .jobs
            .values()
            .filter(|j| ids.is_none_or(|want| want.iter().any(|id| id == &j.id)))
            .map(view_of)
            .collect();
        views.sort_by(|a, b| a.id.cmp(&b.id));
        views
    }

    /// Remove finished jobs in `ids` (or every finished job) from the
    /// registry and unpin their blobs. Running jobs are left alone.
    pub fn reap(&self, ids: Option<&[String]>) -> Vec<JobView> {
        let mut inner = lock(&self.inner);
        let finished: Vec<String> = inner
            .jobs
            .values()
            .filter(|j| !matches!(*lock(&j.status), JobStatus::Running))
            .filter(|j| ids.is_none_or(|want| want.iter().any(|id| id == &j.id)))
            .map(|j| j.id.clone())
            .collect();
        let mut out = Vec::with_capacity(finished.len());
        for id in finished {
            if let Some(job) = inner.jobs.remove(&id) {
                if let Some(path) = &job.raw_path {
                    spool::unpin(path);
                }
                out.push(view_of(&job));
            }
        }
        out.sort_by(|a, b| a.id.cmp(&b.id));
        out
    }

    /// Kill the job's process group. The supervisor records the exit; the
    /// job stays in the registry until `reap`. Returns `false` if the id
    /// is unknown.
    pub fn cancel(&self, id: &str) -> bool {
        let inner = lock(&self.inner);
        let Some(job) = inner.jobs.get(id) else {
            return false;
        };
        if matches!(*lock(&job.status), JobStatus::Running) {
            verify::kill_process_group(job.pid);
        }
        true
    }

    /// Kill every group and drop the table. Called when the MCP server's
    /// stdin closes — without this, `setsid` children survive as orphans.
    pub fn shutdown(&self) {
        let mut inner = lock(&self.inner);
        for job in inner.jobs.values() {
            if matches!(*lock(&job.status), JobStatus::Running) {
                verify::kill_process_group(job.pid);
            }
            if let Some(path) = &job.raw_path {
                spool::unpin(path);
            }
        }
        inner.jobs.clear();
    }

    pub fn max_jobs(&self) -> usize {
        self.cfg.max_jobs
    }
}

impl Drop for JobRegistry {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn view_of(job: &Job) -> JobView {
    JobView {
        id: job.id.clone(),
        label: job.label.clone(),
        command: job.command.clone(),
        question: job.question.clone(),
        raw_path: job.raw_path.clone(),
        started_at: job.started_at,
        elapsed: job.started.elapsed(),
        status: *lock(&job.status),
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Last path-like token of `command`, or a short collapsed form. A
/// ten-job payload is unreadable if every row echoes the full command
/// line (`docs/wait.md` §3.2).
pub fn label_for(command: &str) -> String {
    let tokens: Vec<&str> = command.split_whitespace().collect();
    for token in tokens.iter().rev() {
        let trimmed = token.trim_matches(|c| c == '"' || c == '\'');
        if trimmed.contains('/') {
            if let Some(name) = Path::new(trimmed).file_name() {
                let s = name.to_string_lossy();
                if !s.is_empty() && s != "/" {
                    return s.into_owned();
                }
            }
        }
    }
    let collapsed = tokens.join(" ");
    const CAP: usize = 40;
    if collapsed.chars().count() > CAP {
        format!("{}…", collapsed.chars().take(CAP - 1).collect::<String>())
    } else if collapsed.is_empty() {
        "job".into()
    } else {
        collapsed
    }
}

struct Supervise {
    child: std::process::Child,
    stdout: Option<std::process::ChildStdout>,
    stderr: Option<std::process::ChildStderr>,
    raw_path: Option<PathBuf>,
    idle: Option<Duration>,
    wall: Option<Duration>,
    started: Instant,
    status: Arc<Mutex<JobStatus>>,
}

fn supervise(s: Supervise) {
    let Supervise { mut child, stdout, stderr, raw_path, idle, wall, started, status } = s;
    let last_output_ms = Arc::new(AtomicU64::new(0));
    let mut readers = Vec::new();
    if let Some(pipe) = stdout {
        readers.push(spawn_appender(pipe, raw_path.clone(), Arc::clone(&last_output_ms), started));
    }
    if let Some(pipe) = stderr {
        readers.push(spawn_appender(pipe, raw_path, Arc::clone(&last_output_ms), started));
    }

    let mut exit_code = None;
    let mut timed_out = false;
    loop {
        match child.try_wait() {
            Ok(Some(st)) => {
                exit_code = st.code();
                break;
            }
            Ok(None) => {}
            Err(_) => break,
        }
        let elapsed = started.elapsed();
        if wall.is_some_and(|w| elapsed >= w) {
            timed_out = true;
            break;
        }
        if let Some(idle) = idle {
            let quiet_for = elapsed
                .saturating_sub(Duration::from_millis(last_output_ms.load(Ordering::Relaxed)));
            if quiet_for >= idle {
                timed_out = true;
                break;
            }
        }
        thread::sleep(POLL_INTERVAL);
    }

    if timed_out {
        verify::kill_process_group(child.id());
        let _ = child.wait();
        exit_code = None;
    }

    if !join_readers(&mut readers, READER_DRAIN) {
        verify::kill_process_group(child.id());
        let _ = join_readers(&mut readers, READER_DRAIN);
    }

    *lock(&status) = JobStatus::Exited { exit_code };
}

fn spawn_appender<R: Read + Send + 'static>(
    mut pipe: R,
    path: Option<PathBuf>,
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
                    if let Some(path) = &path {
                        let _ = spool::append(path, &chunk[..n]);
                    }
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(_) => break,
            }
        }
    })
}

fn join_readers(handles: &mut Vec<thread::JoinHandle<()>>, deadline: Duration) -> bool {
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

/// Poll until `pred` is true or `timeout` elapses. Tests and the async
/// `wait` tool share this so the readiness rule lives in one place.
pub fn partition(views: &[JobView]) -> (Vec<&JobView>, Vec<&JobView>) {
    let mut done = Vec::new();
    let mut pending = Vec::new();
    for v in views {
        match v.status {
            JobStatus::Running => pending.push(v),
            JobStatus::Exited { .. } => done.push(v),
        }
    }
    (done, pending)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn registry(dir: &TempDir, cfg: JobConfig) -> JobRegistry {
        JobRegistry::new(dir.path().to_path_buf(), cfg)
    }

    fn wait_until(reg: &JobRegistry, id: &str, timeout: Duration) -> JobView {
        let deadline = Instant::now() + timeout;
        loop {
            let snap = reg.snapshot(Some(&[id.to_string()]));
            if let Some(v) = snap.into_iter().next() {
                if !matches!(v.status, JobStatus::Running) {
                    return v;
                }
            }
            assert!(Instant::now() < deadline, "job {id} still running after {timeout:?}");
            thread::sleep(Duration::from_millis(20));
        }
    }

    #[cfg(unix)]
    fn alive(pid: i32) -> bool {
        unsafe { libc::kill(pid, 0) == 0 }
    }

    #[test]
    fn a_short_job_finishes_with_its_output_on_the_spool() {
        let dir = TempDir::new().unwrap();
        let reg = registry(&dir, JobConfig::default());
        let started = reg.start("echo hi; echo bye", dir.path(), None, None).unwrap();
        assert!(started.id.starts_with('j'), "{}", started.id);
        assert_eq!(started.status, JobStatus::Running);

        let done = wait_until(&reg, &started.id, Duration::from_secs(3));
        assert_eq!(done.status, JobStatus::Exited { exit_code: Some(0) });
        let body = fs::read_to_string(done.raw_path.as_ref().unwrap()).unwrap();
        assert!(body.contains("hi"), "{body:?}");
        assert!(body.contains("bye"), "{body:?}");
    }

    #[test]
    fn a_nonzero_exit_is_recorded_uninterpreted() {
        let dir = TempDir::new().unwrap();
        let reg = registry(&dir, JobConfig::default());
        let started = reg.start("exit 3", dir.path(), None, None).unwrap();
        let done = wait_until(&reg, &started.id, Duration::from_secs(3));
        assert_eq!(done.status, JobStatus::Exited { exit_code: Some(3) });
    }

    #[test]
    fn snapshot_does_not_reap_and_reap_removes_only_finished_jobs() {
        let dir = TempDir::new().unwrap();
        let reg = registry(&dir, JobConfig::default());
        let a = reg.start("echo a", dir.path(), None, None).unwrap();
        let b = reg.start("sleep 30", dir.path(), None, None).unwrap();
        wait_until(&reg, &a.id, Duration::from_secs(3));

        assert_eq!(reg.snapshot(None).len(), 2, "snapshot is non-destructive");
        let reaped = reg.reap(None);
        assert_eq!(reaped.len(), 1);
        assert_eq!(reaped[0].id, a.id);
        let left: Vec<_> = reg.snapshot(None).into_iter().map(|v| v.id).collect();
        assert_eq!(left, vec![b.id.clone()]);

        // The running job is still there after a targeted reap of the finished one.
        assert!(reg.cancel(&b.id));
        wait_until(&reg, &b.id, Duration::from_secs(3));
        assert_eq!(reg.reap(Some(std::slice::from_ref(&b.id))).len(), 1);
        assert!(reg.snapshot(None).is_empty());
    }

    #[test]
    fn cancel_of_an_unknown_id_is_false() {
        let dir = TempDir::new().unwrap();
        let reg = registry(&dir, JobConfig::default());
        assert!(!reg.cancel("j9999"));
    }

    #[test]
    fn max_jobs_rejects_the_next_live_start_but_not_after_one_finishes() {
        let dir = TempDir::new().unwrap();
        let reg = registry(&dir, JobConfig { max_jobs: 1, ..JobConfig::default() });
        let live = reg.start("sleep 30", dir.path(), None, None).unwrap();
        match reg.start("echo nope", dir.path(), None, None) {
            Err(StartError::AtCapacity { max_jobs: 1 }) => {}
            other => panic!("expected AtCapacity, got {other:?}"),
        }
        assert!(reg.cancel(&live.id));
        wait_until(&reg, &live.id, Duration::from_secs(3));
        // Finished jobs do not count against the cap.
        let next = reg.start("echo ok", dir.path(), None, None).unwrap();
        wait_until(&reg, &next.id, Duration::from_secs(3));
    }

    #[test]
    fn a_bad_cwd_does_not_spawn() {
        let dir = TempDir::new().unwrap();
        let reg = registry(&dir, JobConfig::default());
        let marker = dir.path().join("ran.txt");
        let err = reg
            .start(&format!("touch {}", marker.display()), Path::new("/nope/nowhere"), None, None)
            .unwrap_err();
        assert_eq!(err, StartError::NotADirectory);
        assert!(!marker.exists());
    }

    #[test]
    #[cfg(unix)]
    fn shutdown_kills_the_whole_process_group() {
        let dir = TempDir::new().unwrap();
        let pidfile = dir.path().join("grandchild.pid");
        let reg = registry(&dir, JobConfig::default());
        let cmd = format!("sleep 300 & echo $! > {}; wait", pidfile.display());
        let _ = reg.start(&cmd, dir.path(), None, None).unwrap();

        let deadline = Instant::now() + Duration::from_secs(3);
        while !pidfile.exists() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(20));
        }
        let pid: i32 = fs::read_to_string(&pidfile).unwrap().trim().parse().unwrap();
        assert!(alive(pid), "grandchild should be running before shutdown");

        reg.shutdown();
        let deadline = Instant::now() + Duration::from_secs(3);
        while alive(pid) && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(20));
        }
        assert!(!alive(pid), "grandchild {pid} survived registry shutdown");
    }

    #[test]
    fn label_prefers_the_last_path_component() {
        assert_eq!(
            label_for(".venv/bin/python -m jupyter nbconvert --inplace notebooks/foo.ipynb"),
            "foo.ipynb"
        );
        assert_eq!(label_for("echo hello"), "echo hello");
        assert_eq!(label_for(""), "job");
        let long = "a ".repeat(30);
        let got = label_for(&long);
        assert!(got.chars().count() <= 40, "{got}");
        assert!(got.ends_with('…'), "{got}");
    }
}
