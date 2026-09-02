//! One HTTPS GET, for `drt netcheck --reflect`.
//!
//! ## surface block
//!
//! - [`get`] — the only entry point: a URL in, a short body out.
//! - Bounds: [`MAX_BODY`], [`TIMEOUT`].
//!
//! Deliberately not the `rest` connector. `rest` is a guest-facing surface
//! whose job is an origin allowlist, injected credentials and a bounded
//! reply; this is the diagnostic asking one operator-named URL what it saw.
//! Wiring the verb layer through a connector crate to borrow an HTTP client
//! would couple them for the sake of about forty lines, and would drag a
//! capability scope into a place where no guest is involved.
//!
//! It shares `rest`'s *crates* — tokio-rustls and webpki-roots are already
//! in the `full` profile behind `rest` — so the `netcheck` feature adds no
//! new dependency to the artifact.
//!
//! What this is not: a general HTTP client. No redirects (a reflect edge
//! that redirects is a reflect edge that is misconfigured, and following one
//! would ask a second host the question), no keep-alive, no compression,
//! one request per call.

use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// A reflect answer is a few hundred bytes. This bounds a body that never
/// ends rather than expressing an opinion about JSON.
const MAX_BODY: usize = 64 * 1024;
/// Long enough for a slow edge, short enough that a diagnostic still
/// answers. The whole point of `netcheck` is to finish and say something.
const TIMEOUT: Duration = Duration::from_secs(10);

/// Fetch a URL and return its body as text.
pub async fn get(url: &str) -> Result<String, String> {
    tokio::time::timeout(TIMEOUT, fetch(url))
        .await
        .map_err(|_| format!("no answer within {}s", TIMEOUT.as_secs()))?
}

async fn fetch(url: &str) -> Result<String, String> {
    let (tls, rest) = match url.split_once("://") {
        Some(("https", rest)) => (true, rest),
        Some(("http", rest)) => (false, rest),
        _ => return Err("not an http or https url".into()),
    };
    let (authority, path) = match rest.split_once('/') {
        Some((a, p)) => (a, format!("/{p}")),
        None => (rest, "/".to_string()),
    };
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) if p.chars().all(|c| c.is_ascii_digit()) && !p.is_empty() => (
            h.to_string(),
            p.parse::<u16>().map_err(|_| "bad port".to_string())?,
        ),
        _ => (authority.to_string(), if tls { 443 } else { 80 }),
    };
    if host.is_empty() {
        return Err("no host in the url".into());
    }

    let request = format!(
        "GET {path} HTTP/1.1\r\nhost: {host}\r\nuser-agent: drt-netcheck\r\n\
         accept: application/json\r\nconnection: close\r\n\r\n"
    );

    let stream = tokio::net::TcpStream::connect((host.as_str(), port))
        .await
        .map_err(|e| format!("connect: {e}"))?;

    if tls {
        let mut roots = tokio_rustls::rustls::RootCertStore::empty();
        roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        let config = tokio_rustls::rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();
        let name = tokio_rustls::rustls::pki_types::ServerName::try_from(host.clone())
            .map_err(|_| "the url's host is not a name a certificate can be checked against")?;
        let io = tokio_rustls::TlsConnector::from(std::sync::Arc::new(config))
            .connect(name, stream)
            .await
            .map_err(|e| format!("tls: {e}"))?;
        exchange(io, request.as_bytes()).await
    } else {
        exchange(stream, request.as_bytes()).await
    }
}

// depth: read the whole response, then split it
//
// `connection: close` makes read-to-EOF terminate, which is what lets this
// be twenty lines instead of a chunked-transfer state machine. The body is
// still decoded when the edge frames it that way -- `rest` shipped without
// that and handed guests the framing (see doc/Failure-Modes.md and the
// changelog), which is a mistake worth not making twice in one repository.
async fn exchange<S: AsyncReadExt + AsyncWriteExt + Unpin>(
    mut io: S,
    request: &[u8],
) -> Result<String, String> {
    io.write_all(request)
        .await
        .map_err(|e| format!("send: {e}"))?;
    io.flush().await.map_err(|e| format!("send: {e}"))?;

    let mut buf = Vec::new();
    let mut chunk = [0u8; 8 * 1024];
    loop {
        let n = io
            .read(&mut chunk)
            .await
            .map_err(|e| format!("read: {e}"))?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
        if buf.len() > MAX_BODY {
            return Err("the edge answered more than this reads".into());
        }
    }

    let at = buf
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or("the response head never completed")?;
    let head = String::from_utf8_lossy(&buf[..at]);
    let status: u16 = head
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|c| c.parse().ok())
        .ok_or("the edge answered something that is not a response")?;
    // 429 is the shape discofetch's HAProxy uses for its per-address rate
    // limit. It is named here so the evidence line can say "rate limited"
    // rather than anything that sounds like a finding about the network.
    if status == 429 {
        return Err("rate limited by the edge".into());
    }
    if !(200..300).contains(&status) {
        return Err(format!("the edge answered {status}"));
    }

    let raw = &buf[at + 4..];
    let chunked = head
        .lines()
        .skip(1)
        .filter_map(|l| l.split_once(':'))
        .any(|(n, v)| {
            n.trim().eq_ignore_ascii_case("transfer-encoding")
                && v.to_ascii_lowercase().contains("chunked")
        });
    let body = if chunked { dechunk(raw)? } else { raw.to_vec() };
    String::from_utf8(body).map_err(|_| "the edge answered bytes that are not text".into())
}

/// Chunked decoding, strict: a frame this cannot read is an error rather
/// than a best-effort salvage. Same rule, and same reason, as the `rest`
/// connector's.
fn dechunk(mut raw: &[u8]) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    loop {
        let at = raw
            .windows(2)
            .position(|w| w == b"\r\n")
            .ok_or("chunked: a chunk header never ended")?;
        let head = &raw[..at];
        let size_text = match head.iter().position(|&b| b == b';') {
            Some(i) => &head[..i],
            None => head,
        };
        let size = std::str::from_utf8(size_text)
            .ok()
            .and_then(|t| usize::from_str_radix(t.trim(), 16).ok())
            .ok_or("chunked: not a chunk size")?;
        let rest = &raw[at + 2..];
        if size == 0 {
            return Ok(out);
        }
        if out.len() + size > MAX_BODY || rest.len() < size + 2 {
            return Err("chunked: a chunk was shorter than its size".into());
        }
        out.extend_from_slice(&rest[..size]);
        raw = &rest[size + 2..];
    }
}
