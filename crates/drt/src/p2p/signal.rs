//! A listening peer's own signalling port (`doc/P2P.md` §2.3, `--signal`):
//! the caller's request of `doc/DRT-Signalling.md` §3, answered at once
//! with this peer's record, since the record is fixed and nobody has to
//! be asked. `POST /` and `POST /v1/<any name>/calls` alike (§11), and
//! `access-control-allow-origin: *`, because admission here is the UDP
//! side's (`--host`) and never the page's origin.
//!
//! ## surface block
//!
//! - Entry points: [`serve`], the accept loop over a listener the caller
//!   bound.
//! - Configurable: [`MAX_BODY`], [`READ_TIMEOUT`].
//! - Fan-out: the match on method and path in `answer`.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use drt_rtc::{Command, Record, Sender};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;

/// A request body: a record is 512 bytes, the profile allows 1 KiB (§6).
pub const MAX_BODY: usize = 1024;
/// How long a client may take to send its request.
pub const READ_TIMEOUT: Duration = Duration::from_secs(10);

const CORS: &str = "access-control-allow-origin: *\r\n\
    access-control-allow-methods: POST, OPTIONS\r\n\
    access-control-allow-headers: authorization, content-type\r\n\
    access-control-expose-headers: location\r\n";

/// Accept forever; each request is answered on its own task.
pub async fn serve(listener: TcpListener, sender: Sender, record: watch::Receiver<String>) {
    let calls = std::sync::Arc::new(AtomicU64::new(0));
    loop {
        let Ok((conn, peer)) = listener.accept().await else {
            continue;
        };
        let _ = conn.set_nodelay(true);
        let (sender, record, calls) = (sender.clone(), record.clone(), calls.clone());
        tokio::spawn(async move {
            if let Err(e) =
                tokio::time::timeout(READ_TIMEOUT, answer(conn, &sender, &record, &calls))
                    .await
                    .unwrap_or_else(|_| Err("the request took too long".into()))
            {
                eprintln!("drt p2p: signal: {peer}: {e}");
            }
        });
    }
}

// depth: one request

async fn answer(
    mut conn: TcpStream,
    sender: &Sender,
    record: &watch::Receiver<String>,
    calls: &AtomicU64,
) -> Result<(), String> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 2048];
    let head_end = loop {
        if let Some(at) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break at;
        }
        if buf.len() > 8192 {
            return reply(
                &mut conn,
                431,
                "{\"error\":\"the request head is too long\"}",
                "",
            )
            .await;
        }
        let n = conn.read(&mut chunk).await.map_err(|e| e.to_string())?;
        if n == 0 {
            return Err("the request never completed".into());
        }
        buf.extend_from_slice(&chunk[..n]);
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
    let mut lines = head.lines();
    let mut request = lines.next().unwrap_or("").split_whitespace();
    let (method, path) = (
        request.next().unwrap_or("").to_string(),
        request.next().unwrap_or("").to_string(),
    );
    let path = path.split('?').next().unwrap_or("").to_string();
    let length: usize = lines
        .filter_map(|l| l.split_once(':'))
        .find(|(n, _)| n.trim().eq_ignore_ascii_case("content-length"))
        .and_then(|(_, v)| v.trim().parse().ok())
        .unwrap_or(0);
    if method == "OPTIONS" {
        return reply(&mut conn, 204, "", "").await;
    }
    let named = path
        .strip_prefix("/v1/")
        .and_then(|rest| rest.strip_suffix("/calls"))
        .filter(|name| !name.is_empty() && !name.contains('/'));
    if method != "POST" || !(path == "/" || named.is_some()) {
        return reply(
            &mut conn,
            404,
            "{\"error\":\"POST / or /v1/<name>/calls\"}",
            "",
        )
        .await;
    }
    if length > MAX_BODY {
        return reply(
            &mut conn,
            413,
            "{\"error\":\"a record is at most 512 bytes\"}",
            "",
        )
        .await;
    }
    let mut body = buf[head_end + 4..].to_vec();
    while body.len() < length {
        let n = conn.read(&mut chunk).await.map_err(|e| e.to_string())?;
        if n == 0 {
            return Err("the body never completed".into());
        }
        body.extend_from_slice(&chunk[..n]);
    }
    body.truncate(length);
    let text = match std::str::from_utf8(&body) {
        Ok(t) => t.trim().to_string(),
        Err(_) => return reply(&mut conn, 400, "{\"error\":\"a record is text\"}", "").await,
    };
    if let Err(e) = Record::decode(&text) {
        let why = serde_json::json!({"error": format!("not a record: {e}")}).to_string();
        return reply(&mut conn, 400, &why, "").await;
    }
    let n = calls.fetch_add(1, Ordering::Relaxed) + 1;
    let base = match named {
        Some(name) => format!("/v1/{name}/calls/"),
        None => "/calls/".to_string(),
    };
    sender.send(Command::Open {
        peer: format!("call-{n}"),
        rtc: text,
    });
    let ours = record.borrow().clone();
    reply(&mut conn, 200, &ours, &format!("location: {base}c{n}\r\n")).await
}

async fn reply(conn: &mut TcpStream, status: u16, body: &str, extra: &str) -> Result<(), String> {
    let reason = match status {
        200 => "OK",
        204 => "No Content",
        400 => "Bad Request",
        404 => "Not Found",
        413 => "Payload Too Large",
        431 => "Request Header Fields Too Large",
        _ => "",
    };
    let head = format!(
        "HTTP/1.1 {status} {reason}\r\ncontent-type: application/json\r\n\
         content-length: {}\r\n{CORS}{extra}connection: close\r\n\r\n",
        body.len()
    );
    conn.write_all(head.as_bytes())
        .await
        .map_err(|e| e.to_string())?;
    conn.write_all(body.as_bytes())
        .await
        .map_err(|e| e.to_string())?;
    let _ = conn.shutdown().await;
    Ok(())
}
