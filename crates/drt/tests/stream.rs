//! Streamed responses from the http listener, end to end over a socket,
//! through both acceptors: the threaded one natively, the polled one that
//! wasi serves with.

#![cfg(feature = "listen")]

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::{Duration, Instant};

use drt::listen::Acceptor;
use drt::start;
use drt_config::RootConfig;
use drt_connector::{Dispatcher, Registry};

/// Streams on `/events`, `/hold` and `/flood` (6.4 MB at once), sends a chunk with no head on
/// `/early`, and answers `/closed` with the reason of the last closed
/// notice it was given.
const STREAMER: &str = "\
    local q   = queue.declare('http_in',  {capacity = 8})\n\
    local out = queue.declare('http_out', {capacity = 256, exported = true})\n\
    local closed = 'none'\n\
    while true do\n\
      local id, req = queue.wait({q})\n\
      if req.event == 'closed' then\n\
        closed = req.reason\n\
      elseif req.path == '/events' then\n\
        queue.push(out, {conn = req.conn, stream = true,\n\
                         content_type = 'text/event-stream', body = 'data: 1\\n\\n'})\n\
        queue.push(out, {conn = req.conn, chunk = 'data: 2\\n\\n'})\n\
        queue.push(out, {conn = req.conn, chunk = 'data: 3\\n\\n', done = true})\n\
      elseif req.path == '/hold' then\n\
        queue.push(out, {conn = req.conn, stream = true,\n\
                         content_type = 'text/plain', body = 'held\\n'})\n\
      elseif req.path == '/flood' then\n\
        queue.push(out, {conn = req.conn, stream = true, content_type = 'text/plain'})\n\
        local block = string.rep('x', 32768)\n\
        for i = 1, 200 do queue.push(out, {conn = req.conn, chunk = block}) end\n\
      elseif req.path == '/early' then\n\
        queue.push(out, {conn = req.conn, chunk = 'x'})\n\
      elseif req.path == '/closed' then\n\
        queue.push(out, {conn = req.conn, content_type = 'text/plain', body = closed})\n\
      end\n\
    end\n";

fn served(polled: bool, listener_json: &str) -> SocketAddr {
    let program_json = serde_json::to_string(STREAMER).unwrap();
    let cfg: RootConfig = serde_json::from_str(&format!(
        r#"{{"program": {{"source": {program_json}}}, "listeners": [{listener_json}]}}"#
    ))
    .unwrap();
    if polled {
        let bound = drt::listen::polled::Bound::bind(&cfg.listeners).unwrap();
        let addr = bound.addrs()[0];
        std::thread::spawn(move || {
            let _ = start::serve(&cfg, Dispatcher::new(Registry::new()), bound);
        });
        addr
    } else {
        let bound = drt::listen::bind(&cfg.listeners).unwrap();
        let addr = bound.addrs()[0];
        std::thread::spawn(move || {
            let _ = start::serve(&cfg, Dispatcher::new(Registry::new()), bound);
        });
        addr
    }
}

fn get(addr: SocketAddr, path: &str) -> TcpStream {
    let mut conn = TcpStream::connect(addr).unwrap();
    conn.set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    conn.write_all(format!("GET {path} HTTP/1.1\r\nHost: t\r\n\r\n").as_bytes())
        .unwrap();
    conn
}

fn whole(addr: SocketAddr, path: &str) -> String {
    let mut response = String::new();
    get(addr, path).read_to_string(&mut response).unwrap();
    response
}

/// Read until `want` has arrived, without waiting for the close.
fn read_until(conn: &mut TcpStream, want: &str) -> String {
    let mut got = Vec::new();
    let mut buf = [0u8; 1024];
    while !String::from_utf8_lossy(&got).contains(want) {
        let n = conn.read(&mut buf).unwrap();
        assert!(
            n > 0,
            "closed before {want:?}: {}",
            String::from_utf8_lossy(&got)
        );
        got.extend_from_slice(&buf[..n]);
    }
    String::from_utf8(got).unwrap()
}

/// The reason of the last closed notice, once the program has one.
fn closed_reason(addr: SocketAddr) -> String {
    let started = Instant::now();
    loop {
        let response = whole(addr, "/closed");
        let reason = response.rsplit("\r\n\r\n").next().unwrap().to_string();
        if reason != "none" || started.elapsed() > Duration::from_secs(5) {
            return reason;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

const STREAMING: &str = r#"{"scheme": "http", "address": "127.0.0.1:0", "streaming": true,
                            "stream_idle_ms": 0}"#;

fn a_stream_is_written_chunk_by_chunk_and_ends_cleanly(polled: bool) {
    let addr = served(polled, STREAMING);
    let response = whole(addr, "/events");
    assert!(response.starts_with("HTTP/1.1 200 OK\r\n"), "{response}");
    assert!(
        response.contains("Content-Type: text/event-stream\r\n"),
        "{response}"
    );
    assert!(
        response.contains("Transfer-Encoding: chunked\r\n"),
        "{response}"
    );
    assert!(!response.contains("Content-Length"), "{response}");
    assert!(
        response.ends_with(
            "\r\n\r\n9\r\ndata: 1\n\n\r\n9\r\ndata: 2\n\n\r\n9\r\ndata: 3\n\n\r\n0\r\n\r\n"
        ),
        "{response:?}"
    );
}

fn a_client_that_leaves_is_reported_to_the_program(polled: bool) {
    let addr = served(polled, STREAMING);
    let mut conn = get(addr, "/hold");
    // The head and the first chunk arrive while the response is still
    // open: that is the stream.
    let seen = read_until(&mut conn, "held\n");
    assert!(seen.starts_with("HTTP/1.1 200 OK\r\n"), "{seen}");
    drop(conn);
    assert_eq!(closed_reason(addr), "client");
}

fn a_stream_with_no_chunk_for_the_idle_limit_is_closed(polled: bool) {
    let addr = served(
        polled,
        r#"{"scheme": "http", "address": "127.0.0.1:0", "streaming": true,
            "stream_idle_ms": 300}"#,
    );
    let started = Instant::now();
    let response = whole(addr, "/hold");
    assert!(response.ends_with("5\r\nheld\n\r\n"), "{response:?}");
    // No terminating chunk: the client can tell it was cut.
    assert!(!response.ends_with("0\r\n\r\n"), "{response:?}");
    assert!(started.elapsed() < Duration::from_secs(5));
    assert_eq!(closed_reason(addr), "idle");
}

fn a_program_that_writes_far_ahead_of_its_client_is_cut_off(polled: bool) {
    let addr = served(polled, STREAMING);
    // A client that reads nothing: the socket buffers fill, and what the
    // program writes past them is held by the host up to its bound.
    let conn = get(addr, "/flood");
    assert_eq!(closed_reason(addr), "backlog");
    drop(conn);
}

fn a_stream_is_refused_where_streaming_is_not_set(polled: bool) {
    let addr = served(polled, r#"{"scheme": "http", "address": "127.0.0.1:0"}"#);
    let response = whole(addr, "/hold");
    assert!(response.starts_with("HTTP/1.1 500 "), "{response}");
    assert!(response.contains("'streaming'"), "{response}");
    let response = whole(addr, "/early");
    assert!(response.starts_with("HTTP/1.1 500 "), "{response}");
    assert!(
        response.contains("before a streamed response's head"),
        "{response}"
    );
}

mod threaded {
    #[test]
    fn a_program_that_writes_far_ahead_of_its_client_is_cut_off() {
        super::a_program_that_writes_far_ahead_of_its_client_is_cut_off(false);
    }
    #[test]
    fn a_stream_is_written_chunk_by_chunk_and_ends_cleanly() {
        super::a_stream_is_written_chunk_by_chunk_and_ends_cleanly(false);
    }
    #[test]
    fn a_client_that_leaves_is_reported_to_the_program() {
        super::a_client_that_leaves_is_reported_to_the_program(false);
    }
    #[test]
    fn a_stream_with_no_chunk_for_the_idle_limit_is_closed() {
        super::a_stream_with_no_chunk_for_the_idle_limit_is_closed(false);
    }
    #[test]
    fn a_stream_is_refused_where_streaming_is_not_set() {
        super::a_stream_is_refused_where_streaming_is_not_set(false);
    }
}

mod polled {
    #[test]
    fn a_program_that_writes_far_ahead_of_its_client_is_cut_off() {
        super::a_program_that_writes_far_ahead_of_its_client_is_cut_off(true);
    }
    #[test]
    fn a_stream_is_written_chunk_by_chunk_and_ends_cleanly() {
        super::a_stream_is_written_chunk_by_chunk_and_ends_cleanly(true);
    }
    #[test]
    fn a_client_that_leaves_is_reported_to_the_program() {
        super::a_client_that_leaves_is_reported_to_the_program(true);
    }
    #[test]
    fn a_stream_with_no_chunk_for_the_idle_limit_is_closed() {
        super::a_stream_with_no_chunk_for_the_idle_limit_is_closed(true);
    }
    #[test]
    fn a_stream_is_refused_where_streaming_is_not_set() {
        super::a_stream_is_refused_where_streaming_is_not_set(true);
    }
}
