//! The serving side's forward (`doc/P2P.md` §5.1): where a stream that
//! names no target goes, by what this side forwards to; who may connect
//! (§6); and a `hello` that names services and not the scope behind them
//! (§7.2). On loopback, against the native peer of `common/peer.rs`.
//!
//! ## surface block
//!
//! - Entry points: the `#[tokio::test]`s below, one per row of §5.1's
//!   table that a peer can see from outside, one for `--accept`, one for
//!   `hello`.
//! - Configurable: [`LIMIT`] (`common/peer.rs`), [`NOTHING`].
//! - Fan-out: [`host`] builds the host from a [`Forward`]; [`Echo`] is a
//!   [`Service`] inside the process; [`echo_server`] a target outside it.

use std::sync::Arc;
use std::time::Duration;

use drt_rtc::host::{Event, Opening, SessionState, Window};
use drt_rtc::wisp::{self, reason};
use drt_rtc::{
    Cidr, Command, Entry, Forward, Host, HostConfig, Identity, PortSet, Record, Scope, Service,
    Sink,
};
use str0m::IceCreds;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

#[path = "common/peer.rs"]
mod peer;
use peer::{Client, LIMIT};

/// How long a test that proves nothing connects waits for it not to.
const NOTHING: Duration = Duration::from_secs(3);

// depth: the targets

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

/// An in-process service: echoes, with the host and port it was asked for
/// in front, so a test sees what the host passed on.
struct Echo;

impl Service for Echo {
    fn name(&self) -> String {
        "echo".into()
    }
    fn open(&self, host: &str, port: u16, _window: Window) -> Opening {
        let tag = format!("[{host}:{port}]");
        Box::pin(async move {
            let (mine, theirs) = tokio::io::duplex(65536);
            tokio::spawn(async move {
                let (mut r, mut w) = tokio::io::split(mine);
                let _ = w.write_all(tag.as_bytes()).await;
                let mut buf = vec![0u8; 65536];
                while let Ok(n) = r.read(&mut buf).await {
                    if n == 0 || w.write_all(&buf[..n]).await.is_err() {
                        break;
                    }
                }
            });
            Ok(Box::pin(theirs) as _)
        })
    }
}

// depth: the host

async fn host(
    forward: Forward,
    services: Vec<(String, Sink)>,
    accept: Vec<Cidr>,
) -> (Host, Record) {
    let cfg = HostConfig {
        bind: "127.0.0.1:0".parse().unwrap(),
        identity: Identity::generate().unwrap(),
        stun: vec![],
        publish_host_candidates: true,
        service: String::new(),
        default: None,
        scope: Scope::new(Vec::new()),
        services,
        max_sessions: 4,
        max_streams: 8,
        idle_timeout: Duration::from_secs(300),
        connect_timeout: Duration::from_secs(5),
        stun_refresh: Duration::from_secs(25),
        direct: true,
        hello_scope: false,
        forward,
        accept,
    };
    let mut host = Host::start(cfg).unwrap();
    let rtc = match host.next_event().await {
        Some(Event::Record { rtc }) => rtc,
        other => panic!("the host's first word is its record, not {other:?}"),
    };
    (host, Record::decode(&rtc).unwrap())
}

async fn connected(host: &mut Host, record: &Record, peer: &str) -> Client {
    let (mut client, rtc) = Client::new(record).await;
    host.send(Command::Open {
        peer: peer.into(),
        rtc,
    });
    client.connect().await;
    loop {
        match tokio::time::timeout(LIMIT, host.next_event())
            .await
            .expect("a session event")
        {
            Some(Event::Session {
                peer: p,
                state: SessionState::Connected,
                ..
            }) if p == peer => return client,
            Some(Event::Session {
                peer: p, reason, ..
            }) if p == peer => {
                panic!("the session ended: {reason:?}")
            }
            Some(_) => {}
            None => panic!("the host stopped"),
        }
    }
}

fn direct_creds(tag: char) -> IceCreds {
    let ufrag: String = std::iter::repeat_n(tag, 24)
        .chain("Direct09".chars())
        .collect();
    IceCreds {
        ufrag: ufrag.clone(),
        pass: ufrag,
    }
}

fn hello_of(client: &Client) -> serde_json::Value {
    serde_json::from_str(client.hello.as_deref().unwrap()).unwrap()
}

// depth: the tests

#[tokio::test]
async fn one_forward_takes_every_stream_whatever_port_it_asks_for() {
    let port = echo_server().await;
    let entry = Entry::parse(&format!("http://127.0.0.1:{port}")).unwrap();
    let (mut host, record) = host(Forward::One(Sink::Dial(entry)), vec![], vec![]).await;
    let mut client = connected(&mut host, &record, "p1").await;
    // §7.2: the addresses behind a forward are policy, not routing
    // information, so hello says nothing about them.
    let hello = hello_of(&client);
    assert!(hello.get("scope").is_none(), "{hello}");
    assert!(hello.get("services").is_none(), "{hello}");

    client.send(wisp::connect(1, wisp::STREAM_TCP, 0, ""));
    client.send(wisp::data(1, b"no target named"));
    assert_eq!(client.read_stream(1, 15).await.0, b"no target named");
    // A port beside an empty host is ignored by a one-target forward.
    client.send(wisp::connect(3, wisp::STREAM_TCP, 9, ""));
    client.send(wisp::data(3, b"port ignored"));
    assert_eq!(client.read_stream(3, 12).await.0, b"port ignored");
    // A host and port still name a target, and nothing is in scope.
    client.send(wisp::connect(5, wisp::STREAM_TCP, port, "127.0.0.1"));
    assert_eq!(client.read_stream(5, 1).await.1, Some(reason::BLOCKED));
}

#[tokio::test]
async fn a_port_set_routes_by_port_and_refuses_the_rest_as_closed() {
    let a = echo_server().await;
    let b = echo_server().await;
    let ports = PortSet::parse(&format!("{a},{b}")).unwrap();
    let forward = Forward::Ports {
        host: "127.0.0.1".into(),
        ports,
    };
    let (mut host, record) = host(forward, vec![], vec![]).await;
    let mut client = connected(&mut host, &record, "p1").await;
    // Two ports: a stream naming none is refused as a closed port is.
    client.send(wisp::connect(1, wisp::STREAM_TCP, 0, ""));
    assert_eq!(client.read_stream(1, 1).await.1, Some(reason::BLOCKED));
    client.send(wisp::connect(3, wisp::STREAM_TCP, a, ""));
    client.send(wisp::data(3, b"to a"));
    assert_eq!(client.read_stream(3, 4).await.0, b"to a");
    // The forward's host with a port in the set is the same thing by name.
    client.send(wisp::connect(5, wisp::STREAM_TCP, b, "127.0.0.1"));
    client.send(wisp::data(5, b"to b"));
    assert_eq!(client.read_stream(5, 4).await.0, b"to b");
    // A port the set does not hold: closed, and nothing about what is held.
    client.send(wisp::connect(7, wisp::STREAM_TCP, 1, ""));
    assert_eq!(client.read_stream(7, 1).await.1, Some(reason::BLOCKED));
    client.send(wisp::connect(9, wisp::STREAM_TCP, a, "127.0.0.2"));
    assert_eq!(client.read_stream(9, 1).await.1, Some(reason::BLOCKED));
}

#[tokio::test]
async fn a_one_port_set_is_what_a_stream_naming_no_port_gets() {
    let a = echo_server().await;
    let forward = Forward::Ports {
        host: "127.0.0.1".into(),
        ports: PortSet::parse(&a.to_string()).unwrap(),
    };
    let (mut host, record) = host(forward, vec![], vec![]).await;
    let mut client = connected(&mut host, &record, "p1").await;
    client.send(wisp::connect(1, wisp::STREAM_TCP, 0, ""));
    client.send(wisp::data(1, b"the one port"));
    assert_eq!(client.read_stream(1, 12).await.0, b"the one port");
}

#[tokio::test]
async fn a_service_in_the_process_gets_the_stream_and_what_it_asked_for() {
    let echo: Arc<dyn Service> = Arc::new(Echo);
    let services = vec![("echo".to_string(), Sink::Local(echo.clone()))];
    let (mut host, record) = host(Forward::One(Sink::Local(echo)), services, vec![]).await;
    let mut client = connected(&mut host, &record, "p1").await;
    assert_eq!(hello_of(&client)["services"], serde_json::json!(["echo"]));
    client.send(wisp::connect(1, wisp::STREAM_TCP, 0, ""));
    client.send(wisp::data(1, b"!"));
    assert_eq!(client.read_stream(1, 5).await.0, b"[:0]!");
    client.send(wisp::connect(3, wisp::STREAM_TCP, 0, "echo"));
    client.send(wisp::data(3, b"!"));
    assert_eq!(client.read_stream(3, 9).await.0, b"[echo:0]!");
    client.send(wisp::connect(5, wisp::STREAM_TCP, 22, ""));
    client.send(wisp::data(5, b"!"));
    assert_eq!(client.read_stream(5, 6).await.0, b"[:22]!");
}

#[tokio::test]
async fn without_a_forward_an_empty_host_is_malformed_as_it_always_was() {
    let (mut host, record) = host(Forward::None, vec![], vec![]).await;
    let mut client = connected(&mut host, &record, "p1").await;
    client.send(wisp::connect(1, wisp::STREAM_TCP, 0, ""));
    assert_eq!(client.read_stream(1, 1).await.1, Some(reason::INVALID));
    client.send(wisp::connect(3, wisp::STREAM_TCP, 80, ""));
    assert_eq!(client.read_stream(3, 1).await.1, Some(reason::INVALID));
}

#[tokio::test]
async fn a_session_from_outside_the_accepted_ranges_gets_nothing() {
    let far = vec![Cidr::parse("203.0.113.0/24").unwrap()];
    let (_host, record) = host(Forward::None, vec![], far).await;
    // Direct mode, so the record alone would make a session: the address
    // the checks come from is what refuses it.
    let (mut client, _) = Client::with_creds(&record, direct_creds('a')).await;
    assert!(!client.within(NOTHING, |c| c.connected).await);

    let near = vec![
        Cidr::parse("203.0.113.0/24").unwrap(),
        Cidr::parse("127.0.0.0/8").unwrap(),
    ];
    let (_host, record) = host(Forward::None, vec![], near).await;
    let (mut client, _) = Client::with_creds(&record, direct_creds('b')).await;
    client.connect().await;
    assert!(client.connected);
}

#[test]
fn a_port_set_is_a_list_of_ports_and_ranges_and_nothing_is_guessed() {
    let set = PortSet::parse("80, 8080:8090,31200").unwrap();
    assert!(set.contains(80) && set.contains(8085) && set.contains(8090) && set.contains(31200));
    assert!(!set.contains(81) && !set.contains(8091));
    assert_eq!(set.single(), None);
    assert_eq!(set.to_string(), "80,8080:8090,31200");
    assert_eq!(PortSet::parse("22").unwrap().single(), Some(22));
    assert!(PortSet::any().contains(1) && PortSet::any().single().is_none());
    for bad in ["", "80,", "0", "8090:8080", "http", "80-90", "65536"] {
        assert!(PortSet::parse(bad).is_err(), "{bad}");
    }
}
