//! SSH over WSS, end to end: a **real** SSH session — modern-suite kex,
//! pinned host key, pubkey auth, an exec channel — carried through both
//! halves of `drt tunnel`. Nothing below the bridge is mocked:
//!
//! ```text
//! russh client -TCP-> [stream_to_ws] -WS-> [serve_ws_bridge] -TCP-> sshd
//! ```
//!
//! If the bridge reordered, truncated, or buffered bytes wrongly, the SSH
//! transport layer's MACs would kill the session — which is exactly why a
//! real handshake is the test, and a byte-echo test would prove less.

#![cfg(feature = "tunnel")]

use std::time::Duration;

use ego_transport::ssh::{
    generate_ed25519, ClientAuthorization, HostKeyVerification, SshChannelEvent, SshChannelKind,
    SshClientConfig, SshClientConnection, SshListener, SshServerConfig,
};

#[tokio::test(flavor = "multi_thread")]
async fn a_real_ssh_session_crosses_the_wss_bridge() {
    // A real sshd (ego-transport's listener) answering one exec.
    let host_key = generate_ed25519();
    let host_pub = host_key.public_key().clone();
    let client_key = generate_ed25519();
    let mut config = SshServerConfig::new(host_key);
    config.authorization = ClientAuthorization::Keys(vec![client_key.public_key().clone()]);
    let sshd = SshListener::bind("127.0.0.1:0", config).await.unwrap();
    let sshd_addr = sshd.local_addr().to_string();
    tokio::spawn(async move {
        while let Ok(mut conn) = sshd.accept().await {
            tokio::spawn(async move {
                while let Ok(mut channel) = conn.next_channel().await {
                    let SshChannelKind::Exec(command) = channel.kind().clone() else {
                        continue;
                    };
                    let mut out = b"over wss: ".to_vec();
                    out.extend_from_slice(&command);
                    use ego_transport::transport::Transport;
                    channel.send(&out).await.unwrap();
                    channel.exit_status(0).await.unwrap();
                    channel.send_eof().await.ok();
                    channel.close().await.ok();
                }
            });
        }
    });

    // The server half: WebSockets in, TCP to the sshd out.
    let ws_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let ws_url = format!("ws://{}", ws_listener.local_addr().unwrap());
    tokio::spawn(async move {
        let _ = drt::tunnel::serve_ws_bridge(ws_listener, &sshd_addr).await;
    });

    // The client half: what `ssh -o ProxyCommand="drt tunnel <url>"` does,
    // with a local TCP socket standing in for stdio so a stock SSH client
    // can dial it.
    let entry = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let entry_addr = entry.local_addr().unwrap().to_string();
    tokio::spawn(async move {
        loop {
            let Ok((conn, _)) = entry.accept().await else {
                continue;
            };
            let url = ws_url.clone();
            tokio::spawn(async move {
                let _ = drt::tunnel::stream_to_ws(conn, &url, &[]).await;
            });
        }
    });

    // A stock modern-suite SSH client dials the entry: kex, the pinned host
    // key, pubkey auth, and one exec — all through TCP -> WS -> TCP.
    let conn = SshClientConnection::connect(
        &entry_addr,
        SshClientConfig {
            user: "tester".into(),
            key: client_key,
            host_verification: HostKeyVerification::Keys(vec![host_pub]),
            inactivity_timeout: None,
        },
    )
    .await
    .expect("the SSH handshake did not survive the bridge");

    let mut channel = conn.open_exec(b"uname").await.unwrap();
    let mut stdout = Vec::new();
    let mut exit = None;
    loop {
        match channel.next_event().await {
            SshChannelEvent::Data(bytes) => stdout.extend_from_slice(&bytes),
            SshChannelEvent::ExitStatus(code) => exit = Some(code),
            SshChannelEvent::Eof | SshChannelEvent::Closed => break,
            _ => {}
        }
    }
    assert_eq!(String::from_utf8_lossy(&stdout), "over wss: uname");
    assert_eq!(exit, Some(0));
}

/// The bridge alone, no SSH: bytes in, bytes back, through
/// TCP -> WS -> TCP against a plain echo server.
#[tokio::test(flavor = "multi_thread")]
async fn bytes_cross_the_bridge_alone() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let echo = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let echo_addr = echo.local_addr().unwrap().to_string();
    tokio::spawn(async move {
        loop {
            let Ok((mut c, _)) = echo.accept().await else {
                continue;
            };
            tokio::spawn(async move {
                let mut b = [0u8; 1024];
                while let Ok(n) = c.read(&mut b).await {
                    if n == 0 || c.write_all(&b[..n]).await.is_err() {
                        break;
                    }
                }
            });
        }
    });
    let ws_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let ws_url = format!("ws://{}", ws_listener.local_addr().unwrap());
    tokio::spawn(async move {
        let _ = drt::tunnel::serve_ws_bridge(ws_listener, &echo_addr).await;
    });
    let entry = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let entry_addr = entry.local_addr().unwrap();
    tokio::spawn(async move {
        let (conn, _) = entry.accept().await.unwrap();
        let _ = drt::tunnel::stream_to_ws(conn, &ws_url, &[]).await;
    });
    let mut client = tokio::net::TcpStream::connect(entry_addr).await.unwrap();
    client.write_all(b"marco").await.unwrap();
    let mut back = [0u8; 5];
    client.read_exact(&mut back).await.unwrap();
    assert_eq!(&back, b"marco");
}

/// Issue #13, the program-shaped caller: a local listener where each
/// accepted connection claims one fresh leg. Two connections at once are
/// two legs, each spliced through its own WS to the echo and answered on
/// its own socket; and a claim the far side refuses -- a 403 at upgrade
/// time, the wrong-key case, or a relay that is not there -- closes the
/// local connection at once rather than leaving a client sitting on a
/// half-open one.
#[tokio::test(flavor = "multi_thread")]
async fn a_local_listener_claims_one_leg_per_connection_and_refuses_by_closing() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let echo = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let echo_addr = echo.local_addr().unwrap().to_string();
    tokio::spawn(async move {
        loop {
            let Ok((mut c, _)) = echo.accept().await else {
                continue;
            };
            tokio::spawn(async move {
                let mut b = [0u8; 1024];
                while let Ok(n) = c.read(&mut b).await {
                    if n == 0 || c.write_all(&b[..n]).await.is_err() {
                        break;
                    }
                }
            });
        }
    });
    let ws_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let ws_url = format!("ws://{}", ws_listener.local_addr().unwrap());
    tokio::spawn(async move {
        let _ = drt::tunnel::serve_ws_bridge(ws_listener, &echo_addr).await;
    });

    // `drt tunnel <url> --local 127.0.0.1:0`, with the port read back.
    let local = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let local_addr = local.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = drt::tunnel::serve_local(local, &ws_url, &[]).await;
    });

    // Two callers at once, each on its own leg; each answer lands on the
    // socket that asked, which is what "nothing multiplexed" means.
    let mut a = tokio::net::TcpStream::connect(local_addr).await.unwrap();
    let mut b = tokio::net::TcpStream::connect(local_addr).await.unwrap();
    b.write_all(b"polo!").await.unwrap();
    a.write_all(b"marco").await.unwrap();
    let mut back_a = [0u8; 5];
    let mut back_b = [0u8; 5];
    tokio::time::timeout(Duration::from_secs(5), a.read_exact(&mut back_a))
        .await
        .expect("a's answer came back")
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), b.read_exact(&mut back_b))
        .await
        .expect("b's answer came back")
        .unwrap();
    assert_eq!(&back_a, b"marco");
    assert_eq!(&back_b, b"polo!");

    // A relay that refuses the claim: what a wrong key or an unknown
    // label gets, spelled as the 403 the relay sends at upgrade time.
    let refusing = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let refusing_url = format!("ws://{}", refusing.local_addr().unwrap());
    tokio::spawn(async move {
        loop {
            let Ok((mut c, _)) = refusing.accept().await else {
                continue;
            };
            tokio::spawn(async move {
                let mut b = [0u8; 4096];
                let _ = c.read(&mut b).await;
                let _ = c
                    .write_all(b"HTTP/1.1 403 Forbidden\r\ncontent-length: 0\r\n\r\n")
                    .await;
            });
        }
    });
    let local = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let local_addr = local.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = drt::tunnel::serve_local(local, &refusing_url, &[]).await;
    });
    let mut c = tokio::net::TcpStream::connect(local_addr).await.unwrap();
    let mut buf = [0u8; 16];
    let n = tokio::time::timeout(Duration::from_secs(5), c.read(&mut buf))
        .await
        .expect("the refused connection was closed within five seconds, not left half-open")
        .unwrap_or(0);
    assert_eq!(n, 0, "bytes arrived on a leg the relay refused");

    // And a relay that is not there at all: the same close, for the
    // connect error instead of the 403.
    let gone = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let gone_url = format!("ws://{}", gone.local_addr().unwrap());
    drop(gone);
    let local = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let local_addr = local.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = drt::tunnel::serve_local(local, &gone_url, &[]).await;
    });
    let mut c = tokio::net::TcpStream::connect(local_addr).await.unwrap();
    let n = tokio::time::timeout(Duration::from_secs(5), c.read(&mut buf))
        .await
        .expect("the connection to a missing relay was closed within five seconds")
        .unwrap_or(0);
    assert_eq!(n, 0);
}

// ---------------------------------------------------------------------------
// The wss:// gate: the shape every real deployment uses
// ---------------------------------------------------------------------------

/// A TLS terminator on loopback, forwarding to a plain-ws upstream.
///
/// This is nginx's job in a real deployment, in twenty lines: DRT's relay
/// speaks plain `ws://` on purpose and TLS belongs to the gate in front of
/// it. Returns the port to dial and the self-signed certificate a client
/// has to be told to trust, since no public CA will vouch for it.
async fn tls_gate(
    upstream: String,
) -> (
    u16,
    tokio_rustls::rustls::pki_types::CertificateDer<'static>,
) {
    use std::sync::Arc;
    use tokio_rustls::rustls::pki_types::{CertificateDer, PrivatePkcs8KeyDer};
    use tokio_rustls::rustls::ServerConfig;

    let issued = rcgen::generate_simple_self_signed(vec!["localhost".to_string()])
        .expect("a self-signed certificate for the gate");
    let cert = CertificateDer::from(issued.cert.der().to_vec());
    let key = PrivatePkcs8KeyDer::from(issued.key_pair.serialize_der());

    let config = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![cert.clone()], key.into())
        .expect("the gate's certificate and key agree");
    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        while let Ok((sock, _)) = listener.accept().await {
            let acceptor = acceptor.clone();
            let upstream = upstream.clone();
            tokio::spawn(async move {
                let Ok(mut tls) = acceptor.accept(sock).await else {
                    return;
                };
                let Ok(mut up) = tokio::net::TcpStream::connect(&upstream).await else {
                    return;
                };
                let _ = tokio::io::copy_bidirectional(&mut tls, &mut up).await;
            });
        }
    });
    (port, cert)
}

/// A real SSH session over a real `wss://` gate — the test that did not
/// exist, and whose absence shipped six candidates that could not reach
/// any relay outside a lab.
///
/// `tokio-tungstenite` compiled with no TLS backend answers every `wss://`
/// with `TLS support not compiled in`, and nothing here would have
/// noticed: every other test and example reaches its relay on loopback
/// over plain `ws://`, the one shape that needs no TLS. The bug was found
/// by a person pointing `ssh -o ProxyCommand` at a real gate, which is not
/// a gate.
///
/// So this is the SSH session the first test in this file runs, with the
/// bridge behind a TLS terminator: modern kex, a pinned host key, pubkey
/// auth and an exec channel, every byte through rustls. It also exercises
/// `--extra-root`, because a self-signed gate is the only kind a test can
/// stand up, and an internal CA is the case that wants it in the field.
#[tokio::test(flavor = "multi_thread")]
async fn a_real_ssh_session_crosses_a_wss_gate() {
    let host_key = generate_ed25519();
    let host_pub = host_key.public_key().clone();
    let client_key = generate_ed25519();
    let mut config = SshServerConfig::new(host_key);
    config.authorization = ClientAuthorization::Keys(vec![client_key.public_key().clone()]);
    let sshd = SshListener::bind("127.0.0.1:0", config).await.unwrap();
    let sshd_addr = sshd.local_addr().to_string();
    tokio::spawn(async move {
        while let Ok(mut conn) = sshd.accept().await {
            tokio::spawn(async move {
                while let Ok(mut channel) = conn.next_channel().await {
                    let SshChannelKind::Exec(command) = channel.kind().clone() else {
                        continue;
                    };
                    let mut out = b"through tls: ".to_vec();
                    out.extend_from_slice(&command);
                    use ego_transport::transport::Transport;
                    channel.send(&out).await.unwrap();
                    channel.exit_status(0).await.unwrap();
                    channel.send_eof().await.ok();
                    channel.close().await.ok();
                }
            });
        }
    });

    // The server half on plain ws, exactly as a deployment runs it behind
    // its gate, and the gate in front of it.
    let ws_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let ws_addr = ws_listener.local_addr().unwrap().to_string();
    tokio::spawn(async move {
        let _ = drt::tunnel::serve_ws_bridge(ws_listener, &sshd_addr).await;
    });
    let (gate_port, gate_cert) = tls_gate(ws_addr).await;

    // `localhost`, not 127.0.0.1: the certificate names it and rustls
    // checks that, which is part of what is under test.
    let wss_url = format!("wss://localhost:{gate_port}");
    let local = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let local_addr = local.local_addr().unwrap().to_string();
    tokio::spawn(async move {
        let _ = drt::tunnel::serve_local(local, &wss_url, &[gate_cert]).await;
    });

    let conn = SshClientConnection::connect(
        &local_addr,
        SshClientConfig {
            user: "tester".into(),
            key: client_key,
            host_verification: HostKeyVerification::Keys(vec![host_pub]),
            inactivity_timeout: None,
        },
    )
    .await
    .expect("the SSH handshake did not survive the wss gate");

    let mut channel = conn.open_exec(b"uname").await.unwrap();
    let mut stdout = Vec::new();
    let mut exit = None;
    loop {
        match channel.next_event().await {
            SshChannelEvent::Data(bytes) => stdout.extend_from_slice(&bytes),
            SshChannelEvent::ExitStatus(code) => exit = Some(code),
            SshChannelEvent::Eof | SshChannelEvent::Closed => break,
            _ => {}
        }
    }
    assert_eq!(String::from_utf8_lossy(&stdout), "through tls: uname");
    assert_eq!(exit, Some(0));
}

/// An unknown gate certificate is refused, and the refusal is a trust
/// failure rather than a missing feature.
///
/// The other half of the guard: without `--extra-root` the same gate must
/// NOT be trusted, or the test above would pass just as well with
/// verification disabled. A build with no TLS compiled in fails here too,
/// but with the wrong words -- so this pins the distinction that the
/// original bug erased.
#[tokio::test(flavor = "multi_thread")]
async fn a_gate_signed_by_nobody_is_refused_on_trust() {
    let echo = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let echo_addr = echo.local_addr().unwrap().to_string();
    let ws_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let ws_addr = ws_listener.local_addr().unwrap().to_string();
    tokio::spawn(async move {
        let _ = drt::tunnel::serve_ws_bridge(ws_listener, &echo_addr).await;
    });
    let (gate_port, _cert) = tls_gate(ws_addr).await;

    let err = drt::tunnel::connect(&format!("wss://localhost:{gate_port}"), &[])
        .await
        .expect_err("a self-signed gate must not be trusted by the public roots");
    let lower = err.to_lowercase();
    assert!(
        lower.contains("certificate") || lower.contains("unknown issuer") || lower.contains("tls"),
        "expected a trust failure, got: {err}"
    );
    assert!(
        !lower.contains("not compiled"),
        "TLS is not compiled into the WebSocket client: {err}"
    );
}

// ---------------------------------------------------------------------------
// The `tunnel` block: the same verb from a file
// ---------------------------------------------------------------------------

/// A port nothing holds, bound and released: `run` binds where it is told
/// and reports the address on stderr, so a test that needs to dial it has
/// to choose the port itself.
fn free_port() -> u16 {
    let sock = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = sock.local_addr().unwrap().port();
    drop(sock);
    port
}

/// Wait for something to accept on `addr`, so a session is not attempted
/// against a listener a spawned task has not bound yet.
async fn until_listening(addr: &str) {
    for _ in 0..100 {
        if tokio::net::TcpStream::connect(addr).await.is_ok() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("nothing listened on {addr} within five seconds");
}

/// Both halves from two files and no flags: a device file (`listen` +
/// `to`, in front of a real sshd) and a caller file (`claim` + `bind`), each
/// read through `config::load` the way `drt --config <file> tunnel` reads
/// it, resolved with an empty command line, and run. Then a stock SSH
/// client dials the caller's port: kex, pinned host key, pubkey auth, one
/// exec. The JSON and the `.host.lua` spellings of the same block load to
/// the same object, which is what "one name for both loaders" means.
#[tokio::test(flavor = "multi_thread")]
async fn a_tunnel_from_two_files_and_no_flags_carries_a_real_ssh_session() {
    let host_key = generate_ed25519();
    let host_pub = host_key.public_key().clone();
    let client_key = generate_ed25519();
    let mut config = SshServerConfig::new(host_key);
    config.authorization = ClientAuthorization::Keys(vec![client_key.public_key().clone()]);
    let sshd = SshListener::bind("127.0.0.1:0", config).await.unwrap();
    let sshd_addr = sshd.local_addr().to_string();
    tokio::spawn(async move {
        while let Ok(mut conn) = sshd.accept().await {
            tokio::spawn(async move {
                while let Ok(mut channel) = conn.next_channel().await {
                    let SshChannelKind::Exec(command) = channel.kind().clone() else {
                        continue;
                    };
                    let mut out = b"from a file: ".to_vec();
                    out.extend_from_slice(&command);
                    use ego_transport::transport::Transport;
                    channel.send(&out).await.unwrap();
                    channel.exit_status(0).await.unwrap();
                    channel.send_eof().await.ok();
                    channel.close().await.ok();
                }
            });
        }
    });

    let listen = format!("127.0.0.1:{}", free_port());
    let bind = format!("127.0.0.1:{}", free_port());
    let dir = tempfile::tempdir().unwrap();
    let device = dir.path().join("device.json");
    std::fs::write(
        &device,
        format!(r#"{{ "tunnel": {{ "listen": "{listen}", "to": "{sshd_addr}" }} }}"#),
    )
    .unwrap();
    let caller = dir.path().join("caller.json");
    std::fs::write(
        &caller,
        format!(r#"{{ "tunnel": {{ "claim": "ws://{listen}", "bind": "{bind}" }} }}"#),
    )
    .unwrap();
    // The same caller block in the C host's dialect, loaded by extension.
    let caller_lua = dir.path().join("caller.host.lua");
    std::fs::write(
        &caller_lua,
        format!(r#"return {{ tunnel = {{ claim = "ws://{listen}", bind = "{bind}" }} }}"#),
    )
    .unwrap();
    let from_json = drt::config::load(Some(&caller)).unwrap();
    let from_lua = drt::config::load(Some(&caller_lua)).unwrap();
    assert_eq!(from_json.tunnel, from_lua.tunnel);
    assert!(from_json.tunnel.is_some());

    let none = drt::tunnel::Flags::default();
    let device_mode = drt::tunnel::resolve(
        drt::config::load(Some(&device)).unwrap().tunnel.as_ref(),
        &none,
    )
    .unwrap();
    assert_eq!(
        device_mode.mode,
        drt::tunnel::Mode::Listen {
            listen: listen.clone(),
            to: sshd_addr.clone()
        }
    );
    assert_eq!(device_mode.extra_roots_key, "tunnel.extra_roots");
    let caller_mode = drt::tunnel::resolve(from_json.tunnel.as_ref(), &none).unwrap();
    assert_eq!(
        caller_mode.mode,
        drt::tunnel::Mode::Local {
            claim: format!("ws://{listen}"),
            bind: bind.clone()
        }
    );

    tokio::spawn(async move {
        let _ = drt::tunnel::run(device_mode.mode, &[]).await;
    });
    until_listening(&listen).await;
    tokio::spawn(async move {
        let _ = drt::tunnel::run(caller_mode.mode, &[]).await;
    });
    until_listening(&bind).await;

    let conn = SshClientConnection::connect(
        &bind,
        SshClientConfig {
            user: "tester".into(),
            key: client_key,
            host_verification: HostKeyVerification::Keys(vec![host_pub]),
            inactivity_timeout: None,
        },
    )
    .await
    .expect("the SSH handshake did not survive two files' worth of bridge");
    let mut channel = conn.open_exec(b"uname").await.unwrap();
    let mut stdout = Vec::new();
    let mut exit = None;
    loop {
        match channel.next_event().await {
            SshChannelEvent::Data(bytes) => stdout.extend_from_slice(&bytes),
            SshChannelEvent::ExitStatus(code) => exit = Some(code),
            SshChannelEvent::Eof | SshChannelEvent::Closed => break,
            _ => {}
        }
    }
    assert_eq!(String::from_utf8_lossy(&stdout), "from a file: uname");
    assert_eq!(exit, Some(0));

    // An address already held is refused by the mode's own bind, naming
    // it, which is the startup refusal a unit file wants to see.
    let held = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let held_addr = held.local_addr().unwrap().to_string();
    let err = drt::tunnel::run(
        drt::tunnel::Mode::Listen {
            listen: held_addr.clone(),
            to: sshd_addr,
        },
        &[],
    )
    .await
    .unwrap_err();
    assert!(err.contains(&held_addr), "{err}");
}

/// Flags win per key, and only per key: a `--local` typed beside a file
/// that names `claim` and `bind` replaces `bind` and leaves `claim`
/// standing. `--extra-root` replaces the file's list wholesale, and the
/// refusals about it change their name to match.
#[test]
fn a_flag_replaces_the_file_key_it_names_and_no_other() {
    let file = drt_config::TunnelConfig {
        claim: Some("wss://relay.example/s/xps?k=file".into()),
        bind: Some("127.0.0.1:2222".into()),
        extra_roots: vec!["/etc/drt/gate.pem".into()],
        ..Default::default()
    };
    let flags = drt::tunnel::Flags {
        local: Some("127.0.0.1:3333".into()),
        ..Default::default()
    };
    let resolved = drt::tunnel::resolve(Some(&file), &flags).unwrap();
    assert_eq!(
        resolved.mode,
        drt::tunnel::Mode::Local {
            claim: "wss://relay.example/s/xps?k=file".into(),
            bind: "127.0.0.1:3333".into(),
        }
    );
    assert_eq!(
        resolved.extra_roots,
        vec![std::path::PathBuf::from("/etc/drt/gate.pem")]
    );
    assert_eq!(resolved.extra_roots_key, "tunnel.extra_roots");

    let flags = drt::tunnel::Flags {
        extra_root: vec!["/tmp/other.pem".into()],
        ..Default::default()
    };
    let resolved = drt::tunnel::resolve(Some(&file), &flags).unwrap();
    assert_eq!(
        resolved.extra_roots,
        vec![std::path::PathBuf::from("/tmp/other.pem")]
    );
    assert_eq!(resolved.extra_roots_key, "--extra-root");

    // The refusal about a PEM names what the operator wrote: the key in
    // the file, or the flag.
    let missing = vec![std::path::PathBuf::from("/nonexistent/gate.pem")];
    let err = drt::roots::load_roots_named("tunnel.extra_roots", &missing).unwrap_err();
    assert!(
        err.starts_with("tunnel.extra_roots '/nonexistent/gate.pem'"),
        "{err}"
    );
    let err = drt::roots::load_roots(&missing).unwrap_err();
    assert!(
        err.starts_with("--extra-root '/nonexistent/gate.pem'"),
        "{err}"
    );
}

/// Two modes in one tunnel are refused by name and by source, whether the
/// two keys came from two flags, two lines of the file, or one of each:
/// the command line and the file are judged as one set.
#[test]
fn two_modes_in_one_tunnel_are_refused_by_name_and_source() {
    let park_file = drt_config::TunnelConfig {
        park: Some("wss://relay.example/park/xps?k=park".into()),
        to: Some("127.0.0.1:22".into()),
        ..Default::default()
    };
    // A URL on the command line beside a file that parks.
    let flags = drt::tunnel::Flags {
        url: Some("wss://relay.example/s/xps?k=caller".into()),
        ..Default::default()
    };
    let err = drt::tunnel::resolve(Some(&park_file), &flags).unwrap_err();
    assert!(err.contains("`tunnel.park` in the config"), "{err}");
    assert!(err.contains("the URL on the command line"), "{err}");
    assert!(err.contains("two modes"), "{err}");

    // Two flags: what clap's `conflicts_with` used to say, said here so
    // the file and the flags share one refusal.
    let flags = drt::tunnel::Flags {
        url: Some("wss://relay.example/s/xps?k=caller".into()),
        park: Some("wss://relay.example/park/xps?k=park".into()),
        to: Some("127.0.0.1:22".into()),
        ..Default::default()
    };
    let err = drt::tunnel::resolve(None, &flags).unwrap_err();
    assert!(err.contains("the URL on the command line"), "{err}");
    assert!(err.contains("`--park`"), "{err}");

    // Two lines of one file.
    let both = drt_config::TunnelConfig {
        listen: Some("127.0.0.1:8022".into()),
        park: Some("wss://relay.example/park/xps?k=park".into()),
        to: Some("127.0.0.1:22".into()),
        ..Default::default()
    };
    let err = drt::tunnel::resolve(Some(&both), &drt::tunnel::Flags::default()).unwrap_err();
    assert!(err.contains("`tunnel.park` in the config"), "{err}");
    assert!(err.contains("`tunnel.listen` in the config"), "{err}");
}

/// A key that belongs to a mode this tunnel is not in is refused, never
/// ignored: in a file, a silently ignored key is exactly the failure a
/// loader exists to catch. And a mode missing the key it needs says which.
#[test]
fn a_key_from_another_mode_is_refused_rather_than_ignored() {
    let none = drt::tunnel::Flags::default();
    let resolve = |file: drt_config::TunnelConfig| drt::tunnel::resolve(Some(&file), &none);

    // `bind` without `claim`.
    let err = resolve(drt_config::TunnelConfig {
        bind: Some("127.0.0.1:2222".into()),
        ..Default::default()
    })
    .unwrap_err();
    assert!(err.contains("`tunnel.bind` in the config"), "{err}");
    assert!(err.contains("belongs with `claim`"), "{err}");

    // `to` beside a claim.
    let err = resolve(drt_config::TunnelConfig {
        claim: Some("wss://relay.example/s/xps?k=caller".into()),
        to: Some("127.0.0.1:22".into()),
        ..Default::default()
    })
    .unwrap_err();
    assert!(err.contains("`tunnel.to` in the config"), "{err}");
    assert!(err.contains("belongs with `park` or `listen`"), "{err}");

    // `bind` beside a park, typed as the flag.
    let err = drt::tunnel::resolve(
        Some(&drt_config::TunnelConfig {
            park: Some("wss://relay.example/park/xps?k=park".into()),
            to: Some("127.0.0.1:22".into()),
            ..Default::default()
        }),
        &drt::tunnel::Flags {
            local: Some("127.0.0.1:2222".into()),
            ..Default::default()
        },
    )
    .unwrap_err();
    assert!(err.contains("`--local`"), "{err}");
    assert!(err.contains("belongs with `claim`"), "{err}");

    // A park and a listen each need `to`, and say so with both spellings.
    for file in [
        drt_config::TunnelConfig {
            park: Some("wss://relay.example/park/xps?k=park".into()),
            ..Default::default()
        },
        drt_config::TunnelConfig {
            listen: Some("127.0.0.1:8022".into()),
            ..Default::default()
        },
    ] {
        let err = resolve(file).unwrap_err();
        assert!(err.contains("needs `to`"), "{err}");
        assert!(err.contains("`--to`"), "{err}");
        assert!(err.contains("`tunnel.to`"), "{err}");
    }

    // Nothing at all names both places a mode can come from.
    let err = drt::tunnel::resolve(None, &none).unwrap_err();
    assert!(err.contains("--park with --to"), "{err}");
    assert!(err.contains("`tunnel` in the --config file"), "{err}");
}
