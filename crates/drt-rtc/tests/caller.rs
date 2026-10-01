//! The native caller (`drt_rtc::caller`, `doc/BrowserAccess.md` §10.1)
//! against the host, on loopback: signaled, in direct mode, by address and
//! by name, and refused.
//!
//! ## surface block
//!
//! - Entry points: the `#[tokio::test]`s below.
//! - Configurable: [`LIMIT`], how long any one wait may take.
//! - Fan-out: [`host`] is the host; [`echo_server`] the target; [`round_trip`]
//!   the check every connected case ends with.

use std::time::Duration;

use drt_rtc::caller::{Caller, Target};
use drt_rtc::host::{Event, SessionState};
use drt_rtc::{Command, Entry, Forward, Host, HostConfig, Identity, Record, Scope, Sink};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

const LIMIT: Duration = Duration::from_secs(10);

// depth: the host and its target

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

async fn host(port: u16, direct: bool) -> (Host, Record) {
    let entry = format!("ssh://127.0.0.1:{port}");
    let cfg = HostConfig {
        bind: "127.0.0.1:0".parse().unwrap(),
        identity: Identity::generate().unwrap(),
        stun: vec![],
        publish_host_candidates: true,
        service: "caller test".into(),
        default: None,
        scope: Scope::new(vec![Entry::parse(&entry).unwrap()]),
        services: vec![("echo".into(), Sink::Dial(Entry::parse(&entry).unwrap()))],
        max_sessions: 4,
        max_streams: 8,
        idle_timeout: Duration::from_secs(300),
        connect_timeout: Duration::from_secs(5),
        stun_refresh: Duration::from_secs(25),
        direct,
        hello_scope: false,
        forward: Forward::None,
        accept: Vec::new(),
    };
    let mut host = Host::start(cfg).unwrap();
    let rtc = match host.next_event().await {
        Some(Event::Record { rtc }) => rtc,
        other => panic!("the host's first word is its record, not {other:?}"),
    };
    (host, Record::decode(&rtc).unwrap())
}

/// A stream the caller opened carries bytes both ways, 1 MiB of them, which
/// is the answerer's credit spent and topped up many times over.
async fn round_trip(mut stream: tokio::io::DuplexStream) {
    const SIZE: usize = 1 << 20;
    let body: Vec<u8> = (0..SIZE).map(|i| (i % 251) as u8).collect();
    let (mut r, mut w) = tokio::io::split(&mut stream);
    let send = async {
        w.write_all(&body).await.unwrap();
    };
    let recv = async {
        let mut got = vec![0u8; SIZE];
        r.read_exact(&mut got).await.unwrap();
        got
    };
    let (_, got) = tokio::time::timeout(LIMIT, async { tokio::join!(send, recv) })
        .await
        .unwrap();
    assert!(got == body, "the bytes come back intact and in order");
}

// depth: the tests

#[tokio::test]
async fn a_signaled_caller_reaches_an_address_in_scope() {
    let port = echo_server().await;
    let (mut host, record) = host(port, false).await;
    let caller = Caller::new("127.0.0.1:0".parse().unwrap()).await.unwrap();
    host.send(Command::Open {
        peer: "native".into(),
        rtc: caller.record().encode().unwrap(),
    });
    let call = caller.connect(&record, LIMIT).await.unwrap();
    assert!(
        call.hello().contains("\"services\":[\"echo\"]"),
        "{}",
        call.hello()
    );
    let target = Target::Address {
        host: "127.0.0.1".into(),
        port,
    };
    let (stream, _) = call.open(&target).await.unwrap();
    round_trip(stream).await;
    loop {
        match tokio::time::timeout(LIMIT, host.next_event())
            .await
            .unwrap()
        {
            Some(Event::Session {
                peer,
                state: SessionState::Connected,
                ..
            }) if peer == "native" => break,
            Some(_) => {}
            None => panic!("the host stopped"),
        }
    }
}

#[tokio::test]
async fn a_direct_mode_caller_reaches_a_named_service_with_no_signaling() {
    let port = echo_server().await;
    let (_host, record) = host(port, true).await;
    let caller = Caller::direct("127.0.0.1:0".parse().unwrap())
        .await
        .unwrap();
    assert_eq!(caller.record().ufrag.len(), 32);
    assert_eq!(caller.record().ufrag, caller.record().pwd);
    let call = caller.connect(&record, LIMIT).await.unwrap();
    let (stream, _) = call.open(&Target::parse("echo").unwrap()).await.unwrap();
    round_trip(stream).await;
}

#[tokio::test]
async fn a_name_the_answerer_does_not_serve_ends_the_stream_with_0x48() {
    let port = echo_server().await;
    let (_host, record) = host(port, true).await;
    let call = Caller::direct("127.0.0.1:0".parse().unwrap())
        .await
        .unwrap()
        .connect(&record, LIMIT)
        .await
        .unwrap();
    let (mut stream, closed) = call.open(&Target::parse("telnet").unwrap()).await.unwrap();
    assert_eq!(tokio::time::timeout(LIMIT, closed).await.unwrap(), Ok(0x48));
    let mut buf = [0u8; 1];
    assert_eq!(stream.read(&mut buf).await.unwrap(), 0, "the stream ends");
}

#[test]
fn a_target_is_a_service_name_or_host_and_port() {
    assert_eq!(Target::parse("ssh"), Ok(Target::Service("ssh".into())));
    assert_eq!(
        Target::parse("127.0.0.1:22"),
        Ok(Target::Address {
            host: "127.0.0.1".into(),
            port: 22
        })
    );
    assert_eq!(
        Target::parse("[::1]:22"),
        Ok(Target::Address {
            host: "::1".into(),
            port: 22
        })
    );
    assert!(Target::parse("SSH").is_err());
    assert!(Target::parse("host:0").is_err());
    // `doc/P2P.md` §5.1: nothing asks for what the far side forwards to,
    // and `:port` for one port of it.
    assert_eq!(Target::parse(""), Ok(Target::Default));
    assert_eq!(Target::parse(":8080"), Ok(Target::Port(8080)));
    assert!(Target::parse(":0").is_err());
}
