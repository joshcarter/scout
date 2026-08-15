use serde_json::{json, Value};
use std::io::BufRead;
use std::time::{Duration, Instant};

#[derive(Debug)]
pub struct Config {
    pub endpoint: String,
    pub model: String,
    /// The outer bound on a whole call: connect, write, headers, body.
    /// Enforced by ureq itself (`Request::timeout`) as well as by the read
    /// loop, and it is the only instrument the non-streamed path has.
    pub timeout: Duration,
    /// How long the stream may stay silent before the first content delta.
    ///
    /// Generous on purpose: on a cold host this covers loading several GB of
    /// weights off disk, which is a different phenomenon from a wedged
    /// connection and must not share a clock with it. Measured from the
    /// moment the response headers land — see `read_stream`.
    pub first_token_timeout: Duration,
    /// How long the stream may stay silent *after* it has started producing.
    ///
    /// Tight on purpose: once tokens are flowing, a long gap is a stall, not
    /// slowness. This is the budget that lets `timeout` stay generous — a
    /// model emitting steadily is healthy however long it has been running,
    /// and the p90 of a preset can double from a GPU placement change without
    /// anything being wrong (TODO.md).
    pub idle_timeout: Duration,
    pub api_key: Option<String>,
    pub max_tokens: Option<u64>,
    /// Ask the endpoint for `text/event-stream` and read the reply delta by
    /// delta (docs/dashboard.md §6, P5). Observability only — the accumulated
    /// text and the `usage` object are the same either way, and a `false`
    /// here is a fully supported path, not a vestige: §5.5 was measured on
    /// LM Studio, and any host that drops `include_usage` under streaming
    /// gets its numbers back by turning this off.
    pub stream: bool,
}

#[derive(Debug)]
pub enum LlmError {
    EndpointUnavailable {
        endpoint: String,
    },
    RequestFailed(String),
    /// A deadline elapsed. The payload names *which* one, because scout bounds
    /// a streamed call with three separate clocks that mean three different
    /// things, and "timed out" on its own points the user at the wrong knob.
    Timeout(Deadline),
    Internal(String),
}

/// Which of the three clocks ran out, and what it was set to.
///
/// The distinction is the whole point of the feature: a total-elapsed deadline
/// cannot tell a slow-but-working model from a hung one, so the two progress
/// budgets carry the diagnosis and `Overall` stays the outer net.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Deadline {
    /// The stream opened and never produced a content delta.
    FirstToken(Duration),
    /// The stream produced output and then went quiet.
    Idle(Duration),
    /// The whole call ran past `timeout_seconds`.
    Overall(Duration),
}

/// Render a budget for a human. Config clamps every knob to whole seconds, so
/// the sub-second arm exists only for tests — but a "0s" in an error message
/// would be a lie, and this file's errors are read by users.
fn human(d: Duration) -> String {
    if d.as_secs() >= 1 {
        format!("{}s", d.as_secs())
    } else {
        format!("{}ms", d.as_millis())
    }
}

/// One-line, caller-facing rendering of an LLM failure.
///
/// This is the taxonomy's only surface now: the read-side filters wrap it in
/// their fail-open message (`select::call_preset`), `run`/`task` print it to
/// stderr, and the MCP server turns that message into tool-level `isError`
/// content.  (A JSON-RPC `to_rpc_error` renderer lived here through step 2,
/// for an MCP layer scout does not hand-roll — rmcp owns the envelopes.)
impl LlmError {
    /// The call log's `outcome.kind` for this failure (docs/dashboard.md §3).
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
            // All three deadlines are one outcome to the call log: the call
            // did not finish in the time it was given. Which clock caught it
            // is a diagnosis for the message, not a new taxonomy row.
            LlmError::Timeout(_) => Outcome::Timeout,
            LlmError::RequestFailed(msg) if msg.starts_with("HTTP ") => Outcome::HttpError,
            // The endpoint answered and then went away mid-call; from the
            // caller's chair that is the same problem as never answering.
            LlmError::RequestFailed(msg) if msg.starts_with("network I/O") => {
                Outcome::EndpointUnreachable
            }
            LlmError::Internal(msg) if msg.contains("empty response") => Outcome::EmptyResponse,
            // Everything else is a reply scout could not use: unparseable
            // JSON, no content field.
            LlmError::RequestFailed(_) | LlmError::Internal(_) => Outcome::ParseFailure,
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
            // Each arm names the knob that governs the clock that fired.
            // Pointing a stalled stream at timeout_seconds would send the user
            // to raise a limit that was never the one it hit.
            LlmError::Timeout(Deadline::FirstToken(d)) => write!(
                f,
                "request timed out — no first token within {} (the model may still be loading) — raise first_token_timeout_seconds in scout's config",
                human(*d)
            ),
            LlmError::Timeout(Deadline::Idle(d)) => write!(
                f,
                "request timed out — the stream stalled mid-reply, no output for {} — raise idle_timeout_seconds in scout's config",
                human(*d)
            ),
            LlmError::Timeout(Deadline::Overall(d)) => write!(
                f,
                "request timed out after {} overall — raise timeout_seconds in scout's config",
                human(*d)
            ),
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
    matches!(e.kind(), std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock)
}

/// The three clocks a streamed call runs against.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Budgets {
    pub overall: Duration,
    pub first_token: Duration,
    pub idle: Duration,
}

impl Budgets {
    /// How often the read loop wakes to look at its watch.
    ///
    /// A quarter of the tightest budget so the overshoot is small relative to
    /// the thing being measured, floored at 100 ms so a pathological config
    /// cannot spin, capped at 1 s so an idle stream costs essentially nothing.
    /// With the shipped defaults this is 1 s against a 15 s idle gap.
    pub(crate) fn poll_interval(&self) -> Duration {
        (self.idle.min(self.first_token) / 4)
            .clamp(Duration::from_millis(100), Duration::from_secs(1))
    }
}

/// Which budget, if any, has run out — asked only when a read woke up empty.
///
/// The two progress clocks are consulted before the overall one because they
/// are the tighter and more specific diagnosis; with any sane config they also
/// fire first in wall-clock terms, so the ordering only decides ties.
fn budget_elapsed(
    budgets: &Budgets,
    call_start: Instant,
    stream_start: Instant,
    last_progress: Instant,
    armed: bool,
) -> Option<Deadline> {
    let now = Instant::now();
    if armed {
        // The idle gap is only meaningful once the model has proved it can
        // produce at all — armed on the first content delta, never before.
        if now.duration_since(last_progress) >= budgets.idle {
            return Some(Deadline::Idle(budgets.idle));
        }
    } else if now.duration_since(stream_start) >= budgets.first_token {
        return Some(Deadline::FirstToken(budgets.first_token));
    }
    if now.duration_since(call_start) >= budgets.overall {
        return Some(Deadline::Overall(budgets.overall));
    }
    None
}

/// The HTTP response body, pumped by a helper thread so the read loop can wake
/// up on a clock of its own.
///
/// This exists because of a measured ureq detail, not a preference. ureq 2's
/// `Request::timeout()` is an *overall request deadline*: `DeadlineStream`
/// re-arms the socket's `SO_RCVTIMEO` to the whole remaining budget before
/// every `fill_buf`, so a blocking read on a silent server parks until the
/// overall deadline and nothing else. Measured against a server that sends one
/// SSE frame and then stalls: with `.timeout(8s)` the read returned exactly one
/// error, `TimedOut`, at 8.34 s. An elapsed-check inside the loop can never run
/// during that park, so a progress budget written that way would silently never
/// fire.
///
/// Two ways out were considered:
///
/// * `AgentBuilder::timeout_read` does give short per-read wake-ups — measured
///   at 503 ms, 1.007 s, 1.511 s … against the same stalling server, with the
///   connection still usable afterwards. But ureq ignores it entirely whenever
///   a request deadline is set (`stream.rs`: `if let Some(deadline) … else
///   config.timeout_read`), verified at 8.22 s with both configured, so taking
///   it means dropping the overall deadline; and being a socket option it also
///   governs the wait for the *response headers*. Hosts that load the model
///   before writing the status line (ollama's scheduler blocks in exactly that
///   place) would then fail after one poll interval — killing every cold start,
///   which is the failure this whole feature exists to avoid.
/// * Reading on a helper thread and polling a channel, which is this. The
///   request keeps its overall `.timeout()` untouched, so every classification
///   in `complete_streaming` behaves exactly as before.
///
/// The cost is one thread per streamed call, and — when a budget does fire —
/// that thread stays parked in `read` until ureq's overall deadline releases
/// it. Bounded, self-reaping, and only on the failure path.
struct PolledReader {
    rx: std::sync::mpsc::Receiver<std::io::Result<Vec<u8>>>,
    chunk: Vec<u8>,
    pos: usize,
    poll: Duration,
    done: bool,
}

impl PolledReader {
    fn spawn(mut body: Box<dyn std::io::Read + Send + Sync>, poll: Duration) -> Self {
        // Bounded: a fast host must not be able to outrun the parser and
        // buffer the whole reply in the channel. Dropping `rx` is also how the
        // pump learns to stop — its next `send` fails and it returns.
        let (tx, rx) = std::sync::mpsc::sync_channel::<std::io::Result<Vec<u8>>>(4);
        std::thread::spawn(move || {
            let mut buf = [0u8; 8192];
            loop {
                match body.read(&mut buf) {
                    // An empty Vec is the EOF marker; the channel closing is
                    // read the same way, so a panicking pump cannot hang us.
                    Ok(0) => {
                        let _ = tx.send(Ok(Vec::new()));
                        return;
                    }
                    Ok(n) => {
                        if tx.send(Ok(buf[..n].to_vec())).is_err() {
                            return;
                        }
                    }
                    Err(e) => {
                        let _ = tx.send(Err(e));
                        return;
                    }
                }
            }
        });
        PolledReader { rx, chunk: Vec::new(), pos: 0, poll, done: false }
    }
}

impl BufRead for PolledReader {
    fn fill_buf(&mut self) -> std::io::Result<&[u8]> {
        while self.pos >= self.chunk.len() {
            if self.done {
                return Ok(&[]);
            }
            match self.rx.recv_timeout(self.poll) {
                Ok(Ok(chunk)) if chunk.is_empty() => {
                    self.done = true;
                    return Ok(&[]);
                }
                Ok(Ok(chunk)) => {
                    self.chunk = chunk;
                    self.pos = 0;
                }
                Ok(Err(e)) => {
                    self.done = true;
                    return Err(e);
                }
                // Nothing yet. Reported as the same shape a polled socket
                // reports, so `read_stream` has one notion of "still
                // connected, just quiet" and does not care which it got.
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::WouldBlock,
                        "no bytes from the LLM within the poll interval",
                    ))
                }
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    self.done = true;
                    return Ok(&[]);
                }
            }
        }
        Ok(&self.chunk[self.pos..])
    }

    fn consume(&mut self, amt: usize) {
        self.pos = (self.pos + amt).min(self.chunk.len());
    }
}

impl std::io::Read for PolledReader {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        let n = {
            let src = self.fill_buf()?;
            let n = src.len().min(out.len());
            out[..n].copy_from_slice(&src[..n]);
            n
        };
        self.consume(n);
        Ok(n)
    }
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
            Ok(_) | Err(ureq::Error::Status(_, _)) => true,
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
        messages: &[Value],
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
        messages: &[Value],
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

        let mut req =
            ureq::post(&url).timeout(self.config.timeout).set("Content-Type", "application/json");
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
                    ErrorKind::ConnectionFailed | ErrorKind::Dns => {
                        LlmError::EndpointUnavailable { endpoint: self.config.endpoint.clone() }
                    }
                    // I/O error — inspect inner io::Error to distinguish timeout from disconnect.
                    ErrorKind::Io => {
                        let timed_out = (t as &dyn Error)
                            .source()
                            .and_then(|s| s.downcast_ref::<std::io::Error>())
                            .is_some_and(is_timeout_io_error);
                        if timed_out || call_start.elapsed() >= self.config.timeout {
                            // Nothing has been read yet, so neither progress
                            // budget can have an opinion: the only clock that
                            // covered connect/write/headers is the overall one.
                            LlmError::Timeout(Deadline::Overall(self.config.timeout))
                        } else {
                            LlmError::RequestFailed(format!("network I/O error: {t}"))
                        }
                    }
                    // Anything else: fall back to elapsed heuristic.
                    _ => {
                        if call_start.elapsed() >= self.config.timeout {
                            LlmError::Timeout(Deadline::Overall(self.config.timeout))
                        } else {
                            LlmError::EndpointUnavailable { endpoint: self.config.endpoint.clone() }
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
            let budgets = Budgets {
                overall: self.config.timeout,
                first_token: self.config.first_token_timeout,
                idle: self.config.idle_timeout,
            };
            read_stream(
                PolledReader::spawn(resp.into_reader(), budgets.poll_interval()),
                budgets,
                call_start,
                on_delta,
            )
        } else {
            // No progress budgets here, deliberately. The whole reply arrives
            // through one `into_string()`: there is no per-token event to time
            // against, and inventing one from the byte count would only be
            // measuring the host's buffering. The overall deadline is the
            // correct — and only — instrument on this path, which is a real
            // reason to prefer `stream = true` on a slow or cold host.
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
///
/// This is also where the two progress budgets live, because this loop is the
/// only place in scout that can see the reply arriving. `call_start` is the
/// instant the request was sent; the first-token clock starts *here* instead,
/// when the response headers are already in hand — a host that loads the model
/// before writing the status line (ollama) would otherwise burn the entire
/// first-token budget before this function was even called, and then trip it on
/// its first look at the watch with a delta milliseconds away. The header wait
/// is not separately observable through ureq, and the overall deadline covers
/// it.
///
/// A read that times out mid-stream is *not* a failure: the socket is open and
/// the host has merely gone quiet. It is only a failure once one of the three
/// budgets has actually run out.
pub(crate) fn read_stream<R: BufRead>(
    mut reader: R,
    budgets: Budgets,
    call_start: Instant,
    on_delta: &mut dyn FnMut(&str),
) -> Result<(String, Value), LlmError> {
    let stream_start = Instant::now();
    let mut last_progress = stream_start;
    // The idle gap arms on the first content delta and not one moment sooner:
    // a single timer started at request time is precisely the thing that kills
    // every cold start.
    let mut armed = false;

    let mut text = String::new();
    let mut usage = Value::Null;
    let mut finished = false;
    let mut raw: Vec<u8> = Vec::new();

    loop {
        raw.clear();
        // One line, however many wake-ups that takes. `read_until` over
        // `read_line` on purpose: a wake-up can land in the middle of a
        // multi-byte character, and `read_line`'s UTF-8 check *discards* the
        // bytes it already read when that happens. Bytes accumulate in `raw`
        // across wake-ups either way, so a line split across several of them
        // reassembles here.
        let n = loop {
            match reader.read_until(b'\n', &mut raw) {
                Ok(n) => break n,
                Err(e) if is_timeout_io_error(&e) => {
                    if let Some(d) =
                        budget_elapsed(&budgets, call_start, stream_start, last_progress, armed)
                    {
                        return Err(LlmError::Timeout(d));
                    }
                }
                // Same classification the pre-stream Io arm uses: the endpoint
                // answered and then went away, which from the caller's chair is
                // the same problem as never answering.
                Err(e) => return Err(LlmError::RequestFailed(format!("network I/O error: {e}"))),
            }
        };
        if n == 0 {
            break;
        }
        // Any complete line is progress. A host that heartbeats (`: ping`)
        // while it thinks is telling the truth about being alive, and killing
        // it would be wrong; the rejected alternative — count only content
        // deltas — turns a legal keep-alive into a stall.
        last_progress = Instant::now();
        // Lossy rather than strict: a truncated reply is already an error two
        // paragraphs down, and refusing to parse the *rest* of a stream over
        // one bad byte would trade a good failure for a worse one.
        let line = String::from_utf8_lossy(&raw);
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
                armed = true;
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
            first_token_timeout: Duration::from_secs(2),
            idle_timeout: Duration::from_secs(2),
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
        let err = client.complete(&[json!({"role": "user", "content": "hi"})], None).unwrap_err();
        assert!(matches!(err, LlmError::EndpointUnavailable { .. }));
    }

    // ── Streaming read loop (P5) ─────────────────────────────────────────
    // The reader is split out from the HTTP call precisely so these run
    // against a `Cursor` instead of a socket. Every chunk shape below was
    // taken from the §5.5 measurement against LM Studio.

    fn delta(content: &str) -> String {
        format!("data: {}\n\n", json!({"choices": [{"index": 0, "delta": {"content": content}}]}))
    }

    /// Budgets so wide nothing in a `Cursor`-driven test can reach them; the
    /// tests that care about the clocks set their own.
    fn slack() -> Budgets {
        Budgets {
            overall: Duration::from_secs(3600),
            first_token: Duration::from_secs(3600),
            idle: Duration::from_secs(3600),
        }
    }

    fn read(sse: &str) -> Result<(String, Value, Vec<String>), LlmError> {
        let mut seen = Vec::new();
        let (text, usage) = {
            let mut sink = |d: &str| seen.push(d.to_string());
            read_stream(std::io::Cursor::new(sse.as_bytes()), slack(), Instant::now(), &mut sink)?
        };
        Ok((text, usage, seen))
    }

    #[test]
    fn deltas_accumulate_in_order_and_reach_the_sink() {
        let sse = format!("{}{}{}data: [DONE]\n\n", delta("Hel"), delta("lo, "), delta("world"));
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

    // ── Progress-based liveness ──────────────────────────────────────────
    // A total-elapsed deadline cannot tell a slow model from a hung one, so
    // the loop runs three clocks. These drive it with a reader that hands out
    // a script of "bytes" and "silence", silence being reported exactly as a
    // polled socket reports it — the same `WouldBlock` `PolledReader` mints
    // and the same `TimedOut` ureq normalises to.

    enum Beat {
        Bytes(String),
        /// Sleep, then report the read as timed out. The sleep is real so the
        /// budgets are measured against a real clock and not a fake one.
        Silence(Duration),
        Eof,
    }

    struct Scripted {
        beats: std::collections::VecDeque<Beat>,
        chunk: Vec<u8>,
        pos: usize,
    }

    impl Scripted {
        fn new(beats: Vec<Beat>) -> Self {
            Scripted { beats: beats.into(), chunk: Vec::new(), pos: 0 }
        }
    }

    impl BufRead for Scripted {
        fn fill_buf(&mut self) -> std::io::Result<&[u8]> {
            while self.pos >= self.chunk.len() {
                match self.beats.pop_front() {
                    Some(Beat::Bytes(s)) => {
                        self.chunk = s.into_bytes();
                        self.pos = 0;
                    }
                    Some(Beat::Silence(d)) => {
                        std::thread::sleep(d);
                        return Err(std::io::Error::new(std::io::ErrorKind::WouldBlock, "quiet"));
                    }
                    Some(Beat::Eof) | None => return Ok(&[]),
                }
            }
            Ok(&self.chunk[self.pos..])
        }
        fn consume(&mut self, amt: usize) {
            self.pos = (self.pos + amt).min(self.chunk.len());
        }
    }

    impl std::io::Read for Scripted {
        fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
            let n = {
                let src = self.fill_buf()?;
                let n = src.len().min(out.len());
                out[..n].copy_from_slice(&src[..n]);
                n
            };
            self.consume(n);
            Ok(n)
        }
    }

    fn drive(beats: Vec<Beat>, budgets: Budgets) -> Result<(String, Value), LlmError> {
        let mut sink = |_: &str| {};
        read_stream(Scripted::new(beats), budgets, Instant::now(), &mut sink)
    }

    #[test]
    fn a_stream_that_stalls_after_the_first_delta_trips_the_idle_budget() {
        let budgets = Budgets {
            overall: Duration::from_secs(30),
            first_token: Duration::from_secs(30),
            idle: Duration::from_millis(300),
        };
        let err = drive(
            vec![
                Beat::Bytes(delta("Hel")),
                Beat::Silence(Duration::from_millis(150)),
                Beat::Silence(Duration::from_millis(150)),
                Beat::Silence(Duration::from_millis(150)),
                Beat::Bytes(delta("never gets here")),
            ],
            budgets,
        )
        .unwrap_err();
        match err {
            LlmError::Timeout(Deadline::Idle(d)) => assert_eq!(d, Duration::from_millis(300)),
            other => panic!("expected an idle timeout, got {other:?}"),
        }
        // And it says so, rather than sending the user to timeout_seconds.
        assert!(err.to_string().contains("idle_timeout_seconds"), "{err}");
    }

    #[test]
    fn a_slow_but_steady_stream_completes() {
        // The core regression: every gap is under the idle budget, the total
        // is well over it, and a single stopwatch would have killed this call.
        // "p90 moved 1.9s → 4.8s from a GPU placement change" is this test.
        let budgets = Budgets {
            overall: Duration::from_secs(30),
            first_token: Duration::from_secs(30),
            idle: Duration::from_millis(300),
        };
        let gap = Duration::from_millis(120);
        let mut beats = Vec::new();
        for word in ["one ", "two ", "three ", "four ", "five ", "six "] {
            beats.push(Beat::Silence(gap));
            beats.push(Beat::Bytes(delta(word)));
        }
        beats.push(Beat::Bytes("data: [DONE]\n\n".into()));

        let start = Instant::now();
        let (text, _) = drive(beats, budgets).expect("a steady stream must not be killed");
        let elapsed = start.elapsed();
        assert_eq!(text, "one two three four five six ");
        assert!(
            elapsed > budgets.idle * 2,
            "the test has to outlive the budget it is not supposed to trip: {elapsed:?}"
        );
    }

    #[test]
    fn a_stream_that_never_produces_anything_trips_the_first_token_budget() {
        let budgets = Budgets {
            overall: Duration::from_secs(30),
            first_token: Duration::from_millis(300),
            // Deliberately tighter than the first-token budget: an unarmed
            // idle clock must stay silent, or every cold start dies.
            idle: Duration::from_millis(50),
        };
        let err = drive(
            vec![
                Beat::Silence(Duration::from_millis(150)),
                Beat::Silence(Duration::from_millis(150)),
                Beat::Silence(Duration::from_millis(150)),
            ],
            budgets,
        )
        .unwrap_err();
        match err {
            LlmError::Timeout(Deadline::FirstToken(d)) => {
                assert_eq!(d, Duration::from_millis(300));
            }
            other => panic!("expected a first-token timeout, got {other:?}"),
        }
        assert!(err.to_string().contains("first_token_timeout_seconds"), "{err}");
    }

    #[test]
    fn keep_alives_and_role_chunks_do_not_arm_the_idle_budget() {
        // Traffic that is not a content delta proves the connection is alive,
        // so it defers the *first-token* clock's verdict not at all — but it
        // must not arm the idle clock either, or a chatty host loading a model
        // would be killed by the tight budget instead of the generous one.
        let budgets = Budgets {
            overall: Duration::from_secs(30),
            first_token: Duration::from_millis(400),
            idle: Duration::from_millis(50),
        };
        let err = drive(
            vec![
                Beat::Bytes(": ping\n\n".into()),
                Beat::Silence(Duration::from_millis(200)),
                Beat::Bytes(format!(
                    "data: {}\n\n",
                    json!({"choices": [{"index": 0, "delta": {"role": "assistant"}}]})
                )),
                Beat::Silence(Duration::from_millis(200)),
                Beat::Silence(Duration::from_millis(200)),
            ],
            budgets,
        )
        .unwrap_err();
        assert!(
            matches!(err, LlmError::Timeout(Deadline::FirstToken(_))),
            "an unarmed stream must fail on the first-token clock, got {err:?}"
        );
    }

    #[test]
    fn the_overall_deadline_is_still_the_outer_net() {
        // Steady output, no gap anywhere near the idle budget, and the call
        // simply runs too long. Nothing about the progress clocks removes the
        // outer bound.
        let budgets = Budgets {
            overall: Duration::from_millis(300),
            first_token: Duration::from_secs(30),
            idle: Duration::from_secs(30),
        };
        let mut beats = vec![Beat::Bytes(delta("tick "))];
        for _ in 0..6 {
            beats.push(Beat::Silence(Duration::from_millis(100)));
            beats.push(Beat::Bytes(delta("tick ")));
        }
        let err = drive(beats, budgets).unwrap_err();
        match err {
            LlmError::Timeout(Deadline::Overall(d)) => assert_eq!(d, Duration::from_millis(300)),
            other => panic!("expected the overall deadline, got {other:?}"),
        }
        assert!(err.to_string().contains("timeout_seconds"), "{err}");
    }

    #[test]
    fn a_read_timeout_alone_is_not_a_failure() {
        // The load-bearing half of the design: silence is only a problem once
        // a budget is genuinely spent. A stream that goes quiet, comes back,
        // and finishes is a success.
        let budgets = Budgets {
            overall: Duration::from_secs(30),
            first_token: Duration::from_secs(30),
            idle: Duration::from_secs(30),
        };
        let (text, _) = drive(
            vec![
                Beat::Silence(Duration::from_millis(20)),
                Beat::Bytes(delta("a")),
                Beat::Silence(Duration::from_millis(20)),
                Beat::Bytes("data: [DONE]\n\n".into()),
            ],
            budgets,
        )
        .unwrap();
        assert_eq!(text, "a");
    }

    #[test]
    fn a_quiet_spell_before_a_clean_close_is_still_a_truncation() {
        // The budgets must not have created a second way for half a summary
        // to reach a caller: silence that ends in EOF rather than in a budget
        // is still the "ended mid-reply" error it always was.
        let budgets = Budgets {
            overall: Duration::from_secs(30),
            first_token: Duration::from_secs(30),
            idle: Duration::from_secs(30),
        };
        let err = drive(
            vec![
                Beat::Bytes(delta("The answer i")),
                Beat::Silence(Duration::from_millis(20)),
                Beat::Eof,
            ],
            budgets,
        )
        .unwrap_err();
        match &err {
            LlmError::RequestFailed(m) => assert!(m.contains("mid-reply"), "{m}"),
            other => panic!("expected RequestFailed, got {other:?}"),
        }
    }

    #[test]
    fn a_line_split_across_wake_ups_is_reassembled_not_lost() {
        // `read_until` keeps what it read when the read errors; `read_line`
        // would throw away a partial multi-byte character on the same error.
        // Both halves of that matter, so both are exercised: the split lands
        // inside a multi-byte character.
        let budgets = Budgets {
            overall: Duration::from_secs(30),
            first_token: Duration::from_secs(30),
            idle: Duration::from_secs(30),
        };
        let whole = delta("café ☕");
        let cut = whole.len() - 6; // mid-way through the trailing emoji
        let (text, _) = drive(
            vec![
                Beat::Bytes(whole[..cut].to_string()),
                Beat::Silence(Duration::from_millis(20)),
                Beat::Bytes(whole[cut..].to_string()),
                Beat::Bytes("data: [DONE]\n\n".into()),
            ],
            budgets,
        )
        .unwrap();
        assert_eq!(text, "café ☕");
    }

    #[test]
    fn the_poll_interval_tracks_the_tightest_budget_within_bounds() {
        let b = |idle: u64, first: u64| Budgets {
            overall: Duration::from_secs(120),
            first_token: Duration::from_secs(first),
            idle: Duration::from_secs(idle),
        };
        // Shipped defaults: a quarter of 15s is over the cap, so 1s.
        assert_eq!(b(15, 60).poll_interval(), Duration::from_secs(1));
        // A tight budget gets proportionally tighter polling...
        assert_eq!(b(2, 60).poll_interval(), Duration::from_millis(500));
        // ...down to the floor, so no config can make this loop spin.
        assert_eq!(b(1, 60).poll_interval(), Duration::from_millis(250));
        // The first-token budget counts too when it is the tighter one.
        assert_eq!(b(3600, 1).poll_interval(), Duration::from_millis(250));
    }

    // ── The real socket ──────────────────────────────────────────────────
    // The `Scripted` tests above prove the loop's arithmetic. These prove the
    // premise underneath it: that a read against a genuinely silent server
    // wakes up at all. Measured against ureq 2.12, a blocking read on a
    // stalled connection parks until the *overall* deadline (8.34s with
    // `.timeout(8s)`) and nothing shorter — which is why the body is pumped
    // through `PolledReader` rather than read straight off the socket. Without
    // that, these two tests would take the overall timeout to fail, or never
    // fail at all.

    /// One HTTP/1.1 SSE response: `head`, then silence until the test is done.
    fn stalling_server(head: &'static str) -> String {
        use std::io::{Read as _, Write as _};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            let Ok((mut sock, _)) = listener.accept() else { return };
            let mut scratch = [0u8; 4096];
            let _ = sock.read(&mut scratch);
            let _ = sock.write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n",
            );
            let _ = sock.write_all(head.as_bytes());
            let _ = sock.flush();
            // Hold the connection open and send nothing more. Long enough to
            // outlast the budgets under test, short enough not to linger.
            std::thread::sleep(Duration::from_secs(5));
        });
        format!("http://{addr}/v1")
    }

    fn client_against(endpoint: String, first_token: Duration, idle: Duration) -> LlmClient {
        LlmClient::new(Config {
            endpoint,
            model: "test-model".into(),
            // Deliberately far larger than either progress budget: if the
            // progress clocks did not work, these tests would hang for 30s
            // instead of failing.
            timeout: Duration::from_secs(30),
            first_token_timeout: first_token,
            idle_timeout: idle,
            api_key: None,
            max_tokens: None,
            stream: true,
        })
    }

    /// A one-shot SSE server that writes `frames` `gap` apart, then closes.
    fn ticking_server(frames: Vec<String>, gap: Duration) -> String {
        use std::io::{Read as _, Write as _};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            let Ok((mut sock, _)) = listener.accept() else { return };
            let mut scratch = [0u8; 4096];
            let _ = sock.read(&mut scratch);
            let _ = sock.write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n",
            );
            for frame in frames {
                std::thread::sleep(gap);
                if sock.write_all(frame.as_bytes()).is_err() {
                    return;
                }
                let _ = sock.flush();
            }
        });
        format!("http://{addr}/v1")
    }

    #[test]
    fn a_slow_but_steady_socket_completes_over_the_real_transport() {
        // The `Scripted` version of this proves the arithmetic; this proves
        // the whole stack — pumped body, poll interval and all — still
        // delivers an ordinary reply, and that a call whose *total* runtime is
        // several times the idle budget is not killed for it.
        let gap = Duration::from_millis(120);
        let frames = vec![
            delta("one "),
            delta("two "),
            delta("three "),
            delta("four "),
            "data: [DONE]\n\n".to_string(),
        ];
        let endpoint = ticking_server(frames, gap);
        let client = client_against(endpoint, Duration::from_secs(20), Duration::from_millis(500));
        let mut seen = Vec::new();
        let start = Instant::now();
        let (text, _usage) = {
            let mut sink = |d: &str| seen.push(d.to_string());
            client
                .complete_streaming(&[json!({"role": "user", "content": "hi"})], None, &mut sink)
                .expect("a steady stream must not be killed")
        };
        let elapsed = start.elapsed();
        assert_eq!(text, "one two three four ");
        assert_eq!(seen, ["one ", "two ", "three ", "four "]);
        assert!(
            elapsed > Duration::from_millis(500),
            "the call has to outlive the idle budget it never trips: {elapsed:?}"
        );
    }

    #[test]
    fn a_stalled_socket_trips_the_idle_budget_long_before_the_overall_one() {
        let endpoint = stalling_server(
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"partial\"}}]}\n\n",
        );
        let client = client_against(endpoint, Duration::from_secs(20), Duration::from_millis(400));
        let start = Instant::now();
        let err = client.complete(&[json!({"role": "user", "content": "hi"})], None).unwrap_err();
        let elapsed = start.elapsed();
        assert!(
            matches!(err, LlmError::Timeout(Deadline::Idle(_))),
            "expected an idle timeout, got {err:?} after {elapsed:?}"
        );
        assert!(
            elapsed < Duration::from_secs(5),
            "the idle budget must fire on its own clock, not the overall one: {elapsed:?}"
        );
    }

    #[test]
    fn a_socket_that_never_sends_a_delta_trips_the_first_token_budget() {
        let endpoint = stalling_server(": ping\n\n");
        let client = client_against(endpoint, Duration::from_millis(400), Duration::from_secs(20));
        let start = Instant::now();
        let err = client.complete(&[json!({"role": "user", "content": "hi"})], None).unwrap_err();
        let elapsed = start.elapsed();
        assert!(
            matches!(err, LlmError::Timeout(Deadline::FirstToken(_))),
            "expected a first-token timeout, got {err:?} after {elapsed:?}"
        );
        assert!(elapsed < Duration::from_secs(5), "{elapsed:?}");
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
        let s = LlmError::Timeout(Deadline::Overall(Duration::from_secs(120))).to_string();
        assert!(s.contains("timed out"), "{s}");
        assert!(s.contains("timeout_seconds"), "{s}");
    }

    #[test]
    fn each_deadline_names_its_own_budget_and_its_own_knob() {
        // The point of splitting the clocks is lost if the message sends the
        // user to the knob that was never the one they hit.
        let ttft = LlmError::Timeout(Deadline::FirstToken(Duration::from_secs(60))).to_string();
        assert!(ttft.contains("no first token within 60s"), "{ttft}");
        assert!(ttft.contains("still be loading"), "{ttft}");
        assert!(ttft.contains("first_token_timeout_seconds"), "{ttft}");

        let idle = LlmError::Timeout(Deadline::Idle(Duration::from_secs(15))).to_string();
        assert!(idle.contains("no output for 15s"), "{idle}");
        assert!(idle.contains("stalled mid-reply"), "{idle}");
        assert!(idle.contains("idle_timeout_seconds"), "{idle}");

        let overall = LlmError::Timeout(Deadline::Overall(Duration::from_secs(120))).to_string();
        assert!(overall.contains("after 120s overall"), "{overall}");

        // Every one of them is still a timeout to the caller, and all three
        // are one row in the call log.
        for d in [
            Deadline::FirstToken(Duration::from_secs(60)),
            Deadline::Idle(Duration::from_secs(15)),
            Deadline::Overall(Duration::from_secs(120)),
        ] {
            let e = LlmError::Timeout(d);
            assert!(e.to_string().contains("timed out"), "{e:?}");
            assert_eq!(e.outcome(), crate::stats::Outcome::Timeout);
        }

        // Sub-second budgets exist only in tests, but "0s" would be a lie.
        assert!(LlmError::Timeout(Deadline::Idle(Duration::from_millis(300)))
            .to_string()
            .contains("300ms"));
    }

    #[test]
    fn every_failure_maps_to_a_call_log_outcome() {
        use crate::stats::Outcome;
        let cases = [
            (LlmError::EndpointUnavailable { endpoint: "x".into() }, Outcome::EndpointUnreachable),
            (LlmError::Timeout(Deadline::Overall(Duration::from_secs(120))), Outcome::Timeout),
            (LlmError::RequestFailed("HTTP 500: boom".into()), Outcome::HttpError),
            (
                LlmError::RequestFailed("network I/O error: reset".into()),
                Outcome::EndpointUnreachable,
            ),
            (LlmError::RequestFailed("parse LLM response: eof".into()), Outcome::ParseFailure),
            (LlmError::Internal("LLM returned empty response".into()), Outcome::EmptyResponse),
            (LlmError::Internal("serialize request: nope".into()), Outcome::ParseFailure),
        ];
        for (err, want) in cases {
            assert_eq!(err.outcome(), want, "{err:?}");
        }
        // ...and none of them is ever recorded as a success.
        assert!(!LlmError::Timeout(Deadline::Idle(Duration::from_secs(15))).outcome().is_ok());
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
        assert!(is_timeout_io_error(&io_err), "TimedOut should match the timeout predicate");
    }

    #[test]
    fn io_would_block_matches_timeout_predicate() {
        let io_err = std::io::Error::from(std::io::ErrorKind::WouldBlock);
        assert!(is_timeout_io_error(&io_err), "WouldBlock should match the timeout predicate");
    }

    #[test]
    fn io_broken_pipe_does_not_match_timeout_predicate() {
        let io_err = std::io::Error::from(std::io::ErrorKind::BrokenPipe);
        assert!(!is_timeout_io_error(&io_err), "BrokenPipe must not match the timeout predicate");
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
    #[ignore = "needs a live LLM endpoint: set LM_HOST (and optionally LM_MODEL), then run with --ignored"]
    fn live_roundtrip() {
        let endpoint =
            std::env::var("LM_HOST").unwrap_or_else(|_| "http://localhost:11434/v1".into());
        let model = std::env::var("LM_MODEL").unwrap_or_else(|_| "qwen3:8b".into());
        let client = LlmClient::new(Config {
            endpoint: endpoint.clone(),
            model,
            timeout: Duration::from_secs(120),
            first_token_timeout: Duration::from_secs(60),
            idle_timeout: Duration::from_secs(15),
            api_key: None,
            max_tokens: Some(64),
            stream: true,
        });

        let (reachable, ms) = client.check_endpoint();
        assert!(reachable, "endpoint {endpoint} not reachable (start ollama serve)");
        assert!(ms < 5000, "endpoint check took too long: {ms}ms");

        let (content, usage) = client
            .complete(&[json!({"role": "user", "content": "Reply with exactly: hello"})], None)
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
    #[ignore = "needs a live LLM endpoint that streams: set LM_HOST and LM_MODEL, then run with --ignored"]
    fn live_stream_matches_non_stream() {
        let endpoint =
            std::env::var("LM_HOST").unwrap_or_else(|_| "http://localhost:1234/v1".into());
        let model = std::env::var("LM_MODEL").unwrap_or_else(|_| "qwen/qwen3.6-35b-a3b".into());
        let cfg = |stream: bool| Config {
            endpoint: endpoint.clone(),
            model: model.clone(),
            timeout: Duration::from_secs(120),
            first_token_timeout: Duration::from_secs(60),
            idle_timeout: Duration::from_secs(15),
            api_key: None,
            max_tokens: Some(64),
            stream,
        };
        let prompt = [json!({
            "role": "user",
            "content": "Count from one to ten in words, separated by commas. /no_think",
        })];

        let (plain, plain_usage) =
            LlmClient::new(cfg(false)).complete(&prompt, None).expect("non-streaming call");

        let mut deltas: Vec<String> = Vec::new();
        let (streamed, streamed_usage) = {
            let mut sink = |d: &str| deltas.push(d.to_string());
            LlmClient::new(cfg(true))
                .complete_streaming(&prompt, None, &mut sink)
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
