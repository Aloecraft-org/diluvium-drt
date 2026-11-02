//! The `bridge` block as `drt start` runs it: the real binary, a config
//! file naming the block and `stdlib:bridge`, a WebSocket offering
//! websockify's `binary` subprotocol the way a browser's Emscripten
//! socket layer does, and a TCP echo server as the one target.
//!
//! What is checked is the wiring a person relies on: the block binds what
//! it names, answers `binary`, carries bytes both ways, and the program
//! hears `opened` and `closed` with the byte counts.

#![cfg(feature = "bridge")]

use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;

/// A process killed when the test ends, pass or fail.
struct Killed(std::process::Child);

impl Drop for Killed {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Lines from a child's pipe, on a thread, so a test can wait for one with
/// a deadline instead of blocking on a read that never ends.
fn lines(pipe: impl std::io::Read + Send + 'static) -> mpsc::Receiver<String> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(pipe).lines() {
            let Ok(line) = line else { break };
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    rx
}

/// The first line `pick` accepts within ten seconds.
fn wait_for<T>(rx: &mpsc::Receiver<String>, what: &str, pick: impl Fn(&str) -> Option<T>) -> T {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        match rx.recv_timeout(left) {
            Ok(line) => {
                if let Some(found) = pick(&line) {
                    return found;
                }
            }
            Err(_) => panic!("no line {what} within ten seconds"),
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_bridge_block_carries_a_browser_shaped_websocket_to_its_target() {
    // The one target: a TCP echo server.
    let echo = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let echo_addr = echo.local_addr().unwrap().to_string();
    tokio::spawn(async move {
        while let Ok((mut c, _)) = echo.accept().await {
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

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("bridge.json");
    let config = serde_json::json!({
        "entry": "stdlib:bridge",
        "bridge": { "bind": "127.0.0.1:0", "to": echo_addr },
    });
    std::fs::write(&path, config.to_string()).unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_drt"))
        .arg("--config")
        .arg(&path)
        .arg("start")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let stdout = lines(child.stdout.take().unwrap());
    let stderr = lines(child.stderr.take().unwrap());
    let _child = Killed(child);

    // The block says where it bound; that line is the port.
    let addr: String = wait_for(&stderr, "naming the bridge's address", |line| {
        line.strip_prefix("drt bridge: ws on ")
            .and_then(|rest| rest.split_whitespace().next())
            .map(str::to_string)
    });

    // As Emscripten's SOCKFS opens it: any path, `binary` offered.
    let mut request = format!("ws://{addr}/websockify")
        .into_client_request()
        .unwrap();
    request
        .headers_mut()
        .insert("Sec-WebSocket-Protocol", "binary".parse().unwrap());
    let (mut ws, response) = tokio_tungstenite::connect_async(request).await.unwrap();
    assert_eq!(
        response
            .headers()
            .get("Sec-WebSocket-Protocol")
            .map(|v| v.to_str().unwrap()),
        Some("binary")
    );
    ws.send(Message::Binary(b"marco".to_vec())).await.unwrap();
    let back = loop {
        match ws.next().await.unwrap().unwrap() {
            Message::Binary(b) => break b,
            Message::Ping(_) | Message::Pong(_) => continue,
            other => panic!("expected a binary frame, got {other:?}"),
        }
    };
    assert_eq!(back, b"marco");
    ws.close(None).await.unwrap();

    // The program hears both ends of the session, with what crossed.
    let opened = wait_for(&stdout, "reporting `opened`", |line| {
        line.starts_with("opened ").then(|| line.to_string())
    });
    assert!(opened.contains(&format!("to={echo_addr}")), "{opened}");
    let closed = wait_for(&stdout, "reporting `closed`", |line| {
        line.starts_with("closed ").then(|| line.to_string())
    });
    assert!(closed.contains("sent=5"), "{closed}");
    assert!(closed.contains("received=5"), "{closed}");
    assert!(closed.contains(&format!("to={echo_addr}")), "{closed}");
}

/// A target with no port is refused before anything binds, naming the key.
#[test]
fn a_bridge_block_without_a_port_in_its_target_is_refused_at_start() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("bridge.json");
    let config = serde_json::json!({
        "entry": "stdlib:bridge",
        "bridge": { "bind": "127.0.0.1:0", "to": "10.0.0.5" },
    });
    std::fs::write(&path, config.to_string()).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_drt"))
        .arg("--config")
        .arg(&path)
        .arg("start")
        .output()
        .unwrap();
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("bridge.to `10.0.0.5` is not host:port"),
        "{stderr}"
    );
}
