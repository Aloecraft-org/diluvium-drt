//! `--turn` (`doc/P2P.md` §2.6): a TURN allocation as one more candidate,
//! below the direct ones, so ICE uses it only when nothing direct works.
//! The allocation is made here, with webrtc-rs's client, the one the
//! WireGuard fallback already uses; drt-rtc gets it as a
//! [`drt_rtc::Relayed`], two channels of datagrams.
//!
//! ## surface block
//!
//! - Entry points: [`TurnUri::parse`]; [`allocate`], the allocation as a
//!   [`drt_rtc::Relayed`].
//! - Configurable: [`DEFAULT_PORT`], [`ALLOCATE_TIMEOUT`].
//! - Fan-out: [`allocate`] has two builds: with `turn-client`, and
//!   without, which refuses by name.

use std::time::Duration;

/// TURN's port when the URI names none (RFC 8656).
pub const DEFAULT_PORT: u16 = 3478;
/// How long an allocation may take before the role goes on without one.
pub const ALLOCATE_TIMEOUT: Duration = Duration::from_secs(5);

/// `turn://<user>:<password>@host[:port]`: a TURN server and the
/// credential it allocates for. The credential is coturn's
/// `use-auth-secret` pair when the server is `drt turn` or coturn, minted
/// by `crypto/turn_credential`.
#[derive(Clone, PartialEq, Eq)]
pub struct TurnUri {
    /// `host:port`.
    pub server: String,
    pub username: String,
    pub password: String,
}

impl std::fmt::Debug for TurnUri {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The password is a credential: never in a log or a panic.
        f.debug_struct("TurnUri")
            .field("server", &self.server)
            .field("username", &self.username)
            .finish_non_exhaustive()
    }
}

impl TurnUri {
    pub fn parse(s: &str) -> Result<TurnUri, String> {
        let shape = "turn://<user>:<password>@host[:port]";
        let rest = s
            .strip_prefix("turn://")
            .ok_or_else(|| format!("a TURN server is {shape}"))?;
        let (credential, server) = rest
            .rsplit_once('@')
            .ok_or_else(|| format!("{shape}: the credential is missing"))?;
        let (username, password) = credential
            .split_once(':')
            .ok_or_else(|| format!("{shape}: the password is missing"))?;
        let server = server.trim_end_matches('/');
        if username.is_empty() || password.is_empty() || server.is_empty() {
            return Err(format!("a TURN server is {shape}"));
        }
        let server = match server.rsplit_once(':') {
            Some((_, port)) if !server.ends_with(']') => {
                port.parse::<u16>()
                    .map_err(|_| format!("{shape}: '{port}' is not a port"))?;
                server.to_string()
            }
            _ => format!("{server}:{DEFAULT_PORT}"),
        };
        Ok(TurnUri {
            server,
            username: percent_decoded(username)?,
            password: percent_decoded(password)?,
        })
    }

    /// The server, without the credential: what a log may say.
    pub fn shown(&self) -> String {
        format!("turn://{}", self.server)
    }
}

/// `%XX` escapes, for a credential holding `:` or `@`: coturn's usernames
/// are `<expiry>:<principal>`, so the colon arrives as `%3A`.
fn percent_decoded(s: &str) -> Result<String, String> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = s
                .get(i + 1..i + 3)
                .and_then(|h| u8::from_str_radix(h, 16).ok())
                .ok_or_else(|| format!("'{s}': a % is not followed by two hex digits"))?;
            out.push(hex);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).map_err(|_| format!("'{s}' is not UTF-8 once decoded"))
}

// depth: the allocation, and the two pumps between it and drt-rtc

/// Allocate on `uri`'s server and hand the allocation over as a
/// [`drt_rtc::Relayed`]. The client and the allocation live as long as
/// the channels do: dropping the host or caller that holds them ends both.
#[cfg(feature = "turn-client")]
pub async fn allocate(uri: &TurnUri) -> Result<drt_rtc::Relayed, String> {
    use std::sync::Arc;
    use webrtc_util::Conn;

    let server = tokio::net::lookup_host(uri.server.as_str())
        .await
        .map_err(|e| format!("{}: {e}", uri.shown()))?
        .next()
        .ok_or_else(|| format!("{}: no address", uri.shown()))?;
    let bind = if server.is_ipv4() {
        "0.0.0.0:0"
    } else {
        "[::]:0"
    };
    let socket = tokio::net::UdpSocket::bind(bind)
        .await
        .map_err(|e| format!("a socket for the TURN client: {e}"))?;
    let local = socket
        .local_addr()
        .map_err(|e| format!("a socket for the TURN client: {e}"))?;
    let client = turn::client::Client::new(turn::client::ClientConfig {
        stun_serv_addr: server.to_string(),
        turn_serv_addr: server.to_string(),
        username: uri.username.clone(),
        password: uri.password.clone(),
        realm: String::new(),
        software: String::new(),
        rto_in_ms: 0,
        conn: Arc::new(socket),
        vnet: None,
    })
    .await
    .map_err(|e| format!("{}: {e}", uri.shown()))?;
    client
        .listen()
        .await
        .map_err(|e| format!("{}: {e}", uri.shown()))?;
    let conn = match tokio::time::timeout(ALLOCATE_TIMEOUT, client.allocate()).await {
        Ok(Ok(conn)) => Arc::new(conn),
        Ok(Err(e)) => return Err(format!("{} would not allocate: {e}", uri.shown())),
        Err(_) => {
            return Err(format!(
                "{} did not allocate within {}s",
                uri.shown(),
                ALLOCATE_TIMEOUT.as_secs()
            ))
        }
    };
    let address = conn
        .local_addr()
        .map_err(|e| format!("{}: the allocation has no address: {e}", uri.shown()))?;
    let (outbound, mut to_relay) =
        tokio::sync::mpsc::unbounded_channel::<drt_rtc::relayed::Datagram>();
    let (from_relay, inbound) = tokio::sync::mpsc::unbounded_channel();
    // Out: what the host or caller sends from the relayed address. The
    // client creates the peer's permission on the first send to it.
    let sending = conn.clone();
    let out = tokio::spawn(async move {
        while let Some((data, to)) = to_relay.recv().await {
            let _ = sending.send_to(&data, to).await;
        }
    });
    // In: what peers sent to the relayed address. Ends, with the client
    // and the allocation, once the other side of `inbound` is gone.
    tokio::spawn(async move {
        let mut buf = vec![0u8; 2048];
        loop {
            tokio::select! {
                got = conn.recv_from(&mut buf) => match got {
                    Ok((n, from)) => {
                        if from_relay.send((buf[..n].to_vec(), from)).is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                },
                _ = from_relay.closed() => break,
            }
        }
        out.abort();
        let _ = conn.close().await;
        let _ = client.close().await;
    });
    Ok(drt_rtc::Relayed {
        address,
        local,
        outbound,
        inbound,
    })
}

/// Without a TURN client in the build, `--turn` is refused by name.
#[cfg(not(feature = "turn-client"))]
pub async fn allocate(uri: &TurnUri) -> Result<drt_rtc::Relayed, String> {
    Err(format!(
        "--turn {}: this build has no TURN client (it is in `full`)",
        uri.shown()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_turn_uri_is_a_credential_at_a_server() {
        let u = TurnUri::parse("turn://alice:s3cret@turn.example").unwrap();
        assert_eq!(u.server, "turn.example:3478");
        assert_eq!(
            (u.username.as_str(), u.password.as_str()),
            ("alice", "s3cret")
        );
        let u = TurnUri::parse("turn://1700000000%3Afp:pw%40x@127.0.0.1:3479").unwrap();
        assert_eq!(u.server, "127.0.0.1:3479");
        assert_eq!(u.username, "1700000000:fp");
        assert_eq!(u.password, "pw@x");
        assert_eq!(u.shown(), "turn://127.0.0.1:3479");
        assert!(!format!("{u:?}").contains("pw@x"));
        for bad in [
            "turn.example",
            "turn:alice:pw@turn.example",
            "turn://turn.example",
            "turn://alice@turn.example",
            "turn://alice:pw@turn.example:x",
            "turn://alice:pw%zz@turn.example",
        ] {
            assert!(TurnUri::parse(bad).is_err(), "{bad}");
        }
    }
}
