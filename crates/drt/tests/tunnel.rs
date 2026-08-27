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
