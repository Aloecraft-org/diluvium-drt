//! The relay end to end: a device parks, a caller claims, bytes splice both
//! ways — plus the refusals that make the keys keys.

#![cfg(feature = "relay")]

use drt::relay::{serve, Relay};
use drt_config::{RelayConfig, RelayLabel};
use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite::Message;

/// A port the OS just handed out, and nobody else holds *yet*.
///
/// `serve()` takes a bind string, not a listener, so a test has to name a
/// port before the relay owns it — which leaves a window between the probe
/// closing and the relay binding that any other test in the workspace can
/// win. It has been won: CI run 127's `test` job died on "relay cannot
/// bind 127.0.0.1:44073: Address already in use" with nothing in the
/// change anywhere near the relay. So a caller pairs this with
/// [`accepting`] and tries again if it loses the race, rather than
/// reporting someone else's port as this test's failure.
fn free_port() -> std::net::SocketAddr {
    let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = probe.local_addr().unwrap();
    drop(probe);
    addr
}

/// Wait for something to start accepting on `addr`, up to a second.
/// False means the relay never came up — the race above, almost always.
fn accepting(addr: std::net::SocketAddr) -> bool {
    for _ in 0..100 {
        if std::net::TcpStream::connect_timeout(&addr, std::time::Duration::from_millis(50)).is_ok()
        {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    false
}

/// How many times a lost port race is worth retrying before the failure
/// is real. Three is generous: losing twice in a row is not a race.
const PORT_TRIES: usize = 3;

async fn relay_on_port() -> (std::net::SocketAddr, std::sync::Arc<Relay>) {
    for attempt in 1..=PORT_TRIES {
        let (addr, relay) = start_relay(free_port()).await;
        if accepting(addr) {
            return (addr, relay);
        }
        assert!(
            attempt < PORT_TRIES,
            "the relay never accepted on {addr} in {PORT_TRIES} attempts, so this is \
             not a lost port race"
        );
    }
    unreachable!("the loop returns or asserts")
}

/// The relay itself, on the port it was told.
async fn start_relay(addr: std::net::SocketAddr) -> (std::net::SocketAddr, std::sync::Arc<Relay>) {
    let config = RelayConfig {
        bind: addr.to_string(),
        labels: [(
            "xps".to_string(),
            RelayLabel {
                park_key: "park-secret-0123456789".into(),
                caller_key: "caller-secret-987654321".into(),
            },
        )]
        .into(),
        queue: "relay_in".into(),
        reply_queue: String::new(),
        admit_timeout_ms: 2000,
    };
    let relay = Relay::new(config);
    let serving = relay.clone();
    tokio::spawn(async move {
        let _ = serve(serving).await;
    });
    (addr, relay)
}

/// Wait until `want` legs are parked under `label`.
///
/// A device's `/park` handshake completing is not the same moment as its
/// leg being claimable: `park_leg` registers the leg in its own task, just
/// after. A caller in between is told the device is not home, correctly --
/// and a test that claims without waiting is racing that window. In CI it
/// lost, and the device then sat out `PARK_IDLE` (300s) before the relay
/// dropped it, which is the `ResetWithoutClosingHandshake` that failed
/// this file. So: no sleeps, ask the relay.
async fn parked(relay: &Relay, label: &str, want: usize) {
    for _ in 0..1000 {
        if relay.parked(label) >= want {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    panic!(
        "{want} leg(s) never parked under {label}: {} are",
        relay.parked(label)
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_caller_and_a_device_splice_through_the_relay() {
    let (addr, relay) = relay_on_port().await;

    // The device parks a leg, as websocat would: a dumb pipe, key in the URL.
    let (mut device, _) =
        tokio_tungstenite::connect_async(format!("ws://{addr}/park/xps?k=park-secret-0123456789"))
            .await
            .expect("the device could not park");
    // Claimable, not merely connected: see `parked`.
    parked(&relay, "xps", 1).await;

    // A caller claims it. The claim manifests as the first caller byte —
    // exactly what an SSH client's banner is.
    let (mut caller, _) =
        tokio_tungstenite::connect_async(format!("ws://{addr}/s/xps?k=caller-secret-987654321"))
            .await
            .expect("the caller could not connect");
    caller
        .send(Message::Binary(b"SSH-2.0-caller\r\n".to_vec()))
        .await
        .unwrap();

    // The device sees the caller's bytes (its cue to dial 127.0.0.1:22
    // lazily)…
    let got = loop {
        match device.next().await.expect("device leg died").unwrap() {
            Message::Binary(b) => break b,
            Message::Ping(_) => continue,
            other => panic!("unexpected on device leg: {other:?}"),
        }
    };
    assert_eq!(&got[..], b"SSH-2.0-caller\r\n");

    // …and answers; the caller reads it back through the splice.
    device
        .send(Message::Binary(b"SSH-2.0-device\r\n".to_vec()))
        .await
        .unwrap();
    let back = loop {
        match caller.next().await.expect("caller leg died").unwrap() {
            Message::Binary(b) => break b,
            Message::Ping(_) => continue,
            other => panic!("unexpected on caller leg: {other:?}"),
        }
    };
    assert_eq!(&back[..], b"SSH-2.0-device\r\n");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_wrong_key_never_upgrades_and_an_empty_pool_says_not_home() {
    let (addr, _relay) = relay_on_port().await;

    // Wrong caller key: refused at the handshake — the WebSocket never
    // exists, which is what keeps an open relay from being an open proxy.
    let err = tokio_tungstenite::connect_async(format!("ws://{addr}/s/xps?k=wrong"))
        .await
        .unwrap_err();
    assert!(format!("{err}").contains("403"), "{err}");

    // Unknown label: same refusal, indistinguishable from a bad key.
    let err = tokio_tungstenite::connect_async(format!("ws://{addr}/s/nope?k=x"))
        .await
        .unwrap_err();
    assert!(format!("{err}").contains("403"), "{err}");

    // Right key, no parked leg: the device is not home, and the caller is
    // told so with a close, not a hang.
    let (mut caller, _) =
        tokio_tungstenite::connect_async(format!("ws://{addr}/s/xps?k=caller-secret-987654321"))
            .await
            .unwrap();
    match caller.next().await.unwrap().unwrap() {
        Message::Close(Some(frame)) => {
            assert!(frame.reason.contains("not home"), "{frame:?}");
        }
        other => panic!("expected a close, got {other:?}"),
    }
}

/// The whole triangle, DRT on all three corners: `drt relay` in the
/// middle, `park()` as the device (lazy local dial, replay of the first
/// bytes), and a caller claiming through `/s/`. Twice, back to back —
/// the second session proves replenish-on-claim parked a fresh leg while
/// the first was still alive.
#[cfg(feature = "tunnel")]
#[tokio::test(flavor = "multi_thread")]
async fn the_full_triangle_and_replenish_on_claim() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let (addr, relay) = relay_on_port().await;

    // The "sshd": a local echo server the device dials lazily.
    let echo = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let echo_addr = echo.local_addr().unwrap().to_string();
    tokio::spawn(async move {
        loop {
            let Ok((mut c, _)) = echo.accept().await else {
                continue;
            };
            tokio::spawn(async move {
                let mut b = [0u8; 4096];
                while let Ok(n) = c.read(&mut b).await {
                    if n == 0 || c.write_all(&b[..n]).await.is_err() {
                        break;
                    }
                }
            });
        }
    });

    // The device: parks, re-parks on claim, forever.
    let park_url = format!("ws://{addr}/park/xps?k=park-secret-0123456789");
    tokio::spawn(async move {
        let _ = drt::tunnel::park(&park_url, &echo_addr).await;
    });
    parked(&relay, "xps", 1).await;

    for round in 0..2u8 {
        let (mut caller, _) = tokio_tungstenite::connect_async(format!(
            "ws://{addr}/s/xps?k=caller-secret-987654321"
        ))
        .await
        .unwrap();
        let hello = format!("hello-{round}");
        caller
            .send(Message::Binary(hello.clone().into_bytes()))
            .await
            .unwrap();
        let back = loop {
            match caller.next().await.expect("caller leg died").unwrap() {
                Message::Binary(b) => break b,
                Message::Ping(_) => continue,
                Message::Close(f) => panic!("closed in round {round}: {f:?}"),
                _ => continue,
            }
        };
        assert_eq!(&back[..], hello.as_bytes(), "round {round}");
        // Leave the first session OPEN while the second claims: the fresh
        // leg must come from replenish, not from this session ending.
        if round == 1 {
            drop(caller);
        } else {
            std::mem::forget(caller);
            // What this round is actually asserting: a fresh leg is parked
            // by replenish-on-claim while the first session is still open.
            // Waiting for it beats sleeping and hoping.
            parked(&relay, "xps", 1).await;
        }
    }
}

// ---------------------------------------------------------------------------
// The control plane: the relay inside a deployment
// ---------------------------------------------------------------------------

/// A supervisor that watches the relay and arbitrates for it. It records
/// every event to an exported queue the test reads, and refuses any label
/// named `blocked` — which is the whole point of arbitration: a decision
/// the relay's static keys cannot express, made in Lua.
#[cfg(feature = "relay")]
const WATCHER: &str = "\
local ev  = queue.declare('relay_in',  {capacity = 64})\n\
local out = queue.declare('relay_out', {capacity = 64})\n\
local log = queue.declare('log',       {capacity = 64, exported = true})\n\
while true do\n\
  local id, m = queue.wait({ev})\n\
  if m.event == 'admit' then\n\
    queue.push(out, {tok = m.tok, ok = m.label ~= 'blocked'})\n\
    queue.push(log, 'admit:' .. m.label .. ':' .. tostring(m.label ~= 'blocked'))\n\
  elseif m.event == 'closed' then\n\
    queue.push(log, 'closed:' .. m.label .. ':' .. tostring(m.bytes))\n\
  else\n\
    queue.push(log, m.event .. ':' .. m.label)\n\
  end\n\
end\n";

/// Start a deployment whose config carries a relay, and hand back the
/// relay's address plus a way to read what the supervisor logged.
#[cfg(feature = "relay")]
fn deployment_with_relay(
    arbitrating: bool,
) -> (std::net::SocketAddr, std::sync::mpsc::Receiver<String>) {
    // The same lost-race retry as `relay_on_port`, and the site that
    // actually lost it (CI run 127).
    for attempt in 1..=PORT_TRIES {
        let (addr, rx) = start_deployment(arbitrating, free_port());
        if accepting(addr) {
            return (addr, rx);
        }
        assert!(
            attempt < PORT_TRIES,
            "the deployment's relay never accepted on {addr} in {PORT_TRIES} attempts, \
             so this is not a lost port race"
        );
    }
    unreachable!("the loop returns or asserts")
}

#[cfg(feature = "relay")]
fn start_deployment(
    arbitrating: bool,
    relay_addr: std::net::SocketAddr,
) -> (std::net::SocketAddr, std::sync::mpsc::Receiver<String>) {
    use drt_connector::{Dispatcher, Registry};
    let program = serde_json::to_string(WATCHER).unwrap();
    let reply_queue = if arbitrating { "relay_out" } else { "" };
    let cfg: drt_config::RootConfig = serde_json::from_str(&format!(
        r#"{{"program": {{"source": {program}}},
            "listeners": [{{"scheme": "http", "address": "127.0.0.1:0"}}],
            "relay": {{"bind": "{relay_addr}",
                       "queue": "relay_in", "reply_queue": "{reply_queue}",
                       "labels": {{"xps": {{"park_key": "pk-0123456789abcdef",
                                          "caller_key": "ck-0123456789abcdef"}},
                                  "blocked": {{"park_key": "pk-0123456789abcdef",
                                              "caller_key": "ck-0123456789abcdef"}}}}}}}}"#
    ))
    .unwrap();

    // The deployment runs on its own thread; the test reads its log queue
    // through a channel, since `Swarm` is not shared.
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let bound = drt::listen::bind(&cfg.listeners).unwrap();
        // A tiny wrapper loop: serve() owns the swarm, so drain the log by
        // letting the supervisor export it and polling from inside.
        let _ = drt::start::serve_with_observer(
            &cfg,
            Dispatcher::new(Registry::new()),
            bound,
            move |sw, root| {
                if let Some(inst) = sw.instance_mut(root) {
                    if let Some(q) = inst.queue("log") {
                        while let Ok(Some(raw)) = inst.pop(q) {
                            if let Ok(v) = rmpv::decode::read_value(&mut &raw[..]) {
                                if let Some(line) = v.as_str() {
                                    let _ = tx.send(line.to_string());
                                }
                            }
                        }
                    }
                }
            },
        );
    });
    // No fixed sleep: the caller waits for the relay to accept, which is
    // the thing this was sleeping in the hope of.
    (relay_addr, rx)
}

#[cfg(feature = "relay")]
fn drain(rx: &std::sync::mpsc::Receiver<String>, want: usize) -> Vec<String> {
    let mut lines = Vec::new();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while lines.len() < want && std::time::Instant::now() < deadline {
        if let Ok(l) = rx.recv_timeout(std::time::Duration::from_millis(200)) {
            lines.push(l);
        }
    }
    lines
}

/// Wait for one presence line, keeping whatever else arrives on the way.
///
/// The deployment's own view of "the leg is claimable" -- `park_leg` emits
/// `Parked` immediately after registering it -- so a test that has the
/// supervisor's channel should sequence on that rather than on a sleep.
#[cfg(feature = "relay")]
async fn until(rx: &std::sync::mpsc::Receiver<String>, want: &str) -> Vec<String> {
    let mut seen = Vec::new();
    for _ in 0..1000 {
        while let Ok(line) = rx.try_recv() {
            let hit = line == want;
            seen.push(line);
            if hit {
                return seen;
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    panic!("{want} never arrived; saw {seen:?}");
}

/// Read past the keepalive pings for the next frame that carries bytes.
#[cfg(feature = "relay")]
async fn next_binary<S>(ws: &mut tokio_tungstenite::WebSocketStream<S>) -> Vec<u8>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    loop {
        match ws.next().await.expect("the leg died").unwrap() {
            Message::Binary(b) => return b,
            Message::Ping(_) | Message::Pong(_) => continue,
            other => panic!("unexpected frame: {other:?}"),
        }
    }
}

/// Presence and metering: the supervisor sees a leg park, a caller claim
/// it, and the session close carrying its byte count — the three facts a
/// panel and a meter are built from, arriving as ordinary queue messages.
#[cfg(feature = "relay")]
/// One runtime for the whole binary, never dropped — see the long note in
/// `tests/stun.rs`. Same reason: a core dump caught `Runtime` teardown
/// racing a worker's `Condvar::notify_one` into freed memory, and a
/// `static` never runs that path. These tests had not crashed, but they
/// drop runtimes the same way, so they carry the same exposure.
fn rt() -> &'static tokio::runtime::Runtime {
    static RT: std::sync::OnceLock<tokio::runtime::Runtime> = std::sync::OnceLock::new();
    RT.get_or_init(|| tokio::runtime::Runtime::new().expect("a tokio runtime"))
}

#[test]
fn the_deployment_sees_presence_and_bytes() {
    let (addr, rx) = deployment_with_relay(false);
    let seen = rt().block_on(async {
        let (mut device, _) =
            tokio_tungstenite::connect_async(format!("ws://{addr}/park/xps?k=pk-0123456789abcdef"))
                .await
                .unwrap();
        // Claimable, not merely connected: the supervisor says when, and a
        // caller that beats it is told the device is not home.
        let seen = until(&rx, "parked:xps").await;

        let (mut caller, _) =
            tokio_tungstenite::connect_async(format!("ws://{addr}/s/xps?k=ck-0123456789abcdef"))
                .await
                .unwrap();
        caller
            .send(Message::Binary(b"hello".to_vec()))
            .await
            .unwrap();
        // The device echoes, so bytes cross both ways: 5 + 5. A parked leg
        // is pinged (the first `interval` tick fires at once), so read past
        // the pings for the frame that carries bytes.
        let b = next_binary(&mut device).await;
        device.send(Message::Binary(b)).await.unwrap();
        assert_eq!(next_binary(&mut caller).await, b"hello");
        drop(caller);
        drop(device);
        seen
    });

    // `parked:xps` was consumed while sequencing; the other two follow it.
    let mut lines = seen;
    lines.extend(drain(&rx, 2));
    assert!(lines.iter().any(|l| l == "parked:xps"), "{lines:?}");
    assert!(lines.iter().any(|l| l == "claimed:xps"), "{lines:?}");
    assert!(
        lines.iter().any(|l| l == "closed:xps:10"),
        "the session's byte count should reach the supervisor: {lines:?}"
    );
}

/// Arbitration: a label the static keys admit, but the supervisor refuses.
/// This is the thing keys cannot express and the reason the control plane
/// exists — the deployment gets the last word.
#[cfg(feature = "relay")]
#[test]
fn the_deployment_can_refuse_a_leg_the_keys_admit() {
    let (addr, rx) = deployment_with_relay(true);
    let refused = rt().block_on(async {
        // The key is valid for `blocked`; only the supervisor objects.
        let (mut ws, _) = tokio_tungstenite::connect_async(format!(
            "ws://{addr}/park/blocked?k=pk-0123456789abcdef"
        ))
        .await
        .unwrap();
        matches!(ws.next().await, Some(Ok(Message::Close(Some(f)))) if u16::from(f.code) == 1008)
    });
    assert!(
        refused,
        "the deployment's refusal should close the leg 1008"
    );
    let lines = drain(&rx, 1);
    assert!(
        lines.iter().any(|l| l == "admit:blocked:false"),
        "the supervisor should have been asked: {lines:?}"
    );
}
/// A claim that beats the park is not held for it, and the leg it missed
/// is left parked.
///
/// This is the relay's contract rather than a defect -- `claim_leg` takes
/// a leg or says "not home", and never waits for one to appear -- but it
/// is the contract that made this file flaky, so it is written down.
/// Without it the failure is invisible for `PARK_IDLE`, which is five
/// minutes: the device waits, nobody comes, the relay drops the leg, and
/// the test that was reading it sees a reset with no closing handshake and
/// no explanation. That is exactly what CI saw. Here it takes 0.75s.
///
/// The lesson for everything above: wait on `parked`, never on a sleep.
#[tokio::test(flavor = "multi_thread")]
async fn a_claim_that_beats_the_park_is_told_not_home_and_the_leg_stays() {
    let (addr, _relay) = relay_on_port().await;

    // The caller arrives first: the pool is empty, so it is closed.
    let (mut caller, _) =
        tokio_tungstenite::connect_async(format!("ws://{addr}/s/xps?k=caller-secret-987654321"))
            .await
            .unwrap();
    caller
        .send(Message::Binary(b"SSH-2.0-caller\r\n".to_vec()))
        .await
        .ok();

    // The device parks a moment later, as it would if the two raced.
    let (mut device, _) =
        tokio_tungstenite::connect_async(format!("ws://{addr}/park/xps?k=park-secret-0123456789"))
            .await
            .expect("the device could not park");

    // The caller was refused...
    match caller.next().await.unwrap().unwrap() {
        Message::Close(Some(f)) => assert!(f.reason.contains("not home"), "{f:?}"),
        other => panic!("expected the 1013 close, got {other:?}"),
    }

    // ...and the device now waits for a caller that already gave up. In CI
    // this is where the test sat for PARK_IDLE (300s) before the relay
    // dropped the leg and `next()` yielded ResetWithoutClosingHandshake.
    // (Pings start at once -- `interval` fires on its first tick -- so the
    // question is whether any *bytes* ever arrive.)
    let deadline = std::time::Duration::from_millis(750);
    let starved = tokio::time::timeout(deadline, async {
        loop {
            match device.next().await.expect("device leg died").unwrap() {
                Message::Ping(_) => continue,
                other => break other,
            }
        }
    })
    .await;
    assert!(
        starved.is_err(),
        "the device should be left parked, got {starved:?}"
    );
}
