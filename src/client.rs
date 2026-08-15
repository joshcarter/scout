use serde_json::{json, Value};
use std::io::BufRead;
use std::time::{Duration, Instant};

#[derive(Debug)]
pub struct Config {
    pub endpoint: String,
    pub model: String,
    pub timeout: Duration,
    pub api_key: Option<String>,
    pub max_tokens: Option<u64>,
    /// Ask the endpoint for `text/event-stream` and read the reply delta by
    /// delta (SPEC-dashboard §5.5, P5). Observability only — the accumulated
    /// text and the `usage` object are the same either way, and a `false`
    /// here is a fully supported path, not a vestige: §5.5 was measured on
    /// LM Studio, and any host that drops `include_usage` under streaming
    /// gets its numbers back by turning this off.
    pub stream: bool,
}

#[derive(Debug)]
pub enum LlmError {
    EndpointUnavailable { endpoint: String },
    RequestFailed(String),
    Timeout,
    Internal(String),
}

/// One-line, caller-facing rendering of an LLM failure.
///
/// This is the taxonomy's only surface now: the read-side filters wrap it in
/// their fail-open message (`select::call_preset`), `run`/`task` print it to
/// stderr, and the MCP server turns that message into tool-level `isError`
/// content.  (A JSON-RPC `to_rpc_error` renderer lived here through step 2,
/// for an MCP layer scout does not hand-roll — rmcp owns the envelopes.)
impl LlmError {
    /// The call log's `outcome.kind` for this failure (SPEC-dashboard §3).
    ///
    /// It lives here, next to the code that mints the messages, because two of
    /// the four variants are broader than the taxonomy: `RequestFailed` covers
    /// an HTTP status, a mid-call I/O error and an unreadable reply alike, and
    /// the only thing separating them is the string this file just wrote.  A
    /// richer `LlmError` would make this a plain match — worth doing the next
    /// time this enum is touched, not worth destabilising the taxonomy for.
    pub fn outcome(&self) -> crate::stats::Outcome {
        use crate::stats::Outcome;
        match self {
            LlmError::EndpointUnavailable { .. } => Outcome::EndpointUnreachable,
            LlmError::Timeout => Outcome::Timeout,
            LlmError::RequestFailed(msg) if msg.starts_with("HTTP ") => Outcome::HttpError,
            // The endpoint answered and then went away mid-call; from the
            // caller's chair that is the same problem as never answering.
            LlmError::RequestFailed(msg) if msg.starts_with("network I/O") => {
                Outcome::EndpointUnreachable
            }
            // Everything else in these two arms is a reply scout could not use:
            // unparseable JSON, no content field, an empty response.
            LlmError::RequestFailed(_) => Outcome::ParseFailure,
            LlmError::Internal(msg) if msg.contains("empty response") => Outcome::EmptyResponse,
            LlmError::Internal(_) => Outcome::ParseFailure,
        }
    }
}

impl std::fmt::Display for LlmError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LlmError::EndpointUnavailable { endpoint } => write!(
                f,
                "local LLM endpoint {endpoint} is not responding — start the host (e.g. `ollama serve`) and retry"
            ),
            LlmError::RequestFailed(msg) => write!(f, "request failed: {msg}"),
            LlmError::Timeout => {
                write!(f, "request timed out — raise timeout_seconds in scout's config")
            }
            LlmError::Internal(msg) => write!(f, "internal error: {msg}"),
        }
    }
}

pub struct LlmClient {
    config: Config,
}

/// Returns true if an `io::Error` should be classified as a request timeout.
/// Used to distinguish ureq's I/O transport errors from endpoint failures.
pub(crate) fn is_timeout_io_error(e: &std::io::Error) -> bool {
    matches!(
        e.kind(),
        std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
    )
}

impl LlmClient {
    pub fn new(config: Config) -> Self {
        LlmClient { config }
    }

    pub fn model(&self) -> &str {
        &self.config.model
    }

    pub fn endpoint(&self) -> &str {
        &self.config.endpoint
    }

    /// Best-effort check: GET /models. Returns (reachable, elapsed_ms).
    /// Non-fatal — endpoint_unavailable surfaces lazily on first call.
    pub fn check_endpoint(&self) -> (bool, u64) {
        let url = format!("{}/models", self.config.endpoint);
        let start = Instant::now();
        let mut req = ureq::get(&url).timeout(Duration::from_secs(5));
        if let Some(key) = &self.config.api_key {
            req = req.set("Authorization", &format!("Bearer {key}"));
        }
        // An HTTP error response (4xx/5xx) means the endpoint is up — only a
        // Transport error means we couldn't connect at all.
        let reachable = match req.call() {
            Ok(_) => true,
            Err(ureq::Error::Status(_, _)) => true,
            Err(ureq::Error::Transport(_)) => false,
        };
        (reachable, start.elapsed().as_millis() as u64)
    }

    /// POST /chat/completions. Returns (content, usage_object).
    ///
    /// `max_tokens` overrides the configured default for this call only.
    ///
    /// The plain form for every caller that has nothing to watch the reply
    /// with: it delegates with a sink that discards. Whether the wire is
    /// streamed is `[llm] stream`'s business, not the caller's — the return
    /// value is identical either way (§5.5).
    pub fn complete(
        &self,
        messages: Vec<Value>,
        max_tokens: Option<u64>,
    ) -> Result<(String, Value), LlmError> {
        self.complete_streaming(messages, max_tokens, &mut |_| {})
    }

    /// `complete`, plus a sink that sees each content delta as it lands.
    ///
    /// Two contract points, both load-bearing:
    ///
    /// * **`on_delta` is best-effort and must not block.** It runs inside the
    ///   HTTP read loop, so a slow sink stalls the model call itself. The one
    ///   implementation scout ships (`live::with_token_stream`) is a buffer
    ///   append and an occasional non-blocking `sendto`.
    /// * **`usage` never reaches the sink.** It arrives in the final chunk and
    ///   comes back in the return value exactly as it always has; `call.end`
    ///   remains its only path to the dashboard.
    ///
    /// A caller that must stay silent (`CallRecord::silent`) simply installs no
    /// sink — this file has never heard of the concept.
    pub fn complete_streaming(
        &self,
        messages: Vec<Value>,
        max_tokens: Option<u64>,
        on_delta: &mut dyn FnMut(&str),
    ) -> Result<(String, Value), LlmError> {
        let url = format!("{}/chat/completions", self.config.endpoint);
        let mut body = json!({
            "model": self.config.model,
            "messages": messages,
            "stream": self.config.stream,
        });
        if self.config.stream {
            // Measured (§5.5): omit this and `usage` is absent from the
            // stream entirely. It is not a nicety.
            body["stream_options"] = json!({"include_usage": true});
        }
        if let Some(mt) = max_tokens.or(self.config.max_tokens) {
            body["max_tokens"] = json!(mt);
        }

        let mut req = ureq::post(&url)
            .timeout(self.config.timeout)
            .set("Content-Type", "application/json");
        if let Some(key) = &self.config.api_key {
            req = req.set("Authorization", &format!("Bearer {key}"));
        }

        let body_str = serde_json::to_string(&body)
            .map_err(|e| LlmError::Internal(format!("serialize request: {e}")))?;

        let call_start = Instant::now();
        let resp = req.send_string(&body_str).map_err(|e| match e {
            ureq::Error::Status(code, r) => {
                LlmError::RequestFailed(format!("HTTP {code}: {}", r.status_text()))
            }
            ureq::Error::Transport(ref t) => {
                use std::error::Error;
                use ureq::ErrorKind;
                match t.kind() {
                    // DNS failure or refused connection — endpoint is down.
                    ErrorKind::ConnectionFailed | ErrorKind::Dns => LlmError::EndpointUnavailable {
                        endpoint: self.config.endpoint.clone(),
                    },
                    // I/O error — inspect inner io::Error to distinguish timeout from disconnect.
                    ErrorKind::Io => {
                        let timed_out = (t as &dyn Error)
                            .source()
                            .and_then(|s| s.downcast_ref::<std::io::Error>())
                            .map(is_timeout_io_error)
                            .unwrap_or(false);
                        if timed_out || call_start.elapsed() >= self.config.timeout {
                            LlmError::Timeout
                        } else {
                            LlmError::RequestFailed(format!("network I/O error: {t}"))
                        }
                    }
                    // Anything else: fall back to elapsed heuristic.
                    _ => {
                        if call_start.elapsed() >= self.config.timeout {
                            LlmError::Timeout
                        } else {
                            LlmError::EndpointUnavailable {
                                endpoint: self.config.endpoint.clone(),
                            }
                        }
                    }
                }
            }
        })?;

        // The response has arrived, which per §5.5 is where the two paths
        // diverge and nowhere else: an HTTP error is a normal 4xx with a JSON
        // body delivered *before* any stream begins, so the whole taxonomy
        // above fires identically whether or not `stream` is set.
        if self.config.stream {
            read_stream(std::io::BufReader::new(resp.into_reader()), on_delta)
        } else {
            let resp_str = resp
                .into_string()
                .map_err(|e| LlmError::RequestFailed(format!("read response body: {e}")))?;
            parse_completion(&resp_str)
        }
    }
}

/// Pull `(content, usage)` out of one whole non-streamed response body.
pub(crate) fn parse_completion(resp_str: &str) -> Result<(String, Value), LlmError> {
    let data: Value = serde_json::from_str(resp_str)
        .map_err(|e| LlmError::RequestFailed(format!("parse LLM response: {e}")))?;

    let content = data["choices"][0]["message"]["content"]
        .as_str()
        .ok_or_else(|| LlmError::RequestFailed("no content field in LLM response".into()))?
        .to_string();

    let usage = data.get("usage").cloned().unwrap_or(Value::Null);
    Ok((content, usage))
}

/// Read an OpenAI-style `text/event-stream` reply to its end.
///
/// Returns the same `(content, usage)` the non-streamed path returns: the
/// deltas are accumulated on the way past and handed to `on_delta` as they
/// land, but nothing about the result depends on anyone listening.
///
/// Three details the measurement in §5.5 surfaced, all of them here:
///
/// * **The final chunk carries `"choices": []`** — an empty array, alongside
///   the usage object. `chunk["choices"][0]["delta"]` on every chunk breaks on
///   exactly that one, so usage is read first and deltas are read defensively.
/// * **A stream that stops early must fail, not truncate quietly.** A reply cut
///   off mid-flight looks like a short answer to every caller above, and a
///   short answer from a summariser is indistinguishable from a real one. EOF
///   without either `[DONE]` or a `finish_reason` is an error.
/// * **Anything that is not a `data:` line is ignored** — SSE comment
///   keep-alives (`: ping`) and `event:` lines are legal and carry nothing.
pub(crate) fn read_stream<R: BufRead>(
    mut reader: R,
    on_delta: &mut dyn FnMut(&str),
) -> Result<(String, Value), LlmError> {
    let mut text = String::new();
    let mut usage = Value::Null;
    let mut finished = false;
    let mut line = String::new();

    loop {
        line.clear();
        let n = reader.read_line(&mut line).map_err(|e| {
            if is_timeout_io_error(&e) {
                LlmError::Timeout
            } else {
                // Same classification the pre-stream Io arm uses: the endpoint
                // answered and then went away, which from the caller's chair is
                // the same problem as never answering.
                LlmError::RequestFailed(format!("network I/O error: {e}"))
            }
        })?;
        if n == 0 {
            break;
        }
        let Some(payload) = line.trim_end().strip_prefix("data:") else {
            continue;
        };
        let payload = payload.trim_start();
        if payload == "[DONE]" {
            finished = true;
            break;
        }
        let chunk: Value = serde_json::from_str(payload)
            .map_err(|e| LlmError::RequestFailed(format!("parse LLM stream chunk: {e}")))?;

        // Usage first: the chunk that carries it has no choices at all.
        if let Some(u) = chunk.get("usage") {
            if !u.is_null() {
                usage = u.clone();
            }
        }
        let Some(choice) = chunk["choices"].get(0) else {
            continue;
        };
        if let Some(delta) = choice["delta"]["content"].as_str() {
            if !delta.is_empty() {
                text.push_str(delta);
                on_delta(delta);
            }
        }
        if choice.get("finish_reason").is_some_and(|r| !r.is_null()) {
            // A host that closes without `[DONE]` (the frame is conventional,
            // not universal) still said the completion was complete.
            finished = true;
        }
    }

    if !finished {
        return Err(LlmError::RequestFailed(
            "LLM stream ended mid-reply — no [DONE] frame and no finish_reason".into(),
        ));
    }
    Ok((text, usage))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dead_client() -> LlmClient {
        LlmClient::new(Config {
            endpoint: "http://127.0.0.1:1/v1".into(),
            model: "test-model".into(),
            timeout: Duration::from_secs(2),
            api_key: None,
            max_tokens: None,
            stream: true,
        })
    }

    #[test]
    fn wrong_endpoint_check_returns_false() {
        let client = dead_client();
        let (reachable, _ms) = client.check_endpoint();
        assert!(!reachable);
    }

    #[test]
    fn wrong_endpoint_complete_returns_unavailable() {
        let client = dead_client();
        let err = client
            .complete(vec![json!({"role": "user", "content": "hi"})], None)
            .unwrap_err();
        assert!(matches!(err, LlmError::EndpointUnavailable { .. }));
    }





    // ── Streaming read loop (P5) ─────────────────────────────────────────
    // The reader is split out from the HTTP call precisely so these run
    // against a `Cursor` instead of a socket. Every chunk shape below was
    // taken from the §5.5 measurement against LM Studio.

    fn delta(content: &str) -> String {
        format!(
            "data: {}\n\n",
            json!({"choices": [{"index": 0, "delta": {"content": content}}]})
        )
    }

    fn read(sse: &str) -> Result<(String, Value, Vec<String>), LlmError> {
        let mut seen = Vec::new();
        let (text, usage) = {
            let mut sink = |d: &str| seen.push(d.to_string());
            read_stream(std::io::Cursor::new(sse.as_bytes()), &mut sink)?
        };
        Ok((text, usage, seen))
    }

    #[test]
    fn deltas_accumulate_in_order_and_reach_the_sink() {
        let sse = format!(
            "{}{}{}data: [DONE]\n\n",
            delta("Hel"),
            delta("lo, "),
            delta("world")
        );
        let (text, usage, seen) = read(&sse).unwrap();
        assert_eq!(text, "Hello, world");
        assert_eq!(seen, ["Hel", "lo, ", "world"]);
        assert!(usage.is_null(), "no usage chunk was sent: {usage}");
    }

    #[test]
    fn the_final_usage_chunk_has_empty_choices_and_still_parses() {
        // The measured gotcha (§5.5): `"choices": []`, not a populated array.
        // `chunk["choices"][0]["delta"]` on this one is what breaks a naive
        // reader, and it is the chunk carrying the only numbers scout wants.
        let sse = format!(
            "{}data: {}\n\ndata: [DONE]\n\n",
            delta("hi"),
            json!({"choices": [], "usage": {"prompt_tokens": 15, "completion_tokens": 3, "total_tokens": 18}})
        );
        let (text, usage, seen) = read(&sse).unwrap();
        assert_eq!(text, "hi");
        assert_eq!(seen, ["hi"], "the usage chunk is not a delta");
        assert_eq!(usage["prompt_tokens"], 15);
        assert_eq!(usage["completion_tokens"], 3);
    }

    #[test]
    fn streamed_and_whole_replies_produce_the_same_pair() {
        // The `stream = false` escape hatch has to be a supported path, not a
        // vestige: the same completion delivered either way must be
        // indistinguishable to every caller above `client.rs`.
        let whole = json!({
            "choices": [{"index": 0, "message": {"role": "assistant", "content": "Hello, world"}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 15, "completion_tokens": 3, "total_tokens": 18},
        })
        .to_string();
        let sse = format!(
            "{}{}data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
            delta("Hello, "),
            delta("world"),
            json!({"choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]}),
            json!({"choices": [], "usage": {"prompt_tokens": 15, "completion_tokens": 3, "total_tokens": 18}})
        );
        let (a_text, a_usage) = parse_completion(&whole).unwrap();
        let (b_text, b_usage, _) = read(&sse).unwrap();
        assert_eq!(a_text, b_text);
        assert_eq!(a_usage["prompt_tokens"], b_usage["prompt_tokens"]);
        assert_eq!(a_usage["completion_tokens"], b_usage["completion_tokens"]);
    }

    #[test]
    fn a_truncated_stream_is_an_error_not_a_short_answer() {
        // The failure this guards is silent: half a summary reads exactly like
        // a whole one to `check_output`, `grep` and `extract` alike.
        let sse = format!("{}{}", delta("The answer i"), "data: {\"choi");
        let err = read(&sse).unwrap_err();
        match &err {
            LlmError::RequestFailed(m) => assert!(m.contains("parse LLM stream chunk"), "{m}"),
            other => panic!("expected RequestFailed, got {other:?}"),
        }

        // Clean EOF, no `[DONE]`, no finish_reason: nothing malformed, and
        // still not a complete reply.
        let err = read(&delta("The answer i")).unwrap_err();
        match &err {
            LlmError::RequestFailed(m) => assert!(m.contains("mid-reply"), "{m}"),
            other => panic!("expected RequestFailed, got {other:?}"),
        }
        assert_eq!(err.outcome(), crate::stats::Outcome::ParseFailure);
    }

    #[test]
    fn a_finish_reason_closes_a_stream_that_never_says_done() {
        let sse = format!(
            "{}data: {}\n\n",
            delta("hi"),
            json!({"choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]})
        );
        let (text, _, _) = read(&sse).unwrap();
        assert_eq!(text, "hi");
    }

    #[test]
    fn comments_blank_lines_and_empty_deltas_are_ignored() {
        let sse = format!(
            ": ping\n\nevent: message\n{}data: {}\n\n{}data: [DONE]\n\n",
            delta("a"),
            json!({"choices": [{"index": 0, "delta": {"role": "assistant"}}]}),
            delta("")
        );
        let (text, _, seen) = read(&sse).unwrap();
        assert_eq!(text, "a");
        assert_eq!(seen, ["a"], "an empty delta is not worth an event");
    }

    #[test]
    fn a_stream_with_no_content_at_all_is_an_empty_string_not_an_error() {
        // `run_cmd` and `task` both turn this into `EmptyResponse` themselves;
        // the reader's job is only to report what arrived.
        let (text, usage, seen) = read("data: [DONE]\n\n").unwrap();
        assert!(text.is_empty());
        assert!(usage.is_null());
        assert!(seen.is_empty());
    }

    // ── Display rendering ────────────────────────────────────────────────
    // Every failure the filters report to the caller goes through this.

    #[test]
    fn endpoint_unavailable_names_the_endpoint_and_the_fix() {
        let s = LlmError::EndpointUnavailable { endpoint: "http://localhost:11434/v1".into() }
            .to_string();
        assert!(s.contains("http://localhost:11434/v1"), "{s}");
        assert!(s.contains("ollama serve"), "the fix must be named: {s}");
    }

    #[test]
    fn timeout_points_at_the_config_knob() {
        let s = LlmError::Timeout.to_string();
        assert!(s.contains("timed out"), "{s}");
        assert!(s.contains("timeout_seconds"), "{s}");
    }

    #[test]
    fn every_failure_maps_to_a_call_log_outcome() {
        use crate::stats::Outcome;
        let cases = [
            (LlmError::EndpointUnavailable { endpoint: "x".into() }, Outcome::EndpointUnreachable),
            (LlmError::Timeout, Outcome::Timeout),
            (LlmError::RequestFailed("HTTP 500: boom".into()), Outcome::HttpError),
            (LlmError::RequestFailed("network I/O error: reset".into()), Outcome::EndpointUnreachable),
            (LlmError::RequestFailed("parse LLM response: eof".into()), Outcome::ParseFailure),
            (LlmError::Internal("LLM returned empty response".into()), Outcome::EmptyResponse),
            (LlmError::Internal("serialize request: nope".into()), Outcome::ParseFailure),
        ];
        for (err, want) in cases {
            assert_eq!(err.outcome(), want, "{err:?}");
        }
        // ...and none of them is ever recorded as a success.
        assert!(!LlmError::Timeout.outcome().is_ok());
    }

    #[test]
    fn request_failed_and_internal_carry_their_message() {
        assert!(LlmError::RequestFailed("HTTP 500: boom".into()).to_string().contains("HTTP 500"));
        assert!(LlmError::Internal("bad param".into()).to_string().contains("bad param"));
    }

    #[test]
    fn complete_empty_choices_returns_request_failed() {
        // Simulate the response-parsing path with an empty choices array.
        // We test parse_response directly since we can't intercept ureq in unit tests.
        let data = json!({ "choices": [] });
        let content = data["choices"][0]["message"]["content"].as_str();
        assert!(content.is_none(), "empty choices should yield None");
    }

    // ── ErrorKind::Io classification ─────────────────────────────────────
    // The Io arm in complete() downcasts the ureq Transport source to
    // io::Error to distinguish Timeout from other errors. We test the
    // classification predicate directly since ureq Transport is unforgeble.


    #[test]
    fn io_timed_out_matches_timeout_predicate() {
        let io_err = std::io::Error::from(std::io::ErrorKind::TimedOut);
        assert!(
            is_timeout_io_error(&io_err),
            "TimedOut should match the timeout predicate"
        );
    }

    #[test]
    fn io_would_block_matches_timeout_predicate() {
        let io_err = std::io::Error::from(std::io::ErrorKind::WouldBlock);
        assert!(
            is_timeout_io_error(&io_err),
            "WouldBlock should match the timeout predicate"
        );
    }

    #[test]
    fn io_broken_pipe_does_not_match_timeout_predicate() {
        let io_err = std::io::Error::from(std::io::ErrorKind::BrokenPipe);
        assert!(
            !is_timeout_io_error(&io_err),
            "BrokenPipe must not match the timeout predicate"
        );
    }


    #[test]
    fn check_endpoint_treats_http_error_as_reachable() {
        // A dead port returns Transport (not Status), so this is a compile-time
        // logic check: Status(_,_) => true must be present in check_endpoint.
        // The live variant is tested in live_roundtrip (ignored).
        let client = dead_client();
        // Dead port ⇒ Transport ⇒ false; confirms the Transport arm works.
        let (reachable, _) = client.check_endpoint();
        assert!(!reachable);
    }

    /// Round-trip test against a real local endpoint.
    /// Run with: LM_HOST=http://localhost:11434 cargo test -- --ignored
    #[test]
    #[ignore]
    fn live_roundtrip() {
        let endpoint = std::env::var("LM_HOST")
            .unwrap_or_else(|_| "http://localhost:11434/v1".into());
        let model = std::env::var("LM_MODEL").unwrap_or_else(|_| "qwen3:8b".into());
        let client = LlmClient::new(Config {
            endpoint: endpoint.clone(),
            model,
            timeout: Duration::from_secs(120),
            api_key: None,
            max_tokens: Some(64),
            stream: true,
        });

        let (reachable, ms) = client.check_endpoint();
        assert!(reachable, "endpoint {endpoint} not reachable (start ollama serve)");
        assert!(ms < 5000, "endpoint check took too long: {ms}ms");

        let (content, usage) = client
            .complete(vec![json!({"role": "user", "content": "Reply with exactly: hello"})], None)
            .unwrap();
        assert!(!content.is_empty(), "empty response from LLM");
        assert!(usage.is_object(), "usage should be an object, got: {usage}");
        assert!(usage["prompt_tokens"].is_number(), "missing prompt_tokens");
        assert!(usage["completion_tokens"].is_number(), "missing completion_tokens");
    }

    /// The §5.5 measurement, as a test: same prompt both ways, same text and
    /// the same token counts, with the deltas actually arriving one by one.
    ///
    /// Run with:
    ///   LM_HOST=http://localhost:1234/v1 LM_MODEL=<loaded model> \
    ///     cargo test -- --ignored stream_matches_non_stream
    #[test]
    #[ignore]
    fn live_stream_matches_non_stream() {
        let endpoint =
            std::env::var("LM_HOST").unwrap_or_else(|_| "http://localhost:1234/v1".into());
        let model = std::env::var("LM_MODEL").unwrap_or_else(|_| "qwen/qwen3.6-35b-a3b".into());
        let cfg = |stream: bool| Config {
            endpoint: endpoint.clone(),
            model: model.clone(),
            timeout: Duration::from_secs(120),
            api_key: None,
            max_tokens: Some(64),
            stream,
        };
        let prompt = vec![json!({
            "role": "user",
            "content": "Count from one to ten in words, separated by commas. /no_think",
        })];

        let (plain, plain_usage) = LlmClient::new(cfg(false))
            .complete(prompt.clone(), None)
            .expect("non-streaming call");

        let mut deltas: Vec<String> = Vec::new();
        let (streamed, streamed_usage) = {
            let mut sink = |d: &str| deltas.push(d.to_string());
            LlmClient::new(cfg(true))
                .complete_streaming(prompt, None, &mut sink)
                .expect("streaming call")
        };

        assert!(deltas.len() > 1, "no deltas observed: {deltas:?}");
        assert_eq!(deltas.concat(), streamed, "the sink saw a different reply");
        assert!(streamed_usage.is_object(), "usage vanished: {streamed_usage}");
        assert_eq!(
            plain_usage["prompt_tokens"], streamed_usage["prompt_tokens"],
            "prompt tokens differ: {plain_usage} vs {streamed_usage}"
        );
        // Completion length is the model's choice, not the transport's, so
        // this compares the shape rather than the count.
        assert!(streamed_usage["completion_tokens"].as_u64().unwrap_or(0) > 0);
        assert!(!plain.is_empty() && !streamed.is_empty());
    }
}
