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
//! - Fan-out: [`Client`] is the browser; [`host_with`] is the host;
//!   [`echo_server`] and [`sink_server`] are the targets.

use std::net::SocketAddr;
use std::time::Duration;

use drt_rtc::host::{Event, SessionState, StreamState};
use drt_rtc::wisp::{self, reason, Packet};
use drt_rtc::{Command, Entry, Host, HostConfig, Identity, Record, Scope};
use str0m::channel::{ChannelConfig, ChannelId, Reliability};
use str0m::config::Fingerprint;
use str0m::net::{Protocol, Receive};
use str0m::{Candidate, Event as RtcEvent, IceCreds, Input, Output, Rtc};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, UdpSocket};
use tokio::time::Instant;

const LIMIT: Duration = Duration::from_secs(10);

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
    };
    let mut host = Host::start(cfg).await.unwrap();
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

// depth: the browser

struct Client {
    rtc: Rtc,
    socket: UdpSocket,
    addr: SocketAddr,
    control: ChannelId,
    wisp: ChannelId,
    open: [bool; 2],
    connected: bool,
    hello: Option<String>,
    inbox: Vec<Vec<u8>>,
}

impl Client {
    /// What a browser does with the host's record: everything but the SDP.
    async fn new(host: &Record) -> (Client, String) {
        let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let addr = socket.local_addr().unwrap();
        let creds = IceCreds::new();
        let mut rtc = Rtc::builder()
            .set_local_ice_credentials(creds.clone())
            .build(Instant::now().into_std());
        let local = Candidate::host(addr, "udp").unwrap();
        rtc.add_local_candidate(local.clone());
        let fingerprint: [u8; 32] = rtc
            .direct_api()
            .local_dtls_fingerprint()
            .bytes
            .clone()
            .try_into()
            .unwrap();
        let mut api = rtc.direct_api();
        api.set_ice_controlling(true);
        api.set_remote_ice_credentials(IceCreds {
            ufrag: host.ufrag.clone(),
            pass: host.pwd.clone(),
        });
        api.set_remote_fingerprint(Fingerprint {
            hash_func: "sha-256".into(),
            bytes: host.fingerprint.to_vec(),
        });
        api.start_dtls(true).unwrap();
        api.start_sctp(true);
        let channel = |label: &str, id: u16| ChannelConfig {
            label: label.into(),
            ordered: true,
            reliability: Reliability::Reliable,
            negotiated: Some(id),
            protocol: String::new(),
        };
        let control = api.create_data_channel(channel("control", 0));
        let wisp = api.create_data_channel(channel("wisp", 1));
        for c in &host.candidates {
            rtc.add_remote_candidate(Candidate::from_sdp_string(c).unwrap());
        }
        let record = Record {
            ufrag: creds.ufrag,
            pwd: creds.pass,
            fingerprint,
            candidates: vec![local.to_sdp_string()],
        };
        let client = Client {
            rtc,
            socket,
            addr,
            control,
            wisp,
            open: [false; 2],
            connected: false,
            hello: None,
            inbox: Vec::new(),
        };
        (client, record.encode().unwrap())
    }

    fn drain(&mut self) -> Instant {
        loop {
            match self.rtc.poll_output().unwrap() {
                Output::Timeout(t) => return Instant::from_std(t),
                Output::Transmit(t) => {
                    let _ = self.socket.try_send_to(&t.contents, t.destination);
                }
                Output::Event(e) => match e {
                    RtcEvent::Connected => self.connected = true,
                    RtcEvent::ChannelOpen(id, _) if id == self.control => self.open[0] = true,
                    RtcEvent::ChannelOpen(id, _) if id == self.wisp => self.open[1] = true,
                    RtcEvent::ChannelData(d) if d.id == self.control => {
                        self.hello = Some(String::from_utf8(d.data).unwrap())
                    }
                    RtcEvent::ChannelData(d) if d.id == self.wisp => self.inbox.push(d.data),
                    _ => {}
                },
            }
        }
    }

    /// Run the client until `done` says so, or fail the test at [`LIMIT`].
    async fn until(&mut self, what: &str, mut done: impl FnMut(&mut Client) -> bool) {
        let deadline = Instant::now() + LIMIT;
        let mut buf = vec![0u8; 2000];
        loop {
            let wake = self.drain();
            if done(self) {
                return;
            }
            let now = Instant::now();
            assert!(now < deadline, "the client gave up waiting for {what}");
            let wait = wake
                .min(deadline)
                .saturating_duration_since(now)
                .max(Duration::from_millis(1));
            let input = match tokio::time::timeout(wait, self.socket.recv_from(&mut buf)).await {
                Ok(Ok((n, source))) => Input::Receive(
                    Instant::now().into_std(),
                    Receive {
                        proto: Protocol::Udp,
                        source,
                        destination: self.addr,
                        contents: buf[..n].try_into().unwrap(),
                    },
                ),
                _ => Input::Timeout(Instant::now().into_std()),
            };
            self.rtc.handle_input(input).unwrap();
        }
    }

    async fn connect(&mut self) {
        self.until("both channels to open", |c| c.open == [true, true])
            .await;
        self.until("hello on control", |c| c.hello.is_some()).await;
        let credit = self.next_packet().await;
        assert_eq!(
            credit,
            wisp::cont(0, 128),
            "CONTINUE on stream 0 comes first"
        );
    }

    fn send(&mut self, pkt: Vec<u8>) {
        assert!(self
            .rtc
            .channel(self.wisp)
            .unwrap()
            .write(true, &pkt)
            .unwrap());
    }

    async fn next_packet(&mut self) -> Vec<u8> {
        self.until("a wisp packet", |c| !c.inbox.is_empty()).await;
        self.inbox.remove(0)
    }

    /// Everything that arrives for `stream`, until its CLOSE or `want`
    /// bytes of DATA, returned as (data, close reason).
    async fn read_stream(&mut self, stream: u32, want: usize) -> (Vec<u8>, Option<u8>) {
        let mut data = Vec::new();
        loop {
            let pkt = self.next_packet().await;
            match wisp::parse(&pkt).unwrap() {
                Packet::Data { stream: s, payload } if s == stream => {
                    data.extend_from_slice(payload);
                    if data.len() >= want {
                        return (data, None);
                    }
                }
                Packet::Close { stream: s, reason } if s == stream => return (data, Some(reason)),
                _ => {}
            }
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

#[tokio::test]
async fn a_large_download_arrives_whole_and_in_order() {
    // Past str0m's 128 KiB send buffer many times over, so the outbox and
    // the gate both have to work for this to come out right.
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
