//! `examples/30-signaling-room` against `doc/DRT-Signalling.md`: the
//! example's own program and config, run by the real `drt start` on a port
//! of the test's, and asked what its demo does not show. The demo is the
//! happy path; these are the refusals of §2's status table and the
//! reconnect of §5.
//!
//! ## surface block
//!
//! - Entry points: the `#[test]`s below, one per part of the profile.
//! - Configurable: [`WITHIN`], how long any one answer may take.
//! - Fan-out: [`Server`] is `drt start` on the example; [`call`] is one
//!   HTTP request and its whole response; [`held`] is a caller waiting on
//!   its call in a thread of its own.

#![cfg(all(feature = "listen", feature = "connector-time"))]

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::thread::JoinHandle;
use std::time::Duration;

const WITHIN: Duration = Duration::from_secs(10);

/// `drt start` on the example's config, its address moved to a free port.
struct Server {
    child: Child,
    addr: SocketAddr,
    name: String,
    answerer: String,
    caller: String,
    _dir: tempfile::TempDir,
}

impl Server {
    fn start() -> Server {
        let example = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../examples/30-signaling-room")
            .canonicalize()
            .unwrap();
        let mut config: serde_json::Value =
            serde_json::from_slice(&std::fs::read(example.join("app.json")).unwrap()).unwrap();
        config["program"]["path"] = example.join("app.dlua").to_string_lossy().into();
        config["listeners"][0]["address"] = "127.0.0.1:0".into();
        let args = config["args"].clone();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("room.json");
        std::fs::write(&path, config.to_string()).unwrap();
        let mut child = Command::new(env!("CARGO_BIN_EXE_drt"))
            .arg("--config")
            .arg(&path)
            .arg("start")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        // The listener says where it bound; that line is the port.
        let mut lines = BufReader::new(child.stderr.take().unwrap()).lines();
        let addr = loop {
            let line = lines.next().expect("drt start ended first").unwrap();
            if let Some(at) = line.strip_prefix("drt start: http listening on ") {
                break at.trim().parse().unwrap();
            }
        };
        std::thread::spawn(move || for _ in lines {});
        let text = |k: &str| args[k].as_str().unwrap().to_string();
        Server {
            child,
            addr,
            name: text("name"),
            answerer: text("answerer_token"),
            caller: text("caller_token"),
            _dir: dir,
        }
    }

    fn base(&self) -> String {
        format!("/v1/{}", self.name)
    }

    /// One poll, which also makes the answerer present (§4.2).
    fn poll(&self, since: &str) -> serde_json::Value {
        let (status, _, body) = call(
            self.addr,
            "GET",
            &format!("{}/calls?since={since}&k={}", self.base(), self.answerer),
            "",
            &[],
        );
        assert_eq!(status, 200, "{body}");
        serde_json::from_str(&body).unwrap()
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// One request; the status, the head and the body.
fn call(
    addr: SocketAddr,
    method: &str,
    target: &str,
    body: &str,
    headers: &[(&str, &str)],
) -> (u16, String, String) {
    let mut conn = TcpStream::connect(addr).unwrap();
    conn.set_read_timeout(Some(WITHIN + WITHIN)).unwrap();
    let mut request = format!(
        "{method} {target} HTTP/1.1\r\nHost: t\r\nContent-Length: {}\r\n",
        body.len()
    );
    for (name, value) in headers {
        request.push_str(&format!("{name}: {value}\r\n"));
    }
    request.push_str("\r\n");
    request.push_str(body);
    conn.write_all(request.as_bytes()).unwrap();
    let mut response = String::new();
    conn.read_to_string(&mut response).unwrap();
    let (head, body) = response.split_once("\r\n\r\n").unwrap();
    let status = head.split_whitespace().nth(1).unwrap().parse().unwrap();
    (status, head.to_lowercase(), body.to_string())
}

/// A caller whose call is held: its answer, once there is one.
fn held(server: &Server, record: &str) -> JoinHandle<(u16, String, String)> {
    let (addr, record) = (server.addr, record.to_string());
    let target = format!("{}/calls?k={}", server.base(), server.caller);
    let handle = std::thread::spawn(move || call(addr, "POST", &target, &record, &[]));
    // Long enough for the call to be in the room before the test goes on.
    std::thread::sleep(Duration::from_millis(300));
    handle
}

#[test]
fn each_token_may_do_only_its_own_requests() {
    let s = Server::start();
    let calls = format!("{}/calls", s.base());
    // No token, and a token the server does not know: 401.
    assert_eq!(call(s.addr, "GET", &calls, "", &[]).0, 401);
    assert_eq!(
        call(s.addr, "GET", &format!("{calls}?k=nobody"), "", &[]).0,
        401
    );
    // The caller's token may not read calls, and the answerer's may not call.
    assert_eq!(
        call(s.addr, "GET", &format!("{calls}?k={}", s.caller), "", &[]).0,
        403
    );
    assert_eq!(
        call(
            s.addr,
            "POST",
            &format!("{calls}?k={}", s.answerer),
            "{}",
            &[]
        )
        .0,
        403
    );
    // A bearer header does what `?k=` does.
    let bearer = format!("Bearer {}", s.answerer);
    assert_eq!(
        call(s.addr, "GET", &calls, "", &[("Authorization", &bearer)]).0,
        200
    );
    // A name the server does not serve, with a good token: 404.
    let (status, _, body) = call(s.addr, "GET", "/v1/other/calls?k=x", "", &[]);
    assert_eq!(status, 404, "{body}");
    // A preflight is answered for anyone, with what a browser asks for.
    let (status, head, _) = call(s.addr, "OPTIONS", &calls, "", &[]);
    assert_eq!(status, 204);
    assert!(
        head.contains("access-control-allow-headers: authorization"),
        "{head}"
    );
}

#[test]
fn a_withdrawn_call_tells_its_caller_410() {
    let s = Server::start();
    s.poll("0");
    let caller = held(&s, r#"{"caller":"one"}"#);
    let got = s.poll("0");
    assert_eq!(got["calls"][0]["id"], "c1", "{got}");
    let one = format!("{}/calls/c1?k={}", s.base(), s.answerer);
    assert_eq!(call(s.addr, "DELETE", &one, "", &[]).0, 204);
    let (status, _, body) = caller.join().unwrap();
    assert_eq!(status, 410, "{body}");
    // Gone now: withdrawing or answering it again finds nothing.
    assert_eq!(call(s.addr, "DELETE", &one, "", &[]).0, 404);
    let answer = format!("{}/calls/c1/answer?k={}", s.base(), s.answerer);
    assert_eq!(call(s.addr, "POST", &answer, "{}", &[]).0, 404);
    // The cursor moved past it, so a poll from there is empty.
    assert_eq!(s.poll("1")["calls"], serde_json::json!([]));
}

#[test]
fn malformed_and_oversized_requests_are_refused() {
    let s = Server::start();
    s.poll("0");
    let calls = format!("{}/calls", s.base());
    let (status, _, body) = call(
        s.addr,
        "GET",
        &format!("{calls}?since=x&k={}", s.answerer),
        "",
        &[],
    );
    assert_eq!(status, 400, "{body}");
    // Over a record's 512 bytes and under the listener's 1 KiB, so the
    // refusal is the program's. Past 1 KiB the listener refuses first.
    let big = "x".repeat(600);
    let (status, _, body) = call(
        s.addr,
        "POST",
        &format!("{calls}?k={}", s.caller),
        &big,
        &[],
    );
    assert_eq!(status, 413, "{body}");
    assert!(body.contains("512"), "{body}");
}

#[test]
fn the_seventeenth_waiting_call_is_429_with_retry_after() {
    let s = Server::start();
    s.poll("0");
    let callers: Vec<_> = (0..16)
        .map(|i| held(&s, &format!("{{\"n\":{i}}}")))
        .collect();
    let (status, head, body) = call(
        s.addr,
        "POST",
        &format!("{}/calls?k={}", s.base(), s.caller),
        "{}",
        &[],
    );
    assert_eq!(status, 429, "{body}");
    assert!(head.contains("retry-after: "), "{head}");
    assert_eq!(s.poll("0")["calls"].as_array().unwrap().len(), 16);
    drop(s);
    for c in callers {
        let _ = c.join();
    }
}

#[test]
fn a_reconnecting_stream_is_told_at_once_of_calls_it_missed() {
    let s = Server::start();
    s.poll("0");
    let _caller = held(&s, r#"{"caller":"one"}"#);
    // An EventSource that had seen nothing sends Last-Event-ID: 0.
    let mut stream = TcpStream::connect(s.addr).unwrap();
    stream.set_read_timeout(Some(WITHIN)).unwrap();
    write!(
        stream,
        "GET {}/events?k={} HTTP/1.1\r\nHost: t\r\nLast-Event-ID: 0\r\n\r\n",
        s.base(),
        s.answerer
    )
    .unwrap();
    let mut got = Vec::new();
    let mut buf = [0u8; 1024];
    while !String::from_utf8_lossy(&got).contains("data: {\"cursor\":\"1\"}") {
        let n = stream.read(&mut buf).unwrap();
        assert!(n > 0, "closed: {}", String::from_utf8_lossy(&got));
        got.extend_from_slice(&buf[..n]);
    }
    let got = String::from_utf8(got).unwrap();
    assert!(got.contains("Content-Type: text/event-stream"), "{got}");
    assert!(got.contains("retry: 2000"), "{got}");
    assert!(got.contains("id: 1\nevent: call\n"), "{got}");
    // Holding the stream is being present: no poll for this second call.
    let _second = held(&s, r#"{"caller":"two"}"#);
    let mut more = Vec::new();
    while !String::from_utf8_lossy(&more).contains("id: 2\n") {
        let n = stream.read(&mut buf).unwrap();
        assert!(n > 0, "closed: {}", String::from_utf8_lossy(&more));
        more.extend_from_slice(&buf[..n]);
    }
}
