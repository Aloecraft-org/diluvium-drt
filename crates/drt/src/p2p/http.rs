//! The HTTP this verb speaks: the requests of `doc/DRT-Signalling.md` §2,
//! made by a caller (`POST …/calls`) and a parked side (`GET …/calls`,
//! `POST …/answer`, the call notification stream). One small client,
//! because the whole profile is a few requests with short bodies and
//! nothing here wants a connection pool.
//!
//! ## surface block
//!
//! - Entry points: [`request`], one request and its whole reply;
//!   [`stream`], one request whose body keeps coming (`text/event-stream`).
//! - Configurable: [`MAX_REPLY`], [`MAX_HEAD`], [`TIMEOUT`], [`REFUSALS`].
//! - Fan-out: [`Body`], the three ways a reply's length is known
//!   (chunked, `content-length`, close).

use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio_rustls::rustls::pki_types::CertificateDer;

/// A record is 512 bytes (§1); this bounds a reply that is not one.
pub const MAX_REPLY: usize = 16 * 1024;
/// The response head, status line and headers together.
pub const MAX_HEAD: usize = 16 * 1024;
/// How long one request may take: the profile's longest hold time (§3,
/// 30 s) and a margin, so a server's own 504 arrives before this gives up.
pub const TIMEOUT: Duration = Duration::from_secs(35);

/// The statuses `doc/DRT-Signalling.md` §2 names, as a client reads them.
/// A status not here is reported by its number alone.
pub const REFUSALS: &[(u16, &str)] = &[
    (400, "the request was malformed"),
    (
        401,
        "no token, or one this server does not know (`--H auth=…`)",
    ),
    (403, "this token may not do that"),
    (404, "no such name on this server"),
    (409, "the call was already answered"),
    (410, "the answerer refused the call"),
    (413, "this record is too large"),
    (429, "too many are waiting; try again"),
    (503, "no answerer is present"),
    (504, "nobody answered within the server's hold time"),
];

/// One reply, whole.
#[derive(Debug, Clone)]
pub struct Response {
    pub status: u16,
    /// Names lowercased.
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Response {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }

    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    /// What a status outside 2xx means: the server's own words when its
    /// body is the profile's `{"error": …}`, since one status covers
    /// several refusals (a 403 is a wrong token or an address outside
    /// DRT-Accept), else the profile's.
    pub fn refusal(&self, url: &str) -> Option<String> {
        if (200..300).contains(&self.status) {
            return None;
        }
        let shown = crate::tunnel::shown(url);
        let said = serde_json::from_slice::<serde_json::Value>(&self.body)
            .ok()
            .and_then(|v| v["error"].as_str().map(str::to_string))
            .filter(|e| !e.is_empty() && e.len() <= 200 && !e.contains('\n'));
        if let Some(said) = said {
            return Some(format!("{shown} answered {}: {said}", self.status));
        }
        Some(match REFUSALS.iter().find(|(s, _)| *s == self.status) {
            Some((_, meaning)) => format!("{shown} answered {}: {meaning}", self.status),
            None => format!("{shown} answered {}", self.status),
        })
    }
}

/// One request, and its reply read to the end. `body` is sent as
/// `text/plain;charset=utf-8`, which is what §2 asks for.
pub async fn request(
    method: &str,
    url: &str,
    headers: &[(String, String)],
    body: Option<&str>,
    extra_roots: &[CertificateDer<'static>],
) -> Result<Response, String> {
    let (mut head, mut reader) =
        tokio::time::timeout(TIMEOUT, stream(method, url, headers, body, extra_roots))
            .await
            .map_err(|_| {
                format!(
                    "{} did not answer within {}s",
                    crate::tunnel::shown(url),
                    TIMEOUT.as_secs()
                )
            })??;
    let shown = crate::tunnel::shown(url);
    let rest = tokio::time::timeout(TIMEOUT, async {
        let mut all = Vec::new();
        while let Some(chunk) = reader.next().await? {
            all.extend_from_slice(&chunk);
            if all.len() > MAX_REPLY {
                return Err(format!("{shown} answered more than a record"));
            }
        }
        Ok(all)
    })
    .await
    .map_err(|_| {
        format!(
            "{shown} did not finish answering within {}s",
            TIMEOUT.as_secs()
        )
    })??;
    head.body = rest;
    Ok(head)
}

/// One request, with the reply's head read and its body left to read as
/// it arrives: the call notification stream (§5), or anything else a
/// caller wants to read as it comes.
pub async fn stream(
    method: &str,
    url: &str,
    headers: &[(String, String)],
    body: Option<&str>,
    extra_roots: &[CertificateDer<'static>],
) -> Result<(Response, Body), String> {
    let shown = crate::tunnel::shown(url);
    let (tls, rest) = match url.split_once("://") {
        Some(("https", rest)) => (true, rest),
        Some(("http", rest)) => (false, rest),
        _ => return Err(format!("{shown} is not an http or https URL")),
    };
    let (authority, path) = match rest.split_once('/') {
        Some((a, p)) => (a, format!("/{p}")),
        None => (rest, "/".to_string()),
    };
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) if !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()) => (
            h.to_string(),
            p.parse::<u16>().map_err(|_| format!("{shown}: bad port"))?,
        ),
        _ => (authority.to_string(), if tls { 443 } else { 80 }),
    };
    let body = body.unwrap_or("");
    let mut request = format!(
        "{method} {path} HTTP/1.1\r\nhost: {authority}\r\nuser-agent: drt-p2p\r\n\
         accept: */*\r\nconnection: close\r\n"
    );
    if !body.is_empty() || method == "POST" {
        request.push_str(&format!(
            "content-type: text/plain;charset=utf-8\r\ncontent-length: {}\r\n",
            body.len()
        ));
    }
    for (name, value) in headers {
        request.push_str(&format!("{name}: {value}\r\n"));
    }
    request.push_str("\r\n");
    request.push_str(body);
    let tcp =
        tokio::net::TcpStream::connect((host.trim_start_matches('[').trim_end_matches(']'), port))
            .await
            .map_err(|e| format!("{shown}: {e}"))?;
    let _ = tcp.set_nodelay(true);
    let io: Box<dyn Io> = if tls {
        let config = tokio_rustls::rustls::ClientConfig::builder()
            .with_root_certificates(crate::roots::store(extra_roots))
            .with_no_client_auth();
        let name = tokio_rustls::rustls::pki_types::ServerName::try_from(
            host.trim_start_matches('[')
                .trim_end_matches(']')
                .to_string(),
        )
        .map_err(|_| {
            format!("{shown}: the host is not a name a certificate can be checked against")
        })?;
        Box::new(
            tokio_rustls::TlsConnector::from(std::sync::Arc::new(config))
                .connect(name, tcp)
                .await
                .map_err(|e| format!("{shown}: tls: {e}"))?,
        )
    } else {
        Box::new(tcp)
    };
    exchange(io, request.as_bytes(), &shown).await
}

trait Io: AsyncRead + AsyncWrite + Send + Unpin {}
impl<T: AsyncRead + AsyncWrite + Send + Unpin> Io for T {}

// depth: the reply's head, then its body by whichever rule frames it

async fn exchange(
    mut io: Box<dyn Io>,
    request: &[u8],
    shown: &str,
) -> Result<(Response, Body), String> {
    io.write_all(request)
        .await
        .map_err(|e| format!("{shown}: {e}"))?;
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    let at = loop {
        if let Some(at) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break at;
        }
        if buf.len() > MAX_HEAD {
            return Err(format!("{shown}: the response head is too long"));
        }
        let n = io
            .read(&mut chunk)
            .await
            .map_err(|e| format!("{shown}: {e}"))?;
        if n == 0 {
            return Err(format!("{shown}: the response head never completed"));
        }
        buf.extend_from_slice(&chunk[..n]);
    };
    let head = String::from_utf8_lossy(&buf[..at]).into_owned();
    let rest = buf[at + 4..].to_vec();
    let mut lines = head.lines();
    let status: u16 = lines
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|c| c.parse().ok())
        .ok_or_else(|| format!("{shown} answered something that is not HTTP"))?;
    let headers: Vec<(String, String)> = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(n, v)| (n.trim().to_ascii_lowercase(), v.trim().to_string()))
        .collect();
    let find = |name: &str| {
        headers
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    };
    let framing =
        if find("transfer-encoding").is_some_and(|v| v.to_ascii_lowercase().contains("chunked")) {
            Framing::Chunked {
                remaining: 0,
                done: false,
            }
        } else if let Some(len) = find("content-length").and_then(|v| v.parse::<usize>().ok()) {
            Framing::Length(len)
        } else if status == 204 || status == 304 {
            Framing::Length(0)
        } else {
            Framing::Close
        };
    let response = Response {
        status,
        headers,
        body: Vec::new(),
    };
    Ok((
        response,
        Body {
            io,
            framing,
            pending: rest,
            shown: shown.to_string(),
        },
    ))
}

/// How a body's end is known.
#[derive(Clone, Copy)]
enum Framing {
    Chunked { remaining: usize, done: bool },
    Length(usize),
    Close,
}

/// A reply's body, read as it arrives.
pub struct Body {
    io: Box<dyn Io>,
    framing: Framing,
    /// Bytes read past the head and not yet handed out.
    pending: Vec<u8>,
    shown: String,
}

impl Body {
    /// The next piece of the body, or `None` at its end. A piece is
    /// never empty.
    pub async fn next(&mut self) -> Result<Option<Vec<u8>>, String> {
        loop {
            match self.framing {
                Framing::Length(0) => return Ok(None),
                Framing::Length(left) => {
                    if self.pending.is_empty() && !self.fill().await? {
                        return Err(format!("{}: the body ended early", self.shown));
                    }
                    let n = self.pending.len().min(left);
                    self.framing = Framing::Length(left - n);
                    return Ok(Some(self.pending.drain(..n).collect()));
                }
                Framing::Close => {
                    if self.pending.is_empty() && !self.fill().await? {
                        return Ok(None);
                    }
                    return Ok(Some(std::mem::take(&mut self.pending)));
                }
                Framing::Chunked { done: true, .. } => return Ok(None),
                Framing::Chunked { remaining: 0, .. } => {
                    // A size line, then the chunk; a size of 0 ends it.
                    let Some(eol) = self.pending.windows(2).position(|w| w == b"\r\n") else {
                        if !self.fill().await? {
                            return Err(format!("{}: the body ended early", self.shown));
                        }
                        continue;
                    };
                    let line = String::from_utf8_lossy(&self.pending[..eol]).into_owned();
                    self.pending.drain(..eol + 2);
                    let size = line.split(';').next().unwrap_or("").trim();
                    if size.is_empty() {
                        // The CRLF that ends the previous chunk.
                        continue;
                    }
                    let size = usize::from_str_radix(size, 16)
                        .map_err(|_| format!("{}: a chunk size that is not hex", self.shown))?;
                    if size == 0 {
                        self.framing = Framing::Chunked {
                            remaining: 0,
                            done: true,
                        };
                        return Ok(None);
                    }
                    self.framing = Framing::Chunked {
                        remaining: size,
                        done: false,
                    };
                }
                Framing::Chunked { remaining, .. } => {
                    if self.pending.is_empty() && !self.fill().await? {
                        return Err(format!("{}: the body ended early", self.shown));
                    }
                    let n = self.pending.len().min(remaining);
                    let piece: Vec<u8> = self.pending.drain(..n).collect();
                    self.framing = Framing::Chunked {
                        remaining: remaining - n,
                        done: false,
                    };
                    return Ok(Some(piece));
                }
            }
        }
    }

    async fn fill(&mut self) -> Result<bool, String> {
        let mut chunk = [0u8; 4096];
        let n = self
            .io
            .read(&mut chunk)
            .await
            .map_err(|e| format!("{}: {e}", self.shown))?;
        self.pending.extend_from_slice(&chunk[..n]);
        Ok(n > 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::TcpListener;

    /// A server that answers every request with `reply`, verbatim.
    async fn server(reply: &'static [u8]) -> String {
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/v1/x/calls", l.local_addr().unwrap());
        tokio::spawn(async move {
            while let Ok((mut s, _)) = l.accept().await {
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 4096];
                    let _ = s.read(&mut buf).await;
                    let _ = s.write_all(reply).await;
                });
            }
        });
        url
    }

    #[tokio::test]
    async fn a_reply_is_read_by_length_by_chunk_or_to_the_close() {
        let by_length = server(
            b"HTTP/1.1 200 OK\r\ncontent-length: 5\r\nlocation: /v1/x/calls/c1\r\n\r\nhello",
        )
        .await;
        let r = request("POST", &by_length, &[], Some("{}"), &[])
            .await
            .unwrap();
        assert_eq!(
            (r.status, r.text().as_str(), r.header("location")),
            (200, "hello", Some("/v1/x/calls/c1"))
        );

        let chunked = server(b"HTTP/1.1 200 OK\r\ntransfer-encoding: chunked\r\n\r\n3\r\nabc\r\n2;ext\r\nde\r\n0\r\n\r\n").await;
        let r = request("GET", &chunked, &[], None, &[]).await.unwrap();
        assert_eq!(r.text(), "abcde");

        let to_close =
            server(b"HTTP/1.1 503 Service Unavailable\r\n\r\n{\"error\":\"nobody\"}").await;
        let r = request("GET", &to_close, &[], None, &[]).await.unwrap();
        assert_eq!(r.status, 503);
        assert!(r
            .refusal(&to_close)
            .unwrap()
            .contains("no answerer is present"));
        assert!(r
            .refusal(&to_close)
            .unwrap()
            .starts_with("http://127.0.0.1:"));
    }

    #[tokio::test]
    async fn a_streamed_body_arrives_piece_by_piece() {
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/v1/x/events", l.local_addr().unwrap());
        tokio::spawn(async move {
            let (mut s, _) = l.accept().await.unwrap();
            let mut buf = vec![0u8; 4096];
            let _ = s.read(&mut buf).await;
            s.write_all(b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ntransfer-encoding: chunked\r\n\r\n").await.unwrap();
            for piece in ["retry: 2000\n\n", "id: 1\nevent: call\ndata: {}\n\n"] {
                s.write_all(format!("{:x}\r\n{piece}\r\n", piece.len()).as_bytes())
                    .await
                    .unwrap();
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            s.write_all(b"0\r\n\r\n").await.unwrap();
        });
        let (head, mut body) = stream("GET", &url, &[], None, &[]).await.unwrap();
        assert_eq!(head.header("content-type"), Some("text/event-stream"));
        let mut pieces = Vec::new();
        while let Some(p) = body.next().await.unwrap() {
            pieces.push(String::from_utf8(p).unwrap());
        }
        assert_eq!(
            pieces.concat(),
            "retry: 2000\n\nid: 1\nevent: call\ndata: {}\n\n"
        );
        assert!(pieces.len() >= 2, "{pieces:?}");
    }
}
