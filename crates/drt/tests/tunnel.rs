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
                let _ = drt::tunnel::stream_to_ws(conn, &url).await;
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
        let _ = drt::tunnel::stream_to_ws(conn, &ws_url).await;
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
        let _ = drt::tunnel::serve_local(local, &ws_url).await;
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
        let _ = drt::tunnel::serve_local(local, &refusing_url).await;
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
        let _ = drt::tunnel::serve_local(local, &gone_url).await;
    });
    let mut c = tokio::net::TcpStream::connect(local_addr).await.unwrap();
    let n = tokio::time::timeout(Duration::from_secs(5), c.read(&mut buf))
        .await
        .expect("the connection to a missing relay was closed within five seconds")
        .unwrap_or(0);
    assert_eq!(n, 0);
}
