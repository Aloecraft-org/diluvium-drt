//! The STUN binding server as DRT serves it.
//!
//! The protocol is ego-transport's and tested there; what these check is
//! DRT's wiring — that the config block binds what it names, that a real
//! client gets a real reflexive address back, that the pair a deployment
//! actually wants (`stun1`/`stun2`) classifies a mapping, and that the
//! counters reach a supervisor.

#![cfg(feature = "stun")]

use std::time::Duration;

use drt_config::StunConfig;

/// One runtime for the whole binary, deliberately never dropped.
///
/// This is not tidiness, it is the fix for a real crash. A core dump from
/// segv-probe run 6 caught this binary dying in
/// `drop_glue<tokio::runtime::Runtime>` -> `BlockingPool::shutdown` ->
/// `Receiver::wait`, while a runtime worker on another thread was inside
/// `park::Inner::unpark` -> `Condvar::notify_one_slow`, storing into a
/// Condvar that teardown had already freed. A use-after-free in runtime
/// shutdown, on tokio 1.53.1 (the current release) with parking_lot
/// 0.12.5 — not in DRT code and not in the C engine.
///
/// Every probe resolves its server address through `lookup_host`, which
/// is a `spawn_blocking`, so each test left idle blocking workers parked
/// in the pool; dropping the runtime then raced their wakeup. A `static`
/// is never dropped, so the teardown path simply never runs, and the
/// tests get a faster shared runtime as a side effect.
///
/// The shipped code is a separate question, recorded in doc/Release.md:
/// the bridges hand their runtime to a thread that outlives the process,
/// but the foreground verbs (`drt relay`, `drt stun`, `drt tunnel`) do
/// drop a runtime when they return, which is this same path.
fn rt() -> &'static tokio::runtime::Runtime {
    static RT: std::sync::OnceLock<tokio::runtime::Runtime> = std::sync::OnceLock::new();
    RT.get_or_init(|| tokio::runtime::Runtime::new().expect("a tokio runtime"))
}

fn config(bind: &str) -> StunConfig {
    StunConfig {
        bind: bind.into(),
        queue: "stun_in".into(),
        report_ms: 0,
    }
}

/// The point of the whole thing: a client learns the address the server
/// saw, and it is the client's own socket.
#[test]
fn a_client_learns_its_own_reflexive_address() {
    rt().block_on(async {
        let server = drt::stun::bind(&config("127.0.0.1:0")).await.unwrap();
        let addr = server.local_addr();
        server.spawn();

        let probe = tokio::time::timeout(
            Duration::from_secs(5),
            ego_transport::stun::probe(&addr.to_string()),
        )
        .await
        .expect("the server answered within five seconds")
        .expect("a binding response");

        // Loopback to loopback: what the server saw is a 127.0.0.1 port,
        // and it is the port the probe's own socket took.
        assert!(
            probe.reflexive.ip().is_loopback(),
            "reflexive {}",
            probe.reflexive
        );
        // Loopback has no NAT in the way, so the port the server saw is
        // the port the probe went out on.
        assert_eq!(probe.reflexive.port(), probe.local.port());
        assert_eq!(probe.server, addr);

        // Worth knowing before trusting it: the probe's socket binds
        // wildcard, so `local` is `0.0.0.0:port` while `reflexive` is
        // `127.0.0.1:port` — and `is_natted()`, which compares the two
        // whole addresses, therefore reads true here with no NAT within a
        // mile. It answers "did the address change", and a wildcard bind
        // changes it. Compare ports, or bind the probe explicitly.
        assert!(probe.local.ip().is_unspecified());
        assert!(probe.is_natted());
    });
}

/// `bind` reports the address actually taken, so a `:0` port in a config
/// is resolvable rather than a lie the caller has to chase.
#[test]
fn a_zero_port_resolves_to_the_port_taken() {
    rt().block_on(async {
        let server = drt::stun::bind(&config("127.0.0.1:0")).await.unwrap();
        assert_ne!(server.local_addr().port(), 0);
    });
}

/// A port already held fails with the address in the message, at bind
/// time — not as a thread that dies quietly once the deployment is up.
#[test]
fn a_taken_port_fails_by_name() {
    rt().block_on(async {
        let held = drt::stun::bind(&config("127.0.0.1:0")).await.unwrap();
        let taken = held.local_addr().to_string();
        let err = match drt::stun::bind(&config(&taken)).await {
            Ok(_) => panic!("a port already held bound a second time"),
            Err(e) => e,
        };
        assert!(err.contains(&taken), "{err}");
    });
}

/// The reason to run `stun1` *and* `stun2`: two vantage points let a peer
/// classify its NAT, and the classifier refuses below two rather than
/// guessing from one.
#[test]
fn a_pair_classifies_the_mapping_and_one_refuses_to() {
    rt().block_on(async {
        let one = drt::stun::bind(&config("127.0.0.1:0")).await.unwrap();
        let two = drt::stun::bind(&config("127.0.0.1:0")).await.unwrap();
        let (a, b) = (one.local_addr().to_string(), two.local_addr().to_string());
        one.spawn();
        two.spawn();

        let report = tokio::time::timeout(
            Duration::from_secs(5),
            ego_transport::stun::detect_mapping(
                &[a.as_str(), b.as_str()],
                &ego_transport::stun::ProbeConfig::default(),
            ),
        )
        .await
        .expect("both servers answered")
        .expect("a mapping report");
        // Over loopback both see the same socket, which is the endpoint-
        // independent case — the one where hole punching can work.
        assert_eq!(
            report.mapping,
            ego_transport::stun::NatMapping::EndpointIndependent
        );

        // One server is not enough to compare, and it says so rather than
        // reporting a classification it cannot have made.
        let err = ego_transport::stun::detect_mapping(
            &[a.as_str()],
            &ego_transport::stun::ProbeConfig::default(),
        )
        .await
        .unwrap_err();
        assert!(
            matches!(err, ego_transport::stun::StunError::NotEnoughServers(1)),
            "{err:?}"
        );
    });
}

/// Junk is dropped in silence rather than answered: an unconditional reply
/// would make the server a reflector for spoofed traffic. The counter is
/// what a supervisor watches to tell scanners from clients.
#[test]
fn junk_is_dropped_and_counted_not_answered() {
    rt().block_on(async {
        let server = drt::stun::bind(&config("127.0.0.1:0")).await.unwrap();
        let addr = server.local_addr();
        let metrics = server.metrics();
        server.spawn();

        let sock = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        sock.send_to(b"this is not a STUN binding request", addr)
            .await
            .unwrap();

        // Nothing comes back.
        let mut buf = [0u8; 512];
        let answered = tokio::time::timeout(Duration::from_millis(300), sock.recv_from(&mut buf))
            .await
            .is_ok();
        assert!(!answered, "junk was answered; the server is a reflector");

        // And it is counted as dropped, not as a request.
        for _ in 0..50 {
            let snap = metrics.snapshot();
            if snap.dropped >= 1 {
                assert_eq!(snap.requests, 0, "junk counted as a binding request");
                assert_eq!(snap.responses, 0);
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("the dropped counter never moved");
    });
}

/// The `stun` block loads from a `.host.lua`, binds what it names, and
/// its bridge encodes a snapshot the supervisor can read.
///
/// This checks the bridge in isolation — see
/// `the_drive_loop_carries_the_counters_to_the_supervisor` for the same
/// thing through `drt start`'s real loop, which is where it actually
/// has to work.
#[test]
fn the_stun_block_loads_and_binds_and_encodes() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("sup.lua"),
        r#"
local q = queue.declare('stun_in', {capacity = 8})
local seen = 0
while seen < 1 do
  queue.wait({q}, 5000)
  local m = queue.pop(q)
  while m do
    if m.event == 'stun' then
      seen = seen + 1
      print('supervisor: stun on ' .. m.addr ..
            ' requests=' .. tostring(m.requests) ..
            ' responses=' .. tostring(m.responses))
    end
    m = queue.pop(q)
  end
end
"#,
    )
    .unwrap();
    std::fs::write(
        dir.path().join("stun.host.lua"),
        r#"return {
  supervisor = "sup.lua",
  stun = { bind = "127.0.0.1", port = 0, queue = "stun_in", report_ms = 0 },
}"#,
    )
    .unwrap();

    let config = drt::config::load(Some(&dir.path().join("stun.host.lua"))).unwrap();
    let stun = config.stun.clone().expect("the stun block loaded");
    assert_eq!(stun.queue, "stun_in");
    assert_eq!(stun.report_ms, 0);
    // bind and port composed into one address, the way relay and listen do.
    assert_eq!(stun.bind, "127.0.0.1:0");

    // The bridge binds for real, and a live client is answered by the
    // server the deployment stood up.
    let mut bridge = drt::stun::StunBridge::start(&stun).unwrap();
    let addr = bridge.addr();
    assert!(addr.port() != 0);

    let probe = rt()
        .block_on(ego_transport::stun::probe(&addr.to_string()))
        .expect("the deployment's stun server answered");
    assert_eq!(probe.server, addr);

    // And the counters reach a supervisor: report() hands the encoded
    // message to the same push the drive loop uses.
    // Poll for a snapshot that has SEEN the probe, rather than asserting on
    // the first one to arrive. The counters are written by the server on
    // its own thread — `requests` before the reply goes out and
    // `responses` after — so any single snapshot is a valid observation of
    // a moment, including a moment before the probe landed or between its
    // two counter writes. Waiting for the settled state is the only honest
    // assertion; the first version of this test asserted on whatever
    // arrived first and failed roughly one run in fifty.
    let mut pushed: Vec<(String, Vec<u8>)> = Vec::new();
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let settled = loop {
        bridge.report(&mut |q, m| {
            pushed.push((q.to_string(), m.to_vec()));
            true
        });
        if let Some(found) = pushed.iter().find(|(_, m)| {
            let v = rmpv::decode::read_value(&mut &m[..]).unwrap();
            let get = |n: &str| {
                v.as_map()
                    .unwrap()
                    .iter()
                    .find(|(k, _)| k.as_str() == Some(n))
                    .and_then(|(_, v)| v.as_u64())
            };
            get("requests") == Some(1) && get("responses") == Some(1)
        }) {
            break found.clone();
        }
        assert!(
            std::time::Instant::now() < deadline,
            "no snapshot ever showed the probe: {pushed:?}"
        );
        std::thread::sleep(Duration::from_millis(20));
    };
    let (queue, msg) = &settled;
    assert_eq!(queue, "stun_in");
    let value = rmpv::decode::read_value(&mut &msg[..]).unwrap();
    let field = |name: &str| {
        value
            .as_map()
            .unwrap()
            .iter()
            .find(|(k, _)| k.as_str() == Some(name))
            .map(|(_, v)| v.clone())
            .unwrap_or_else(|| panic!("no field {name}"))
    };
    assert_eq!(field("event").as_str(), Some("stun"));
    assert_eq!(field("addr").as_str(), Some(addr.to_string().as_str()));
    assert_eq!(field("requests").as_u64(), Some(1));
    assert_eq!(field("responses").as_u64(), Some(1));
    assert_eq!(field("dropped").as_u64(), Some(0));
}

/// The path that matters, and the one a mock `push` cannot check: a real
/// `drt start` loop carrying a real server's counters to a real Lua
/// supervisor.
///
/// Written because the isolated bridge test above passed while this path
/// was silently broken — the bridge encoded fine, and nothing exercised
/// the loop that delivers it. A test that stubs the delivery cannot fail
/// when the delivery is what is wrong.
#[test]
fn the_drive_loop_carries_the_counters_to_the_supervisor() {
    let dir = tempfile::tempdir().unwrap();
    // The supervisor exits once it has seen a report with a request in it,
    // which is what ends the deployment and so ends the test.
    std::fs::write(
        dir.path().join("sup.lua"),
        r#"
local events = queue.declare('stun_in', {capacity = 64})
-- Bounded on purpose. If the drive loop stops delivering, this supervisor
-- must EXIT so the assertion below fails and names the problem — an
-- unbounded wait would hang CI instead, which is a worse way to learn the
-- same thing. Twenty half-second waits is ten seconds, far longer than the
-- 50 ms report interval needs.
for _ = 1, 20 do
  local _, m = queue.wait({events}, 500)
  -- Wake on RESPONSES, not requests: `requests` is counted before the
  -- reply is sent and `responses` after, so a report can legitimately
  -- land between the two and show requests=1 responses=0. Waiting for
  -- the response is waiting for the exchange to be over.
  if m ~= nil and m.event == 'stun' and m.responses > 0 then
    host.call('fs/write', {
      path = 'seen.txt',
      data = m.addr .. ' requests=' .. tostring(m.requests)
             .. ' responses=' .. tostring(m.responses)
             .. ' dropped=' .. tostring(m.dropped),
    })
    return
  end
end
"#,
    )
    .unwrap();
    std::fs::write(
        dir.path().join("d.host.lua"),
        format!(
            r#"return {{
  supervisor = "sup.lua",
  caps = {{ "host:fs/*" }},
  connectors = {{
    fs = {{ scope = "{}", access = "readwrite", max_bytes = 65536 }},
  }},
  stun = {{ bind = "127.0.0.1", port = 0, report_ms = 50 }},
}}"#,
            dir.path().to_str().unwrap()
        ),
    )
    .unwrap();

    let cfg = drt::config::load(Some(&dir.path().join("d.host.lua"))).unwrap();
    // Port 0 means the deployment picks; a prober has to learn it the same
    // way an operator would, so bind once here to find a free port and let
    // the deployment take it.
    let free = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    let port = free.local_addr().unwrap().port();
    drop(free);
    let mut cfg = cfg;
    cfg.stun.as_mut().unwrap().bind = format!("127.0.0.1:{port}");

    // Probe from the side while the deployment runs, until it answers.
    let prober = std::thread::spawn(move || {
        let sock = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        sock.set_read_timeout(Some(Duration::from_millis(200)))
            .unwrap();
        let mut req = vec![0u8, 1, 0, 0];
        req.extend_from_slice(&0x2112_A442u32.to_be_bytes());
        req.extend_from_slice(&[7u8; 12]);
        let mut buf = [0u8; 512];
        for _ in 0..100 {
            if sock.send_to(&req, ("127.0.0.1", port)).is_ok() && sock.recv_from(&mut buf).is_ok() {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    });

    let mut registry = drt_connector::Registry::new();
    registry
        .wire(
            "fs",
            std::sync::Arc::new(drt_connector_fs::FsConnector::new()),
            cfg.connectors.get("fs").and_then(|w| w.scope.clone()),
        )
        .unwrap();
    drt::start::start(&cfg, drt_connector::Dispatcher::new(registry)).unwrap();
    let _ = prober.join();

    let seen = std::fs::read_to_string(dir.path().join("seen.txt"))
        .expect("the supervisor was told and wrote what it heard");
    assert!(seen.contains(&format!("127.0.0.1:{port}")), "{seen}");
    // The prober retries until answered, so the count is "at least one",
    // not exactly one — pinning it to 1 would be a second race.
    assert!(!seen.contains("responses=0"), "{seen}");
    assert!(!seen.contains("requests=0"), "{seen}");
}
