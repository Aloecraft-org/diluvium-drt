//! A host for `browser-check/check.mjs`: an echo server in scope, the host
//! beside it, and JSON lines on stdio so a script can play signaling.
//!
//! stdout, one object per line: `{"event":"echo","port":N}` first, then
//! `{"event":"record","rtc":"…"}`, then a line per session and stream report.
//! stdin, one browser record per line: each becomes `open` for peer `b<n>`.
//!
//! `cargo run -p drt-rtc --example browser_check [bind]`, bind defaulting to
//! `0.0.0.0:0`, which advertises the address this box routes from.

use std::time::Duration;

use drt_rtc::host::{Event, SessionState, StreamState};
use drt_rtc::{Command, Entry, Host, HostConfig, Identity, Scope};
use serde_json::json;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

#[tokio::main]
async fn main() {
    let bind = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "0.0.0.0:0".into());
    let echo = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = echo.local_addr().unwrap().port();
    tokio::spawn(async move {
        while let Ok((mut s, _)) = echo.accept().await {
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
    println!("{}", json!({"event": "echo", "port": port}));

    let cfg = HostConfig {
        bind: bind.parse().expect("bind is ip:port"),
        identity: Identity::generate().unwrap(),
        stun: vec![],
        publish_host_candidates: true,
        service: "browser check".into(),
        default: None,
        scope: Scope::new(vec![
            Entry::parse(&format!("http://127.0.0.1:{port}")).unwrap()
        ]),
        max_sessions: 8,
        max_streams: 8,
        idle_timeout: Duration::from_secs(60),
        connect_timeout: Duration::from_secs(5),
        stun_refresh: Duration::from_secs(20),
    };
    let mut host = Host::start(cfg).expect("the host starts");
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    let mut n = 0;
    loop {
        tokio::select! {
            line = lines.next_line() => match line {
                Ok(Some(rtc)) if !rtc.trim().is_empty() => {
                    n += 1;
                    host.send(Command::Open { peer: format!("b{n}"), rtc: rtc.trim().to_string() });
                }
                Ok(Some(_)) => {}
                _ => return,
            },
            Some(e) = host.next_event() => println!("{}", match e {
                Event::Record { rtc } => json!({"event": "record", "rtc": rtc}),
                Event::Session { peer, state, reason } => json!({
                    "event": "session", "peer": peer,
                    "state": if state == SessionState::Connected { "connected" } else { "closed" },
                    "reason": reason,
                }),
                Event::Stream { peer, stream, host, port, state, reason, bytes_up, bytes_down } => json!({
                    "event": "stream", "peer": peer, "stream": stream, "host": host, "port": port,
                    "state": if state == StreamState::Open { "open" } else { "closed" },
                    "reason": reason, "bytes_up": bytes_up, "bytes_down": bytes_down,
                }),
            }),
        }
    }
}
