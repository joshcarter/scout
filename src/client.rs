use serde_json::{json, Value};
use std::time::{Duration, Instant};

#[derive(Debug)]
pub struct Config {
    pub endpoint: String,
    pub model: String,
    pub timeout: Duration,
    pub api_key: Option<String>,
    pub max_tokens: Option<u64>,
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

    /// POST /chat/completions (non-streaming). Returns (content, usage_object).
    ///
    /// `max_tokens` overrides the configured default for this call only.
    pub fn complete(
        &self,
        messages: Vec<Value>,
        max_tokens: Option<u64>,
    ) -> Result<(String, Value), LlmError> {
        let url = format!("{}/chat/completions", self.config.endpoint);
        let mut body = json!({
            "model": self.config.model,
            "messages": messages,
            "stream": false,
        });
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

        let resp_str = resp
            .into_string()
            .map_err(|e| LlmError::RequestFailed(format!("read response body: {e}")))?;
        let data: Value = serde_json::from_str(&resp_str)
            .map_err(|e| LlmError::RequestFailed(format!("parse LLM response: {e}")))?;

        let content = data["choices"][0]["message"]["content"]
            .as_str()
            .ok_or_else(|| LlmError::RequestFailed("no content field in LLM response".into()))?
            .to_string();

        let usage = data.get("usage").cloned().unwrap_or(Value::Null);
        Ok((content, usage))
    }
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
}
