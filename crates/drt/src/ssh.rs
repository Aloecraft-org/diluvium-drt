//! `drt ssh`, and `:ssh` in the native REPL: an interactive shell on
//! another machine, on this terminal.
//!
//! The session is `drt_connector_ssh::interactive`'s, the one the config's
//! `host:ssh/shell` runs too. What this file adds is the person's side:
//! a target they type, OpenSSH's `known_hosts` with a question on first
//! use, the agent and `~/.ssh` keys, and a choice of how the bytes travel
//! -- straight to the host over TCP, or to a peer `drt p2p` reaches
//! (`doc/P2P.md` §11): `drt ssh me@drt+ssh://signal.example/v1/mypc`, a
//! record or a file holding one, with `--relay` and `--fallback` as on
//! `drt p2p`.
//!
//! ## surface block
//!
//! - Entry points: [`SshArgs`] (the flags, shared by the command and
//!   `:ssh`); [`run`] (one session, blocking, its exit status);
//!   [`from_line`] (`:ssh …` as the REPL typed it).
//! - Configurable: [`DIAL_TIMEOUT`].
//! - Fan-out: [`dial`], one arm per way the bytes travel: TCP to
//!   `host:port`; a peer address, as `drt p2p` reaches it; `--via` a relay
//!   claim (`ws://`, `wss://`) or `rtc:<peer>`, the older spelling.

use std::path::PathBuf;
use std::time::Duration;

use drt_connector_ssh::interactive::{self, Login, Trust};

/// How long reaching the host may take, before any SSH.
pub const DIAL_TIMEOUT: Duration = Duration::from_secs(20);

/// `drt ssh [user@]host[:port]`, and what `:ssh` takes after it.
#[derive(clap::Args, Debug, Clone)]
pub struct SshArgs {
    /// `[user@]host[:port]` over TCP, or `[user@]<peer>` as `drt p2p` takes
    /// a peer: drt://host/v1/<name>, drt+ssh://…, a record or a file
    /// holding one, an http(s):// URL. With --via, the name the host key
    /// is remembered under in known_hosts; nothing is dialed by it.
    pub target: String,
    /// Carry the session through this peer, as `drt p2p --relay` does.
    #[arg(long, value_name = "PEER")]
    pub relay: Option<String>,
    /// Like --relay, but only when no direct path exists.
    #[arg(long, value_name = "PEER")]
    pub fallback: Option<String>,
    /// Reach the host through this instead of TCP: a relay claim URL
    /// (`wss://<relay>/s/<label>?k=…`, what `drt tunnel` dials), or `rtc:`
    /// and a host record, a file holding one, or a signalling URL.
    #[arg(long, value_name = "URL")]
    pub via: Option<String>,
    /// With `--via rtc:…`: which of the answerer's services or scope to
    /// open. The service `ssh` if omitted.
    #[arg(long, value_name = "SERVICE|HOST:PORT")]
    pub to: Option<String>,
    /// The user, when the target does not say. `-u` is the same flag.
    #[arg(short = 'l', long = "login", short_alias = 'u', value_name = "USER")]
    pub login: Option<String>,
    /// A private key to sign in with, in place of ~/.ssh's. Repeatable.
    #[arg(short = 'i', value_name = "FILE")]
    pub identity: Vec<PathBuf>,
    /// Trust exactly this host key (`SHA256:…`) and leave known_hosts out
    /// of it. Repeatable.
    #[arg(long = "hostkey", value_name = "SHA256:…")]
    pub hostkey: Vec<String>,
    /// The known_hosts file to read and add to; ~/.ssh/known_hosts if
    /// omitted.
    #[arg(long = "known-hosts", value_name = "FILE")]
    pub known_hosts: Option<PathBuf>,
    /// Refuse a host not already in known_hosts, rather than asking.
    #[arg(long)]
    pub strict: bool,
    /// With --via: a header on the relay or signalling request,
    /// `Name: value`. Repeatable.
    #[arg(long = "header", value_name = "NAME: VALUE")]
    pub header: Vec<String>,
    /// With --via: trust this PEM certificate as well as the public roots.
    #[arg(long = "extra-root", value_name = "PEM")]
    pub extra_root: Vec<PathBuf>,
}

/// `[user@]host[:port]`, IPv6 in brackets.
fn split_target(target: &str) -> Result<(Option<String>, String, u16), String> {
    let (user, rest) = match target.split_once('@') {
        Some((u, r)) if !u.is_empty() && !u.contains("://") && !u.starts_with('{') => {
            (Some(u.to_string()), r)
        }
        _ => (None, target),
    };
    // A peer address is not split: `drt p2p` reads it whole, and its port,
    // if any, is the signalling server's.
    if rest.contains("://") || rest.starts_with('{') || std::path::Path::new(rest).is_file() {
        return Ok((user, rest.to_string(), 0));
    }
    let (host, port) = if let Some(inner) = rest.strip_prefix('[') {
        let (h, after) = inner
            .split_once(']')
            .ok_or_else(|| format!("{target}: an unclosed '['"))?;
        let port = match after.strip_prefix(':') {
            Some(p) => p
                .parse()
                .map_err(|_| format!("{target}: not a port: {p}"))?,
            None => 22,
        };
        (h.to_string(), port)
    } else {
        match rest.rsplit_once(':') {
            Some((h, p)) if !h.contains(':') => (
                h.to_string(),
                p.parse()
                    .map_err(|_| format!("{target}: not a port: {p}"))?,
            ),
            _ => (rest.to_string(), 22),
        }
    };
    if host.is_empty() {
        return Err(format!("{target}: no host"));
    }
    Ok((user, host, port))
}

/// `:ssh …` as typed at the REPL: the words after `:ssh`, split as a shell
/// would split them for quotes, parsed by the same flags as the command.
pub fn from_line(rest: &str) -> Result<SshArgs, String> {
    use clap::Parser;
    #[derive(Parser)]
    #[command(name = ":ssh", no_binary_name = false)]
    struct Line {
        #[command(flatten)]
        args: SshArgs,
    }
    let mut words = vec![":ssh".to_string()];
    words.extend(split_words(rest)?);
    Line::try_parse_from(words)
        .map(|l| l.args)
        .map_err(|e| e.to_string().trim_end().to_string())
}

fn split_words(s: &str) -> Result<Vec<String>, String> {
    let mut words = Vec::new();
    let mut cur = String::new();
    let mut quote = None;
    let mut started = false;
    for c in s.chars() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), c) => cur.push(c),
            (None, '\'' | '"') => {
                quote = Some(c);
                started = true;
            }
            (None, c) if c.is_whitespace() => {
                if started || !cur.is_empty() {
                    words.push(std::mem::take(&mut cur));
                    started = false;
                }
            }
            (None, c) => cur.push(c),
        }
    }
    if quote.is_some() {
        return Err("an unclosed quote".into());
    }
    if started || !cur.is_empty() {
        words.push(cur);
    }
    Ok(words)
}

/// One session, start to finish, on this terminal. Its exit status when
/// the host reported one.
pub fn run(args: &SshArgs) -> Result<Option<u32>, String> {
    let runtime = tokio::runtime::Runtime::new().map_err(|e| format!("a tokio runtime: {e}"))?;
    let outcome = runtime.block_on(session(args));
    // Leaked rather than dropped, for tokio 1.53.1's teardown race (the
    // comment beside `drt tunnel`'s runtime in cli.rs).
    std::mem::forget(runtime);
    outcome
}

async fn session(args: &SshArgs) -> Result<Option<u32>, String> {
    let (user, host, port) = split_target(&args.target)?;
    // A peer address in place of a host: the session rides a `drt p2p`
    // call, and the host key is remembered under the address.
    let peer = peer_target(&host);
    let user = user
        .or_else(|| args.login.clone())
        .or_else(|| std::env::var("USER").ok())
        .or_else(|| std::env::var("USERNAME").ok())
        .ok_or("no user: give one as user@host or -l")?;
    let trust = if args.hostkey.is_empty() {
        Trust::KnownHosts {
            file: args
                .known_hosts
                .clone()
                .or_else(interactive::known_hosts)
                .ok_or("no home directory for ~/.ssh/known_hosts; give --known-hosts")?,
            ask: !args.strict,
        }
    } else {
        Trust::Pinned {
            fingerprints: args.hostkey.clone(),
            keys: Vec::new(),
        }
    };
    let login = Login {
        user,
        host: host.clone(),
        port,
        trust,
        credentials: interactive::discover(args.identity.clone()),
    };
    let stream = match peer {
        Some(peer) => peer_stream(&peer, args).await?,
        None => dial(args, &host, port).await?,
    };
    interactive::on_this_terminal(stream, login).await
}

/// The host half of the target as a peer address, when it is one: it has a
/// scheme `drt p2p` takes, it is a record, or it names a file.
fn peer_target(host: &str) -> Option<String> {
    let scheme = host
        .split_once("://")
        .map(|(s, _)| s.to_ascii_lowercase())
        .filter(|s| s == "drt" || s.starts_with("drt+") || s == "http" || s == "https");
    if scheme.is_some() || host.starts_with('{') || std::path::Path::new(host).is_file() {
        Some(host.to_string())
    } else {
        None
    }
}

#[cfg(feature = "p2p")]
async fn peer_stream(peer: &str, args: &SshArgs) -> Result<Stream, String> {
    use crate::p2p::call::{self, Dial};
    use crate::p2p::peer::Peer;
    let peer = Peer::parse(peer)?;
    let roots = crate::roots::load_roots_named("--extra-root", &args.extra_root)?;
    let headers = args
        .header
        .iter()
        .map(|h| {
            h.split_once(':')
                .map(|(n, v)| (n.trim().to_string(), v.trim().to_string()))
                .ok_or_else(|| format!("--header {h}: expected `Name: value`"))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let role = crate::p2p::CallRole {
        destination: args
            .target
            .rsplit_once('@')
            .map_or(args.target.clone(), |(_, r)| r.to_string()),
        maps: Vec::new(),
        dial: Dial {
            stun: Vec::new(),
            headers,
            fingerprint: None,
        },
        relay: args.relay.as_deref().map(Peer::parse).transpose()?,
        fallback: args.fallback.as_deref().map(Peer::parse).transpose()?,
        peer: peer.clone(),
    };
    let connected = call::session(&role, &roots).await?;
    if connected.forwarding {
        eprintln!("drt ssh: via relay");
    }
    let target = match (&peer.service, args.to.as_deref()) {
        (_, Some(to)) => drt_rtc::caller::Target::parse(to).map_err(|e| format!("--to: {e}"))?,
        (Some(s), None) => drt_rtc::caller::Target::Service(s.clone()),
        (None, None) => drt_rtc::caller::Target::Service("ssh".into()),
    };
    let (stream, _closed) = connected.call.open(&target).await?;
    Ok(Box::new(Held {
        stream,
        _call: connected.call,
    }))
}

#[cfg(not(feature = "p2p"))]
async fn peer_stream(_peer: &str, _args: &SshArgs) -> Result<Stream, String> {
    Err("a peer address needs a build with `p2p`".into())
}

/// A byte stream to the host's sshd, however it travels.
type Stream = Box<dyn Duplex>;
trait Duplex: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send {}
impl<T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send> Duplex for T {}

async fn dial(args: &SshArgs, host: &str, port: u16) -> Result<Stream, String> {
    let Some(via) = args.via.as_deref() else {
        if args.to.is_some() {
            return Err("--to goes with --via rtc:…".into());
        }
        let tcp = tokio::time::timeout(DIAL_TIMEOUT, tokio::net::TcpStream::connect((host, port)))
            .await
            .map_err(|_| {
                format!(
                    "{host}:{port} did not answer within {}s",
                    DIAL_TIMEOUT.as_secs()
                )
            })?
            .map_err(|e| format!("{host}:{port}: {e}"))?;
        let _ = tcp.set_nodelay(true);
        return Ok(Box::new(tcp));
    };
    via_stream(via, args).await
}

#[cfg(feature = "tunnel")]
async fn via_stream(via: &str, args: &SshArgs) -> Result<Stream, String> {
    let headers = args
        .header
        .iter()
        .map(|h| {
            h.split_once(':')
                .map(|(n, v)| (n.trim().to_string(), v.trim().to_string()))
                .ok_or_else(|| format!("--header {h}: expected `Name: value`"))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let roots = crate::roots::load_roots_named("--extra-root", &args.extra_root)?;
    let (roots, headers, to) = (&roots[..], &headers[..], args.to.as_deref());
    if via.starts_with("ws://") || via.starts_with("wss://") {
        if to.is_some() {
            return Err("--to goes with --via rtc:…; a relay claim names its device".into());
        }
        // Dialed here, so a refused claim is an error by name rather than
        // an SSH handshake that reads end of file.
        let ws = crate::tunnel::connect(via, roots, headers).await?;
        let (ours, theirs) = tokio::io::duplex(256 * 1024);
        tokio::spawn(async move {
            let _ = crate::tunnel::pump(theirs, ws).await;
        });
        return Ok(Box::new(ours));
    }
    if let Some(rest) = via.strip_prefix("rtc:") {
        return rtc_stream(rest, to.unwrap_or("ssh"), roots, headers).await;
    }
    Err(format!(
        "--via {via}: expected ws://, wss:// (a relay claim) or rtc:"
    ))
}

#[cfg(not(feature = "tunnel"))]
async fn via_stream(_via: &str, _args: &SshArgs) -> Result<Stream, String> {
    Err("--via needs a build with `tunnel`".into())
}

#[cfg(feature = "p2p")]
async fn rtc_stream(
    rest: &str,
    to: &str,
    roots: &[tokio_rustls::rustls::pki_types::CertificateDer<'static>],
    headers: &[(String, String)],
) -> Result<Stream, String> {
    let peer = crate::p2p::peer::Peer::parse(rest)?;
    let dial = crate::p2p::call::Dial {
        stun: Vec::new(),
        headers: headers.to_vec(),
        fingerprint: None,
    };
    let connected = crate::p2p::call::connect(&peer, &dial, roots).await?;
    let target = match &peer.service {
        Some(s) => drt_rtc::caller::Target::Service(s.clone()),
        None => drt_rtc::caller::Target::parse(to).map_err(|e| format!("--to: {e}"))?,
    };
    let (stream, _closed) = connected.call.open(&target).await?;
    Ok(Box::new(Held {
        stream,
        _call: connected.call,
    }))
}

#[cfg(all(feature = "tunnel", not(feature = "p2p")))]
async fn rtc_stream(
    _rest: &str,
    _to: &str,
    _roots: &[tokio_rustls::rustls::pki_types::CertificateDer<'static>],
    _headers: &[(String, String)],
) -> Result<Stream, String> {
    Err("--via rtc: needs a build with `p2p`".into())
}

/// A WebRTC stream and the session it rides on, which ends when dropped.
#[cfg(feature = "p2p")]
struct Held {
    stream: tokio::io::DuplexStream,
    _call: drt_rtc::caller::Call,
}

#[cfg(feature = "p2p")]
impl tokio::io::AsyncRead for Held {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.stream).poll_read(cx, buf)
    }
}

#[cfg(feature = "p2p")]
impl tokio::io::AsyncWrite for Held {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        std::pin::Pin::new(&mut self.stream).poll_write(cx, buf)
    }
    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.stream).poll_flush(cx)
    }
    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.stream).poll_shutdown(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::{from_line, peer_target, split_target};

    /// A peer address is one piece: the user in front, the rest whole, and
    /// no port split off a URL (doc/P2P.md §11).
    #[test]
    fn a_peer_address_is_kept_whole() {
        assert_eq!(
            split_target("me@drt+ssh://signal.example/v1/mypc").unwrap(),
            (
                Some("me".into()),
                "drt+ssh://signal.example/v1/mypc".into(),
                0
            )
        );
        assert_eq!(
            split_target("drt://127.0.0.1:5001").unwrap(),
            (None, "drt://127.0.0.1:5001".into(), 0)
        );
        let record = r#"{"v":1,"u":"abcd","p":"0123456789abcdefghijKL","f":"x","c":[]}"#;
        assert_eq!(split_target(record).unwrap(), (None, record.into(), 0));
        assert!(peer_target("drt+ssh://x/v1/a").is_some());
        assert!(peer_target("box.lan").is_none());
        assert!(peer_target("{").is_some());
    }

    #[test]
    fn targets_split_like_ssh() {
        assert_eq!(split_target("box").unwrap(), (None, "box".into(), 22));
        assert_eq!(
            split_target("me@box:2222").unwrap(),
            (Some("me".into()), "box".into(), 2222)
        );
        assert_eq!(
            split_target("me@[::1]:2222").unwrap(),
            (Some("me".into()), "::1".into(), 2222)
        );
        assert_eq!(split_target("::1").unwrap(), (None, "::1".into(), 22));
        assert!(split_target("me@").is_err());
        assert!(split_target("box:x").is_err());
    }

    #[test]
    fn a_repl_line_takes_the_command_s_flags() {
        let a = from_line("me@box -i ~/k --via 'wss://r/s/box?k=a b'").unwrap();
        assert_eq!(a.target, "me@box");
        assert_eq!(a.via.as_deref(), Some("wss://r/s/box?k=a b"));
        assert_eq!(a.identity.len(), 1);
        assert!(from_line("").is_err());
        assert!(from_line("box --nope").is_err());
    }
}
