//! Browser access signaling over the Discofetch socket, end to end: the
//! real `drt start` running `crates/drt-rtc/signal/host.dlua` with the
//! `webrtc` block and the `ws` connector, against a stub of the API that
//! speaks the host side of the contract over `wss://`, and drt-rtc's native
//! client playing the browser (`doc/BrowserAccess.md` §7).
//!
//! ## surface block
//!
//! - Entry points: the `#[tokio::test]`s below, one per test the signaling
//!   contract asks for (1 to 7, in its order).
//! - Configurable: [`LIMIT`] (the client's, from `peer.rs`), [`QUIET`], how
//!   long "nothing happens" is watched for.
//! - Fan-out: [`Stub`] is the API; [`Host`] is `drt start`; [`Client`] is
//!   the browser; [`echo_server`], [`sink_server`] and [`stun_server`] are
//!   the targets and the NAT.

#![cfg(all(
    feature = "webrtc",
    feature = "connector-ws",
    feature = "connector-time"
))]
// The handshake callback's error type is tungstenite's, not ours to shrink.
#![allow(clippy::result_large_err)]

use std::io::{BufRead, BufReader};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use drt_rtc::wisp;
use drt_rtc::Record;
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, UdpSocket};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::handshake::server::{Request, Response};
use tokio_tungstenite::tungstenite::Message;

#[path = "../../drt-rtc/tests/common/peer.rs"]
mod peer;
use peer::{Client, LIMIT};

const QUIET: Duration = Duration::from_secs(4);

// depth: the API stub

/// What the stub saw: a connection (numbered from 1) and its credential, a
/// frame on one, or one going away.
#[derive(Debug)]
enum Seen {
    Connected(usize, Option<String>),
    Frame(#[allow(dead_code)] usize, Value),
    Gone(#[allow(dead_code)] usize),
}

/// What the test tells the connection that is currently the host's.
enum Say {
    Frame(Value),
    /// Drop the socket without a close frame, as a crashed API would.
    Drop,
    /// Close it with this code, as the API does.
    Close(u16, &'static str),
}

struct Stub {
    port: u16,
    cert: PathBuf,
    seen: mpsc::UnboundedReceiver<Seen>,
    current: Arc<Mutex<Option<mpsc::UnboundedSender<Say>>>>,
    /// Frames seen but not yet asked for, so a wait for one kind of frame
    /// does not lose the others.
    backlog: Vec<Seen>,
}

impl Stub {
    async fn start(dir: &Path) -> Stub {
        use tokio_rustls::rustls::pki_types::{CertificateDer, PrivatePkcs8KeyDer};
        use tokio_rustls::rustls::ServerConfig;
        let issued = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
        let cert = dir.join("stub.pem");
        std::fs::write(&cert, issued.cert.pem()).unwrap();
        let config = ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(
                vec![CertificateDer::from(issued.cert.der().to_vec())],
                PrivatePkcs8KeyDer::from(issued.key_pair.serialize_der()).into(),
            )
            .unwrap();
        let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (seen_tx, seen) = mpsc::unbounded_channel();
        let current: Arc<Mutex<Option<mpsc::UnboundedSender<Say>>>> = Arc::default();
        let slot = current.clone();
        tokio::spawn(async move {
            let mut n = 0;
            while let Ok((tcp, _)) = listener.accept().await {
                let Ok(tls) = acceptor.accept(tcp).await else {
                    continue;
                };
                let auth = Arc::new(Mutex::new(None));
                let got = auth.clone();
                let callback = move |req: &Request, res: Response| {
                    *got.lock().unwrap() = req
                        .headers()
                        .get("authorization")
                        .and_then(|v| v.to_str().ok())
                        .map(String::from);
                    Ok(res)
                };
                let Ok(ws) = tokio_tungstenite::accept_hdr_async(tls, callback).await else {
                    continue;
                };
                n += 1;
                let id = n;
                let _ = seen_tx.send(Seen::Connected(id, auth.lock().unwrap().clone()));
                let (say_tx, mut say) = mpsc::unbounded_channel();
                *slot.lock().unwrap() = Some(say_tx);
                let seen_tx = seen_tx.clone();
                tokio::spawn(async move {
                    let (mut sink, mut stream) = ws.split();
                    loop {
                        tokio::select! {
                            m = stream.next() => match m {
                                Some(Ok(Message::Text(t))) => {
                                    let v: Value = serde_json::from_str(&t).expect("the host sends JSON");
                                    let _ = seen_tx.send(Seen::Frame(id, v));
                                }
                                Some(Ok(_)) => {}
                                _ => { let _ = seen_tx.send(Seen::Gone(id)); return; }
                            },
                            s = say.recv() => match s {
                                Some(Say::Frame(v)) => { let _ = sink.send(Message::Text(v.to_string())).await; }
                                Some(Say::Close(code, reason)) => {
                                    use tokio_tungstenite::tungstenite::protocol::{frame::coding::CloseCode, CloseFrame};
                                    let frame = CloseFrame { code: CloseCode::from(code), reason: reason.into() };
                                    let _ = sink.send(Message::Close(Some(frame))).await;
                                    let _ = seen_tx.send(Seen::Gone(id));
                                    return;
                                }
                                Some(Say::Drop) | None => { let _ = seen_tx.send(Seen::Gone(id)); return; }
                            },
                        }
                    }
                });
            }
        });
        Stub {
            port,
            cert,
            seen,
            current,
            backlog: Vec::new(),
        }
    }

    fn say(&self, v: Value) {
        let slot = self.current.lock().unwrap();
        slot.as_ref()
            .expect("no host is connected")
            .send(Say::Frame(v))
            .unwrap();
    }

    fn close(&self, code: u16, reason: &'static str) {
        let slot = self.current.lock().unwrap();
        slot.as_ref()
            .expect("no host is connected")
            .send(Say::Close(code, reason))
            .unwrap();
    }

    fn drop_socket(&self) {
        let slot = self.current.lock().unwrap();
        slot.as_ref()
            .expect("no host is connected")
            .send(Say::Drop)
            .unwrap();
    }

    /// The first thing seen that `want` accepts, within `within`, keeping
    /// everything else for later waits.
    async fn wait_for(
        &mut self,
        within: Duration,
        mut want: impl FnMut(&Seen) -> bool,
    ) -> Option<Seen> {
        if let Some(i) = self.backlog.iter().position(&mut want) {
            return Some(self.backlog.remove(i));
        }
        let deadline = tokio::time::Instant::now() + within;
        loop {
            match tokio::time::timeout_at(deadline, self.seen.recv()).await {
                Ok(Some(s)) if want(&s) => return Some(s),
                Ok(Some(s)) => self.backlog.push(s),
                _ => return None,
            }
        }
    }

    async fn frame(&mut self, t: &str) -> Value {
        let t = t.to_string();
        match self
            .wait_for(LIMIT, |s| matches!(s, Seen::Frame(_, v) if v["t"] == t))
            .await
        {
            Some(Seen::Frame(_, v)) => v,
            _ => panic!("the host sent no `{t}`"),
        }
    }

    async fn connection(&mut self) -> (usize, Option<String>) {
        match self
            .wait_for(LIMIT, |s| matches!(s, Seen::Connected(..)))
            .await
        {
            Some(Seen::Connected(n, auth)) => (n, auth),
            _ => panic!("the host never connected"),
        }
    }

    /// Every frame of type `t` seen so far, and any that arrive in `within`.
    async fn all(&mut self, t: &str, within: Duration) -> Vec<Value> {
        let deadline = tokio::time::Instant::now() + within;
        while let Ok(Some(s)) = tokio::time::timeout_at(deadline, self.seen.recv()).await {
            self.backlog.push(s);
        }
        let mut out = Vec::new();
        self.backlog.retain(|s| match s {
            Seen::Frame(_, v) if v["t"] == t => {
                out.push(v.clone());
                false
            }
            _ => true,
        });
        out
    }

    /// The host's record, off its first `record` frame.
    async fn record(&mut self) -> Record {
        let frame = self.frame("record").await;
        Record::decode(&frame["record"].to_string()).expect("the host's record decodes")
    }
}

// depth: the host, as a deployment

struct Host {
    child: Child,
    log: Arc<Mutex<Vec<String>>>,
    _dir: tempfile::TempDir,
}

impl Drop for Host {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        if std::thread::panicking() {
            eprintln!("--- drt start said:");
            for line in self.log.lock().unwrap().iter() {
                eprintln!("{line}");
            }
        }
    }
}

impl Host {
    fn exited(&mut self) -> bool {
        self.child.try_wait().unwrap().is_some()
    }
}

/// `drt start` with the signaling program, the socket at the stub, one
/// target in scope, and whatever `webrtc` settings the test adds.
fn host(stub: &Stub, dir: tempfile::TempDir, target: u16, webrtc: Value) -> Host {
    let program = Path::new(env!("CARGO_MANIFEST_DIR")).join("../drt-rtc/signal/host.dlua");
    let mut block = json!({
        "bind": "127.0.0.1:0",
        "identity_file": dir.path().join("identity.json"),
        "service": "signal test",
        "scope": [format!("http://127.0.0.1:{target}")],
        "default": format!("http://127.0.0.1:{target}"),
    });
    for (k, v) in webrtc.as_object().unwrap() {
        block[k] = v.clone();
    }
    let config = json!({
        "program": {"path": program},
        "caps": [
            {"capability": "host:ws/*"},
            {"capability": "host:time/monotonic"}
        ],
        "connectors": {
            "time": {},
            "ws": {"scope": {
                "allow": [format!("wss://localhost:{}", stub.port)],
                "allow_private": true,
                "extra_roots": [stub.cert.clone()]
            }}
        },
        "args": {
            "key": "advertise-token",
            "signal": format!("wss://localhost:{}/?room=topic", stub.port)
        },
        "webrtc": block,
    });
    let path = dir.path().join("host.json");
    std::fs::write(&path, serde_json::to_vec_pretty(&config).unwrap()).unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_drt"))
        .arg("--config")
        .arg(&path)
        .arg("start")
        .current_dir(dir.path())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let log = Arc::new(Mutex::new(Vec::new()));
    for pipe in [
        Box::new(child.stdout.take().unwrap()) as Box<dyn std::io::Read + Send>,
        Box::new(child.stderr.take().unwrap()),
    ] {
        let log = log.clone();
        std::thread::spawn(move || {
            for line in BufReader::new(pipe).lines().map_while(Result::ok) {
                log.lock().unwrap().push(line);
            }
        });
    }
    Host {
        child,
        log,
        _dir: dir,
    }
}

/// The stub, the host connected to it, and the host's record.
async fn deployment(target: u16, webrtc: Value) -> (Stub, Host, Record) {
    let dir = tempfile::tempdir().unwrap();
    let mut stub = Stub::start(dir.path()).await;
    let host = host(&stub, dir, target, webrtc);
    let (_, auth) = stub.connection().await;
    assert_eq!(auth.as_deref(), Some("Bearer advertise-token"));
    let record = stub.record().await;
    stub.say(json!({"t": "ready", "v": 1, "room": "topic"}));
    (stub, host, record)
}

/// Announce a browser as the API would, and bring it up.
async fn announce(stub: &Stub, record: &Record, session: &str, as_object: bool) -> Client {
    let (mut client, rtc) = Client::new(record).await;
    let record: Value = if as_object {
        serde_json::from_str(&rtc).unwrap()
    } else {
        Value::String(rtc)
    };
    stub.say(json!({
        "t": "peer", "session": session, "record": record,
        "observed": {"address": "127.0.0.1"}
    }));
    client.connect().await;
    client
}

// depth: the targets, and a NAT that moves

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

async fn sink_server() -> (u16, mpsc::UnboundedReceiver<&'static str>) {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = l.local_addr().unwrap().port();
    let (tx, rx) = mpsc::unbounded_channel();
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

/// A STUN server that maps the host to `first` for its first `switch`
/// answers and to `then` after: a NAT whose mapping moved.
async fn stun_server(first: SocketAddr, then: SocketAddr, switch: usize) -> u16 {
    use ego_transport::stun::{decode, encode_binding_success, StunMessage};
    let sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let port = sock.local_addr().unwrap().port();
    tokio::spawn(async move {
        let mut buf = [0u8; 1500];
        let mut answered = 0;
        while let Ok((n, from)) = sock.recv_from(&mut buf).await {
            if let Ok(StunMessage::BindingRequest { txid }) = decode(&buf[..n]) {
                let mapped = if answered < switch { first } else { then };
                answered += 1;
                let _ = sock
                    .send_to(&encode_binding_success(&txid, mapped), from)
                    .await;
            }
        }
    });
    port
}

/// One CONNECT to `port`, a few bytes there and back.
async fn round_trip(client: &mut Client, stream: u32, port: u16) {
    client.send(wisp::connect(stream, wisp::STREAM_TCP, port, "127.0.0.1"));
    client.send(wisp::data(stream, b"through the host"));
    let (echoed, closed) = client.read_stream(stream, 16).await;
    assert_eq!(
        (echoed.as_slice(), closed),
        (&b"through the host"[..], None)
    );
}

// depth: the tests, in the contract's order

/// 1. The host connects with its token and sends its record; a `peer`
/// makes a session, the channels open, and an allowed CONNECT round-trips.
#[tokio::test]
async fn a_peer_makes_a_session_and_an_allowed_connect_round_trips() {
    let echo = echo_server().await;
    let (mut stub, _host, record) = deployment(echo, json!({})).await;
    assert!(
        !record.candidates.is_empty(),
        "the record carries the host candidate"
    );
    // The record as an object, as the API forwards it.
    let mut client = announce(&stub, &record, "s1", true).await;
    round_trip(&mut client, 1, echo).await;
    let outcome = stub.frame("outcome").await;
    assert_eq!(
        (outcome["session"].as_str(), outcome["path"].as_str()),
        (Some("s1"), Some("direct"))
    );
}

/// 2. `bye` from the API closes the session and its TCP sockets, and the
/// host does not echo a `bye` back.
#[tokio::test]
async fn bye_closes_the_session_and_its_sockets() {
    let (sink, mut sank) = sink_server().await;
    let (mut stub, _host, record) = deployment(sink, json!({})).await;
    let mut client = announce(&stub, &record, "s1", false).await;
    client.send(wisp::connect(1, wisp::STREAM_TCP, sink, "127.0.0.1"));
    client.until("the stream to settle", |_| true).await;
    assert_eq!(
        tokio::time::timeout(LIMIT, sank.recv()).await.unwrap(),
        Some("accepted")
    );
    stub.say(json!({"t": "bye", "session": "s1", "reason": "left"}));
    assert_eq!(
        tokio::time::timeout(LIMIT, sank.recv()).await.unwrap(),
        Some("closed"),
        "the TCP socket closes with the session"
    );
    assert!(
        stub.all("bye", QUIET).await.is_empty(),
        "the host echoed the API's bye"
    );
}

/// 3. ICE from a browser the API never announced finds no session.
#[tokio::test]
async fn ice_from_an_unannounced_browser_gets_no_session() {
    let echo = echo_server().await;
    let (mut stub, _host, record) = deployment(echo, json!({})).await;
    let (mut client, _rtc) = Client::new(&record).await;
    assert!(
        !client.within(QUIET, |c| c.connected).await,
        "a browser connected without a `peer`"
    );
    assert!(stub.all("outcome", Duration::ZERO).await.is_empty());
}

/// 4. The socket drops: the session carries on, the host dials again with
/// backoff and sends its record, and a new `peer` works.
#[tokio::test]
async fn sessions_outlive_the_socket_and_the_host_redials() {
    let echo = echo_server().await;
    let (mut stub, _host, record) = deployment(echo, json!({})).await;
    let mut first = announce(&stub, &record, "s1", false).await;
    stub.frame("outcome").await;
    stub.drop_socket();
    round_trip(&mut first, 1, echo).await;
    let (n, auth) = stub.connection().await;
    assert_eq!((n, auth.as_deref()), (2, Some("Bearer advertise-token")));
    let again = stub.record().await;
    assert_eq!(again, record, "the same record on the new socket");
    stub.say(json!({"t": "ready", "v": 1, "room": "topic"}));
    round_trip(&mut first, 3, echo).await;
    let mut second = announce(&stub, &record, "s2", true).await;
    round_trip(&mut second, 1, echo).await;
}

/// 5. A close code in 4000..=4099 -- 4001, replaced by a newer host
/// socket; 4003, the credential refused -- stops the host for good, and
/// `drt start` exits.
#[tokio::test]
async fn an_api_close_code_stops_the_host_for_good() {
    let echo = echo_server().await;
    for (code, reason) in [(4001, "replaced"), (4003, "credential refused")] {
        let (mut stub, mut host, _record) = deployment(echo, json!({})).await;
        stub.close(code, reason);
        assert!(
            stub.wait_for(QUIET, |s| matches!(s, Seen::Connected(..)))
                .await
                .is_none(),
            "the host dialed again after {code}"
        );
        let log = host.log.lock().unwrap().join("\n");
        assert!(
            log.contains(&format!(
                "closed {code} {reason}: the API does not want this host back"
            )),
            "{log}"
        );
        assert!(
            host.exited(),
            "the host is still running after {code}:\n{log}"
        );
    }
}

/// 5, the other half: any other close is redialed with backoff.
#[tokio::test]
async fn any_other_close_is_redialed() {
    let echo = echo_server().await;
    let (mut stub, _host, _record) = deployment(echo, json!({})).await;
    stub.close(1011, "restarting");
    let (n, auth) = stub.connection().await;
    assert_eq!((n, auth.as_deref()), (2, Some("Bearer advertise-token")));
    stub.record().await;
}

/// 6. Exactly one `outcome` per session: `direct` for one that connected,
/// `failed` for one refused at the cap, whatever happens after.
#[tokio::test]
async fn exactly_one_outcome_per_session() {
    let echo = echo_server().await;
    let (mut stub, _host, record) = deployment(echo, json!({"max_sessions": 1})).await;
    let _up = announce(&stub, &record, "s1", false).await;
    let (_client, rtc) = Client::new(&record).await;
    stub.say(
        json!({"t": "peer", "session": "s2", "record": rtc, "observed": {"address": "127.0.0.1"}}),
    );
    let bye = stub.frame("bye").await;
    assert_eq!(
        (bye["session"].as_str(), bye["reason"].as_str()),
        (Some("s2"), Some("busy"))
    );
    stub.say(json!({"t": "bye", "session": "s1", "reason": "left"}));
    let outcomes = stub.all("outcome", QUIET).await;
    let mut seen: Vec<(String, String)> = outcomes
        .iter()
        .map(|o| {
            (
                o["session"].as_str().unwrap().into(),
                o["path"].as_str().unwrap().into(),
            )
        })
        .collect();
    seen.sort();
    assert_eq!(
        seen,
        [
            ("s1".to_string(), "direct".to_string()),
            ("s2".into(), "failed".into())
        ]
    );
}

/// 7. When the server-reflexive address moves, the host sends a new record.
#[tokio::test]
async fn a_moved_mapping_sends_a_new_record() {
    let echo = echo_server().await;
    let first: SocketAddr = "203.0.113.7:40000".parse().unwrap();
    let then: SocketAddr = "203.0.113.8:40001".parse().unwrap();
    let stun = stun_server(first, then, 2).await;
    let dir = tempfile::tempdir().unwrap();
    let mut stub = Stub::start(dir.path()).await;
    let _host = host(
        &stub,
        dir,
        echo,
        json!({"stun": [format!("127.0.0.1:{stun}")], "stun_refresh_s": 1}),
    );
    stub.connection().await;
    let has = |r: &Record, a: SocketAddr| {
        r.candidates
            .iter()
            .any(|c| c.contains(&format!(" {} {} typ srflx", a.ip(), a.port())))
    };
    let mut records = Vec::new();
    let deadline = tokio::time::Instant::now() + LIMIT;
    while !records.iter().any(|r| has(r, then)) {
        assert!(
            tokio::time::Instant::now() < deadline,
            "no record with the moved mapping: {records:?}"
        );
        records.push(stub.record().await);
    }
    assert!(
        records.iter().any(|r| has(r, first)),
        "the first mapping was never published: {records:?}"
    );
    let last = records.last().unwrap();
    assert!(
        !has(last, first),
        "the old mapping is still advertised: {last:?}"
    );
}
