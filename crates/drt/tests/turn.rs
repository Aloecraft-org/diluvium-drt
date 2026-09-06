//! The TURN relay as DRT serves it.
//!
//! The protocol is ego-transport's and tested there, against an
//! independent client; what these check is DRT's wiring — that the
//! config block binds what it names and refuses what it must, that a
//! credential this deployment minted (by `ephemeral_credentials_for` and
//! by `crypto/turn_credential` alike) allocates while a forged or expired
//! one does not, and that an allocation's closing report reaches a
//! supervisor with the principal on it, through the real drive loop.

#![cfg(feature = "turn")]

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use drt_config::TurnConfig;
use ego_transport::turn::ephemeral_credentials_for;
use turn::client::{Client, ClientConfig};
use webrtc_util::Conn;

/// One runtime for the whole binary, deliberately never dropped: the
/// tokio 1.53.1 teardown use-after-free `tests/stun.rs` documents.
fn rt() -> &'static tokio::runtime::Runtime {
    static RT: std::sync::OnceLock<tokio::runtime::Runtime> = std::sync::OnceLock::new();
    RT.get_or_init(|| tokio::runtime::Runtime::new().expect("a tokio runtime"))
}

const SECRET: &str = "a-shared-secret-coturn-would-accept-too";
const REALM: &str = "drt";

fn config(bind: &str) -> TurnConfig {
    TurnConfig {
        bind: bind.into(),
        relay_address: "127.0.0.1".into(),
        relay_bind: "127.0.0.1".into(),
        realm: REALM.into(),
        key: Some(SECRET.into()),
        key_file: None,
        key_env: None,
        max_allocations: 4,
        queue: "turn_in".into(),
        report_ms: 0,
    }
}

/// The independent client: webrtc-rs's, not ours, so the server is
/// proven against an implementation it shares nothing with.
async fn client(server: SocketAddr, username: &str, password: &str) -> Client {
    let conn = Arc::new(tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap());
    let client = Client::new(ClientConfig {
        stun_serv_addr: server.to_string(),
        turn_serv_addr: server.to_string(),
        username: username.to_string(),
        password: password.to_string(),
        realm: REALM.to_string(),
        software: String::new(),
        rto_in_ms: 200,
        conn,
        vnet: None,
    })
    .await
    .unwrap();
    client.listen().await.unwrap();
    client
}

/// Not allocated, without waiting out the client's whole retry budget:
/// an explicit refusal and a client still retrying against a server that
/// keeps saying no both mean no.
async fn refused(client: &Client, why: &str) {
    if let Ok(Ok(_)) = tokio::time::timeout(Duration::from_secs(5), client.allocate()).await {
        panic!("{why}: the allocation was granted");
    }
}

/// The point of the whole thing: a credential minted under the block's
/// secret allocates a relay, and one minted under any other secret, or
/// already expired, does not.
#[test]
fn a_minted_credential_allocates_and_a_forged_or_expired_one_does_not() {
    rt().block_on(async {
        let server = drt::turn::bind(&config("127.0.0.1:0"), None).await.unwrap();
        let addr = server.local_addr();
        assert_ne!(addr.port(), 0);
        assert!(server.relay_address().is_loopback());

        let (username, password) =
            ephemeral_credentials_for(SECRET, Duration::from_secs(60), "fp-label").unwrap();
        assert!(username.ends_with(":fp-label"), "{username}");
        let good = client(addr, &username, &password).await;
        let relay = tokio::time::timeout(Duration::from_secs(5), good.allocate())
            .await
            .expect("allocated within five seconds")
            .expect("a credential minted under the block's secret allocates");
        assert!(relay.local_addr().unwrap().ip().is_loopback());

        let (username, password) = ephemeral_credentials_for(
            "not-the-secret-but-as-long-as-one",
            Duration::from_secs(60),
            "fp-label",
        )
        .unwrap();
        let forged = client(addr, &username, &password).await;
        refused(&forged, "a credential under another secret").await;

        let (username, password) =
            ephemeral_credentials_for(SECRET, Duration::ZERO, "fp-label").unwrap();
        let expired = client(addr, &username, &password).await;
        refused(&expired, "an expired credential").await;

        // Two refusals, counted differently, and worth knowing before
        // reading a panel: the expired credential is refused by the
        // handler itself (`auth_refused`); the forged one has a
        // well-formed, unexpired username, so the handler answers with
        // the key it computes (`auth_ok`) and the TURN layer's integrity
        // check is what refuses the message. Neither allocates.
        let snap = server.metrics().snapshot();
        assert_eq!(snap.allocations_granted, 1, "{snap:?}");
        assert_eq!(snap.live_allocations, 1, "{snap:?}");
        assert_eq!(snap.auth_refused, 1, "{snap:?}");
        assert_eq!(snap.auth_ok, 2, "{snap:?}");

        relay.close().await.ok();
        good.close().await.ok();
        forged.close().await.ok();
        expired.close().await.ok();
        server.close().await.unwrap();
    });
}

/// `crypto/turn_credential` and this server share one secret and one
/// scheme, so a credential the connector minted is one the relay
/// accepts: the two halves of issue #12, measured against each other.
#[cfg(feature = "connector-crypto")]
#[test]
fn the_crypto_connectors_credential_is_accepted_by_the_deployments_relay() {
    use drt_caps::Scope;
    use drt_connector::Connector;

    rt().block_on(async {
        let server = drt::turn::bind(&config("127.0.0.1:0"), None).await.unwrap();
        let addr = server.local_addr();

        let connector = drt_connector_crypto::CryptoConnector::new();
        let scope = Scope(rmpv::Value::Map(vec![
            ("key".into(), "the-master-key-for-this-deployment".into()),
            (
                "turn".into(),
                rmpv::Value::Map(vec![
                    ("key".into(), SECRET.into()),
                    ("ttl".into(), rmpv::Value::from(60u64)),
                ]),
            ),
        ]));
        let args = rmpv::Value::Map(vec![("user".into(), "fp-label".into())]);
        let minted = connector
            .call("crypto/turn_credential", Some(args), Some(&scope))
            .await
            .expect("the connector minted a credential");
        let field = |name: &str| {
            minted
                .as_map()
                .unwrap()
                .iter()
                .find(|(k, _)| k.as_str() == Some(name))
                .and_then(|(_, v)| v.as_str())
                .unwrap_or_else(|| panic!("no {name} in {minted:?}"))
                .to_string()
        };
        let (username, password) = (field("username"), field("password"));
        assert!(username.ends_with(":fp-label"), "{username}");

        let c = client(addr, &username, &password).await;
        let relay = tokio::time::timeout(Duration::from_secs(5), c.allocate())
            .await
            .expect("allocated within five seconds")
            .expect("the connector's credential allocates on the deployment's relay");
        relay.close().await.ok();
        c.close().await.ok();
        server.close().await.unwrap();
    });
}

/// The refusal a config earns, or a panic naming the server that bound
/// instead. (`TurnServer` is not `Debug`, so `unwrap_err` cannot say it.)
async fn refusal(config: &TurnConfig) -> String {
    match drt::turn::bind(config, None).await {
        Ok(_) => panic!("{config:?} bound a server instead of refusing"),
        Err(e) => e,
    }
}

/// What refuses, and by name, at bind time rather than after the first
/// client authenticates.
#[test]
fn a_missing_secret_and_a_wildcard_bind_with_no_relay_address_refuse_by_name() {
    rt().block_on(async {
        let mut none = config("127.0.0.1:0");
        none.key = None;
        let err = refusal(&none).await;
        assert!(err.contains("key_file, key_env, key"), "{err}");
        assert!(err.contains("open relay"), "{err}");

        let mut unset = config("127.0.0.1:0");
        unset.key = None;
        unset.key_env = Some("DRT_TEST_TURN_SECRET_THAT_IS_NOT_SET".into());
        let err = refusal(&unset).await;
        assert!(
            err.contains("DRT_TEST_TURN_SECRET_THAT_IS_NOT_SET"),
            "{err}"
        );

        let mut short = config("127.0.0.1:0");
        short.key = Some("short".into());
        let err = refusal(&short).await;
        assert!(err.contains("shorter than 16 bytes"), "{err}");

        let mut wildcard = config("0.0.0.0:0");
        wildcard.relay_address = String::new();
        let err = refusal(&wildcard).await;
        assert!(err.contains("relay_address"), "{err}");
        assert!(err.contains("wildcard"), "{err}");

        // And a specific bind derives the relay address from itself.
        let mut derived = config("127.0.0.1:0");
        derived.relay_address = String::new();
        let server = drt::turn::bind(&derived, None).await.unwrap();
        assert!(server.relay_address().is_loopback());
        server.close().await.unwrap();
    });
}

/// A port already held fails with the address in the message.
#[test]
fn a_taken_port_fails_by_name() {
    rt().block_on(async {
        let held = drt::turn::bind(&config("127.0.0.1:0"), None).await.unwrap();
        let taken = held.local_addr().to_string();
        let err = refusal(&config(&taken)).await;
        assert!(err.contains(&taken), "{err}");
        held.close().await.unwrap();
    });
}

/// The `turn` block loads from a `.host.lua`, binds what it names, and
/// its bridge carries an allocation's closing report — principal and
/// bytes — the way the drive loop will.
#[test]
fn the_turn_block_loads_and_binds_and_reports_a_close_with_its_principal() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("sup.lua"),
        "local q = queue.declare('turn_in', {capacity = 8})\n",
    )
    .unwrap();
    std::fs::write(
        dir.path().join("turn.host.lua"),
        format!(
            r#"return {{
  supervisor = "sup.lua",
  turn = {{ bind = "127.0.0.1", port = 0, relay_bind = "127.0.0.1",
           key = "{SECRET}", report_ms = 0 }},
}}"#
        ),
    )
    .unwrap();

    let config = drt::config::load(Some(&dir.path().join("turn.host.lua"))).unwrap();
    let turn = config.turn.clone().expect("the turn block loaded");
    // bind and port composed into one address, the way stun's are; the
    // defaults are the documented ones.
    assert_eq!(turn.bind, "127.0.0.1:0");
    assert_eq!(turn.relay_address, "");
    assert_eq!(turn.realm, "drt");
    assert_eq!(turn.queue, "turn_in");
    assert_eq!(turn.max_allocations, 256);
    assert_eq!(turn.key.as_deref(), Some(SECRET));

    let mut bridge = drt::turn::TurnBridge::start(&turn).unwrap();
    let addr = bridge.addr();
    assert_ne!(addr.port(), 0);
    // Derived from the specific bind, since the block said nothing.
    assert!(bridge.relay_address().is_loopback());

    // A client allocates, relays a few bytes to an echoing peer so the
    // allocation has something to bill, and releases the allocation.
    let (username, password) =
        ephemeral_credentials_for(SECRET, Duration::from_secs(60), "fp-label").unwrap();
    rt().block_on(async {
        let c = client(addr, &username, &password).await;
        let relay = tokio::time::timeout(Duration::from_secs(5), c.allocate())
            .await
            .expect("allocated within five seconds")
            .expect("the deployment's relay allocates");
        let peer = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let peer_addr = peer.local_addr().unwrap();
        tokio::spawn(async move {
            let mut buf = [0u8; 256];
            while let Ok((n, from)) = peer.recv_from(&mut buf).await {
                let _ = peer.send_to(&buf[..n], from).await;
            }
        });
        relay.send_to(b"billable bytes", peer_addr).await.unwrap();
        let mut buf = [0u8; 256];
        tokio::time::timeout(Duration::from_secs(5), relay.recv_from(&mut buf))
            .await
            .expect("the relayed reply came back")
            .unwrap();
        relay.close().await.ok();
        c.close().await.ok();
    });

    // The closing report reaches the queue with the principal on it, and
    // a snapshot that counted the grant comes with it. Polled, because
    // the close is the server's own task's to deliver.
    let mut pushed: Vec<rmpv::Value> = Vec::new();
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let closed = loop {
        bridge.report(&mut |q, m| {
            assert_eq!(q, "turn_in");
            pushed.push(rmpv::decode::read_value(&mut &m[..]).unwrap());
            true
        });
        if let Some(found) = pushed
            .iter()
            .find(|v| field(v, "event").as_str() == Some("turn_closed"))
        {
            break found.clone();
        }
        assert!(
            std::time::Instant::now() < deadline,
            "no closing report arrived: {pushed:?}"
        );
        std::thread::sleep(Duration::from_millis(20));
    };
    assert_eq!(field(&closed, "principal").as_str(), Some("fp-label"));
    assert_eq!(field(&closed, "username").as_str(), Some(username.as_str()));
    assert!(
        field(&closed, "relayed_bytes").as_u64().unwrap() > 0,
        "{closed:?}"
    );
    let snapshot = pushed
        .iter()
        .find(|v| field(v, "event").as_str() == Some("turn"))
        .expect("a counter snapshot");
    assert_eq!(
        field(snapshot, "addr").as_str(),
        Some(addr.to_string().as_str())
    );
    assert_eq!(field(snapshot, "relay_address").as_str(), Some("127.0.0.1"));
    assert!(field(snapshot, "allocations_granted").as_u64().unwrap() >= 1);
}

/// The path that matters, and the one the bridge test cannot check: a
/// real `drt start` loop carrying a real relay's closing report to a
/// real Lua supervisor. Written for `tests/stun.rs`'s reason — a test
/// that stubs the delivery cannot fail when the delivery is what is
/// wrong.
#[test]
fn the_drive_loop_carries_a_closing_report_to_the_supervisor() {
    let dir = tempfile::tempdir().unwrap();
    // The supervisor exits once it has seen one allocation close, which
    // ends the deployment and so the test. Bounded, so a loop that stops
    // delivering fails here by name instead of hanging CI.
    std::fs::write(
        dir.path().join("sup.lua"),
        r#"
local events = queue.declare('turn_in', {capacity = 64})
for _ = 1, 40 do
  local _, m = queue.wait({events}, 500)
  if m ~= nil and m.event == 'turn_closed' then
    host.call('fs/write', {
      path = 'seen.txt',
      data = 'principal=' .. tostring(m.principal)
             .. ' relayed_bytes=' .. tostring(m.relayed_bytes),
    })
    return
  end
end
"#,
    )
    .unwrap();
    let free = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    let port = free.local_addr().unwrap().port();
    drop(free);
    std::fs::write(
        dir.path().join("d.host.lua"),
        format!(
            r#"return {{
  supervisor = "sup.lua",
  caps = {{ "host:fs/*" }},
  connectors = {{
    fs = {{ scope = "{}", access = "readwrite", max_bytes = 65536 }},
  }},
  turn = {{ bind = "127.0.0.1", port = {port}, relay_bind = "127.0.0.1",
           key = "{SECRET}", report_ms = 50 }},
}}"#,
            dir.path().to_str().unwrap()
        ),
    )
    .unwrap();
    let cfg = drt::config::load(Some(&dir.path().join("d.host.lua"))).unwrap();

    // A client from the side, on its own runtime, once the deployment
    // answers: allocate, relay a few bytes, release.
    let addr: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
    let (username, password) =
        ephemeral_credentials_for(SECRET, Duration::from_secs(60), "fp-label").unwrap();
    let side = std::thread::spawn(move || {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let mut relay = None;
            for _ in 0..50 {
                let c = client(addr, &username, &password).await;
                if let Ok(Ok(r)) =
                    tokio::time::timeout(Duration::from_millis(500), c.allocate()).await
                {
                    relay = Some((c, r));
                    break;
                }
                c.close().await.ok();
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            let (c, relay) = relay.expect("the deployment's relay answered within 30 s");
            let peer = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
            let peer_addr = peer.local_addr().unwrap();
            tokio::spawn(async move {
                let mut buf = [0u8; 256];
                while let Ok((n, from)) = peer.recv_from(&mut buf).await {
                    let _ = peer.send_to(&buf[..n], from).await;
                }
            });
            relay.send_to(b"billable bytes", peer_addr).await.unwrap();
            let mut buf = [0u8; 256];
            let _ = tokio::time::timeout(Duration::from_secs(5), relay.recv_from(&mut buf)).await;
            relay.close().await.ok();
            c.close().await.ok();
        });
        // Leaked, for the reason `rt()` is.
        std::mem::forget(rt);
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
    let _ = side.join();

    let seen = std::fs::read_to_string(dir.path().join("seen.txt"))
        .expect("the supervisor was told and wrote what it heard");
    assert!(seen.contains("principal=fp-label"), "{seen}");
    assert!(!seen.contains("relayed_bytes=0"), "{seen}");
}

fn field<'a>(v: &'a rmpv::Value, name: &str) -> &'a rmpv::Value {
    v.as_map()
        .unwrap()
        .iter()
        .find(|(k, _)| k.as_str() == Some(name))
        .map(|(_, v)| v)
        .unwrap_or(&rmpv::Value::Nil)
}

/// The wrong error code for a bad credential, pinned until upstream fixes it.
///
/// RFC 8489 §9.2.4 says a request whose MESSAGE-INTEGRITY does not verify,
/// or whose username is unknown, is answered **401 Unauthorized** with a
/// fresh NONCE — the code that tells a client to authenticate again. The
/// `turn` crate answers **400 Bad Request** to both, because
/// `authenticate_request` builds one `bad_request_msg` and sends it down
/// every auth-failure path (`turn-0.17.2/src/server/request.rs`).
///
/// It matters for the client DRT's TURN server exists to serve: a
/// browser's ICE agent retries on 401 and gives up on 400, so a credential
/// that expired mid-session takes the allocation with it instead of being
/// renewed. `doc/TURN-401-Upstream.md` is the brief.
///
/// This asserts the behaviour DRT has today, not the behaviour it should
/// have. **When this test fails, upstream has fixed it**: change the 400
/// to 401 here and delete the brief.
#[test]
fn a_forged_credential_is_refused_with_the_wrong_code_for_now() {
    rt().block_on(async {
        let server = drt::turn::bind(&config("127.0.0.1:0"), None).await.unwrap();
        let addr = server.local_addr();
        let (username, password) = ephemeral_credentials_for(
            "a-different-secret-entirely-0123",
            Duration::from_secs(300),
            "fp",
        )
        .expect("a credential");
        let forged = client(addr, &username, &password).await;
        let answer = match tokio::time::timeout(Duration::from_secs(5), forged.allocate()).await {
            Ok(Ok(_)) => panic!("a credential minted under another secret allocated"),
            Ok(Err(e)) => format!("{e}"),
            Err(_) => panic!("the server never answered a forged credential"),
        };
        assert!(
            answer.contains("400"),
            "expected today's 400 (see doc/TURN-401-Upstream.md); got: {answer}"
        );
    });
}
