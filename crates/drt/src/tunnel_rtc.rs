//! `drt tunnel rtc:…`: stdio over a WebRTC session instead of a relay
//! (`doc/ssh-transport-matrix.md`, rows 3 and 6).
//!
//! The same `ProxyCommand` contract as the relay's claim, carried by the
//! native caller (`drt_rtc::caller`): the process is the caller in a
//! browser access session, opens one stream to what the answerer serves,
//! and moves stdin and stdout over it. Nothing between the two ends reads
//! a byte, and nothing carries one either: the path is direct, or there is
//! no path.
//!
//! ```text
//! ssh -o ProxyCommand="drt tunnel rtc:box.record.json" user@box       # direct mode
//! ssh -o ProxyCommand="drt tunnel rtc:https://box.example/session" user@box
//! ```
//!
//! ## surface block
//!
//! - Entry point: [`stdio`], a [`Source`] and a target carried out.
//! - Configurable: [`MAX_REPLY`], [`SIGNAL_TIMEOUT`].
//! - Fan-out: [`Source`], the two ways the answerer's record is had: in
//!   hand, which is direct mode (`doc/BrowserAccess.md` §3.4), or by
//!   `POST`ing this caller's record to a signaling endpoint that answers
//!   with the answerer's (the shape of `examples/29-browser-access`).

use std::time::Duration;

use drt_rtc::caller::{Caller, Target, CONNECT_TIMEOUT};
use drt_rtc::Record;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_rustls::rustls::pki_types::CertificateDer;

/// A record is 512 bytes (§2); this bounds a reply that is not one.
pub const MAX_REPLY: usize = 16 * 1024;
/// How long the signaling endpoint may take to answer.
pub const SIGNAL_TIMEOUT: Duration = Duration::from_secs(15);

/// Where the answerer's record comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    /// The record itself, or a file holding it: direct mode, no signaling.
    Record(String),
    /// An `http(s)://` endpoint: `POST` this caller's record, read the
    /// answerer's back.
    Post(String),
}

impl Source {
    /// What follows `rtc:`: a URL, a record, or a path to a file with one.
    pub fn parse(rest: &str) -> Result<Source, String> {
        if rest.starts_with("http://") || rest.starts_with("https://") {
            return Ok(Source::Post(rest.to_string()));
        }
        if rest.trim_start().starts_with('{') {
            return Ok(Source::Record(rest.to_string()));
        }
        let text = std::fs::read_to_string(rest).map_err(|e| {
            format!("rtc:{rest}: not a URL or a record, and not a file that reads: {e}")
        })?;
        Ok(Source::Record(text.trim().to_string()))
    }
}

/// Connect as the caller, open one stream to `to`, and move stdio over it
/// until either side ends.
pub async fn stdio(
    source: &Source,
    to: &str,
    extra_roots: &[CertificateDer<'static>],
    headers: &[(String, String)],
) -> Result<(), String> {
    let target = Target::parse(to).map_err(|e| format!("--to: {e}"))?;
    let any = "0.0.0.0:0".parse().expect("a literal");
    let (caller, answerer) = match source {
        Source::Record(text) => {
            let record = Record::decode(text).map_err(|e| format!("the answerer's record: {e}"))?;
            (Caller::direct(any).await?, record)
        }
        Source::Post(url) => {
            let caller = Caller::new(any).await?;
            let mine = caller
                .record()
                .encode()
                .map_err(|e| format!("this caller's record: {e}"))?;
            let reply =
                tokio::time::timeout(SIGNAL_TIMEOUT, post(url, &mine, extra_roots, headers))
                    .await
                    .map_err(|_| {
                        format!("{url} did not answer within {}s", SIGNAL_TIMEOUT.as_secs())
                    })??;
            let record = Record::decode(reply.trim())
                .map_err(|e| format!("{url} answered something that is not a record: {e}"))?;
            (caller, record)
        }
    };
    let call = caller.connect(&answerer, CONNECT_TIMEOUT).await?;
    let (stream, closed) = call.open(&target).await?;
    if std::io::IsTerminal::is_terminal(&std::io::stdin()) {
        eprintln!("drt tunnel: connected over WebRTC to {to}; stdin and stdout are the session");
    }
    let (mut from_peer, mut to_peer) = tokio::io::split(stream);
    let up = async {
        let _ = tokio::io::copy(&mut tokio::io::stdin(), &mut to_peer).await;
    };
    let down = async {
        let mut out = tokio::io::stdout();
        let n = tokio::io::copy(&mut from_peer, &mut out).await.unwrap_or(0);
        let _ = out.flush().await;
        n
    };
    // Either direction ending ends the session, as the relay's claim does.
    let received = tokio::select! {
        _ = up => None,
        n = down => Some(n),
    };
    // A stream refused before a byte came back says why, in the words the
    // wire has for it.
    if received == Some(0) {
        if let Ok(code) = closed.await {
            if code != drt_rtc::wisp::reason::VOLUNTARY {
                return Err(format!(
                    "the answerer closed the stream to {to}: {} (0x{code:02x})",
                    drt_rtc::wisp::reason::name(code)
                ));
            }
        }
    }
    Ok(())
}

// depth: one POST, and its reply

async fn post(
    url: &str,
    body: &str,
    extra_roots: &[CertificateDer<'static>],
    headers: &[(String, String)],
) -> Result<String, String> {
    let (tls, rest) = match url.split_once("://") {
        Some(("https", rest)) => (true, rest),
        Some(("http", rest)) => (false, rest),
        _ => return Err(format!("{url} is not an http or https URL")),
    };
    let (authority, path) = match rest.split_once('/') {
        Some((a, p)) => (a, format!("/{p}")),
        None => (rest, "/".to_string()),
    };
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) if !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()) => (
            h.to_string(),
            p.parse::<u16>().map_err(|_| format!("{url}: bad port"))?,
        ),
        _ => (authority.to_string(), if tls { 443 } else { 80 }),
    };
    let mut request = format!(
        "POST {path} HTTP/1.1\r\nhost: {authority}\r\nuser-agent: drt-tunnel\r\n\
         content-type: text/plain\r\ncontent-length: {}\r\nconnection: close\r\n",
        body.len()
    );
    for (name, value) in headers {
        request.push_str(&format!("{name}: {value}\r\n"));
    }
    request.push_str("\r\n");
    request.push_str(body);
    let tcp =
        tokio::net::TcpStream::connect((host.trim_start_matches('[').trim_end_matches(']'), port))
            .await
            .map_err(|e| format!("{url}: {e}"))?;
    let _ = tcp.set_nodelay(true);
    if tls {
        let config = tokio_rustls::rustls::ClientConfig::builder()
            .with_root_certificates(crate::roots::store(extra_roots))
            .with_no_client_auth();
        let name =
            tokio_rustls::rustls::pki_types::ServerName::try_from(host.clone()).map_err(|_| {
                format!("{url}: the host is not a name a certificate can be checked against")
            })?;
        let io = tokio_rustls::TlsConnector::from(std::sync::Arc::new(config))
            .connect(name, tcp)
            .await
            .map_err(|e| format!("{url}: tls: {e}"))?;
        exchange(io, request.as_bytes(), url).await
    } else {
        exchange(tcp, request.as_bytes(), url).await
    }
}

async fn exchange<S: AsyncReadExt + AsyncWriteExt + Unpin>(
    mut io: S,
    request: &[u8],
    url: &str,
) -> Result<String, String> {
    io.write_all(request)
        .await
        .map_err(|e| format!("{url}: {e}"))?;
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        let n = io
            .read(&mut chunk)
            .await
            .map_err(|e| format!("{url}: {e}"))?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
        if buf.len() > MAX_REPLY {
            return Err(format!("{url} answered more than a record"));
        }
    }
    let at = buf
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or_else(|| format!("{url}: the response head never completed"))?;
    let head = String::from_utf8_lossy(&buf[..at]);
    let status: u16 = head
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|c| c.parse().ok())
        .ok_or_else(|| format!("{url} answered something that is not HTTP"))?;
    if !(200..300).contains(&status) {
        return Err(format!("{url} answered {status}"));
    }
    if head
        .to_ascii_lowercase()
        .contains("transfer-encoding: chunked")
    {
        return Err(format!(
            "{url} answered chunked; a record is one short body"
        ));
    }
    String::from_utf8(buf[at + 4..].to_vec())
        .map_err(|_| format!("{url} answered bytes that are not text"))
}
