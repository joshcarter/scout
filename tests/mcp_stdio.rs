// End-to-end tests for the MCP stdio server — contract #1.
//
// `scout mcp` is how Claude Code actually talks to scout: JSON-RPC over the
// process's stdin/stdout. Everything about it crosses a process boundary, so
// none of it is reachable from a unit test. The unit tests in `mcp_server.rs`
// call `Scout::tools()` directly; they say nothing about whether the binary
// starts, whether the transport frames messages the way a client expects, or
// whether `initialize` ever completes. Those are the parts that break silently,
// because a broken handshake looks to the user like "the tools just aren't
// there".
//
// The schema assertions defend a specific, live seam: a tool's `inputSchema` is
// *data*, read at runtime from a preset TOML that a user can override in
// `$XDG_CONFIG_HOME/scout/presets/`, while the argument contract is *code* — the
// hardcoded `match` in `Scout::dispatch`. Nothing enforces that the two agree.
// An override that omits `input_schema`, or a preset edit that drops a
// property, makes scout advertise a tool the model cannot successfully call, and
// the failure surfaces as the model passing wrong arguments rather than as
// anything scout reports. These tests pin the built-in schemas: non-empty
// properties, and the required args each handler actually reads.
//
// Most tests stay on `initialize` and `tools/list` — pure protocol, no local
// LLM. The wait-family test is the exception: `wrap(detach)` + `wait` must
// round-trip over the real transport, because that is the path that used to
// not exist.

mod support;

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use support::Sandbox;

/// Ceiling on any single read from the server.
///
/// The reason every read is bounded: a wedged child with nobody reading its
/// stdout would block the test thread forever, and cargo's test harness has no
/// per-test timeout of its own. A hung suite is worse than a failing one — it
/// gives no diagnosis and, in CI, burns the job's whole wall clock. Generous
/// enough that a cold-start on a loaded machine is not a flake; short enough
/// that a real hang is reported in seconds.
const READ_TIMEOUT: Duration = Duration::from_secs(20);

/// A live `scout mcp` process, framed as JSON-RPC line-per-message.
///
/// Reading happens on a background thread feeding a channel, so the test thread
/// only ever waits with a deadline (`recv_timeout`) and never on a raw pipe.
struct McpServer {
    child: Child,
    stdin: Option<ChildStdin>,
    lines: Receiver<String>,
    stderr: Arc<Mutex<String>>,
}

impl McpServer {
    fn spawn(sandbox: &Sandbox) -> McpServer {
        let mut cmd: Command = sandbox.scout();
        let mut child = cmd
            .arg("mcp")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn scout mcp");

        let stdout = child.stdout.take().expect("piped stdout");
        let (tx, lines) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                match line {
                    Ok(l) => {
                        if tx.send(l).is_err() {
                            return;
                        }
                    }
                    Err(_) => return,
                }
            }
            // Dropping `tx` on EOF turns a dead server into a prompt
            // `Disconnected` rather than a full timeout.
        });

        // stderr is drained too. An undrained pipe is a deadlock waiting to
        // happen if the server ever gets chatty, and having the text on hand
        // makes a startup failure diagnosable instead of just "timed out".
        let stderr_buf = Arc::new(Mutex::new(String::new()));
        let sink = Arc::clone(&stderr_buf);
        let stderr = child.stderr.take().expect("piped stderr");
        std::thread::spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                sink.lock().unwrap().push_str(&line);
                sink.lock().unwrap().push('\n');
            }
        });

        let stdin = child.stdin.take().expect("piped stdin");
        McpServer { child, stdin: Some(stdin), lines, stderr: stderr_buf }
    }

    fn send(&mut self, msg: &Value) {
        let stdin = self.stdin.as_mut().expect("stdin still open");
        writeln!(stdin, "{msg}").expect("write to scout mcp");
        stdin.flush().expect("flush");
    }

    /// Send a request and wait for the response carrying the same id.
    ///
    /// Skips anything else on the wire (notifications, log frames) rather than
    /// assuming the next line is ours — a client that assumes strict ordering
    /// is exactly the kind of thing that works until the server adds a
    /// notification.
    fn request(&mut self, id: u64, method: &str, params: &Value) -> Value {
        self.send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        let deadline = Instant::now() + READ_TIMEOUT;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            let line = match self.lines.recv_timeout(remaining) {
                Ok(l) => l,
                Err(RecvTimeoutError::Timeout) => {
                    panic!(
                        "no response to {method:?} within {READ_TIMEOUT:?}{}",
                        self.diagnostics()
                    )
                }
                Err(RecvTimeoutError::Disconnected) => {
                    panic!(
                        "scout mcp closed stdout before answering {method:?}{}",
                        self.diagnostics()
                    )
                }
            };
            if line.trim().is_empty() {
                continue;
            }
            let msg: Value = serde_json::from_str(&line)
                .unwrap_or_else(|e| panic!("non-JSON frame from scout mcp: {e}: {line:?}"));
            if msg.get("id").and_then(Value::as_u64) == Some(id) {
                assert_eq!(msg["jsonrpc"], "2.0", "wrong JSON-RPC envelope: {msg}");
                assert!(msg.get("error").is_none(), "{method} failed: {msg}");
                return msg["result"].clone();
            }
        }
    }

    fn diagnostics(&self) -> String {
        let err = self.stderr.lock().unwrap();
        if err.is_empty() {
            String::new()
        } else {
            format!("\n--- server stderr ---\n{err}")
        }
    }
}

impl Drop for McpServer {
    /// Always reap the child.
    ///
    /// Closing stdin is the graceful shutdown — the stdio transport ends when
    /// its input does — but the test may be unwinding from a panic precisely
    /// because the server is wedged, so the kill is unconditional rather than a
    /// fallback. Nothing may outlive the test.
    fn drop(&mut self) {
        drop(self.stdin.take());
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Complete the handshake and return the `initialize` result.
fn handshake(server: &mut McpServer) -> Value {
    let result = server.request(
        1,
        "initialize",
        &json!({
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": {"name": "scout-integration-test", "version": "0"}
        }),
    );
    server.send(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
    result
}

#[test]
fn initialize_completes_and_reports_a_protocol_version() {
    let sandbox = Sandbox::new();
    let mut server = McpServer::spawn(&sandbox);
    let result = handshake(&mut server);

    let version = result["protocolVersion"]
        .as_str()
        .unwrap_or_else(|| panic!("no protocolVersion in initialize result: {result}"));
    assert!(!version.is_empty(), "empty protocolVersion");

    // The server must advertise tools, or a client has no reason to ask for
    // them and scout is invisible however correct its tool table is.
    assert!(
        result["capabilities"].get("tools").is_some(),
        "tools capability not advertised: {result}"
    );
    assert_eq!(result["serverInfo"]["name"], "scout", "server identity: {result}");
    assert!(
        result["serverInfo"]["version"].as_str().is_some_and(|v| !v.is_empty()),
        "no server version: {result}"
    );
    // The instructions are the server's one chance to tell the model what it is
    // for; an empty string here is a silent regression in steering.
    assert!(
        result["instructions"].as_str().is_some_and(|s| s.contains("local LLM")),
        "instructions missing or unrecognizable: {result}"
    );
}

#[test]
fn tools_list_advertises_the_expected_tool_set() {
    let sandbox = Sandbox::new();
    let mut server = McpServer::spawn(&sandbox);
    handshake(&mut server);

    let result = server.request(2, "tools/list", &json!({}));
    let names: Vec<&str> = result["tools"]
        .as_array()
        .expect("tools array")
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();

    // Order is part of the contract only incidentally; membership is not. These
    // eight are what `MCP_PRESETS` plus the built-in `ping` and the wait
    // family produce, and a tool silently dropping off this list is the
    // failure this test exists to catch.
    assert_eq!(
        names,
        vec!["ping", "check_output", "wrap", "extract", "grep", "wait", "jobs", "cancel"],
        "advertised tools changed"
    );
}

#[test]
fn every_advertised_tool_carries_a_usable_input_schema() {
    let sandbox = Sandbox::new();
    let mut server = McpServer::spawn(&sandbox);
    handshake(&mut server);

    let result = server.request(2, "tools/list", &json!({}));
    let tools = result["tools"].as_array().expect("tools array");

    for tool in tools {
        let name = tool["name"].as_str().unwrap();
        let schema = &tool["inputSchema"];
        assert_eq!(schema["type"], "object", "{name}: schema is not an object schema: {schema}");
        let props = schema["properties"]
            .as_object()
            .unwrap_or_else(|| panic!("{name}: no properties: {schema}"));
        // `ping` is the only tool whose arguments are all optional; every other
        // tool with an empty property bag is unusable, and that is precisely
        // what a preset missing its `input_schema` produces — scout falls back
        // to `{"type":"object","properties":{}}` and advertises a tool the model
        // cannot call correctly.
        assert!(!props.is_empty(), "{name}: advertised with no properties at all: {schema}");
        assert!(
            tool["description"].as_str().is_some_and(|d| d.len() > 40),
            "{name}: description missing or too short to steer with"
        );
    }
}

#[test]
fn each_tools_required_args_match_what_its_handler_reads() {
    let sandbox = Sandbox::new();
    let mut server = McpServer::spawn(&sandbox);
    handshake(&mut server);

    let result = server.request(2, "tools/list", &json!({}));
    let tools = result["tools"].as_array().expect("tools array");
    let find = |name: &str| {
        tools
            .iter()
            .find(|t| t["name"] == name)
            .unwrap_or_else(|| panic!("{name} not advertised"))
            .clone()
    };

    // The half of the seam that only an end-to-end test can see: these are the
    // arguments `Scout::dispatch` routes to handlers that will error without
    // them. The schema is data (preset TOML); the requirement is code. If a
    // preset edit drops one of these from `required`, the model is free to omit
    // it and the tool fails at call time with no schema violation to point at.
    for (tool, required) in [
        ("check_output", vec!["command"]),
        ("wrap", vec!["command"]),
        ("extract", vec!["file", "question"]),
        ("grep", vec!["pattern", "intent"]),
        ("cancel", vec!["job_id"]),
    ] {
        let schema = find(tool)["inputSchema"].clone();
        let got: Vec<&str> = schema["required"]
            .as_array()
            .unwrap_or_else(|| panic!("{tool}: no required list: {schema}"))
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert_eq!(got, required, "{tool}: required args drifted");

        // Every required arg must also be described, or the model is told it
        // must pass something with no indication of what.
        let props = schema["properties"].as_object().unwrap();
        for arg in required {
            let prop = props.get(arg).unwrap_or_else(|| {
                panic!("{tool}: {arg} is required but has no property: {schema}")
            });
            assert!(
                prop["description"].as_str().is_some_and(|d| !d.is_empty()),
                "{tool}.{arg}: required with no description"
            );
        }
    }
}

#[test]
fn a_user_override_that_omits_its_schema_still_advertises_a_usable_tool() {
    // The bug this closes, end to end and through the real binary: the overlay
    // in `presets::load_all` is whole-struct replace by name, so a `grep.toml`
    // dropped into `$XDG_CONFIG_HOME/scout/presets/` to reword the prompt used
    // to replace the built-in schema with `{"properties":{},"required":[]}`.
    // The tool stayed advertised, the model called it with nothing, and
    // `grep::run` answered "'pattern' argument is required" — with nothing
    // logged and nothing warned.
    let sandbox = Sandbox::new();
    sandbox.write_preset(
        "grep.toml",
        r#"
system = "You are a grep filter. Answer tersely."
user   = "Pattern: ${args.pattern}\nIntent: ${args.intent}\n"

[preset]
name = "grep"
description = "A user's own wording for the grep tool, changing nothing about its arguments."
"#,
    );

    let mut server = McpServer::spawn(&sandbox);
    handshake(&mut server);

    let result = server.request(2, "tools/list", &json!({}));
    let tools = result["tools"].as_array().expect("tools array");
    let grep = tools.iter().find(|t| t["name"] == "grep").expect("grep not advertised");

    // The override still wins where it spoke.
    assert!(
        grep["description"].as_str().is_some_and(|d| d.starts_with("A user's own wording")),
        "the override's description should be advertised: {grep}"
    );

    // …and the built-in schema survives where it did not.
    let schema = &grep["inputSchema"];
    let required: Vec<&str> = schema["required"]
        .as_array()
        .unwrap_or_else(|| panic!("grep: no required list: {schema}"))
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert_eq!(
        required,
        vec!["pattern", "intent"],
        "schema-less override wiped the argument contract: {schema}"
    );
    assert!(
        schema["properties"].as_object().is_some_and(|p| p.contains_key("pattern")),
        "grep advertised without a pattern property: {schema}"
    );
}

#[test]
fn wrap_detach_then_wait_returns_the_job_over_stdio() {
    // The first tools/call in this suite: detach must return immediately,
    // wait must block until the child exits, and the payload must be wrap's
    // (a short echo is a pass-through). No local model is involved.
    let sandbox = Sandbox::new();
    let mut server = McpServer::spawn(&sandbox);
    handshake(&mut server);

    let started = call_tool(
        &mut server,
        10,
        "wrap",
        &json!({"command": "echo hi; echo bye", "detach": true}),
    );
    let job_id = started["job_id"].as_str().expect("job_id").to_string();
    assert!(job_id.starts_with('j'), "{started}");
    assert!(started.get("raw_path").is_some(), "{started}");

    let waited = call_tool(
        &mut server,
        11,
        "wait",
        &json!({"job_ids": [job_id], "until": "all", "timeout_s": 5}),
    );
    assert_eq!(waited["timed_out"], false, "{waited}");
    assert_eq!(waited["done"].as_array().unwrap().len(), 1, "{waited}");
    assert_eq!(waited["done"][0]["exit_code"], 0, "{waited}");
    assert_eq!(waited["done"][0]["filtered"], false, "{waited}");
    assert!(waited["done"][0]["output"].as_str().unwrap().contains("hi"), "{waited}");
}

/// Call a tool and parse the JSON text content. Panics on a protocol or
/// tool-level error — the wait-family path under test is not fail-open in
/// the "unknown tool" sense; a failure here is a broken contract.
fn call_tool(server: &mut McpServer, id: u64, name: &str, args: &Value) -> Value {
    let result = server.request(id, "tools/call", &json!({"name": name, "arguments": args}));
    assert!(
        result.get("isError").and_then(Value::as_bool) != Some(true),
        "{name} returned isError: {result}"
    );
    let text = result["content"]
        .as_array()
        .and_then(|c| c.first())
        .and_then(|b| b["text"].as_str())
        .unwrap_or_else(|| panic!("{name} had no text content: {result}"));
    serde_json::from_str(text)
        .unwrap_or_else(|e| panic!("{name} content was not JSON: {e}: {text}"))
}

#[test]
fn the_server_shuts_down_when_its_stdin_closes() {
    // The stdio transport's only shutdown signal. If this regresses, every
    // Claude Code session leaks a scout process on exit — which is invisible
    // until a machine has forty of them.
    let sandbox = Sandbox::new();
    let mut server = McpServer::spawn(&sandbox);
    handshake(&mut server);

    drop(server.stdin.take());

    let deadline = Instant::now() + READ_TIMEOUT;
    loop {
        match server.child.try_wait().expect("try_wait") {
            Some(_) => return,
            None if Instant::now() >= deadline => {
                panic!(
                    "scout mcp still running {READ_TIMEOUT:?} after stdin closed{}",
                    server.diagnostics()
                )
            }
            None => std::thread::sleep(Duration::from_millis(25)),
        }
    }
}
