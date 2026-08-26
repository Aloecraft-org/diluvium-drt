//! The ssh connector against a real SSH server — ego-transport's own
//! listener, in process, over loopback. Nothing is mocked below the
//! connector: the handshake, pubkey auth, host-key verification, and the
//! exec channel are the real protocol.

use std::sync::Arc;
use std::time::Duration;

use drt_caps::{CapSet, Grant, Scope};
use drt_connector::{Connector, Dispatcher, Registry};
use drt_connector_ssh::SshConnector;
use drt_hostcall::{to_bytes, Request, Status};
use ego_transport::ssh::{
    generate_ed25519, key_identity, ClientAuthorization, SshChannelKind, SshListener,
    SshServerConfig,
};
use ego_transport::transport::Transport;

struct TestServer {
    addr: String,
    host_key_line: String,
    host_fingerprint: String,
    key_path: std::path::PathBuf,
    _dir: tempfile::TempDir,
}

/// Bind a real SSH server that answers one exec channel per connection:
/// stdout echoes the command, stderr says hello, exit is 0 for any command
/// except `fail`, which exits 7.
async fn start_server() -> (TestServer, tokio::task::JoinHandle<()>) {
    let host_key = generate_ed25519();
    let host_pub = host_key.public_key().clone();
    let client_key = generate_ed25519();

    let dir = tempfile::tempdir().unwrap();
    let key_path = dir.path().join("id_ed25519");
    std::fs::write(
        &key_path,
        client_key
            .to_openssh(Default::default())
            .unwrap()
            .as_bytes(),
    )
    .unwrap();

    let mut config = SshServerConfig::new(host_key);
    config.authorization = ClientAuthorization::Keys(vec![client_key.public_key().clone()]);
    let listener = SshListener::bind("127.0.0.1:0", config).await.unwrap();
    let server = TestServer {
        addr: listener.local_addr().to_string(),
        host_key_line: host_pub.to_openssh().unwrap(),
        host_fingerprint: key_identity(&host_pub).fingerprint_sha256,
        key_path,
        _dir: dir,
    };

    let task = tokio::spawn(async move {
        while let Ok(mut conn) = listener.accept().await {
            tokio::spawn(async move {
                while let Ok(mut channel) = conn.next_channel().await {
                    let SshChannelKind::Exec(command) = channel.kind().clone() else {
                        continue;
                    };
                    if command == b"hang" {
                        // Accepted, never answered: the client's wall clock
                        // is the only thing that ends this.
                        std::future::pending::<()>().await;
                    }
                    let mut out = b"ran: ".to_vec();
                    out.extend_from_slice(&command);
                    channel.send(&out).await.unwrap();
                    let code = if command == b"fail" { 7 } else { 0 };
                    channel.exit_status(code).await.unwrap();
                    channel.send_eof().await.ok();
                    channel.close().await.ok();
                }
            });
        }
    });
    (server, task)
}

fn scope(server: &TestServer, extra: &[(&str, rmpv::Value)]) -> Scope {
    let mut map = vec![
        ("host".into(), server.addr.clone().into()),
        ("user".into(), "tester".into()),
        ("key_path".into(), server.key_path.to_str().unwrap().into()),
        ("host_key".into(), server.host_key_line.clone().into()),
    ];
    for (k, v) in extra {
        map.push(((*k).into(), v.clone()));
    }
    Scope(rmpv::Value::Map(map))
}

fn exec_args(command: &str) -> rmpv::Value {
    rmpv::Value::Map(vec![("command".into(), command.into())])
}

fn field<'a>(value: &'a rmpv::Value, name: &str) -> &'a rmpv::Value {
    value
        .as_map()
        .unwrap()
        .iter()
        .find(|(k, _)| k.as_str() == Some(name))
        .map(|(_, v)| v)
        .unwrap_or(&rmpv::Value::Nil)
}

#[tokio::test]
async fn exec_round_trips_stdout_stderr_and_exit() {
    let (server, _task) = start_server().await;
    let connector = SshConnector::new();
    let sc = scope(&server, &[]);

    let value = connector
        .call("ssh/exec", Some(exec_args("uname -a")), Some(&sc))
        .await
        .unwrap();
    assert_eq!(field(&value, "exit").as_u64(), Some(0));
    assert_eq!(
        field(&value, "stdout").as_slice().unwrap(),
        b"ran: uname -a"
    );
    // The server wrapper carries no stderr channel today; absence arrives
    // as empty bytes, not a missing field.
    assert_eq!(field(&value, "stderr").as_slice().unwrap(), b"");

    let failed = connector
        .call("ssh/exec", Some(exec_args("fail")), Some(&sc))
        .await
        .unwrap();
    assert_eq!(
        field(&failed, "exit").as_u64(),
        Some(7),
        "a nonzero exit is data, not an error"
    );
}

#[tokio::test]
async fn fingerprint_trust_anchor_works_too() {
    let (server, _task) = start_server().await;
    let connector = SshConnector::new();
    let mut sc = scope(&server, &[]);
    // Swap the key anchor for the fingerprint anchor.
    let rmpv::Value::Map(map) = &mut sc.0 else {
        unreachable!()
    };
    map.retain(|(k, _)| k.as_str() != Some("host_key"));
    map.push((
        "host_fingerprint".into(),
        server.host_fingerprint.clone().into(),
    ));

    let value = connector
        .call("ssh/exec", Some(exec_args("true")), Some(&sc))
        .await
        .unwrap();
    assert_eq!(field(&value, "exit").as_u64(), Some(0));
}

#[tokio::test]
async fn the_wrong_host_key_is_refused() {
    let (server, _task) = start_server().await;
    let connector = SshConnector::new();
    let impostor = generate_ed25519().public_key().to_openssh().unwrap();
    let mut sc = scope(&server, &[]);
    let rmpv::Value::Map(map) = &mut sc.0 else {
        unreachable!()
    };
    map.retain(|(k, _)| k.as_str() != Some("host_key"));
    map.push(("host_key".into(), impostor.into()));

    let err = connector
        .call("ssh/exec", Some(exec_args("true")), Some(&sc))
        .await
        .unwrap_err();
    assert!(
        err.to_string().contains("host key"),
        "typed refusal crossed: {err}"
    );
}

#[tokio::test]
async fn the_wall_clock_is_the_only_bound_and_it_holds() {
    let (server, _task) = start_server().await;
    let connector = SshConnector::new();
    let sc = scope(&server, &[("timeout_ms", 400u64.into())]);
    let started = std::time::Instant::now();
    let err = connector
        .call("ssh/exec", Some(exec_args("hang")), Some(&sc))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("timed out"), "{err}");
    assert!(started.elapsed() < Duration::from_secs(5));
}

#[tokio::test]
async fn ill_scoped_wiring_fails_at_startup_by_name() {
    let (server, _task) = start_server().await;

    // No trust anchor: refused at wiring, naming the fix.
    let mut registry = Registry::new();
    let mut sc = scope(&server, &[]);
    let rmpv::Value::Map(map) = &mut sc.0 else {
        unreachable!()
    };
    map.retain(|(k, _)| k.as_str() != Some("host_key"));
    let err = registry
        .wire("ssh", Arc::new(SshConnector::new()), Some(sc))
        .unwrap_err();
    assert_eq!(err.capability, "host:ssh");
    assert!(err.to_string().contains("trust-on-first-use"));

    // A key path that does not exist: also a startup refusal, by name.
    let mut sc = scope(&server, &[]);
    let rmpv::Value::Map(map) = &mut sc.0 else {
        unreachable!()
    };
    map.retain(|(k, _)| k.as_str() != Some("key_path"));
    map.push(("key_path".into(), "/nonexistent/id_ed25519".into()));
    let err = registry
        .wire("ssh", Arc::new(SshConnector::new()), Some(sc))
        .unwrap_err();
    assert!(err.to_string().contains("cannot read key file"));
}

/// Through the whole stack: guest-shaped request bytes → dispatcher
/// (capability gating) → connector → real SSH server → reply bytes. The
/// same path `drt serve` will drive.
#[tokio::test]
async fn a_dispatched_hostcall_reaches_a_real_server() {
    let (server, _task) = start_server().await;
    let mut registry = Registry::new();
    registry
        .wire(
            "ssh",
            Arc::new(SshConnector::new()),
            Some(scope(&server, &[])),
        )
        .unwrap();
    let dispatcher = Dispatcher::new(registry);

    let granted = CapSet::root(vec![Grant::grant("host:ssh/*")]);
    let raw = to_bytes(&Request {
        tok: 41,
        call: "ssh/exec".into(),
        args: Some(exec_args("date")),
    })
    .unwrap();
    let reply = dispatcher.dispatch(&granted, &raw).await;
    assert_eq!(reply.status, Status::Ok);
    assert_eq!(reply.tok, Some(41));
    assert_eq!(
        field(reply.value.as_ref().unwrap(), "stdout")
            .as_slice()
            .unwrap(),
        b"ran: date"
    );

    // And without the grant, the same bytes never reach the network.
    let ungranted = CapSet::root(vec![Grant::grant("host:time")]);
    let reply = dispatcher.dispatch(&ungranted, &raw).await;
    assert_eq!(reply.status, Status::Denied);
}
