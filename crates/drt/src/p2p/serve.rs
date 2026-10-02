//! The serving side of `drt p2p` (`doc/P2P.md` §5): the host behind
//! `--listen` and `--park`, built from a `--forward`, and the three
//! services that live in this process rather than at a TCP address: the
//! REPL (as the service `ssh`, through the built-in SSH server, and as the
//! service `repl`, raw PTY bytes), this process's stdio (`-`), and another
//! DRT peer (`drt://…`, the forwarder calls it and joins the sessions).
//!
//! ## surface block
//!
//! - Entry points: [`start`], a host serving a [`ForwardSpec`]; [`listen`],
//!   the `--listen` role carried out; [`state_dir`], where a one-liner
//!   keeps its identity and host key.
//! - Configurable: [`MAX_SESSIONS`], [`MAX_STREAMS`], [`IDLE`],
//!   [`CONNECT_TIMEOUT`], [`STUN_REFRESH`], [`PIPE`].
//! - Fan-out: [`sinks`], one arm per row of §5's table; the three
//!   [`Service`] implementations, [`ReplService`], [`StdioService`] and
//!   [`PeerService`].

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use drt_config::RootConfig;
use drt_rtc::host::{Event, Opening, Report, Service, SessionState, StreamState, Window};
use drt_rtc::{Cidr, Forward, Host, HostConfig, Identity, Scope, Sender, Sink};
use drt_sshd::HostKey;
use tokio::sync::{oneshot, watch};
use tokio_rustls::rustls::pki_types::CertificateDer;

use super::call::{self, Dial};
use super::peer::{fingerprint_text, ForwardSpec, Peer};
use super::{ListenRole, ServeSettings};

/// Sessions a serving peer holds at once.
pub const MAX_SESSIONS: usize = 32;
/// Streams per session.
pub const MAX_STREAMS: usize = 64;
/// A stream with nothing on it for this long ends.
pub const IDLE: Duration = Duration::from_secs(600);
/// How long a forward's TCP target may take to answer.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Seconds between STUN refreshes, as the `webrtc` block's default.
pub const STUN_REFRESH: Duration = Duration::from_secs(25);
/// The pipe between a stream and the service holding its far end.
pub const PIPE: usize = 256 * 1024;

/// A host that is serving, with the parts a role needs beside it.
pub struct Serving {
    /// Queues `open` and `close`.
    pub sender: Sender,
    /// The host's record, as it last reported it.
    pub record: watch::Receiver<String>,
    /// Fires when a `--forward -` session has ended: the process is done.
    pub done: Option<oneshot::Receiver<()>>,
    pub local: SocketAddr,
    /// How many sessions have connected; a listening peer with `--signal`
    /// names each call by it.
    pub calls: Arc<std::sync::atomic::AtomicU64>,
}

/// Build the host for a forward, serve, and wait for its first record.
#[allow(clippy::too_many_arguments)]
pub async fn start(
    spec: &ForwardSpec,
    bind: SocketAddr,
    direct: bool,
    stun: Vec<String>,
    accept: Vec<Cidr>,
    settings: &ServeSettings,
    config: &RootConfig,
    roots: &[CertificateDer<'static>],
) -> Result<Serving, String> {
    let (forward, services, done) = sinks(spec, settings, config, roots)?;
    let identity_file = match &settings.identity_file {
        Some(p) => p.clone(),
        None => state_dir()?.join("identity.json"),
    };
    let cfg = HostConfig {
        bind,
        identity: Identity::load_or_create(&identity_file)?,
        stun,
        stun_refresh: STUN_REFRESH,
        publish_host_candidates: true,
        service: String::new(),
        default: None,
        scope: Scope::new(Vec::new()),
        services,
        max_sessions: MAX_SESSIONS,
        max_streams: MAX_STREAMS,
        idle_timeout: IDLE,
        connect_timeout: CONNECT_TIMEOUT,
        direct,
        hello_scope: false,
        forward,
        // A REPL behind this host holds the config's ceiling; a TCP target
        // or another peer holds nothing of DRT's.
        caps: match spec {
            ForwardSpec::Repl => crate::webrtc::caps_of(config),
            _ => Vec::new(),
        },
        accept,
    };
    let mut host = Host::start(cfg)?;
    let local = host.local_addr();
    let sender = host.sender();
    let first = match host.next_event().await {
        Some(Event::Record { rtc }) => rtc,
        other => {
            return Err(format!(
                "the host's first word was not its record: {other:?}"
            ))
        }
    };
    let (record_tx, record) = watch::channel(first);
    let calls = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let counted = calls.clone();
    tokio::spawn(async move {
        while let Some(e) = host.next_event().await {
            match e {
                Event::Record { rtc } => {
                    eprintln!("drt p2p: record {rtc}");
                    let _ = record_tx.send(rtc);
                }
                Event::Session {
                    peer,
                    state: SessionState::Connected,
                    ..
                } => {
                    counted.fetch_add(1, Ordering::Relaxed);
                    eprintln!("drt p2p: {peer}: connected");
                }
                Event::Session { peer, reason, .. } => eprintln!(
                    "drt p2p: {peer}: closed{}",
                    reason.map(|r| format!(": {r}")).unwrap_or_default()
                ),
                Event::Stream {
                    peer,
                    stream,
                    host,
                    port,
                    state,
                    reason,
                    bytes_up,
                    bytes_down,
                } => {
                    let asked = match (host.as_str(), port) {
                        ("", 0) => String::new(),
                        ("", p) => format!(" port {p}"),
                        (h, 0) => format!(" {h}"),
                        (h, p) => format!(" {h}:{p}"),
                    };
                    match state {
                        StreamState::Open => eprintln!("drt p2p: {peer}/{stream}:{asked} open"),
                        StreamState::Closed => eprintln!(
                            "drt p2p: {peer}/{stream}:{asked} closed{} ({bytes_up} up, {bytes_down} down)",
                            reason.map(|r| format!(" {r}")).unwrap_or_default()
                        ),
                    }
                }
            }
        }
    });
    Ok(Serving {
        sender,
        record,
        done,
        local,
        calls,
    })
}

/// `--listen` (`doc/P2P.md` §2.3): a fixed record on a UDP port, printed
/// with the command that calls it, and a signalling port when asked.
pub async fn listen(
    role: &ListenRole,
    config: &RootConfig,
    roots: &[CertificateDer<'static>],
) -> Result<(), String> {
    let bind = SocketAddr::new(role.host.bind_ip(), role.port);
    let serving = start(
        &role.forward,
        bind,
        true,
        role.stun.clone(),
        role.host.accept(),
        &role.settings,
        config,
        roots,
    )
    .await?;
    let rtc = serving.record.borrow().clone();
    let record = drt_rtc::Record::decode(&rtc).map_err(|e| e.to_string())?;
    eprintln!(
        "drt p2p: listening on udp {}, serving {}",
        serving.local,
        role.forward.describe()
    );
    eprintln!("drt p2p: record {rtc}");
    eprintln!(
        "drt p2p: fingerprint {}",
        fingerprint_text(&record.fingerprint)
    );
    eprintln!("drt p2p: call it with: drt p2p '{rtc}'");
    // Inside a project, the record is also a file under .drt_root/live, so
    // a launcher on this machine calls the peer in direct mode with nothing
    // sent (`doc/P2P.md` §3.1). Rewritten whenever the record changes.
    let live = live_record_path(serving.local.port());
    if let Some(path) = &live {
        write_live_record(path, &rtc);
        let mut record = serving.record.clone();
        let path = path.clone();
        tokio::spawn(async move {
            while record.changed().await.is_ok() {
                let rtc = record.borrow().clone();
                write_live_record(&path, &rtc);
            }
        });
    }
    if let Some(port) = role.signal {
        let addr = SocketAddr::new(bind.ip(), port);
        let listener = tokio::net::TcpListener::bind(addr)
            .await
            .map_err(|e| format!("--signal: cannot bind {addr}: {e}"))?;
        let bound = listener.local_addr().map_err(|e| e.to_string())?;
        let reach = SocketAddr::new(serving.local.ip(), bound.port());
        let scheme = if reach.ip().is_loopback() {
            "drt"
        } else {
            "http"
        };
        eprintln!(
            "drt p2p: signalling on http://{reach}/; call it with: drt p2p {scheme}://{reach}"
        );
        if matches!(role.forward, ForwardSpec::Repl) && !reach.ip().is_loopback() {
            eprintln!(
                "drt p2p: note: anyone who reaches that port gets the REPL with this account's \
                 grants; --host narrows who"
            );
        }
        let sender = serving.sender.clone();
        let record = serving.record.clone();
        tokio::spawn(super::signal::serve(listener, sender, record));
    }
    match serving.done {
        Some(done) => {
            let _ = done.await;
            if let Some(path) = &live {
                let _ = std::fs::remove_file(path);
            }
            eprintln!("drt p2p: the session ended");
            Ok(())
        }
        None => std::future::pending().await,
    }
}

/// `.drt_root/live/p2p-<port>.record.json` when the working directory is
/// a project; otherwise none, and the record is stderr's alone.
fn live_record_path(port: u16) -> Option<PathBuf> {
    let cwd = std::env::current_dir().ok()?;
    let root = crate::drt_root::discover(&cwd, None)?;
    Some(root.live().join(format!("p2p-{port}.record.json")))
}

fn write_live_record(path: &std::path::Path, rtc: &str) {
    let written = path
        .parent()
        .map(std::fs::create_dir_all)
        .unwrap_or(Ok(()))
        .and_then(|_| std::fs::write(path, format!("{rtc}\n")));
    match written {
        Ok(()) => eprintln!("drt p2p: record written to {}", path.display()),
        Err(e) => eprintln!("drt p2p: could not write {}: {e}", path.display()),
    }
}

/// Where a one-liner keeps what must survive a restart: the identity
/// behind its record and fingerprint, and the REPL's host key.
pub fn state_dir() -> Result<PathBuf, String> {
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .ok_or("no HOME to keep this peer's identity under; name p2p.identity_file in a config")?;
    let dir = home.join(".drt").join("p2p");
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    Ok(dir)
}

// depth: the forward table, as sinks

/// One row of `doc/P2P.md` §5's table, as what the host routes with: the
/// forward, the named services, and the `-` session's end signal.
pub(crate) type Sinks = (Forward, Vec<(String, Sink)>, Option<oneshot::Receiver<()>>);

pub(crate) fn sinks(
    spec: &ForwardSpec,
    settings: &ServeSettings,
    config: &RootConfig,
    roots: &[CertificateDer<'static>],
) -> Result<Sinks, String> {
    Ok(match spec {
        ForwardSpec::Repl => {
            let repl = Arc::new(ReplService::new(settings, config)?);
            let ssh: Arc<dyn Service> = Arc::new(ReplOverSsh(repl.clone()));
            let raw: Arc<dyn Service> = Arc::new(ReplRaw(repl));
            (
                Forward::One(Sink::Local(ssh.clone())),
                vec![
                    ("ssh".to_string(), Sink::Local(ssh)),
                    ("repl".to_string(), Sink::Local(raw)),
                ],
                None,
            )
        }
        ForwardSpec::One(entry) => {
            let services = if entry.scheme == "tcp" {
                Vec::new()
            } else {
                vec![(entry.scheme.clone(), Sink::Dial(entry.clone()))]
            };
            (Forward::One(Sink::Dial(entry.clone())), services, None)
        }
        ForwardSpec::Ports { host, ports } => (
            Forward::Ports {
                host: host.clone(),
                ports: ports.clone(),
            },
            Vec::new(),
            None,
        ),
        ForwardSpec::Stdio => {
            let (tx, rx) = oneshot::channel();
            let stdio: Arc<dyn Service> = Arc::new(StdioService {
                busy: AtomicBool::new(false),
                done: Mutex::new(Some(tx)),
            });
            (Forward::One(Sink::Local(stdio)), Vec::new(), Some(rx))
        }
        ForwardSpec::Peer(peer) => {
            let service: Arc<dyn Service> = Arc::new(PeerService(Arc::new(PeerInner {
                peer: peer.clone(),
                dial: Dial {
                    stun: settings.stun.clone(),
                    headers: settings.headers.clone(),
                    fingerprint: None,
                },
                roots: roots.to_vec(),
                call: tokio::sync::Mutex::new(None),
            })));
            (Forward::One(Sink::Local(service)), Vec::new(), None)
        }
        ForwardSpec::Relay => {
            let relay: Arc<dyn drt_rtc::Relay> = Arc::new(PeerRelay {
                dial: Dial {
                    stun: settings.stun.clone(),
                    headers: settings.headers.clone(),
                    fingerprint: None,
                },
                roots: roots.to_vec(),
            });
            (Forward::Relay(relay), Vec::new(), None)
        }
    })
}

// depth: the REPL, as `ssh` and as `repl`

/// What both REPL services share: the host key, who may sign in, and the
/// deployment each session runs inside.
struct ReplService {
    host_key: String,
    keys: crate::sshd::Keys,
    config: Arc<RootConfig>,
}

impl ReplService {
    fn new(settings: &ServeSettings, config: &RootConfig) -> Result<ReplService, String> {
        let keys = crate::sshd::Keys::from_sources(config, settings.authorized_keys.as_deref())?;
        if keys.is_empty() {
            eprintln!(
                "drt p2p: the REPL's ssh service admits no key: name principals, or \
                 --authorized-keys (the service repl needs none)"
            );
        }
        let host_key = match &config.identity.host_key_path {
            Some(path) => std::fs::read_to_string(path)
                .map_err(|e| format!("identity.host_key_path {}: {e}", path.display()))?,
            None => {
                let path = state_dir()?.join("host_key");
                match std::fs::read_to_string(&path) {
                    Ok(text) => text,
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                        let text = HostKey::generate().map_err(|e| e.to_string())?;
                        write_private(&path, &text)?;
                        eprintln!("drt p2p: made a host key at {}", path.display());
                        text
                    }
                    Err(e) => return Err(format!("{}: {e}", path.display())),
                }
            }
        };
        let fingerprint = HostKey::parse(&host_key)
            .map_err(|e| format!("the host key: {e}"))?
            .fingerprint();
        eprintln!(
            "drt p2p: the REPL's ssh host key is {fingerprint}, {} key(s) admitted",
            keys.len()
        );
        Ok(ReplService {
            host_key,
            keys,
            config: Arc::new(config.clone()),
        })
    }
}

fn write_private(path: &std::path::Path, text: &str) -> Result<(), String> {
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)
            .map_err(|e| format!("{}: {e}", path.display()))?;
        f.write_all(text.as_bytes())
            .map_err(|e| format!("{}: {e}", path.display()))
    }
    #[cfg(not(unix))]
    {
        std::fs::write(path, text).map_err(|e| format!("{}: {e}", path.display()))
    }
}

/// The service `ssh`: the built-in SSH server on the stream, a key that
/// signs in getting the REPL with that key's grants.
struct ReplOverSsh(Arc<ReplService>);

impl Service for ReplOverSsh {
    fn name(&self) -> String {
        "ssh".into()
    }
    fn open(&self, _host: &str, _port: u16, _window: Window, report: Report) -> Opening {
        let repl = self.0.clone();
        Box::pin(async move {
            let key =
                HostKey::parse(&repl.host_key).map_err(|_| drt_rtc::wisp::reason::UNSPECIFIED)?;
            let (mine, theirs) = tokio::io::duplex(PIPE);
            let (shells, mut opened) = tokio::sync::mpsc::channel::<drt_sshd::Shell>(4);
            let authorized = repl.keys.authorized();
            tokio::spawn(async move {
                let _ = drt_sshd::serve(mine, key, authorized, shells).await;
            });
            tokio::spawn(async move {
                while let Some(shell) = opened.recv().await {
                    match repl.keys.grants(&shell.key) {
                        Some(grants) => {
                            // Known only now: the key decided it.
                            report.granted(&crate::webrtc::cap_names(&grants));
                            crate::sshd::session(shell, grants, repl.config.clone())
                        }
                        None => shell.close(1).await,
                    }
                }
            });
            Ok(Box::pin(theirs) as _)
        })
    }
}

/// The service `repl`: the PTY's bytes on the stream itself, the window
/// from `control`, and what `drt repl` on this machine would hold. The
/// session is the gate: whoever may reach this peer may use it.
struct ReplRaw(Arc<ReplService>);

impl Service for ReplRaw {
    fn name(&self) -> String {
        "repl".into()
    }
    fn open(&self, _host: &str, _port: u16, window: Window, report: Report) -> Opening {
        let repl = self.0.clone();
        Box::pin(async move {
            let (mine, theirs) = tokio::io::duplex(PIPE);
            let grants = crate::config::ceiling(&repl.config);
            report.granted(&crate::webrtc::cap_names(&grants));
            crate::sshd::raw_session(Box::pin(mine), window, grants, repl.config.clone());
            Ok(Box::pin(theirs) as _)
        })
    }
}

// depth: this process's stdio, one session at a time

struct StdioService {
    busy: AtomicBool,
    done: Mutex<Option<oneshot::Sender<()>>>,
}

/// A stream whose drop says the session ended.
struct Ending<T> {
    io: T,
    done: Option<oneshot::Sender<()>>,
}

impl<T> Drop for Ending<T> {
    fn drop(&mut self) {
        if let Some(done) = self.done.take() {
            let _ = done.send(());
        }
    }
}

impl<T: tokio::io::AsyncRead + Unpin> tokio::io::AsyncRead for Ending<T> {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.io).poll_read(cx, buf)
    }
}

impl<T: tokio::io::AsyncWrite + Unpin> tokio::io::AsyncWrite for Ending<T> {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        std::pin::Pin::new(&mut self.io).poll_write(cx, buf)
    }
    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.io).poll_flush(cx)
    }
    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.io).poll_shutdown(cx)
    }
}

impl Service for StdioService {
    fn name(&self) -> String {
        "stdio".into()
    }
    fn open(&self, _host: &str, _port: u16, _window: Window, _report: Report) -> Opening {
        let taken = self.busy.swap(true, Ordering::SeqCst);
        let done = self.done.lock().ok().and_then(|mut d| d.take());
        Box::pin(async move {
            if taken {
                return Err(drt_rtc::wisp::reason::THROTTLED);
            }
            eprintln!("drt p2p: a session has this process's stdio");
            let io = tokio::io::join(tokio::io::stdin(), tokio::io::stdout());
            Ok(Box::pin(Ending { io, done }) as _)
        })
    }
}

// depth: a relay's call to the destination a caller named

/// `Forward::Relay`'s way out: the destination as a peer address of §3,
/// called as this process calls anyone. The caller's `--H` do not travel;
/// the relay's own `--H`, if any, do.
struct PeerRelay {
    dial: Dial,
    roots: Vec<CertificateDer<'static>>,
}

impl drt_rtc::Relay for PeerRelay {
    fn call(
        &self,
        to: &str,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<drt_rtc::caller::Call, String>> + Send>,
    > {
        let (to, dial, roots) = (to.to_string(), self.dial.clone(), self.roots.clone());
        Box::pin(async move {
            let peer = Peer::parse(&to)?;
            if matches!(peer.how, super::peer::How::Ws(_)) {
                return Err("a relay calls DRT peers, not WebSocket relays".into());
            }
            eprintln!("drt p2p: calling {} for a caller", peer.shown());
            let connected = call::connect(&peer, &dial, &roots).await?;
            Ok(connected.call)
        })
    }
}

// depth: another peer, called once and shared by every stream

struct PeerService(Arc<PeerInner>);

struct PeerInner {
    peer: Peer,
    dial: Dial,
    roots: Vec<CertificateDer<'static>>,
    /// The one call every stream rides, made on the first and remade when
    /// it has ended.
    call: tokio::sync::Mutex<Option<Arc<drt_rtc::caller::Call>>>,
}

impl Service for PeerService {
    fn name(&self) -> String {
        format!("the peer {}", self.0.peer.shown())
    }
    fn open(&self, host: &str, port: u16, _window: Window, _report: Report) -> Opening {
        let target = match (host, port) {
            ("", 0) => match &self.0.peer.service {
                Some(s) => drt_rtc::caller::Target::Service(s.clone()),
                None => drt_rtc::caller::Target::Default,
            },
            ("", p) => drt_rtc::caller::Target::Port(p),
            (h, 0) => drt_rtc::caller::Target::Service(h.to_string()),
            (h, p) => drt_rtc::caller::Target::Address {
                host: h.to_string(),
                port: p,
            },
        };
        let inner = self.0.clone();
        Box::pin(async move {
            // The lock is held across the whole open, so two streams
            // arriving at once make one call between them, not two.
            let mut held = inner.call.lock().await;
            for attempt in 0..2 {
                let current = match held.as_ref() {
                    Some(c) => c.clone(),
                    None => {
                        let connected = call::connect(&inner.peer, &inner.dial, &inner.roots)
                            .await
                            .map_err(|e| {
                                eprintln!("drt p2p: cannot reach {}: {e}", inner.peer.shown());
                                drt_rtc::wisp::reason::UNREACHABLE
                            })?;
                        eprintln!("drt p2p: calling {} for every stream", inner.peer.shown());
                        let c = Arc::new(connected.call);
                        *held = Some(c.clone());
                        c
                    }
                };
                match current.open(&target).await {
                    Ok((stream, _closed)) => return Ok(Box::pin(stream) as _),
                    // The session ended since: call again, once.
                    Err(_) if attempt == 0 => *held = None,
                    Err(_) => break,
                }
            }
            Err(drt_rtc::wisp::reason::UNREACHABLE)
        })
    }
}
