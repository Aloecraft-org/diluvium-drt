//! The WireGuard peer this deployment is (`drt wg`, and the `wireguard`
//! block inside `drt start`).
//!
//! ## Why this is here at all
//!
//! DRT has a ladder of these (`doc/Relay.md`, and `doc/WireGuard.md` for
//! this rung): `netcheck` says what a network can do, `stun` measures the
//! mapping that decides it, the relay carries what cannot be reached
//! directly, and `turn` carries what a browser needs. Every rung answers
//! *can these two hosts exchange packets*. None of them answers *and then
//! what* — until now the answer was "a WSS tunnel per TCP connection",
//! which works and costs a relay.
//!
//! WireGuard is the shortcut: once two hosts can exchange UDP datagrams —
//! which is exactly what `netcheck`'s `punchable` verdict means — the
//! problem of carrying arbitrary traffic between them is solved, by 4000
//! lines of very well-studied protocol, with no connection setup per
//! stream, no relay in the path, and roaming for free. A deployment gets
//! an address for a fetchpoint, and `ssh/exec`, `rest` and `listen` reach
//! it with no new plumbing at all, because it is just an IP address.
//!
//! ## Why gotatun, and not our own
//!
//! For the reason the ssh connector is ego-transport's and the TURN server
//! is: a WireGuard implementation with a bug in its handshake or its nonce
//! handling is not a slow tunnel, it is an open one. gotatun is Mullvad's
//! maintained fork of Cloudflare's boringtun, audited, and a library
//! first — its `Device` is generic over **both** transports, which is what
//! makes it embeddable here rather than merely linkable:
//!
//! - The UDP side is a trait, so the device can be handed the socket
//!   `netcheck --udp-port` measured rather than binding its own behind
//!   our back and getting a different mapping.
//! - The IP side is a trait, so `drt start` gives it a kernel interface
//!   and the tests give it channels — the same device, proven without
//!   privileges (`crates/drt/tests/wireguard.rs` runs two of them in one
//!   process and passes a real packet between them).
//!
//! ## The punch, which is the reason for the reply queue
//!
//! A punched peer has no endpoint until the rendezvous supplies one, and
//! the rendezvous is a program's business, not a config's: two deployments
//! measure themselves with `netcheck --udp-port`, trade the answers over
//! the relay, and each tells its own device where the other turned out to
//! be. That last step is the `endpoint` command on [`WireguardConfig::reply_queue`],
//! and `keepalive` beside it is what holds the mapping open once it is
//! open. Nothing here does the rendezvous; everything here makes one
//! sufficient.
//!
//! ## surface block
//!
//! - Entry points: [`serve`] (`drt wg`), [`bind`], [`WireguardBridge::start`],
//!   [`WireguardBridge::report`] and [`WireguardBridge::collect`]
//!   (`drt start`), and [`drive`], the task both run.
//! - Configurable: [`KEY_LEN`], the key length the protocol fixes;
//!   [`COMMAND`] and [`EVENT`], the names on the wire. Everything else is
//!   the `wireguard` block's (`drt_config::WireguardConfig`).
//! - Fan-out: [`Command`], what a program may ask of a running device, and
//!   [`Report`], what it is told. Two enums, and the match on each is the
//!   only dispatch in this file.

use std::net::SocketAddr;
use std::time::Duration;

use base64::Engine as _;
use gotatun::device::{Device, DeviceBuilder, DeviceTransports, Peer};
use gotatun::x25519::{PublicKey, StaticSecret};
use ipnetwork::IpNetwork;
use tokio::sync::mpsc;

use drt_config::{WireguardConfig, WireguardPeer};

/// A WireGuard key is 32 bytes, always. Curve25519 fixes it, `wg genkey`
/// prints exactly this base64-encoded, and a key of any other length is a
/// pasted-wrong key rather than a configuration choice.
pub const KEY_LEN: usize = 32;

/// The commands a program may put on the reply queue. Named here rather
/// than at their match arms so the whole vocabulary is one list.
pub const COMMAND: [&str; 2] = ["endpoint", "keepalive"];

/// The events a program is sent on the queue.
pub const EVENT: [&str; 2] = ["wireguard", "wireguard_endpoint"];

// ---------------------------------------------------------------------------
// Keys and peers: the config, translated
// ---------------------------------------------------------------------------

/// Decode one base64 WireGuard key, or say which field was wrong and why.
///
/// Standard base64 with padding, which is what `wg genkey`, `wg pubkey`
/// and every `[Interface]` stanza in existence emit. A key that decodes to
/// the wrong length is refused rather than padded or truncated: the two
/// silent failures here are a truncated key (a device nobody can reach)
/// and a swapped public and private key (a device anybody can be), and
/// neither announces itself at run time.
pub fn parse_key(label: &str, text: &str) -> Result<[u8; KEY_LEN], String> {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(text.trim())
        .map_err(|e| {
            format!("{label}: not base64 ({e}); WireGuard keys are `wg genkey`'s output")
        })?;
    <[u8; KEY_LEN]>::try_from(bytes.as_slice()).map_err(|_| {
        format!(
            "{label}: a WireGuard key is {KEY_LEN} bytes, this one is {}",
            bytes.len()
        )
    })
}

/// This device's private key, by the same three knobs as every other
/// secret in a root config, resolved in the same order.
///
/// Refusing when none is given rather than generating one: a device with a
/// fresh key each start has a new identity each start, so every peer's
/// config is stale the moment it restarts, and the failure appears as
/// handshakes that never complete rather than as a missing setting.
pub fn private_key(config: &WireguardConfig) -> Result<StaticSecret, String> {
    let text = if let Some(path) = &config.private_key_file {
        let mut text = std::fs::read_to_string(path).map_err(|e| {
            format!(
                "wireguard: cannot read private_key_file '{}': {e}",
                path.display()
            )
        })?;
        if text.ends_with('\n') {
            text.pop();
        }
        text
    } else if let Some(var) = &config.private_key_env {
        std::env::var(var)
            .map_err(|_| format!("wireguard: env var '{var}' (private_key_env) is not set"))?
    } else if let Some(inline) = &config.private_key {
        inline.clone()
    } else {
        return Err("wireguard: no private key (set one of private_key_file, \
                    private_key_env, private_key); a device without a key has no \
                    identity and can complete no handshake"
            .into());
    };
    Ok(StaticSecret::from(parse_key(
        "wireguard.private_key",
        &text,
    )?))
}

/// The public key to hand a peer, base64, as `wg pubkey` would print it.
pub fn public_key(secret: &StaticSecret) -> String {
    base64::engine::general_purpose::STANDARD.encode(PublicKey::from(secret).as_bytes())
}

/// One configured peer, translated, with every refusal naming the peer it
/// came from — a config with six peers and one bad CIDR should not report
/// "invalid network".
pub fn peer(spec: &WireguardPeer) -> Result<Peer, String> {
    let label = format!("wireguard.peers['{}']", spec.public_key);
    let key = PublicKey::from(parse_key(&format!("{label}.public_key"), &spec.public_key)?);
    let mut peer = Peer::new(key);
    for cidr in &spec.allowed_ips {
        peer =
            peer.with_allowed_ip(cidr.parse::<IpNetwork>().map_err(|e| {
                format!("{label}.allowed_ips: '{cidr}' is not a CIDR network ({e})")
            })?);
    }
    if let Some(endpoint) = &spec.endpoint {
        peer = peer.with_endpoint(
            endpoint
                .parse::<SocketAddr>()
                .map_err(|e| format!("{label}.endpoint: '{endpoint}' is not a host:port ({e})"))?,
        );
    }
    peer.keepalive = spec.keepalive;
    if let Some(var) = &spec.preshared_key_env {
        let text = std::env::var(var)
            .map_err(|_| format!("{label}: env var '{var}' (preshared_key_env) is not set"))?;
        peer.preshared_key = Some(parse_key(&format!("{label}.preshared_key"), &text)?);
    }
    Ok(peer)
}

/// Every configured peer, or the first refusal.
///
/// An empty list is allowed and is not an oversight: a deployment that
/// learns its peers from a rendezvous has none at startup.
pub fn peers(config: &WireguardConfig) -> Result<Vec<Peer>, String> {
    config.peers.iter().map(peer).collect()
}

// ---------------------------------------------------------------------------
// Bringing the device up
// ---------------------------------------------------------------------------

/// The transports a deployment's device uses: a real UDP socket, and a
/// kernel tunnel interface for the IP side. gotatun's own default, named
/// here so the signatures below read as what they are.
pub type Kernel = gotatun::device::DefaultDeviceTransports;

/// Create the interface, bind the port, and add the configured peers.
///
/// Everything that can be refused is refused here, at startup, with the
/// thing that was wrong named: a key that is not a key, a CIDR that is not
/// a network, a port already held, an interface that needs a privilege
/// this process does not have. A deployment that gets past this line has a
/// device that is up.
pub async fn bind(config: &WireguardConfig) -> Result<Device<Kernel>, String> {
    let secret = private_key(config)?;
    let peers = peers(config)?;
    DeviceBuilder::new()
        .with_default_udp()
        .create_tun(&config.interface)
        .map_err(|e| {
            format!(
                "wireguard: cannot create the interface '{}': {e}\n\
                 Creating a tunnel interface needs a privilege: CAP_NET_ADMIN \
                 (or root) on Linux, root on macOS, wintun.dll beside the binary \
                 on Windows. Nothing else here does.",
                config.interface
            )
        })?
        .with_listen_port(config.listen_port)
        .with_private_key(secret)
        .with_peers(peers)
        .build()
        .await
        .map_err(|e| format!("wireguard: cannot bind port {}: {e}", config.listen_port))
}

/// `drt wg`: bring the device up and hold it up, foreground.
///
/// Prints the public key, because a peer cannot be configured without it
/// and deriving it by hand means running `wg pubkey` against a secret an
/// operator would then have on a terminal.
pub async fn serve(config: &WireguardConfig) -> Result<(), String> {
    let secret = private_key(config)?;
    let mut device = bind(config).await?;
    let port = device.read(async |d| d.listen_port()).await;
    eprintln!(
        "drt wg: {} up on port {port}, public key {}",
        config.interface,
        public_key(&secret)
    );
    for spec in &config.peers {
        eprintln!(
            "drt wg: peer {} allowed {}{}",
            spec.public_key,
            spec.allowed_ips.join(","),
            spec.endpoint
                .as_deref()
                .map(|e| format!(" via {e}"))
                .unwrap_or_else(|| " (no endpoint yet)".into()),
        );
    }
    device.wait().await;
    Ok(())
}

// ---------------------------------------------------------------------------
// What crosses between the device and the drive loop
// ---------------------------------------------------------------------------

/// What a program may ask of a running device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// Point a peer at an address, or clear it. This is the punch: the
    /// rendezvous learned where the far side actually is, and this is how
    /// the device is told.
    Endpoint {
        public_key: [u8; KEY_LEN],
        endpoint: Option<SocketAddr>,
    },
    /// Set or clear a peer's keepalive interval. Beside `Endpoint` because
    /// it is the other half of the same job: a punched mapping with no
    /// traffic through it closes again in tens of seconds.
    Keepalive {
        public_key: [u8; KEY_LEN],
        seconds: Option<u16>,
    },
}

/// One peer, as a supervisor sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerReport {
    pub public_key: String,
    pub endpoint: Option<SocketAddr>,
    /// Milliseconds since the last completed handshake, or `None` if there
    /// has never been one. This is the field that says whether a punch
    /// worked: an endpoint with no handshake is an address nobody answered
    /// at.
    pub last_handshake_ms: Option<u64>,
    pub rx_bytes: u64,
    pub tx_bytes: u64,
}

/// What the device tells the deployment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Report {
    /// Every peer, on the timer.
    Peers(Vec<PeerReport>),
    /// One peer's endpoint changed, as it happened. Reported separately
    /// from the snapshot because roaming is the event a punch waits for
    /// and a peer that moved between two snapshots would otherwise look
    /// like it had never moved at all.
    Roamed {
        public_key: String,
        endpoint: Option<SocketAddr>,
        previous: Option<SocketAddr>,
    },
}

// ---------------------------------------------------------------------------
// depth: the task that owns the device
// ---------------------------------------------------------------------------

/// Own the device: snapshot its peers on the timer, notice roaming as it
/// happens, and apply what the deployment asks.
///
/// Generic over the transports so the deployment's kernel-tun device and
/// the tests' channel-tun devices run the *same* loop. Returns when the
/// command channel closes, which is what dropping the bridge does.
pub async fn drive<T: DeviceTransports>(
    device: Device<T>,
    every: Duration,
    reports: mpsc::UnboundedSender<Report>,
    mut commands: mpsc::UnboundedReceiver<Command>,
) {
    let mut last: Vec<PeerReport> = Vec::new();
    let mut tick = tokio::time::interval(every);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            command = commands.recv() => {
                let Some(command) = command else { return };
                apply(&device, command).await;
            }
            _ = tick.tick() => {
                let now = snapshot(&device).await;
                for peer in &now {
                    let before = last.iter().find(|p| p.public_key == peer.public_key);
                    let previous = before.and_then(|p| p.endpoint);
                    // A peer seen for the first time WITH an endpoint has
                    // not roamed, it has arrived; one whose endpoint
                    // changed has.
                    if before.is_some() && previous != peer.endpoint {
                        let _ = reports.send(Report::Roamed {
                            public_key: peer.public_key.clone(),
                            endpoint: peer.endpoint,
                            previous,
                        });
                    }
                }
                if now != last {
                    last = now.clone();
                    let _ = reports.send(Report::Peers(now));
                }
            }
        }
    }
}

/// depth: one command, applied. A command naming a peer the device does
/// not have is dropped with a line rather than failing the device — the
/// deployment's own program wrote it, and the deployment is what would
/// stop.
async fn apply<T: DeviceTransports>(device: &Device<T>, command: Command) {
    let (key, known) = match command {
        Command::Endpoint {
            public_key,
            endpoint,
        } => {
            let key = PublicKey::from(public_key);
            let known = device
                .write(async |d| d.modify_peer(&key, |p| p.set_endpoint(endpoint)).await)
                .await;
            (key, known)
        }
        Command::Keepalive {
            public_key,
            seconds,
        } => {
            let key = PublicKey::from(public_key);
            let known = device
                .write(async |d| d.modify_peer(&key, |p| p.set_keepalive(seconds)).await)
                .await;
            (key, known)
        }
    };
    if !matches!(known, Ok(true)) {
        eprintln!(
            "drt start: wireguard: no peer {}",
            base64::engine::general_purpose::STANDARD.encode(key.as_bytes())
        );
    }
}

/// depth: the device's peers, in this module's shape.
async fn snapshot<T: DeviceTransports>(device: &Device<T>) -> Vec<PeerReport> {
    device
        .read(async |d| {
            d.peers()
                .await
                .into_iter()
                .map(|p| PeerReport {
                    public_key: base64::engine::general_purpose::STANDARD
                        .encode(p.peer.public_key.as_bytes()),
                    endpoint: p.peer.endpoint,
                    last_handshake_ms: p.stats.last_handshake.map(|d| d.as_millis() as u64),
                    rx_bytes: p.stats.rx_bytes as u64,
                    tx_bytes: p.stats.tx_bytes as u64,
                })
                .collect()
        })
        .await
}

// ---------------------------------------------------------------------------
// The deployment's end
// ---------------------------------------------------------------------------

/// The deployment's end of an in-process WireGuard device.
///
/// Same arrangement as [`crate::stun::StunBridge`] and the relay's, and for
/// the same reason: gotatun is tokio and `drt start`'s drive loop is not,
/// so the device runs on its own runtime on its own thread and speaks to
/// the loop through channels. Both ends are non-blocking. The drive loop
/// never awaits a lock the device holds, which matters more here than for
/// `stun`: the device holds its lock while it encrypts.
pub struct WireguardBridge {
    reports: mpsc::UnboundedReceiver<Report>,
    commands: mpsc::UnboundedSender<Command>,
    queue: String,
    reply_queue: String,
    interface: String,
    listen_port: u16,
    public_key: String,
    /// Kept alive for the process's life: dropping it stops the device.
    _runtime: std::thread::JoinHandle<()>,
}

impl WireguardBridge {
    /// Bring the device up and drive it on its own runtime.
    ///
    /// Binding happens here, synchronously, so an interface that needs a
    /// privilege this process lacks fails `drt start` at startup with the
    /// reason, rather than becoming a thread that dies quietly once the
    /// deployment is up and a program is already writing to an address
    /// that will never answer.
    pub fn start(config: &WireguardConfig) -> Result<WireguardBridge, String> {
        let rt = tokio::runtime::Runtime::new()
            .map_err(|e| format!("the wireguard device needs a runtime: {e}"))?;
        let secret = private_key(config)?;
        let device = rt.block_on(bind(config))?;
        let listen_port = rt.block_on(device.read(async |d| d.listen_port()));
        let (report_tx, reports) = mpsc::unbounded_channel();
        let (commands, command_rx) = mpsc::unbounded_channel();
        let every = Duration::from_millis(config.report_ms.max(1));
        let runtime = std::thread::spawn(move || {
            rt.block_on(drive(device, every, report_tx, command_rx));
        });
        Ok(WireguardBridge {
            reports,
            commands,
            queue: config.queue.clone(),
            reply_queue: config.reply_queue.clone(),
            interface: config.interface.clone(),
            listen_port,
            public_key: public_key(&secret),
            _runtime: runtime,
        })
    }

    /// The port actually bound. A `listen_port` of zero resolves here, so
    /// an operator can read it off the startup line and hand it to
    /// `netcheck --udp-port` — though a punch should name its port in the
    /// config instead, since a mapping is measured per port.
    pub fn listen_port(&self) -> u16 {
        self.listen_port
    }

    /// The interface that was created.
    pub fn interface(&self) -> &str {
        &self.interface
    }

    /// This device's public key, base64, to hand a peer.
    pub fn public_key(&self) -> &str {
        &self.public_key
    }

    /// Push whatever the device has said onto the root's queue.
    /// Non-blocking: anything not ready now is picked up next pass.
    pub fn report(&mut self, push: &mut dyn FnMut(&str, &[u8]) -> bool) {
        while let Ok(report) = self.reports.try_recv() {
            let msg = encode(&report_value(&report));
            // A full or undeclared queue is the deployment's own sizing to
            // see. A dropped snapshot costs a panel one interval and the
            // next carries the running totals; failing the device over it
            // would cost the tunnel.
            let _ = push(&self.queue, &msg);
        }
    }

    /// Drain the deployment's commands and apply them.
    ///
    /// Reads nothing when no `reply_queue` is named, which is the default:
    /// a config that did not ask to be steered is not steerable, and a
    /// queue read by mistake would be a program's own messages consumed by
    /// a subsystem.
    pub fn collect(&mut self, pop: &mut dyn FnMut(&str) -> Option<Vec<u8>>) {
        if self.reply_queue.is_empty() {
            return;
        }
        while let Some(raw) = pop(&self.reply_queue) {
            let Ok(value) = rmpv::decode::read_value(&mut &raw[..]) else {
                continue;
            };
            match command_from(&value) {
                Ok(command) => {
                    let _ = self.commands.send(command);
                }
                // The deployment's own program wrote this. Naming what was
                // wrong beats dropping it in silence, and stopping the
                // device over it would be worse than either.
                Err(e) => eprintln!("drt start: wireguard: {e}"),
            }
        }
    }
}

// ---------------------------------------------------------------------------
// depth: the wire shapes
// ---------------------------------------------------------------------------

/// One report as the supervisor sees it. `event` is named the way the
/// relay's, `stun`'s and `turn`'s are, so one `if msg.event == …` chain in
/// a Lua supervisor handles every subsystem without knowing which spoke.
pub fn report_value(report: &Report) -> rmpv::Value {
    match report {
        Report::Peers(peers) => rmpv::Value::Map(vec![
            ("event".into(), "wireguard".into()),
            (
                "peers".into(),
                rmpv::Value::Array(
                    peers
                        .iter()
                        .map(|p| {
                            rmpv::Value::Map(vec![
                                ("public_key".into(), p.public_key.as_str().into()),
                                ("endpoint".into(), addr_value(p.endpoint)),
                                (
                                    "last_handshake_ms".into(),
                                    match p.last_handshake_ms {
                                        Some(ms) => rmpv::Value::from(ms),
                                        None => rmpv::Value::Nil,
                                    },
                                ),
                                ("rx_bytes".into(), rmpv::Value::from(p.rx_bytes)),
                                ("tx_bytes".into(), rmpv::Value::from(p.tx_bytes)),
                            ])
                        })
                        .collect(),
                ),
            ),
        ]),
        Report::Roamed {
            public_key,
            endpoint,
            previous,
        } => rmpv::Value::Map(vec![
            ("event".into(), "wireguard_endpoint".into()),
            ("public_key".into(), public_key.as_str().into()),
            ("endpoint".into(), addr_value(*endpoint)),
            ("previous".into(), addr_value(*previous)),
        ]),
    }
}

/// An address, or nil. Never the empty string: "not known" and "known to
/// be nothing" are the same fact here, and both are absence.
fn addr_value(addr: Option<SocketAddr>) -> rmpv::Value {
    match addr {
        Some(a) => a.to_string().as_str().into(),
        None => rmpv::Value::Nil,
    }
}

/// One command, read off the reply queue, or why it was not one.
///
/// Public because it is the wire contract between a supervisor and this
/// device, and a contract nothing can exercise without a kernel interface
/// is a contract nothing tests.
pub fn command_from(value: &rmpv::Value) -> Result<Command, String> {
    let field = |name: &str| {
        value
            .as_map()?
            .iter()
            .find(|(k, _)| k.as_str() == Some(name))
            .map(|(_, v)| v)
    };
    let name = field("command").and_then(|v| v.as_str()).ok_or_else(|| {
        format!(
            "a message with no `command` (known: {})",
            COMMAND.join(", ")
        )
    })?;
    // The name is checked BEFORE the fields it implies. A supervisor that
    // typed `reboot` has one thing wrong, and reporting it as a missing
    // `public_key` sends the reader to look at the field they got right.
    if !COMMAND.contains(&name) {
        return Err(format!(
            "unknown command '{name}' (known: {})",
            COMMAND.join(", ")
        ));
    }
    let key_text = field("public_key")
        .and_then(|v| v.as_str())
        .ok_or_else(|| format!("{name}: no `public_key`"))?;
    let public_key = parse_key(&format!("{name}.public_key"), key_text)?;
    // An absent field and an explicit nil mean the same thing on purpose:
    // "clear it". A Lua table cannot hold a nil value, so a program that
    // wants to clear an endpoint can only do it by leaving the key out.
    let endpoint = field("endpoint").and_then(|v| v.as_str());
    match name {
        "endpoint" => Ok(Command::Endpoint {
            public_key,
            endpoint: match endpoint {
                Some(text) => Some(
                    text.parse::<SocketAddr>()
                        .map_err(|e| format!("endpoint: '{text}' is not a host:port ({e})"))?,
                ),
                None => None,
            },
        }),
        "keepalive" => Ok(Command::Keepalive {
            public_key,
            seconds: field("seconds").and_then(|v| v.as_u64()).map(|s| s as u16),
        }),
        // Unreachable: the name was checked against COMMAND above, and
        // this match covers it. Kept as a refusal rather than a panic so
        // that adding a name to COMMAND and forgetting an arm is a message
        // on stderr and not a dead deployment.
        other => Err(format!(
            "command '{other}' is named but not implemented; this is a bug"
        )),
    }
}

fn encode(value: &rmpv::Value) -> Vec<u8> {
    let mut out = Vec::new();
    rmpv::encode::write_value(&mut out, value).expect("a wireguard report encodes");
    out
}
