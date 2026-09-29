//! The host against a native client that does what a browser does: builds
//! its session from the host's record with no answer round trip, controls
//! ICE, is the DTLS and SCTP client, and speaks Wisp v1 on channel 1. All on
//! loopback, so it proves the plumbing and not a NAT traversal
//! (`doc/Plan-0.8.0.md` §3.3, M0).
//!
//! ## surface block
//!
//! - Entry points: the `#[tokio::test]`s below, one per rule in
//!   `doc/BrowserAccess.md` §6 that can be seen from outside.
//! - Configurable: [`LIMIT`], how long any one wait may take.
//! - Fan-out: [`Client`] (`common/peer.rs`) is the browser; [`host_with`] is the host;
//!   [`echo_server`] and [`sink_server`] are the targets.

use std::time::Duration;

use drt_rtc::host::{Event, SessionState, StreamState};
use drt_rtc::wisp::{self, reason};
use drt_rtc::{Command, Entry, Host, HostConfig, Identity, Record, Scope};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

#[path = "common/peer.rs"]
mod peer;
use peer::{Client, LIMIT};

// depth: the targets

/// Echoes every connection until it closes.
async fn echo_server() -> u16 {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = l.local_addr().unwrap().port();
    tokio::spawn(async move {
        while let Ok((mut s, _)) = l.accept().await {
            tokio::spawn(async move {
                let mut buf = vec![0u8; 65536];
                while let Ok(n) = s.read(&mut buf).await {
                    if n == 0 || s.write_all(&buf[..n]).await.is_err() {
                        break;
                    }
                }
            });
        }
    });
    port
}

/// Accepts, then reports each connection's end on the channel: the proof
/// that a closed stream closed its socket, or that nothing connected.
async fn sink_server() -> (u16, tokio::sync::mpsc::UnboundedReceiver<&'static str>) {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = l.local_addr().unwrap().port();
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Ok((mut s, _)) = l.accept().await {
            let _ = tx.send("accepted");
            let tx = tx.clone();
            tokio::spawn(async move {
                let mut buf = [0u8; 1024];
                loop {
                    match s.read(&mut buf).await {
                        Ok(0) | Err(_) => {
                            let _ = tx.send("closed");
                            return;
                        }
                        Ok(_) => {}
                    }
                }
            });
        }
    });
    (port, rx)
}

// depth: the host

async fn host_with(scope: &[String]) -> (Host, Record) {
    let entries = scope.iter().map(|s| Entry::parse(s).unwrap()).collect();
    let cfg = HostConfig {
        bind: "127.0.0.1:0".parse().unwrap(),
        identity: Identity::generate().unwrap(),
        stun: vec![],
        publish_host_candidates: true,
        service: "test".into(),
        default: None,
        scope: Scope::new(entries),
        max_sessions: 4,
        max_streams: 8,
        idle_timeout: Duration::from_secs(300),
        connect_timeout: Duration::from_secs(5),
        stun_refresh: Duration::from_secs(25),
    };
    let mut host = Host::start(cfg).unwrap();
    let rtc = match host.next_event().await {
        Some(Event::Record { rtc }) => rtc,
        other => panic!("the host's first word is its record, not {other:?}"),
    };
    (host, Record::decode(&rtc).unwrap())
}

async fn session_event(host: &mut Host, peer: &str) -> (SessionState, Option<String>) {
    loop {
        match tokio::time::timeout(LIMIT, host.next_event())
            .await
            .expect("a session event")
        {
            Some(Event::Session {
                peer: p,
                state,
                reason,
            }) if p == peer => return (state, reason),
            Some(_) => {}
            None => panic!("the host stopped"),
        }
    }
}

async fn stream_event(
    host: &mut Host,
    stream: u32,
) -> (StreamState, Option<&'static str>, u64, u64) {
    loop {
        match tokio::time::timeout(LIMIT, host.next_event())
            .await
            .expect("a stream event")
        {
            Some(Event::Stream {
                stream: s,
                state,
                reason,
                bytes_up,
                bytes_down,
                ..
            }) if s == stream => return (state, reason, bytes_up, bytes_down),
            Some(_) => {}
            None => panic!("the host stopped"),
        }
    }
}

async fn open_session(host: &mut Host, record: &Record, peer: &str) -> Client {
    let (mut client, rtc) = Client::new(record).await;
    host.send(Command::Open {
        peer: peer.into(),
        rtc,
    });
    client.connect().await;
    assert_eq!(
        session_event(host, peer).await,
        (SessionState::Connected, None)
    );
    client
}

// depth: the tests

#[tokio::test]
async fn a_scoped_stream_round_trips_and_hello_carries_the_scope() {
    let port = echo_server().await;
    let scope = vec![format!("http://127.0.0.1:{port}")];
    let (mut host, record) = host_with(&scope).await;
    let mut client = open_session(&mut host, &record, "p1").await;

    let hello: serde_json::Value = serde_json::from_str(client.hello.as_deref().unwrap()).unwrap();
    assert_eq!(hello["t"], "hello");
    assert_eq!(hello["scope"][0]["port"], port);
    assert_eq!(hello["limits"]["max_streams"], 8);

    client.send(wisp::connect(1, wisp::STREAM_TCP, port, "127.0.0.1"));
    // DATA before the connection is up is allowed, and must arrive in order.
    client.send(wisp::data(1, b"hello, "));
    client.send(wisp::data(1, b"host"));
    let (echoed, closed) = client.read_stream(1, 11).await;
    assert_eq!((echoed.as_slice(), closed), (&b"hello, host"[..], None));
    assert_eq!(stream_event(&mut host, 1).await.0, StreamState::Open);

    client.send(wisp::close(1, reason::VOLUNTARY));
    // A write only queues in str0m; driving the client once puts it on the
    // wire.
    client.until("the CLOSE to go out", |_| true).await;
    let (state, why, up, down) = stream_event(&mut host, 1).await;
    assert_eq!(
        (state, why, up, down),
        (StreamState::Closed, Some("voluntary"), 11, 11)
    );
}

#[tokio::test]
async fn what_scope_does_not_name_is_refused_before_any_connection() {
    let (port, mut sink) = sink_server().await;
    let echo = echo_server().await;
    // The sink is not in scope; only the echo server is, by its literal.
    let (mut host, record) = host_with(&[format!("http://127.0.0.1:{echo}")]).await;
    let mut client = open_session(&mut host, &record, "p1").await;

    client.send(wisp::connect(2, wisp::STREAM_TCP, port, "127.0.0.1"));
    assert_eq!(client.read_stream(2, 1).await.1, Some(reason::BLOCKED));
    // Same port, but a name that resolves there: a literal matches only
    // itself.
    client.send(wisp::connect(3, wisp::STREAM_TCP, echo, "localhost"));
    assert_eq!(client.read_stream(3, 1).await.1, Some(reason::BLOCKED));
    // UDP: mandatory in Wisp v1, not carried by this profile.
    client.send(wisp::connect(4, wisp::STREAM_UDP, echo, "127.0.0.1"));
    assert_eq!(client.read_stream(4, 1).await.1, Some(reason::BLOCKED));
    // Stream 0 is the protocol's, and port 0 is nobody's.
    client.send(wisp::connect(0, wisp::STREAM_TCP, echo, "127.0.0.1"));
    assert_eq!(client.read_stream(0, 1).await.1, Some(reason::INVALID));

    assert!(
        tokio::time::timeout(Duration::from_millis(300), sink.recv())
            .await
            .is_err(),
        "a refused CONNECT must not reach the target at all"
    );
    let (_, why, _, _) = stream_event(&mut host, 2).await;
    assert_eq!(why, Some("blocked"), "a refusal is in the audit trail");
}

#[tokio::test]
async fn two_browsers_share_one_socket_and_one_host_ufrag() {
    let port = echo_server().await;
    let (mut host, record) = host_with(&[format!("http://127.0.0.1:{port}")]).await;
    let mut a = open_session(&mut host, &record, "a").await;
    let mut b = open_session(&mut host, &record, "b").await;
    for (c, msg) in [(&mut a, &b"from a"[..]), (&mut b, &b"from b"[..])] {
        c.send(wisp::connect(7, wisp::STREAM_TCP, port, "127.0.0.1"));
        c.send(wisp::data(7, msg));
    }
    assert_eq!(a.read_stream(7, 6).await.0, b"from a");
    assert_eq!(b.read_stream(7, 6).await.0, b"from b");
}

#[tokio::test]
async fn closing_a_session_closes_its_sockets() {
    let (port, mut sink) = sink_server().await;
    let (mut host, record) = host_with(&[format!("http://127.0.0.1:{port}")]).await;
    let mut client = open_session(&mut host, &record, "p1").await;
    client.send(wisp::connect(1, wisp::STREAM_TCP, port, "127.0.0.1"));
    client.until("the stream to settle", |_| true).await;
    assert_eq!(
        tokio::time::timeout(LIMIT, sink.recv()).await.unwrap(),
        Some("accepted")
    );

    host.send(Command::Close { peer: "p1".into() });
    assert_eq!(
        tokio::time::timeout(LIMIT, sink.recv()).await.unwrap(),
        Some("closed"),
        "the TCP socket closes with the session"
    );
    let (state, why) = session_event(&mut host, "p1").await;
    assert_eq!(
        (state, why.as_deref()),
        (SessionState::Closed, Some("closed by the program"))
    );
}

#[tokio::test]
async fn a_bad_or_duplicate_record_is_refused_by_name() {
    let (mut host, record) = host_with(&[]).await;
    host.send(Command::Open {
        peer: "x".into(),
        rtc: "{}".into(),
    });
    let (state, why) = session_event(&mut host, "x").await;
    assert_eq!(state, SessionState::Closed);
    assert!(why.unwrap().contains("no `v`"));

    let (_client, rtc) = Client::new(&record).await;
    host.send(Command::Open {
        peer: "y".into(),
        rtc: rtc.clone(),
    });
    host.send(Command::Open {
        peer: "z".into(),
        rtc,
    });
    let (state, why) = session_event(&mut host, "z").await;
    assert_eq!(state, SessionState::Closed);
    assert!(why.unwrap().starts_with("duplicate"));
}

/// Past str0m's 128 KiB send buffer many times over, so the outbox and the
/// gate both have to work for this to come out right -- and run from a
/// deliberately small stack, because this is the load that overflowed CI's
/// 2 MiB test thread when the host ran on its caller's stack. str0m
/// recurses once per SCTP packet in a burst (`drt_rtc::host`, the module
/// note); the host's own thread has room for that, and this proves none of
/// it lands here, whatever `RUST_MIN_STACK` the runner sets.
#[test]
fn a_large_download_arrives_whole_and_in_order() {
    const CALLER_STACK: usize = 512 * 1024;
    std::thread::Builder::new()
        .stack_size(CALLER_STACK)
        .spawn(|| {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(large_download())
        })
        .unwrap()
        .join()
        .expect("the download completes on a 512 KiB caller stack");
}

async fn large_download() {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = l.local_addr().unwrap().port();
    const SIZE: usize = 4 * 1024 * 1024;
    let body: Vec<u8> = (0..SIZE).map(|i| (i % 251) as u8).collect();
    let served = body.clone();
    tokio::spawn(async move {
        let (mut s, _) = l.accept().await.unwrap();
        s.write_all(&served).await.unwrap();
    });
    let (mut host, record) = host_with(&[format!("http://127.0.0.1:{port}")]).await;
    let mut client = open_session(&mut host, &record, "p1").await;
    client.send(wisp::connect(9, wisp::STREAM_TCP, port, "127.0.0.1"));
    let (got, _) = client.read_stream(9, SIZE).await;
    assert_eq!(got.len(), SIZE);
    assert!(got == body, "the bytes arrive in order and intact");
}
