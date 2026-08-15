// End-to-end test for `scout task` — the generic escape hatch.
//
// It exists for one thing the unit tests cannot reach: whether the reply is
// read *as it arrives*. `scout task` used to be the odd one out. The filters
// and `scout run` both wrapped their call in `live::with_token_stream` and
// asked for `complete_streaming`; `task` called plain `complete`, so no sink
// was ever installed and the dashboard's response pane stayed empty for the
// whole call and then filled at the end. Nothing decided that — `task.rs` was
// simply the oldest of the three copies of the round-trip, written before the
// token stream existed.
//
// The only honest way to see the difference is from outside the process: a
// socket the test owns, a real subprocess writing to it, and an event stream
// arriving in pieces. `call.token` events are emitted only by the streaming
// path, so their presence *is* the assertion.

mod support;

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::time::Duration;

use support::Sandbox;

/// The reply, in the pieces the fake host sends it in.
const PIECES: [&str; 4] = ["The ", "sky ", "is ", "blue."];

/// Wide enough that a `call.token` window (50 ms) closes between pieces, so
/// this really is a reply arriving over time rather than one flush.
const GAP: Duration = Duration::from_millis(80);

/// Read one whole HTTP request off `sock` — headers, then exactly the body
/// `Content-Length` promises.
///
/// Replying before the request has been consumed is what makes a naive
/// one-shot server flaky: closing with unread bytes still in the receive queue
/// sends an RST, and whatever the client had already buffered goes with it.
fn read_request(sock: &mut TcpStream) {
    let mut buf = Vec::new();
    let mut scratch = [0u8; 4096];
    loop {
        if let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&buf[..end]).to_lowercase();
            let len: usize = head
                .lines()
                .find_map(|l| l.strip_prefix("content-length:"))
                .and_then(|v| v.trim().parse().ok())
                .unwrap_or(0);
            if buf.len() >= end + 4 + len {
                return;
            }
        }
        match sock.read(&mut scratch) {
            Ok(0) | Err(_) => return,
            Ok(n) => buf.extend_from_slice(&scratch[..n]),
        }
    }
}

/// A one-shot OpenAI-compatible host that dribbles `PIECES` out as SSE.
/// Returns the base URL to point scout's config at.
fn drip_feeding_server() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind a fake LLM host");
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        let Ok((mut sock, _)) = listener.accept() else { return };
        read_request(&mut sock);
        let _ = sock.write_all(
            b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n",
        );
        let _ = sock.flush();
        for piece in PIECES {
            std::thread::sleep(GAP);
            let frame = format!(
                "data: {{\"choices\":[{{\"index\":0,\"delta\":{{\"content\":\"{piece}\"}}}}]}}\n\n"
            );
            if sock.write_all(frame.as_bytes()).is_err() {
                return;
            }
            let _ = sock.flush();
        }
        let _ = sock.write_all(b"data: [DONE]\n\n");
        let _ = sock.flush();
    });
    format!("http://{addr}/v1")
}

/// Bind the live channel's socket so the run under test has a listener, which
/// is what makes the token sink more than a no-op.
///
/// Non-blocking `accept`, because nothing here waits for a writer: the child
/// has come and gone before anything is read, so its connection is already
/// queued and `accept` either has it or there is nothing to have.
fn bind_live_socket(path: &Path) -> UnixListener {
    let sock = UnixListener::bind(path).expect("bind the live socket");
    sock.set_nonblocking(true).expect("non-blocking accept");
    sock
}

/// Every event the run sent, in arrival order.
///
/// The channel is a unix *stream* carrying length-prefixed frames (see
/// `live.rs`), so this is the daemon's read path in miniature: take each
/// connection the child made and read frames off it until EOF.
fn drain(listener: &UnixListener) -> Vec<serde_json::Value> {
    let mut events = Vec::new();
    while let Ok((mut conn, _)) = listener.accept() {
        // macOS accepts with the listener's `O_NONBLOCK`; Linux does not.
        conn.set_nonblocking(false).expect("blocking reads");
        conn.set_read_timeout(Some(Duration::from_millis(500))).expect("read timeout");
        while let Some(frame) = next_frame(&mut conn) {
            if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&frame) {
                events.push(v);
            }
        }
    }
    events
}

/// One frame: a 4-byte little-endian length, then that many bytes. `None` at
/// EOF or on anything that does not look like a frame.
fn next_frame(conn: &mut UnixStream) -> Option<Vec<u8>> {
    let mut header = [0u8; 4];
    conn.read_exact(&mut header).ok()?;
    let len = u32::from_le_bytes(header) as usize;
    if len == 0 || len > 64 * 1024 {
        return None;
    }
    let mut buf = vec![0u8; len];
    conn.read_exact(&mut buf).ok()?;
    Some(buf)
}

fn kind<'a>(events: &'a [serde_json::Value], want: &str) -> Vec<&'a serde_json::Value> {
    events.iter().filter(|e| e["kind"] == want).collect()
}

#[test]
fn task_streams_the_reply_as_it_arrives() {
    let sandbox = Sandbox::new();
    let endpoint = drip_feeding_server();
    let config = sandbox.root().join("config.toml");
    std::fs::write(
        &config,
        format!(
            "[llm]\nendpoint = \"{endpoint}\"\nmodel = \"test-model\"\n\
             stream = true\ntimeout_seconds = 60\n\
             first_token_timeout_seconds = 30\nidle_timeout_seconds = 30\n"
        ),
    )
    .expect("write config");

    let sock_path = sandbox.root().join("live.sock");
    let live = bind_live_socket(&sock_path);

    let out = sandbox
        .scout()
        .args(["task", "why is the sky blue"])
        .env("SCOUT_CONFIG", &config)
        .env("SCOUT_LIVE_SOCK", &sock_path)
        .output()
        .expect("run scout task");

    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    assert_eq!(out.status.code(), Some(0), "scout task should have succeeded\nstderr: {stderr}");
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim(),
        PIECES.concat(),
        "the reply reaches stdout whole, however it was read"
    );

    let events = drain(&live);
    let tokens = kind(&events, "call.token");
    assert!(
        !tokens.is_empty(),
        "no call.token: the reply was read in one piece, not streamed. events: {:?}",
        events.iter().map(|e| e["kind"].clone()).collect::<Vec<_>>()
    );

    // The pieces the sink saw have to add up to the reply — a stream that
    // arrives but loses text is worse than one that never arrived.
    let streamed: String =
        tokens.iter().filter_map(|e| e["text"].as_str()).collect::<Vec<_>>().concat();
    assert_eq!(streamed, PIECES.concat(), "streamed text differs from the reply");

    // And the P3 invariant the unification had to preserve: one record per
    // invocation, so start, tokens and end all carry the same `id`.
    let start = kind(&events, "call.start");
    let end = kind(&events, "call.end");
    assert_eq!(start.len(), 1, "one call.start per invocation");
    assert_eq!(end.len(), 1, "one call.end per invocation");
    let id = &start[0]["id"];
    assert_eq!(&end[0]["id"], id, "call.end must reconcile with call.start");
    for t in &tokens {
        assert_eq!(&t["id"], id, "every token belongs to the one row");
    }
}
