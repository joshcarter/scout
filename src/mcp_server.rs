// MCP stdio server (rmcp 3.1).
//
// Tools: `ping` (wiring check), the four filters (`check_output`, `wrap`,
// `extract`, `grep`), and the wait family (`wait`, `jobs`, `cancel`).
// Short names on purpose: Claude Code prefixes them with the server
// namespace on its own, so what the model sees is
// `mcp__plugin_<plugin>_<server>__check_output`.  Nothing on this side should
// hardcode that qualified form (see CLAUDE.md) — it is derived from names this code
// never reads, and the model resolves it via `ToolSearch` anyway.
//
// `ServerHandler` is implemented by hand rather than via `#[tool_router]` /
// `#[tool_handler]`: the four real tools advertise the `description` and
// `input_schema` written in their preset TOMLs, which are loaded at runtime
// and cannot be baked into a macro attribute.  One steering surface, one
// source of truth — editing a preset changes what the model is told about it.
//
// Tool bodies are blocking (subprocess + HTTP), so each call runs on
// `spawn_blocking`; the stdio loop stays responsive.

use std::borrow::Cow;
use std::sync::Arc;
use std::time::Duration;

use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, Implementation,
    ListToolsResult, PaginatedRequestParams, ProtocolVersion, ServerCapabilities, ServerInfo, Tool,
};
use rmcp::service::RequestContext;
use rmcp::{schemars, ErrorData, RoleServer, ServerHandler, ServiceExt};
use serde_json::Value;

use crate::client::LlmClient;
// `MCP_PRESETS` — the presets advertised as tools, the other five being
// CLI-only — lives in `presets` rather than here: the preset overlay needs the
// same list to decide whose `input_schema` is load-bearing (see
// `presets::inherit_mcp_schema`), and two copies of it would drift.
use crate::jobs::JobRegistry;
use crate::presets::{Preset, MCP_PRESETS};
use crate::select::{Ctx, ToolError, ToolResult};
use crate::wait::{self, Until};

// The newest protocol era this server actually speaks.
//
// rmcp 3.1's `KNOWN_VERSIONS` lists `2026-07-28`, but it knows that era only
// well enough to apply SEP-2243's HTTP headers: it does not serialize the
// per-result cache fields (`ttlMs`, `cacheScope`) the era requires on
// `tools/list`.  Negotiation echoes back any version on the advertised list
// (`negotiate_protocol_version`), so leaving the default in place means
// agreeing to a contract this server cannot honour — Claude Code offers
// `2026-07-28`, rmcp agrees, and then every `tools/list` result fails the
// client's schema check.  It retries three times and drops the server with no
// tools registered, which surfaces as a *working* plugin advertising nothing:
// the binary is fine, the hooks still fire, and the redirect they issue points
// at tools that are not there.
//
// Narrowing the advertised list is the SDK's own lever for this — anything not
// on it falls back to `get_info`'s version, which is why the two are pinned to
// the same constant and a test holds them together.  Raise this only with
// evidence that the newer era's wire format is actually produced.
const MAX_PROTOCOL_VERSION: ProtocolVersion = ProtocolVersion::V_2025_11_25;

// Every era from the original release up to `MAX_PROTOCOL_VERSION`.  Spelled
// out rather than sliced from `KNOWN_VERSIONS`, so adding an era upstream
// cannot silently widen what this server claims to serve.
const SUPPORTED_PROTOCOL_VERSIONS: &[ProtocolVersion] = &[
    ProtocolVersion::V_2024_11_05,
    ProtocolVersion::V_2025_03_26,
    ProtocolVersion::V_2025_06_18,
    MAX_PROTOCOL_VERSION,
];

const INSTRUCTIONS: &str = "scout offloads small problems to a local LLM so they never consume \
cloud-model context. Prefer its tools for classifying build/test output and targeted file/search \
questions. A job that will run for minutes and then finish is wrap(..., detach: true) \
followed by wait(until: \"all\") — do not sleep or poll.";

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct PingParams {
    /// Optional message to echo back.
    #[serde(default)]
    message: Option<String>,
}

#[derive(Clone)]
struct Scout {
    presets: Arc<Vec<Preset>>,
    jobs: Arc<JobRegistry>,
}

impl Scout {
    fn new() -> Self {
        let wait_cfg =
            crate::config::load_wait_config(&crate::config::config_path()).unwrap_or_default();
        Scout {
            presets: Arc::new(crate::presets::load_presets()),
            jobs: Arc::new(JobRegistry::new(crate::spool::cache_dir(), wait_cfg.job_config())),
        }
    }

    fn ping(message: Option<&str>) -> String {
        let version = env!("CARGO_PKG_VERSION");
        match message {
            Some(m) => format!("scout {version} — pong: {m}"),
            None => format!("scout {version} — pong"),
        }
    }

    /// The tool table: `ping` plus one tool per preset scout exposes over MCP,
    /// described by that preset's own `description` / `input_schema`.
    fn tools(&self) -> Vec<Tool> {
        let mut tools = vec![Tool::new(
            Cow::Borrowed("ping"),
            Cow::Borrowed(
                "Health check for the scout server: returns the server version and echoes an \
                 optional message. Use to verify the local-LLM plugin is wired up.",
            ),
            Arc::new(schema_object(&serde_json::json!({
                "type": "object",
                "properties": {
                    "message": {"type": "string", "description": "Optional message to echo back."}
                },
                "required": []
            }))),
        )];

        for name in MCP_PRESETS {
            if let Some(p) = self.presets.iter().find(|p| p.name == *name) {
                tools.push(Tool::new(
                    Cow::Owned(p.name.clone()),
                    Cow::Owned(p.description.clone()),
                    Arc::new(schema_object(p.input_schema())),
                ));
            }
        }

        // wait / jobs / cancel are not preset-backed (docs/wait.md §3.2):
        // they have no prompt to carry. Registered the way `ping` is.
        tools.push(Tool::new(
            Cow::Borrowed("wait"),
            Cow::Borrowed(
                "Block until detached wrap jobs finish, then return each one's wrap payload \
                 (exit_code, summary, notable, raw_path). Omit job_ids to drain every job. \
                 until is \"any\" (default, return when one finishes) or \"all\" (return when \
                 the batch is done — use this for a homogeneous sweep). A timeout returns \
                 {timed_out: true} with no summary; call wait again rather than polling. \
                 Do not sleep or write an until/pgrep loop.",
            ),
            Arc::new(schema_object(&serde_json::json!({
                "type": "object",
                "properties": {
                    "job_ids": {
                        "type": "array",
                        "items": {"type": "string"},
                        "description": "Jobs to wait on. Omit to drain every job in this session."
                    },
                    "until": {
                        "type": "string",
                        "description": "\"any\" (default) returns when one job finishes; \"all\" waits for the whole batch. Use \"all\" for a homogeneous sweep."
                    },
                    "timeout_s": {
                        "type": "integer",
                        "description": "Seconds to block, capped by [wait] max_block_seconds (default 1500). A timeout is not an error."
                    },
                    "question": {
                        "type": "string",
                        "description": "Optional question forwarded to wrap's condenser for each finished job."
                    }
                },
                "required": []
            }))),
        ));
        tools.push(Tool::new(
            Cow::Borrowed("jobs"),
            Cow::Borrowed(
                "Non-blocking snapshot of detached wrap jobs. Same {done, pending} shape as \
                 wait, but does not reap finished jobs and does not block.",
            ),
            Arc::new(schema_object(&serde_json::json!({
                "type": "object",
                "properties": {
                    "job_ids": {
                        "type": "array",
                        "items": {"type": "string"},
                        "description": "Jobs to list. Omit for every job in this session."
                    }
                },
                "required": []
            }))),
        ));
        tools.push(Tool::new(
            Cow::Borrowed("cancel"),
            Cow::Borrowed(
                "Kill one detached wrap job's process group. The job stays listable until \
                 a later wait reaps it. Does not kill other jobs.",
            ),
            Arc::new(schema_object(&serde_json::json!({
                "type": "object",
                "properties": {
                    "job_id": {
                        "type": "string",
                        "description": "The job_id returned by wrap(..., detach: true)."
                    }
                },
                "required": ["job_id"]
            }))),
        ));
        tools
    }

    /// Run one filter, loading config lazily so a missing `config.toml` is a
    /// per-call tool error naming a fallback, never a dead server.
    fn dispatch(&self, tool: &str, args: &Value) -> ToolResult {
        let cfg = crate::config::load_config(&crate::config::config_path());
        let (client, client_error) = match cfg {
            Ok(c) => (Some(LlmClient::new(c)), None),
            Err(e) => (None, Some(e)),
        };
        let ctx = Ctx {
            client: client.as_ref(),
            client_error,
            presets: &self.presets,
            // The directory Claude Code launched the server in.
            project: crate::resolve_project(None),
            // This is the one entry point Claude reaches on its own, which is
            // exactly what `via` is for (docs/dashboard.md §3).
            via: crate::stats::VIA_MCP,
            tool: tool.to_string(),
            // Silence is mandatory here: stdout is the JSON-RPC transport.
            progress: None,
            ..Default::default()
        };
        let result = match tool {
            "check_output" => crate::check_output::run(&ctx, args),
            "wrap" if args.get("detach").and_then(Value::as_bool) == Some(true) => {
                wait::detach(&ctx, &self.jobs, args)
            }
            "wrap" => crate::wrap::run(&ctx, args),
            "extract" => crate::extract::run(&ctx, args),
            "grep" => crate::grep::run(&ctx, args),
            "jobs" => Ok(wait::jobs(&ctx, &self.jobs, args)),
            "cancel" => wait::cancel(&self.jobs, args),
            other => Err(crate::select::ToolError::new(
                format!("unknown tool {other:?}"),
                "the built-in tools",
            )),
        };
        // Close the call log's last row now that the payload — and its size —
        // exists.  Dropping `ctx` would still write the row; this is what puts
        // `returned_bytes` on it.
        match &result {
            Ok(payload) => ctx.ledger.finish(payload),
            Err(e) => ctx.ledger.fail(&e.text()),
        }
        result
    }

    /// Park until the wait condition is met or the cap elapses.
    ///
    /// Async on purpose: a dropped/`Cancelled` MCP call must stop blocking
    /// without killing the jobs, and `spawn_blocking` cannot be interrupted.
    /// Condensation (the local-model call) still goes through the blocking
    /// pool, and only for jobs that actually finished.
    async fn call_wait(
        &self,
        args: Value,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let cap = crate::config::load_wait_config(&crate::config::config_path())
            .unwrap_or_default()
            .max_block_seconds;
        let timeout_s = wait::parse_timeout_s(&args, cap);
        let until = Until::parse(args.get("until").and_then(Value::as_str));
        let ids = wait::parse_job_ids(&args);
        let started = std::time::Instant::now();
        let deadline = started + Duration::from_secs(timeout_s);
        // Claude Code's stdio idle window is 30 minutes and aborts a silent
        // tool at that mark. The shipped 1500 s cap sits under it so a
        // quiet wait still returns {timed_out: true} instead of a harness
        // error. Do not emit MCP progress to stretch the idle window:
        // whether a given harness treats those notifications as a model
        // wake is not something we have measured, and a wake every 15 s
        // would be the cost this tool exists to remove.

        let mut timed_out = false;
        loop {
            let views = self.jobs.snapshot(ids.as_deref());
            if wait::condition_met(&views, until) {
                break;
            }
            let now = std::time::Instant::now();
            if now >= deadline {
                timed_out = true;
                break;
            }
            let remaining = deadline.saturating_duration_since(now);
            tokio::select! {
                () = context.ct.cancelled() => {
                    // User interrupted: stop blocking, leave jobs running.
                    timed_out = true;
                    break;
                }
                () = tokio::time::sleep(remaining.min(Duration::from_millis(100))) => {}
            }
        }

        let this = self.clone();
        let payload = tokio::task::spawn_blocking(move || {
            let cfg = crate::config::load_config(&crate::config::config_path());
            let (client, client_error) = match cfg {
                Ok(c) => (Some(crate::client::LlmClient::new(c)), None),
                Err(e) => (None, Some(e)),
            };
            let ctx = Ctx {
                client: client.as_ref(),
                client_error,
                presets: &this.presets,
                project: crate::resolve_project(None),
                via: crate::stats::VIA_MCP,
                tool: "wait".to_string(),
                progress: None,
                ..Default::default()
            };
            let question =
                args.get("question").and_then(Value::as_str).filter(|s| !s.trim().is_empty());
            let payload =
                wait::collect(&ctx, &this.jobs, ids.as_deref(), question, true, timed_out);
            ctx.ledger.finish(&payload);
            payload
        })
        .await
        .map_err(|e| ErrorData::internal_error(format!("scout: wait task failed: {e}"), None))?;

        Ok(CallToolResult::success(vec![ContentBlock::text(compact(&payload))]).into())
    }
}

/// Coerce a JSON Schema `Value` into the object map rmcp wants, falling back
/// to a permissive object schema if a preset carries something odd.
fn schema_object(v: &Value) -> serde_json::Map<String, Value> {
    match v {
        Value::Object(map) => map.clone(),
        _ => serde_json::json!({"type": "object", "properties": {}, "required": []})
            .as_object()
            .cloned()
            .unwrap_or_default(),
    }
}

impl ServerHandler for Scout {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_protocol_version(MAX_PROTOCOL_VERSION)
            .with_server_info(Implementation::new("scout", env!("CARGO_PKG_VERSION")))
            .with_instructions(INSTRUCTIONS.to_string())
    }

    // The fallback above is only consulted for versions off this list, so this
    // is the half that actually stops the over-claim.  See
    // `MAX_PROTOCOL_VERSION`.
    fn supported_protocol_versions(&self) -> Cow<'static, [ProtocolVersion]> {
        Cow::Borrowed(SUPPORTED_PROTOCOL_VERSIONS)
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        Ok(ListToolsResult::with_all_items(self.tools()))
    }

    fn get_tool(&self, name: &str) -> Option<Tool> {
        self.tools().into_iter().find(|t| t.name == name)
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let name = request.name.to_string();
        let args = Value::Object(request.arguments.unwrap_or_default());

        if name == "ping" {
            let Parameters(PingParams { message }) = Parameters(
                serde_json::from_value::<PingParams>(args)
                    .map_err(|e| ErrorData::invalid_params(e.to_string(), None))?,
            );
            let pong = Self::ping(message.as_deref());
            return Ok(CallToolResult::success(vec![ContentBlock::text(pong)]).into());
        }

        if name == "wait" {
            return self.call_wait(args, context).await;
        }

        let this = self.clone();
        let tool = name.clone();
        // The filters block on subprocesses and HTTP; keep them off the reactor.
        let result =
            bounded_dispatch(move || this.dispatch(&name, &args), DISPATCH_TIMEOUT, &tool).await?;

        Ok(match result {
            Ok(payload) => CallToolResult::success(vec![ContentBlock::text(compact(&payload))]),
            // A filter failure is the caller's problem to route around, not a
            // protocol error: it comes back as tool-level `isError` content
            // naming the raw tool to fall back to.
            Err(e) => CallToolResult::error(vec![ContentBlock::text(e.text())]),
        }
        .into())
    }
}

/// Last-resort ceiling on one MCP tool call.
///
/// Every deadline that actually belongs to the work lives inside `dispatch` —
/// `check_output`'s wall clock, the HTTP client's timeout, the git providers'.
/// This one exists for the path that misses all of them: without it a single
/// wedged call leaves the calling agent blocked on a tool response that will
/// never arrive, and nothing anywhere says so.
///
/// It has to sit *above* `check_output`'s own ceiling (`MAX_TIMEOUT_SECS`,
/// 3600 s) plus the classify round-trip that follows it, or an hour-long build
/// would be pre-empted here instead of reporting its own timeout — which is the
/// one outcome worse than no backstop, because the build's diagnosis is the
/// useful answer and this layer's is not.  Ten minutes of margin is far more
/// than that tail needs, and costs nothing: in normal operation this never
/// fires.
const DISPATCH_TIMEOUT: Duration = Duration::from_secs(3600 + 600);

/// Run a blocking tool body on the blocking pool under `limit`.
///
/// On timeout the blocking task is *not* cancelled: `spawn_blocking` cannot
/// interrupt a thread mid-syscall, and dropping the handle only detaches it.
/// So a wedged call keeps one blocking-pool thread for as long as it stays
/// wedged, and still writes its ledger row if it ever finishes — what the bound
/// buys is that the *caller* gets an answer instead of waiting forever.  The
/// pool is finite (512 threads by default), so repeated wedges would eventually
/// queue new calls rather than run them; that is the reason this is a backstop
/// for a bug rather than a substitute for the deadlines inside `dispatch`.
async fn bounded_dispatch<F>(job: F, limit: Duration, tool: &str) -> Result<ToolResult, ErrorData>
where
    F: FnOnce() -> ToolResult + Send + 'static,
{
    let handle = tokio::task::spawn_blocking(job);
    match tokio::time::timeout(limit, handle).await {
        Ok(joined) => joined
            .map_err(|e| ErrorData::internal_error(format!("scout: tool task failed: {e}"), None)),
        // Fail open, as everywhere else here: a tool-level error naming what to
        // do instead, not a protocol error the agent cannot route around.
        Err(_) => Ok(Err(ToolError::new(
            format!(
                "scout: {tool} passed the {}s server deadline without returning and was abandoned",
                limit.as_secs()
            ),
            "the underlying tool directly",
        ))),
    }
}

/// Serialize a payload as compact JSON text (the MCP content body).
fn compact(payload: &Value) -> String {
    serde_json::to_string(payload).unwrap_or_else(|_| payload.to_string())
}

pub fn serve() -> anyhow::Result<()> {
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async {
        let scout = Scout::new();
        let jobs = scout.jobs.clone();
        let service = scout.serve(rmcp::transport::stdio()).await?;
        let result = service.waiting().await;
        // stdin EOF is the only shutdown signal. Without this, setsid
        // children survive the session as orphans (docs/wait.md §8).
        jobs.shutdown();
        result?;
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn server() -> Scout {
        Scout::new()
    }

    #[test]
    fn advertises_ping_the_four_filters_and_the_wait_family() {
        let names: Vec<String> = server().tools().iter().map(|t| t.name.to_string()).collect();
        assert_eq!(
            names,
            vec!["ping", "check_output", "wrap", "extract", "grep", "wait", "jobs", "cancel"]
        );
    }

    #[test]
    fn tool_descriptions_come_from_the_presets() {
        let tools = server().tools();
        let grep = tools.iter().find(|t| t.name == "grep").unwrap();
        let desc = grep.description.as_deref().unwrap();
        assert!(desc.contains("intent"), "preset description not used: {desc}");
        assert!(desc.len() > 100, "description looks truncated: {desc}");
    }

    #[test]
    fn tool_schemas_come_from_the_presets() {
        let tools = server().tools();
        for (name, required) in [
            ("check_output", vec!["command"]),
            ("wrap", vec!["command"]),
            ("extract", vec!["file", "question"]),
            ("grep", vec!["pattern", "intent"]),
            ("cancel", vec!["job_id"]),
        ] {
            let t = tools.iter().find(|t| t.name == name).unwrap();
            let schema = Value::Object((*t.input_schema).clone());
            assert_eq!(schema["type"], "object", "{name} schema: {schema}");
            let req: Vec<&str> = schema["required"]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v.as_str().unwrap())
                .collect();
            assert_eq!(req, required, "{name} required args");
            assert!(schema["properties"].is_object(), "{name} has no properties: {schema}");
        }
    }

    #[test]
    fn ping_reports_the_version() {
        let out = Scout::ping(Some("hi"));
        assert!(out.contains(env!("CARGO_PKG_VERSION")));
        assert!(out.contains("hi"));
    }

    #[test]
    fn unknown_tool_is_a_fail_open_error_not_a_panic() {
        let err = server().dispatch("nope", &serde_json::json!({})).unwrap_err();
        assert!(err.text().contains("unknown tool"), "{}", err.text());
    }

    // ── the dispatch backstop ─────────────────────────────────────────────

    #[test]
    fn a_wedged_tool_call_is_bounded_instead_of_hanging_forever() {
        // Exercised with a millisecond bound rather than DISPATCH_TIMEOUT: the
        // real ceiling is deliberately an hour-plus, and what is under test is
        // the shape of the answer, not the number.
        let rt = tokio::runtime::Runtime::new().unwrap();
        let out = rt.block_on(bounded_dispatch(
            || {
                std::thread::sleep(Duration::from_millis(300));
                Ok(serde_json::json!({"never": "seen"}))
            },
            Duration::from_millis(50),
            "check_output",
        ));
        let err = out.expect("a deadline is a tool-level error, not a protocol one").unwrap_err();
        assert!(err.text().contains("deadline"), "{}", err.text());
        assert!(err.text().contains("fall back to"), "no named fallback: {}", err.text());
    }

    #[test]
    fn a_prompt_tool_call_passes_through_the_bound_untouched() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let out = rt.block_on(bounded_dispatch(
            || Ok(serde_json::json!({"ok": true})),
            Duration::from_secs(5),
            "ping",
        ));
        assert_eq!(out.unwrap().unwrap()["ok"], serde_json::json!(true));
    }

    #[test]
    fn the_backstop_sits_above_check_outputs_own_ceiling() {
        // The one way this backstop could do harm: pre-empting a legitimate
        // hour-long build before it reports its own, far more useful, timeout.
        // 3600 is `check_output::MAX_TIMEOUT_SECS`, private to that module.
        assert!(
            DISPATCH_TIMEOUT > Duration::from_secs(3600),
            "the MCP bound would pre-empt check_output's own ceiling: {DISPATCH_TIMEOUT:?}"
        );
    }

    #[test]
    fn get_info_names_the_server_and_enables_tools() {
        let info = server().get_info();
        assert_eq!(info.server_info.name, "scout");
        assert!(info.capabilities.tools.is_some());
        assert!(info.instructions.unwrap().contains("local LLM"));
    }

    #[test]
    fn the_advertised_list_omits_eras_this_server_cannot_serialize() {
        let supported = ServerHandler::supported_protocol_versions(&server());
        assert!(
            !supported.contains(&ProtocolVersion::V_2026_07_28),
            "2026-07-28 wants ttlMs/cacheScope on every tools/list result and rmcp 3.1 emits \
             neither; advertising it makes the client reject the tool list outright"
        );
        assert!(
            supported.contains(&MAX_PROTOCOL_VERSION),
            "the fallback version must itself be advertised"
        );
    }

    #[test]
    fn the_negotiation_fallback_matches_the_advertised_maximum() {
        // Two independent surfaces decide what a client ends up with: the list
        // (for versions it offers) and `get_info` (for everything else).  Let
        // them drift and the off-list path lands on an era that is not on the
        // list — the exact shape of the original bug.
        let info = server().get_info();
        assert_eq!(info.protocol_version, MAX_PROTOCOL_VERSION);
        assert!(
            ServerHandler::supported_protocol_versions(&server()).contains(&info.protocol_version)
        );
    }

    #[test]
    fn the_advertised_list_is_a_prefix_of_what_the_sdk_knows() {
        // Guards the hand-written list against a typo'd or invented era.
        let supported = ServerHandler::supported_protocol_versions(&server());
        for v in supported.iter() {
            assert!(
                ProtocolVersion::KNOWN_VERSIONS.contains(v),
                "advertised a version the SDK does not know: {v}"
            );
        }
    }
}
