// End-to-end tests for `wrap` — contract: what the caller gets back, what the
// model was shown, and what survived on disk (docs/wrap-watch.md §3).
//
// These live out here rather than in `wrap.rs` for a specific reason. The
// filtered path writes a spool blob under `$XDG_CACHE_HOME`, and an in-process
// test could only redirect that by setting a process-global env var that
// `spool`'s own tests already read — two suites racing on one variable, with
// the developer's real `~/.cache/scout/raw/` as the prize for losing. A
// subprocess in a sandbox has its own environment and no such race.
//
// It also buys the assertion that matters most and is invisible from inside:
// the *prompt* the model received. `wrap` shows it an elided copy while
// spooling everything (§3.4), and the only honest way to see the difference is
// to be the model.

mod support;

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::mpsc::{self, Receiver};
use std::time::Duration;

use serde_json::{json, Value};

use support::Sandbox;

/// A well-behaved reply from the local model, in the preset's contract.
const CANNED: &str = r#"{"summary": "The command printed five thousand numbered lines.",
  "answer": "line-4242 is the marker",
  "notable": ["line-1", "line-4242"]}"#;

/// Read one whole HTTP request off `sock` and return its body — headers first,
/// then exactly the bytes `Content-Length` promises.
///
/// Replying before the request has been consumed is what makes a naive one-shot
/// server flaky: closing with unread bytes still queued sends an RST, and
/// whatever the client had buffered goes with it.
fn read_request(sock: &mut TcpStream) -> String {
    let mut buf = Vec::new();
    let mut scratch = [0u8; 8192];
    loop {
        if let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&buf[..end]).to_lowercase();
            let len: usize = head
                .lines()
                .find_map(|l| l.strip_prefix("content-length:"))
                .and_then(|v| v.trim().parse().ok())
                .unwrap_or(0);
            if buf.len() >= end + 4 + len {
                return String::from_utf8_lossy(&buf[end + 4..end + 4 + len]).into_owned();
            }
        }
        match sock.read(&mut scratch) {
            Ok(0) | Err(_) => return String::from_utf8_lossy(&buf).into_owned(),
            Ok(n) => buf.extend_from_slice(&scratch[..n]),
        }
    }
}

/// A one-shot OpenAI-compatible host that answers with `CANNED`.
///
/// Returns the base URL to point scout's config at, and a channel carrying the
/// request body it received — which is the prompt `wrap` built.
fn canned_server() -> (String, Receiver<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind a fake LLM host");
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let Ok((mut sock, _)) = listener.accept() else { return };
        let _ = tx.send(read_request(&mut sock));
        let body = json!({
            "choices": [{"index": 0, "message": {"role": "assistant", "content": CANNED},
                         "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 900, "completion_tokens": 40, "total_tokens": 940},
        })
        .to_string();
        let _ = sock.write_all(
            format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\
                 Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .as_bytes(),
        );
        let _ = sock.flush();
    });
    (format!("http://{addr}/v1"), rx)
}

/// A base URL nothing is listening on: bound to learn a free port, then
/// released, so a connection to it is refused rather than hung.
fn dead_endpoint() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().unwrap();
    drop(listener);
    format!("http://{addr}/v1")
}

/// Write a config naming `endpoint`, and return the path to hand scout.
fn config_at(sandbox: &Sandbox, endpoint: &str) -> std::path::PathBuf {
    let path = sandbox.root().join("config.toml");
    std::fs::write(
        &path,
        format!(
            "[llm]\nendpoint = \"{endpoint}\"\nmodel = \"test-model\"\n\
             stream = false\ntimeout_seconds = 30\n"
        ),
    )
    .expect("write config");
    path
}

/// A command printing `n` numbered lines, one of them a marker.
fn noisy(n: usize) -> String {
    format!("i=1; while [ $i -le {n} ]; do echo \"line-$i\"; i=$((i+1)); done")
}

/// Run `scout wrap`, returning the parsed payload and the process exit code.
fn wrap(sandbox: &Sandbox, config: &std::path::Path, args: &[&str]) -> (Value, Option<i32>) {
    let out = sandbox
        .scout()
        .arg("wrap")
        .args(args)
        .env("SCOUT_CONFIG", config)
        .output()
        .expect("run scout wrap");
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let payload = serde_json::from_str(&stdout).unwrap_or_else(|e| {
        panic!(
            "scout wrap did not print a JSON payload: {e}\nstdout: {stdout}\nstderr: {}",
            String::from_utf8_lossy(&out.stderr)
        )
    });
    (payload, out.status.code())
}

/// Every row in the sandbox's call log.
fn rows(sandbox: &Sandbox) -> Vec<Value> {
    let path = sandbox.root().join("calls.jsonl");
    let Ok(text) = std::fs::read_to_string(path) else { return Vec::new() };
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect()
}

#[test]
fn a_condensed_result_carries_the_models_three_fields_and_scouts_ground_truth() {
    let sandbox = Sandbox::new();
    let (endpoint, prompts) = canned_server();
    let config = config_at(&sandbox, &endpoint);

    let (p, code) = wrap(&sandbox, &config, &[&noisy(5000), "which line is the marker?"]);

    assert_eq!(code, Some(0), "the child exited 0, so scout does too");
    assert_eq!(p["filtered"], true);
    assert_eq!(p["exit_code"], 0);
    assert_eq!(p["summary"], "The command printed five thousand numbered lines.");
    assert_eq!(p["answer"], "line-4242 is the marker");
    assert_eq!(p["notable"], json!(["line-1", "line-4242"]));
    assert_eq!(p["lines_total"], 5000, "counted by scout, not claimed by the model");
    assert_eq!(p["lines_dropped"], 4998, "5000 captured, 2 quoted");
    assert!(p["bytes_total"].as_u64().unwrap() > 40_000, "{p}");
    assert!(p.get("degraded").is_none(), "nothing degraded: {p}");

    // §2.4: the summary is only safe to trust because this is on disk.
    let blobs = sandbox.spooled();
    assert_eq!(blobs.len(), 1, "one filtered call, one blob");
    assert_eq!(p["raw_path"], blobs[0].display().to_string());
    let raw = std::fs::read_to_string(&blobs[0]).expect("the payload names a readable file");
    assert_eq!(raw.lines().count(), 5000, "the spool holds the full capture");
    assert!(raw.contains("line-2500"), "including the middle the model never saw");

    // ...and what the model was actually shown: the elided form, and the
    // question, and nothing like the whole capture.
    let prompt = prompts.recv_timeout(Duration::from_secs(20)).expect("the model was called");
    assert!(prompt.contains("bytes elided"), "the model saw the elided copy");
    assert!(prompt.contains("which line is the marker?"), "the question steers the filter");
    assert!(!prompt.contains("line-2500"), "the elided middle must not reach the prompt");

    // §6: the row names the blob, so the dashboard can drill into it.
    let row = rows(&sandbox).pop().expect("one row per invocation");
    assert_eq!(row["tool"], "wrap");
    assert_eq!(row["preset"], "wrap");
    assert_eq!(row["raw_path"], blobs[0].display().to_string());
    assert!(row["raw_bytes"].as_u64().unwrap() > row["returned_bytes"].as_u64().unwrap());
}

#[test]
fn short_output_is_returned_verbatim_without_a_model_or_a_spool_file() {
    // §3.2: the endpoint here is dead, and it does not matter — a pass-through
    // never calls anything. That is what makes a wrong "this will be verbose"
    // guess cost only the exec.
    let sandbox = Sandbox::new();
    let config = config_at(&sandbox, &dead_endpoint());

    let (p, code) = wrap(&sandbox, &config, &["echo one; echo two"]);

    assert_eq!(code, Some(0));
    assert_eq!(p["filtered"], false);
    assert_eq!(p["output"], "one\ntwo");
    assert!(p.get("raw_path").is_none(), "nothing lossy happened: {p}");
    assert!(p.get("degraded").is_none(), "and nothing went wrong: {p}");
    assert!(sandbox.spooled().is_empty(), "§2.2: a pass-through writes no blob");
}

#[test]
fn the_childs_exit_code_is_scouts_exit_code() {
    // docs/wrap-watch.md §8, decided yes: the wrapped command is the caller's,
    // and in a pipeline its status is the one that means something.
    let sandbox = Sandbox::new();
    let config = config_at(&sandbox, &dead_endpoint());
    let (p, code) = wrap(&sandbox, &config, &["echo nope; exit 3"]);
    assert_eq!(p["exit_code"], 3, "uninterpreted: a non-zero exit is not a verdict");
    assert_eq!(code, Some(3));
}

#[test]
fn an_unreachable_model_costs_the_summary_and_not_the_result() {
    // §3.5, the whole fail-open rule in one run: the command's output still
    // comes back, the reason is named, and the spool write already happened so
    // the escalation path survives the model being down.
    let sandbox = Sandbox::new();
    let config = config_at(&sandbox, &dead_endpoint());

    let (p, code) = wrap(&sandbox, &config, &[&noisy(400)]);

    assert_eq!(code, Some(0), "the command itself succeeded");
    assert_eq!(p["filtered"], false, "nothing was filtered, so nothing claims to have been");
    assert_eq!(p["exit_code"], 0);
    assert!(!p["degraded"].as_str().unwrap().is_empty(), "the reason is named: {p}");
    let output = p["output"].as_str().unwrap();
    assert!(output.contains("line-1") && output.contains("line-400"), "{output}");

    let blobs = sandbox.spooled();
    assert_eq!(blobs.len(), 1, "400 lines is over the pass-through bound, so it spooled");
    assert_eq!(p["raw_path"], blobs[0].display().to_string());
}
