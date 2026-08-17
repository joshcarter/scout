//! `wait` — block on detached wrap jobs until they finish (`docs/wait.md`).
//!
//! The tools themselves are thin: `wrap(..., detach: true)` starts a job
//! in the MCP server's [`JobRegistry`], `wait` parks until the condition
//! is met, `jobs` is the non-blocking snapshot, `cancel` kills one group.
//! Condensation of a finished job is wrap's, not a third schema.

use serde_json::Value;
use std::path::PathBuf;
use std::time::SystemTime;

use crate::jobs::{partition, JobRegistry, JobStatus, JobView, StartError};
use crate::select::{non_empty_arg, Ctx, ToolError, ToolResult};
use crate::stats::Outcome;
use crate::wrap;

/// Raw tool to name when a wait-family call cannot proceed.
const FALLBACK: &str = "the wrap / wait / jobs / cancel tools";

/// The `[wait]` tunables (`docs/wait.md` §3.6).
///
/// Parsed strictly by `config::load_wait_config`. Zero is legal for the
/// two job-level deadlines — a quiet notebook is normal, and the caller
/// sets the wait-side deadline. `max_block_seconds` is not allowed to be
/// zero: that would make every `wait` a `jobs()`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WaitConfig {
    pub max_jobs: u64,
    pub max_block_seconds: u64,
    pub idle_timeout_seconds: u64,
    pub wall_timeout_seconds: u64,
}

impl Default for WaitConfig {
    fn default() -> Self {
        WaitConfig {
            max_jobs: 16,
            max_block_seconds: 1500,
            idle_timeout_seconds: 0,
            wall_timeout_seconds: 0,
        }
    }
}

impl WaitConfig {
    pub fn job_config(self) -> crate::jobs::JobConfig {
        crate::jobs::JobConfig {
            max_jobs: self.max_jobs as usize,
            idle: (self.idle_timeout_seconds > 0)
                .then(|| std::time::Duration::from_secs(self.idle_timeout_seconds)),
            wall: (self.wall_timeout_seconds > 0)
                .then(|| std::time::Duration::from_secs(self.wall_timeout_seconds)),
        }
    }
}

/// When `wait` should return.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Until {
    /// First completion (or already-done job). Fail-fast default.
    Any,
    /// Every requested job has finished. The minimum-turn choice for a
    /// homogeneous sweep.
    All,
}

impl Until {
    /// Unknown values fall through to `Any` rather than becoming a tool
    /// error: a typo must not cost the caller the knowledge that a job
    /// finished.
    pub fn parse(raw: Option<&str>) -> Self {
        match raw.map(str::trim) {
            Some(s) if s.eq_ignore_ascii_case("all") => Until::All,
            _ => Until::Any,
        }
    }
}

/// Start a detached wrap. Called from MCP dispatch when `detach` is true.
pub fn detach(ctx: &Ctx, registry: &JobRegistry, args: &Value) -> ToolResult {
    let command = non_empty_arg(args, "command")
        .ok_or_else(|| fail("'command' argument is required and must be non-empty"))?;
    let cwd: PathBuf = args
        .get("cwd")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .map_or_else(|| PathBuf::from(&ctx.project), PathBuf::from);
    let question = args.get("question").and_then(Value::as_str).filter(|s| !s.trim().is_empty());

    let wall = args
        .get("timeout_seconds")
        .and_then(Value::as_u64)
        .filter(|&n| n > 0)
        .map(std::time::Duration::from_secs);
    match registry.start(&command, &cwd, question, wall) {
        Ok(view) => {
            ctx.ledger.record(
                ctx.record("wrap", args)
                    .outcome(Outcome::Bypassed)
                    .summary(format!("detached {}", view.id))
                    .ms(ctx.ledger.elapsed_ms()),
            );
            Ok(serde_json::json!({
                "job_id": view.id,
                "label": view.label,
                "raw_path": view.raw_path.as_ref().map(|p| p.display().to_string()),
                "started_at": unix_secs(view.started_at),
            }))
        }
        Err(StartError::NotADirectory) => {
            Err(fail(&format!("cwd {} is not a directory", cwd.display())))
        }
        Err(StartError::AtCapacity { max_jobs }) => Err(fail(&format!(
            "already running {max_jobs} jobs (the [wait] max_jobs cap); \
             wait for one to finish or cancel one before starting another"
        ))),
        Err(StartError::SpawnFailed(e)) => Err(fail(&e)),
    }
}

/// Kill one job's process group. The job stays in the registry until a
/// later `wait` reaps it, so the caller still learns it died.
pub fn cancel(registry: &JobRegistry, args: &Value) -> ToolResult {
    let job_id = non_empty_arg(args, "job_id")
        .ok_or_else(|| fail("'job_id' argument is required and must be non-empty"))?;
    if registry.cancel(&job_id) {
        Ok(serde_json::json!({ "ok": true, "job_id": job_id }))
    } else {
        Ok(serde_json::json!({
            "ok": false,
            "job_id": job_id,
            "reason": "unknown job_id",
        }))
    }
}

/// True when `until` is already satisfied by `views`.
pub fn condition_met(views: &[JobView], until: Until) -> bool {
    if views.is_empty() {
        // Nothing to wait for is a satisfied wait, not a hang.
        return true;
    }
    let (done, pending) = partition(views);
    match until {
        Until::Any => !done.is_empty(),
        Until::All => pending.is_empty(),
    }
}

/// Build the `{done, pending, timed_out}` payload, condensing finished
/// jobs through wrap. `reap` removes those finished jobs from the
/// registry (`wait` does; `jobs` does not).
pub fn collect(
    ctx: &Ctx,
    registry: &JobRegistry,
    job_ids: Option<&[String]>,
    question: Option<&str>,
    reap: bool,
    timed_out: bool,
) -> Value {
    let views = registry.snapshot(job_ids);
    let (done_refs, pending_refs) = partition(&views);

    let pending: Vec<Value> = pending_refs
        .iter()
        .map(|v| {
            serde_json::json!({
                "job_id": v.id,
                "label": v.label,
                "elapsed_s": v.elapsed.as_secs(),
            })
        })
        .collect();

    let mut done = Vec::with_capacity(done_refs.len());
    let mut reap_ids = Vec::with_capacity(done_refs.len());
    for v in done_refs {
        done.push(render_done(ctx, v, question));
        reap_ids.push(v.id.clone());
    }
    if reap && !reap_ids.is_empty() {
        let _ = registry.reap(Some(&reap_ids));
    }

    serde_json::json!({
        "done": done,
        "pending": pending,
        "timed_out": timed_out,
    })
}

/// `jobs()` — non-blocking, non-reaping snapshot. Same shape as `wait`.
pub fn jobs(ctx: &Ctx, registry: &JobRegistry, args: &Value) -> Value {
    let ids = parse_job_ids(args);
    let question = args.get("question").and_then(Value::as_str).filter(|s| !s.trim().is_empty());
    collect(ctx, registry, ids.as_deref(), question, false, false)
}

pub fn parse_job_ids(args: &Value) -> Option<Vec<String>> {
    let arr = args.get("job_ids")?.as_array()?;
    let ids: Vec<String> = arr
        .iter()
        .filter_map(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect();
    if ids.is_empty() {
        None
    } else {
        Some(ids)
    }
}

pub fn parse_timeout_s(args: &Value, cap: u64) -> u64 {
    args.get("timeout_s").and_then(Value::as_u64).unwrap_or(cap).min(cap)
}

fn render_done(ctx: &Ctx, view: &JobView, question_override: Option<&str>) -> Value {
    let output =
        view.raw_path.as_ref().and_then(|p| std::fs::read_to_string(p).ok()).unwrap_or_default();
    let exit_code = match view.status {
        JobStatus::Exited { exit_code } => exit_code,
        JobStatus::Running => None,
    };
    let question = question_override.or(view.question.as_deref());
    let mut args = serde_json::json!({ "command": view.command });
    if let Some(q) = question {
        args["question"] = Value::String(q.to_string());
    }
    let mut payload =
        wrap::condense(ctx, &args, &view.command, &output, exit_code, view.raw_path.as_deref());
    payload["job_id"] = Value::String(view.id.clone());
    payload["label"] = Value::String(view.label.clone());
    payload
}

fn unix_secs(t: SystemTime) -> u64 {
    t.duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

fn fail(reason: &str) -> ToolError {
    ToolError::new(format!("scout wait: {reason}"), FALLBACK)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jobs::{JobConfig, JobRegistry};
    use std::thread;
    use std::time::Duration;
    use tempfile::TempDir;

    fn offline_ctx(project: &str) -> Ctx<'static> {
        Ctx {
            client_error: Some("no config in tests".into()),
            project: project.to_string(),
            tool: "wait".to_string(),
            ledger: crate::stats::Ledger::silent(),
            ..Default::default()
        }
    }

    fn wait_done(reg: &JobRegistry, id: &str) {
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        loop {
            let snap = reg.snapshot(Some(&[id.to_string()]));
            if snap.iter().any(|v| !matches!(v.status, JobStatus::Running)) {
                return;
            }
            assert!(std::time::Instant::now() < deadline, "job {id} did not finish");
            thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn until_parses_all_and_defaults_to_any() {
        assert_eq!(Until::parse(Some("all")), Until::All);
        assert_eq!(Until::parse(Some("ALL")), Until::All);
        assert_eq!(Until::parse(Some("any")), Until::Any);
        assert_eq!(Until::parse(Some("nope")), Until::Any);
        assert_eq!(Until::parse(None), Until::Any);
    }

    #[test]
    fn empty_registry_is_already_satisfied() {
        assert!(condition_met(&[], Until::Any));
        assert!(condition_met(&[], Until::All));
    }

    #[test]
    fn detach_then_wait_returns_a_wrap_passthrough_and_reaps() {
        let dir = TempDir::new().unwrap();
        let project = dir.path().to_string_lossy().to_string();
        let ctx = offline_ctx(&project);
        let reg = JobRegistry::new(dir.path().to_path_buf(), JobConfig::default());

        let started = detach(
            &ctx,
            &reg,
            &serde_json::json!({"command": "echo one; echo two", "detach": true}),
        )
        .unwrap();
        let id = started["job_id"].as_str().unwrap().to_string();
        assert_eq!(started["label"], "echo one; echo two");
        wait_done(&reg, &id);

        let payload = collect(&ctx, &reg, None, None, true, false);
        assert_eq!(payload["timed_out"], false);
        assert_eq!(payload["pending"].as_array().unwrap().len(), 0);
        let done = &payload["done"][0];
        assert_eq!(done["job_id"], id);
        assert_eq!(done["exit_code"], 0);
        assert_eq!(done["filtered"], false);
        assert!(done["output"].as_str().unwrap().contains("one"), "{done}");
        assert!(reg.snapshot(None).is_empty(), "wait reaps finished jobs");
    }

    #[test]
    fn jobs_does_not_reap() {
        let dir = TempDir::new().unwrap();
        let project = dir.path().to_string_lossy().to_string();
        let ctx = offline_ctx(&project);
        let reg = JobRegistry::new(dir.path().to_path_buf(), JobConfig::default());
        let started = detach(&ctx, &reg, &serde_json::json!({"command": "echo hi"})).unwrap();
        let id = started["job_id"].as_str().unwrap().to_string();
        wait_done(&reg, &id);

        let snap = jobs(&ctx, &reg, &serde_json::json!({}));
        assert_eq!(snap["done"].as_array().unwrap().len(), 1);
        assert_eq!(reg.snapshot(None).len(), 1, "jobs() is a snapshot");
    }

    #[test]
    fn timeout_is_honest_and_free() {
        let dir = TempDir::new().unwrap();
        let project = dir.path().to_string_lossy().to_string();
        let ctx = offline_ctx(&project);
        let reg = JobRegistry::new(dir.path().to_path_buf(), JobConfig::default());
        let started = detach(&ctx, &reg, &serde_json::json!({"command": "sleep 30"})).unwrap();
        let id = started["job_id"].as_str().unwrap().to_string();

        let payload = collect(&ctx, &reg, None, None, true, true);
        assert_eq!(payload["timed_out"], true);
        assert_eq!(payload["done"].as_array().unwrap().len(), 0);
        assert_eq!(payload["pending"][0]["job_id"], id);
        // Still running — a timeout must not kill it.
        let still = reg.snapshot(Some(std::slice::from_ref(&id)));
        assert!(matches!(still[0].status, JobStatus::Running));
        assert!(reg.cancel(&id));
    }

    #[test]
    fn cancel_of_unknown_id_fails_open() {
        let dir = TempDir::new().unwrap();
        let reg = JobRegistry::new(dir.path().to_path_buf(), JobConfig::default());
        let out = cancel(&reg, &serde_json::json!({"job_id": "j9999"})).unwrap();
        assert_eq!(out["ok"], false);
        assert_eq!(out["reason"], "unknown job_id");
    }

    #[test]
    fn wait_until_any_returns_when_the_first_job_finishes() {
        let dir = TempDir::new().unwrap();
        let project = dir.path().to_string_lossy().to_string();
        let ctx = offline_ctx(&project);
        let reg = JobRegistry::new(dir.path().to_path_buf(), JobConfig::default());
        let fast = detach(&ctx, &reg, &serde_json::json!({"command": "echo fast"})).unwrap();
        let slow = detach(&ctx, &reg, &serde_json::json!({"command": "sleep 30"})).unwrap();
        let fast_id = fast["job_id"].as_str().unwrap().to_string();
        wait_done(&reg, &fast_id);

        let views = reg.snapshot(None);
        assert!(condition_met(&views, Until::Any));
        assert!(!condition_met(&views, Until::All));

        let payload = collect(&ctx, &reg, None, None, true, false);
        assert_eq!(payload["done"].as_array().unwrap().len(), 1);
        assert_eq!(payload["done"][0]["job_id"], fast_id);
        assert_eq!(payload["pending"].as_array().unwrap().len(), 1);
        assert_eq!(payload["pending"][0]["job_id"], slow["job_id"]);
        assert!(reg.cancel(slow["job_id"].as_str().unwrap()));
    }

    #[test]
    fn detach_at_capacity_names_the_cap() {
        let dir = TempDir::new().unwrap();
        let project = dir.path().to_string_lossy().to_string();
        let ctx = offline_ctx(&project);
        let reg = JobRegistry::new(
            dir.path().to_path_buf(),
            JobConfig { max_jobs: 1, ..JobConfig::default() },
        );
        let live = detach(&ctx, &reg, &serde_json::json!({"command": "sleep 30"})).unwrap();
        let err = detach(&ctx, &reg, &serde_json::json!({"command": "echo nope"})).unwrap_err();
        assert!(err.text().contains("max_jobs"), "{}", err.text());
        assert!(reg.cancel(live["job_id"].as_str().unwrap()));
    }
}
