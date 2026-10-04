//! `--turn` (`doc/P2P.md` §2.6): a session carried through a TURN
//! allocation, end to end on loopback.
//!
//! Loopback cannot make a symmetric NAT, so the host is told to use its
//! relayed address and nothing else ([`RelayPolicy::Only`]); the caller
//! keeps its own candidates. A session that comes up then crossed the
//! relay, and the relay's own byte count says so. The allocation is made
//! by `drt p2p`'s own [`allocate`], against `drt turn`'s server, with a
//! credential minted as `crypto/turn_credential` mints one.
//!
//! ## surface block
//!
//! - Entry points: the tests below.
//! - Configurable: [`SECRET`], [`LIMIT`].
//! - Fan-out: none.

#![cfg(all(feature = "turn", feature = "turn-client", feature = "webrtc"))]

use std::time::Duration;

use drt::p2p::turn::{allocate, TurnUri};
use drt_config::TurnConfig;
use drt_rtc::caller::{Caller, Target};
use drt_rtc::host::Event;
use drt_rtc::{
    Command, Entry, Forward, Host, HostConfig, Identity, Record, RelayPolicy, Scope, Sink,
};
use ego_transport::turn::ephemeral_credentials_for;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const SECRET: &str = "a-shared-secret-coturn-would-accept-too";
const LIMIT: Duration = Duration::from_secs(15);

/// One runtime for the whole binary, never dropped: the tokio teardown
/// use-after-free `tests/stun.rs` documents.
fn rt() -> &'static tokio::runtime::Runtime {
    static RT: std::sync::OnceLock<tokio::runtime::Runtime> = std::sync::OnceLock::new();
    RT.get_or_init(|| tokio::runtime::Runtime::new().expect("a tokio runtime"))
}

// depth: the relay, the target and the host

fn turn_config() -> TurnConfig {
    TurnConfig {
        bind: "127.0.0.1:0".into(),
        relay_address: "127.0.0.1".into(),
        relay_bind: "127.0.0.1".into(),
        realm: "drt".into(),
        key: Some(SECRET.into()),
        key_file: None,
        key_env: None,
        max_allocations: 4,
        queue: "turn_in".into(),
        report_ms: 0,
    }
}

async fn echo_server() -> u16 {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
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

fn host_config(port: u16) -> HostConfig {
    let entry = Entry::parse(&format!("ssh://127.0.0.1:{port}")).unwrap();
    HostConfig {
        bind: "127.0.0.1:0".parse().unwrap(),
        identity: Identity::generate().unwrap(),
        stun: vec![],
        publish_host_candidates: true,
        service: "turn test".into(),
        default: None,
        scope: Scope::new(vec![entry.clone()]),
        services: vec![("echo".into(), Sink::Dial(entry))],
        max_sessions: 4,
        max_streams: 8,
        idle_timeout: Duration::from_secs(300),
        connect_timeout: Duration::from_secs(5),
        stun_refresh: Duration::from_secs(25),
        direct: false,
        hello_scope: false,
        forward: Forward::None,
        caps: Vec::new(),
        accept: Vec::new(),
    }
}

// depth: the tests

#[test]
fn a_session_crosses_a_turn_allocation_when_it_is_the_only_path() {
    rt().block_on(async {
        let server = drt::turn::bind(&turn_config(), None).await.unwrap();
        let (username, password) =
            ephemeral_credentials_for(SECRET, Duration::from_secs(60), "p2p").unwrap();
        let uri = TurnUri {
            server: server.local_addr().to_string(),
            username,
            password,
        };
        let relayed = allocate(&uri).await.expect("the credential allocates");
        let relayed_addr = relayed.address;

        let port = echo_server().await;
        let mut host = Host::start_relayed(host_config(port), relayed, RelayPolicy::Only).unwrap();
        let rtc = match host.next_event().await {
            Some(Event::Record { rtc }) => rtc,
            other => panic!("the host's first word is its record, not {other:?}"),
        };
        // The record names the relayed address, in `r`, and nothing else:
        // a page's reader sees no candidate at all.
        let record = Record::decode(&rtc).unwrap();
        assert!(record.candidates.is_empty(), "{rtc}");
        assert_eq!(record.relays.len(), 1, "{rtc}");
        assert!(
            record.relays[0].contains(&format!(
                "{} {} typ relay",
                relayed_addr.ip(),
                relayed_addr.port()
            )),
            "{rtc}"
        );

        let caller = Caller::new("127.0.0.1:0".parse().unwrap()).await.unwrap();
        host.send(Command::Open {
            peer: "through-turn".into(),
            rtc: caller.record().encode().unwrap(),
        });
        let call = caller
            .connect(&record, LIMIT)
            .await
            .expect("a session through the relay");
        // The caller named no TURN server, and still knows it crossed one.
        assert!(call.via_turn());
        let (mut stream, _) = call.open(&Target::Service("echo".into())).await.unwrap();
        let body: Vec<u8> = (0..256 * 1024).map(|i| (i % 251) as u8).collect();
        let (mut r, mut w) = tokio::io::split(&mut stream);
        let send = async { w.write_all(&body).await.unwrap() };
        let recv = async {
            let mut got = vec![0u8; body.len()];
            r.read_exact(&mut got).await.unwrap();
            got
        };
        let (_, got) = tokio::time::timeout(LIMIT, async { tokio::join!(send, recv) })
            .await
            .expect("the echo came back through the relay");
        assert!(got == body);

        // The relay carried it: at least the 256 KiB one way (the server
        // counts what it relays to the allocation's holder), and more for
        // ICE, DTLS and SCTP.
        let carried: u64 = server
            .allocations()
            .await
            .unwrap()
            .iter()
            .map(|a| a.relayed_bytes)
            .sum();
        assert!(
            carried > body.len() as u64,
            "the relay carried {carried} bytes"
        );
    });
}

#[test]
fn a_forged_credential_is_refused_by_name() {
    rt().block_on(async {
        let server = drt::turn::bind(&turn_config(), None).await.unwrap();
        let (username, password) = ephemeral_credentials_for(
            "not-the-secret-but-as-long-as-one",
            Duration::from_secs(60),
            "p2p",
        )
        .unwrap();
        let uri = TurnUri {
            server: server.local_addr().to_string(),
            username,
            password,
        };
        let Err(e) = allocate(&uri).await else {
            panic!("a forged credential allocated");
        };
        assert!(e.contains(&uri.shown()), "{e}");
        assert!(
            !e.contains(&uri.password),
            "the password is never in an error: {e}"
        );
    });
}
