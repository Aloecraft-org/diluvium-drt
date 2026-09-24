//! The ws connector at its boundary, against a real tungstenite server on
//! loopback: messages both ways, the scope's origin and header terms, how
//! a connection ends and what `recv` says about it, `vital`, `wake`,
//! release, the idle check, and a refused handshake.
//!
//! Calls are driven the way the pump drives them: polled with a no-op
//! waker until ready, under a wall-clock bound rather than a hang.

// The handshake callback's error type is tungstenite's, not ours to shrink.
#![allow(clippy::result_large_err)]

use std::future::Future;
use std::task::{Context, Poll, Waker};
use std::time::{Duration, Instant};

use futures_util::{SinkExt, StreamExt};
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::handshake::server::{Request, Response};
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::Message;

use drt_caps::{Scope, ScopeType};
use drt_connector::{Asker, Caller, Connector, Notice};
use drt_connector_ws::{WsConnector, WsScopeType};

const BOUND: Duration = Duration::from_secs(10);

fn drive<T>(fut: impl Future<Output = T>) -> T {
    let mut fut = std::pin::pin!(fut);
    let start = Instant::now();
    loop {
        if let Poll::Ready(v) = fut.as_mut().poll(&mut Context::from_waker(Waker::noop())) {
            return v;
        }
        assert!(start.elapsed() < BOUND, "the call never became ready");
        std::thread::sleep(Duration::from_millis(2));
    }
}

fn v(entries: Vec<(&str, rmpv::Value)>) -> rmpv::Value {
    rmpv::Value::Map(
        entries
            .into_iter()
            .map(|(k, v)| (rmpv::Value::from(k), v))
            .collect(),
    )
}

fn field(value: &rmpv::Value, name: &str) -> rmpv::Value {
    value
        .as_map()
        .unwrap()
        .iter()
        .find(|(k, _)| k.as_str() == Some(name))
        .map(|(_, v)| v.clone())
        .unwrap_or(rmpv::Value::Nil)
}

fn call(
    c: &WsConnector,
    caller: Caller,
    scope: &Scope,
    name: &str,
    args: rmpv::Value,
) -> Result<rmpv::Value, String> {
    let asker = Asker {
        caller,
        grants: &[],
    };
    drive(c.call_as(&asker, name, Some(args), Some(scope))).map_err(|e| e.to_string())
}

/// Granted `origin`, private addresses allowed (it is loopback), and one
/// injected header the program never sees.
fn scope(origin: &str) -> Scope {
    Scope(v(vec![
        (
            "allow",
            rmpv::Value::Array(vec![v(vec![
                ("origin", origin.into()),
                ("headers", v(vec![("x-injected", "from-the-scope".into())])),
            ])]),
        ),
        ("allow_private", true.into()),
    ]))
}

/// What the test server saw and was told, and what it is to do next.
enum Say {
    Text(String),
    Ping,
    Close(u16, &'static str),
    Hang,
}

struct Server {
    url: String,
    origin: String,
    /// The request headers of each handshake, then each message received,
    /// then the close it saw, as lines.
    heard: mpsc::UnboundedReceiver<String>,
    say: mpsc::UnboundedSender<Say>,
    _rt: tokio::runtime::Runtime,
}

impl Server {
    fn heard(&mut self) -> String {
        let start = Instant::now();
        loop {
            if let Ok(line) = self.heard.try_recv() {
                return line;
            }
            assert!(start.elapsed() < BOUND, "the server heard nothing");
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    fn heard_starting(&mut self, prefix: &str) -> String {
        loop {
            let line = self.heard();
            if line.starts_with(prefix) {
                return line;
            }
        }
    }
}

/// One server, one connection at a time, on its own runtime.
fn server() -> Server {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .unwrap();
    let listener = rt.block_on(TcpListener::bind("127.0.0.1:0")).unwrap();
    let port = listener.local_addr().unwrap().port();
    let (heard_tx, heard) = mpsc::unbounded_channel();
    let (say, mut say_rx) = mpsc::unbounded_channel::<Say>();
    rt.spawn(async move {
        while let Ok((tcp, _)) = listener.accept().await {
            let heard = heard_tx.clone();
            let callback = |req: &Request, res: Response| {
                let mut hs: Vec<String> = req
                    .headers()
                    .iter()
                    .map(|(k, v)| format!("{}={}", k, v.to_str().unwrap_or("?")))
                    .collect();
                hs.sort();
                let _ = heard.send(format!("headers {} {}", req.uri(), hs.join(" ")));
                Ok(res)
            };
            let Ok(mut ws) = tokio_tungstenite::accept_hdr_async(tcp, callback).await else {
                continue;
            };
            loop {
                tokio::select! {
                    m = ws.next() => match m {
                        Some(Ok(Message::Text(t))) => { let _ = heard_tx.send(format!("text {t}")); }
                        Some(Ok(Message::Binary(b))) => { let _ = heard_tx.send(format!("binary {}", b.len())); }
                        Some(Ok(Message::Pong(_))) => { let _ = heard_tx.send("pong".into()); }
                        Some(Ok(Message::Close(f))) => {
                            let _ = heard_tx.send(match f {
                                Some(f) => format!("close {} {}", u16::from(f.code), f.reason),
                                None => "close none".into(),
                            });
                            let _ = ws.next().await;
                            break;
                        }
                        Some(Ok(_)) => {}
                        _ => { let _ = heard_tx.send("gone".into()); break; }
                    },
                    s = say_rx.recv() => match s {
                        Some(Say::Text(t)) => { let _ = ws.send(Message::Text(t)).await; }
                        Some(Say::Ping) => { let _ = ws.send(Message::Ping(b"hi".to_vec())).await; }
                        Some(Say::Close(code, reason)) => {
                            let _ = ws.close(Some(CloseFrame { code: CloseCode::from(code), reason: reason.into() })).await;
                            while let Some(Ok(_)) = ws.next().await {}
                            break;
                        }
                        Some(Say::Hang) => {
                            // Hold the connection and say nothing, ever.
                            std::future::pending::<()>().await;
                        }
                        None => return,
                    },
                }
            }
        }
    });
    Server {
        url: format!("ws://127.0.0.1:{port}/signal?room=r1"),
        origin: format!("ws://127.0.0.1:{port}"),
        heard,
        say,
        _rt: rt,
    }
}

fn connect(c: &WsConnector, owner: Caller, s: &Server, extra: Vec<(&str, rmpv::Value)>) -> u64 {
    let mut args = vec![("url", s.url.as_str().into())];
    args.extend(extra);
    let r = call(c, owner, &scope(&s.origin), "ws/connect", v(args)).unwrap();
    assert_eq!(field(&r, "status").as_u64(), Some(101));
    field(&r, "handle").as_u64().unwrap()
}

fn recv(c: &WsConnector, owner: Caller, s: &Server, handle: u64) -> rmpv::Value {
    call(
        c,
        owner,
        &scope(&s.origin),
        "ws/recv",
        v(vec![("handle", handle.into())]),
    )
    .unwrap()
}

#[test]
fn text_goes_both_ways_with_the_programs_and_the_scopes_headers() {
    let mut s = server();
    let c = WsConnector::new();
    let node = Caller::Node(1);
    let h = connect(
        &c,
        node,
        &s,
        vec![("headers", v(vec![("Authorization", "Bearer tok".into())]))],
    );
    let headers = s.heard_starting("headers");
    assert!(headers.contains("/signal?room=r1"), "{headers}");
    assert!(headers.contains("authorization=Bearer tok"), "{headers}");
    assert!(headers.contains("x-injected=from-the-scope"), "{headers}");
    assert!(headers.contains("user-agent=drt/"), "{headers}");

    call(
        &c,
        node,
        &scope(&s.origin),
        "ws/send",
        v(vec![
            ("handle", h.into()),
            ("text", r#"{"t":"record"}"#.into()),
        ]),
    )
    .unwrap();
    assert_eq!(s.heard(), r#"text {"t":"record"}"#);

    s.say.send(Say::Text(r#"{"t":"ready"}"#.into())).unwrap();
    s.say.send(Say::Text(r#"{"t":"peer"}"#.into())).unwrap();
    let mut got = Vec::new();
    while got.len() < 2 {
        let r = recv(&c, node, &s, h);
        assert!(field(&r, "closed").is_nil());
        for m in field(&r, "messages").as_array().unwrap() {
            got.push(m.as_str().unwrap().to_string());
        }
    }
    assert_eq!(got, [r#"{"t":"ready"}"#, r#"{"t":"peer"}"#]);
}

#[test]
fn the_scope_refuses_other_origins_and_headers_it_owns() {
    let s = server();
    let c = WsConnector::new();
    let node = Caller::Node(1);
    let other = call(
        &c,
        node,
        &scope("ws://127.0.0.1:1"),
        "ws/connect",
        v(vec![("url", s.url.as_str().into())]),
    )
    .unwrap_err();
    assert!(
        other.contains("not an origin this instance was granted"),
        "{other}"
    );

    // Loopback without allow_private: the resolved address is refused.
    let strict = Scope(rmpv::Value::from(s.origin.as_str()));
    let private = call(
        &c,
        node,
        &strict,
        "ws/connect",
        v(vec![("url", s.url.as_str().into())]),
    )
    .unwrap_err();
    assert!(private.contains("private address space"), "{private}");

    for name in ["x-injected", "sec-websocket-key", "host", "upgrade"] {
        let e = call(
            &c,
            node,
            &scope(&s.origin),
            "ws/connect",
            v(vec![
                ("url", s.url.as_str().into()),
                ("headers", v(vec![(name, "mine".into())])),
            ]),
        )
        .unwrap_err();
        assert!(
            e.contains("may set") || e.contains("set by the connector"),
            "{name}: {e}"
        );
    }

    let https = call(
        &c,
        node,
        &scope(&s.origin),
        "ws/connect",
        v(vec![("url", "https://127.0.0.1/".into())]),
    )
    .unwrap_err();
    assert!(https.contains("ws:// or wss://"), "{https}");
}

/// The token on the upgrade never crosses a network in the clear: plain
/// origins off this machine are refused in the scope, and plain URLs off
/// it are refused at connect.
#[test]
fn plain_text_goes_to_loopback_only() {
    for origin in [
        "ws://signal.example",
        "ws://192.0.2.1:8080",
        "http://10.0.0.5",
    ] {
        let e = WsScopeType
            .validate(Some(&Scope(rmpv::Value::from(origin))))
            .unwrap_err();
        assert!(e.contains("loopback only"), "{origin}: {e}");
    }
    for origin in [
        "ws://localhost:8080",
        "ws://127.0.0.1:9",
        "ws://[::1]:9",
        "wss://signal.example",
    ] {
        assert!(
            WsScopeType
                .validate(Some(&Scope(rmpv::Value::from(origin))))
                .is_ok(),
            "{origin}"
        );
    }
    let c = WsConnector::new();
    let e = call(
        &c,
        Caller::Node(1),
        &scope("wss://signal.example"),
        "ws/connect",
        v(vec![("url", "ws://signal.example/".into())]),
    )
    .unwrap_err();
    assert!(e.contains("loopback only"), "{e}");
}

#[test]
fn the_scope_reads_websocket_schemes_and_refuses_an_empty_allowlist() {
    assert!(WsScopeType
        .validate(Some(&Scope(rmpv::Value::from("wss://signal.example"))))
        .is_ok());
    assert!(WsScopeType
        .validate(Some(&Scope(v(vec![("allow", rmpv::Value::Array(vec![]))]))))
        .is_err());
    assert!(WsScopeType.validate(None).is_err());
}

#[test]
fn a_close_from_the_far_end_reaches_recv_with_its_code_and_reason() {
    let s = server();
    let c = WsConnector::new();
    let node = Caller::Node(1);
    let h = connect(&c, node, &s, vec![]);
    s.say.send(Say::Text("last".into())).unwrap();
    s.say.send(Say::Close(4001, "replaced")).unwrap();
    let mut messages = Vec::new();
    let closed = loop {
        let r = recv(&c, node, &s, h);
        messages.extend(field(&r, "messages").as_array().unwrap().clone());
        let closed = field(&r, "closed");
        if !closed.is_nil() {
            break closed;
        }
    };
    assert_eq!(messages, [rmpv::Value::from("last")]);
    assert_eq!(field(&closed, "code").as_u64(), Some(4001));
    assert_eq!(field(&closed, "reason").as_str(), Some("replaced"));
    let e = call(
        &c,
        node,
        &scope(&s.origin),
        "ws/send",
        v(vec![("handle", h.into()), ("text", "after".into())]),
    )
    .unwrap_err();
    assert!(e.contains("closed (4001: replaced)"), "{e}");
}

#[test]
fn the_owners_close_sends_its_code_and_a_ping_is_answered() {
    let mut s = server();
    let c = WsConnector::new();
    let node = Caller::Node(1);
    let h = connect(&c, node, &s, vec![]);
    s.say.send(Say::Ping).unwrap();
    assert_eq!(s.heard_starting("pong"), "pong");
    call(
        &c,
        node,
        &scope(&s.origin),
        "ws/close",
        v(vec![
            ("handle", h.into()),
            ("code", 4000u64.into()),
            ("reason", "bye".into()),
        ]),
    )
    .unwrap();
    assert_eq!(s.heard_starting("close"), "close 4000 bye");
    // Closed is gone: the number is no handle any more.
    let e = call(
        &c,
        node,
        &scope(&s.origin),
        "ws/recv",
        v(vec![("handle", h.into())]),
    )
    .unwrap_err();
    assert!(e.contains("no such"), "{e}");
}

#[test]
fn a_vital_connection_ending_names_its_owner_once() {
    let s = server();
    let c = WsConnector::new();
    let node = Caller::Node(7);
    let _h = connect(&c, node, &s, vec![("vital", true.into())]);
    assert!(c.ended().is_empty());
    s.say.send(Say::Close(1000, "")).unwrap();
    let start = Instant::now();
    let ended = loop {
        let e = c.ended();
        if !e.is_empty() {
            break e;
        }
        assert!(start.elapsed() < BOUND);
        std::thread::sleep(Duration::from_millis(5));
    };
    assert_eq!(ended.len(), 1);
    assert_eq!(ended[0].0, node);
    assert!(ended[0].1.contains("vital WebSocket"), "{}", ended[0].1);
    assert!(c.ended().is_empty(), "reported twice");

    // A plain connection ending is nobody's business.
    let s2 = server();
    let _plain = connect(&c, node, &s2, vec![]);
    s2.say.send(Say::Close(1000, "")).unwrap();
    std::thread::sleep(Duration::from_millis(200));
    assert!(c.ended().is_empty());
}

#[test]
fn closing_ones_own_vital_connection_ends_the_owner() {
    let s = server();
    let c = WsConnector::new();
    let node = Caller::Node(3);
    let h = connect(&c, node, &s, vec![("vital", true.into())]);
    call(
        &c,
        node,
        &scope(&s.origin),
        "ws/close",
        v(vec![("handle", h.into())]),
    )
    .unwrap();
    let ended = c.ended();
    assert_eq!(ended.len(), 1);
    assert!(ended[0].1.contains("closed by its owner"), "{}", ended[0].1);
}

#[test]
fn wake_says_ready_once_per_edge_and_recv_rearms_it() {
    let s = server();
    let c = WsConnector::new();
    let node = Caller::Node(2);
    let h = connect(&c, node, &s, vec![("wake", "signal".into())]);
    assert!(c.notices().is_empty());
    s.say.send(Say::Text("one".into())).unwrap();
    let start = Instant::now();
    let notices: Vec<Notice> = loop {
        let n = c.notices();
        if !n.is_empty() {
            break n;
        }
        assert!(start.elapsed() < BOUND);
        std::thread::sleep(Duration::from_millis(5));
    };
    assert_eq!(notices.len(), 1);
    assert_eq!(notices[0].owner, node);
    assert_eq!(notices[0].queue, "signal");
    assert_eq!(field(&notices[0].message, "handle").as_u64(), Some(h));
    assert_eq!(field(&notices[0].message, "ready").as_str(), Some("recv"));
    s.say.send(Say::Text("two".into())).unwrap();
    std::thread::sleep(Duration::from_millis(100));
    assert!(c.notices().is_empty(), "said again before a recv");
    let r = recv(&c, node, &s, h);
    assert!(!field(&r, "messages").as_array().unwrap().is_empty());
    s.say.send(Say::Text("three".into())).unwrap();
    let start = Instant::now();
    while c.notices().is_empty() {
        assert!(start.elapsed() < BOUND, "not re-armed by recv");
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn release_closes_with_going_away_and_says_what_it_cut() {
    let mut s = server();
    let c = WsConnector::new();
    let node = Caller::Node(4);
    let other = Caller::Node(5);
    let h = connect(&c, node, &s, vec![]);
    // Another node's number is no handle of its own.
    let e = call(
        &c,
        other,
        &scope(&s.origin),
        "ws/recv",
        v(vec![("handle", h.into())]),
    )
    .unwrap_err();
    assert!(e.contains("no such"), "{e}");
    let lost = c.release(&node);
    assert_eq!(lost.len(), 1);
    assert!(lost[0].contains("cut"), "{}", lost[0]);
    assert_eq!(s.heard_starting("close"), "close 1001 ");
}

#[test]
fn a_silent_connection_is_closed_after_idle_ms() {
    let s = server();
    let c = WsConnector::new();
    let node = Caller::Node(1);
    let h = connect(&c, node, &s, vec![("idle_ms", 300u64.into())]);
    s.say.send(Say::Hang).unwrap();
    let r = recv(&c, node, &s, h);
    let closed = field(&r, "closed");
    assert_eq!(field(&closed, "code").as_u64(), Some(1006));
    assert!(
        field(&closed, "reason")
            .as_str()
            .unwrap()
            .contains("no frame for 300 ms"),
        "{closed}"
    );
}

#[test]
fn recv_without_wait_answers_at_once() {
    let s = server();
    let c = WsConnector::new();
    let node = Caller::Node(1);
    let h = connect(&c, node, &s, vec![]);
    let r = call(
        &c,
        node,
        &scope(&s.origin),
        "ws/recv",
        v(vec![("handle", h.into()), ("wait", false.into())]),
    )
    .unwrap();
    assert!(field(&r, "messages").as_array().unwrap().is_empty());
    assert!(field(&r, "closed").is_nil());
}

#[test]
fn a_refused_handshake_names_the_http_status() {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .unwrap();
    let listener = rt.block_on(TcpListener::bind("127.0.0.1:0")).unwrap();
    let port = listener.local_addr().unwrap().port();
    rt.spawn(async move {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        while let Ok((mut tcp, _)) = listener.accept().await {
            let mut buf = [0u8; 4096];
            let _ = tcp.read(&mut buf).await;
            let _ = tcp
                .write_all(b"HTTP/1.1 401 Unauthorized\r\ncontent-length: 0\r\n\r\n")
                .await;
        }
    });
    let c = WsConnector::new();
    let e = call(
        &c,
        Caller::Node(1),
        &scope(&format!("ws://127.0.0.1:{port}")),
        "ws/connect",
        v(vec![("url", format!("ws://127.0.0.1:{port}/").into())]),
    )
    .unwrap_err();
    assert!(e.contains("HTTP 401"), "{e}");
}
