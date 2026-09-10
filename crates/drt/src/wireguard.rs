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
//! - The UDP side is a trait. What that buys today is [`measure`]: the
//!   mapping of a probe socket on the same local port, taken microseconds
//!   before the device binds that port — because `netcheck --udp-port`
//!   cannot bind a port the running device already holds. Handing the
//!   device the *same* socket remains possible and is not implemented;
//!   `doc/WireGuard.md` §2 says so rather than implying otherwise.
//! - The IP side is a trait, so `drt start` gives it a kernel interface
//!   and the tests give it channels — the same device, proven without
//!   privileges (`crates/drt/tests/wireguard.rs` runs two of them in one
//!   process and passes a real packet between them).
//!
//! ## The punch, and what is not proven about it
//!
//! **The deployment-side half is here and tested; the punch itself is
//! not measured in this repository.** What the tests prove is that DRT
//! can measure its own mapping, be told where a peer is — including a
//! peer the config never named — and then talk to it. They run on
//! loopback, so there is no NAT in the path and no mapping to punch
//! through.
//!
//! The punch proper rests on a property of WireGuard rather than of this
//! file: a peer with an endpoint and no session retransmits a handshake
//! initiation every five seconds for ninety, so once both sides know
//! where the other is, the simultaneous-open that opens a NAT mapping
//! falls out of the protocol's own behaviour. That is sound and
//! well-established; it is also not something measured here.
//! `doc/WireGuard.md` §2 states which step belongs to whom.
//!
//! ## The reply queue, which is why any of this composes
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
//! - Configurable: [`CONNECT_TIMEOUT`], how long a forward's dial inside
//!   the tunnel waits before it is a `wireguard_error` rather than a hang.
//! - The UDP side: [`Transport`], which is gotatun's own socket plus the
//!   option of a TURN allocation beside it, and [`unroutable`], which names
//!   an `allowed_ips` no route will reach; [`unreachable_forwards`] is its
//!   userspace-mode counterpart, naming a `forward.to` no peer is allowed.
//! - Fan-out: [`INTERFACE_ERRNO`], what each way of failing to create the
//!   interface means, and [`interface_here`], the same facts asked of this
//!   machine before anything is created.

use std::net::SocketAddr;
use std::time::Duration;

use base64::Engine as _;
use std::sync::Arc;

use gotatun::device::{Device, DeviceBuilder, DeviceTransports, Peer};
use gotatun::packet::{Packet, PacketBufPool};
use gotatun::udp::{UdpRecv, UdpSend, UdpTransportFactory};
use gotatun::x25519::{PublicKey, StaticSecret};
use ipnetwork::IpNetwork;
use tokio::sync::mpsc;
use webrtc_util::Conn as _;

use drt_config::{WireguardConfig, WireguardMode, WireguardPeer};

/// A WireGuard key is 32 bytes, always. Curve25519 fixes it, `wg genkey`
/// prints exactly this base64-encoded, and a key of any other length is a
/// pasted-wrong key rather than a configuration choice.
pub const KEY_LEN: usize = 32;

/// The commands a program may put on the reply queue. Named here rather
/// than at their match arms so the whole vocabulary is one list.
pub const COMMAND: [&str; 6] = ["endpoint", "keepalive", "add", "remove", "relay", "remap"];

/// The events a program is sent on the queue.
pub const EVENT: [&str; 5] = [
    "wireguard",
    "wireguard_endpoint",
    "wireguard_mapping",
    "wireguard_relay",
    "wireguard_error",
];

/// How long a `forward`'s dial inside the tunnel may wait for its SYN to be
/// answered before the accepted connection is closed and a
/// `wireguard_error` names the entry.
///
/// Fifteen seconds is three handshake initiations at WireGuard's five
/// second retransmit, plus a round trip: a peer that has not answered
/// three initiations is not about to. A dead tunnel is a timeout, never a
/// client sitting on a SYN forever.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

/// How often the device is polled for a change worth reporting.
///
/// Separate from `report_ms`, which is how often a full snapshot is sent,
/// and much shorter than it: watching a punch means watching for a
/// handshake that either lands within a second or two or never lands at
/// all, and a supervisor learning at the next ten-second snapshot cannot
/// tell "it worked" from "it worked eventually".
pub const WATCH_MS: u64 = 250;

/// How many one-shot reports to hold for a queue that has not taken them.
///
/// A root program that has not yet run the line declaring its queue takes
/// a pass or two, so the case this exists for never approaches the cap. A
/// queue that stays full is the deployment's own sizing to see, and
/// holding for it without a bound would move the overflow out of the
/// queue and into this process. Past the cap the NEWEST are dropped: the
/// report a program is blocked on is the mapping, which is sent first.
pub const HELD_MAX: usize = 64;

/// How long a `remap` waits for the suspended device to let its port go.
///
/// `Device::suspend` returns before its I/O tasks have dropped their
/// sockets — about 6 ms on the machine this was measured on — so a
/// measurement started the instant it returns fails with "address already
/// in use" and reports nothing. Generous by two orders of magnitude
/// against that, because the cost of waiting too long is a slow remap and
/// the cost of not waiting long enough is a remap that always fails.
/// Bounded, because a port that never comes back is a fact to report
/// rather than a loop to spin in.
pub const PORT_RELEASE_MS: u64 = 500;

/// The device node a Linux tunnel interface is created through.
///
/// Named here because three of the four ways [`interface`] can fail are
/// facts about this path rather than about the config, and [`interface_here`]
/// reports them from the same constant the failure quotes.
#[cfg(target_os = "linux")]
pub const TUN_NODE: &str = "/dev/net/tun";

/// CAP_NET_ADMIN's number, which is its bit in `/proc/self/status`'s
/// `CapEff`. Fixed by `linux/capability.h` and here because
/// [`interface_here`] reads that mask.
#[cfg(target_os = "linux")]
pub const CAP_NET_ADMIN: u32 = 12;

/// What each way of failing to create the interface actually means.
///
/// A dispatch table and not a paragraph, because the paragraph was wrong.
/// Every failure used to carry the CAP_NET_ADMIN sentence, which is true
/// of exactly one of these four: a container with no device node and a
/// process that already held the capability were both told to go and get
/// the capability, and the second cost an operator an hour and nearly a
/// design (issue #21).
///
/// All four measured on 2026-09-08 by making each one happen: the node
/// moved aside, the node at 0600 as a non-root user, the node at 0666 as a
/// non-root user, and two devices asking for one interface name.
///
/// Per platform, and not merely per `errno`, for the reason the table
/// exists: two of these entries are facts about a device node, and macOS
/// allocates a `utun` through a kernel control socket that has none. An
/// `ENOENT` there would be something else, so it is answered with silence
/// rather than with Linux's answer.
#[cfg(target_os = "linux")]
pub const INTERFACE_ERRNO: [(i32, &str); 4] = [
    (
        2, // ENOENT
        "There is no /dev/net/tun. In a container, pass --device /dev/net/tun; \
         on a host, `modprobe tun`. This is not a privilege: a process running \
         as root fails here identically.",
    ),
    (
        13, // EACCES
        "/dev/net/tun is there and this process cannot open it. That is a file \
         mode, not a capability, and CAP_NET_ADMIN does not override one -- \
         `ls -l /dev/net/tun`, which most distributions ship 0666.",
    ),
    (
        1, // EPERM
        "The device node opened and the kernel refused the interface. This one \
         IS the privilege, and it is the only one this needs: CAP_NET_ADMIN (or \
         root) on Linux, root on macOS, wintun.dll beside the binary on Windows.",
    ),
    (
        16, // EBUSY
        "The interface name is already taken -- something else, or a previous \
         run, is holding it. `ip link show` names it; `ip link del` frees it if \
         it is left over.",
    ),
];

/// The subset that is true off Linux: no device node, so no entry about
/// one. Same numbers — the BSDs and Linux agree on these two.
#[cfg(all(unix, not(target_os = "linux")))]
pub const INTERFACE_ERRNO: [(i32, &str); 2] = [
    (
        1, // EPERM
        "This one IS the privilege, and it is the only one this needs: root on \
         macOS, CAP_NET_ADMIN (or root) on Linux, wintun.dll beside the binary \
         on Windows.",
    ),
    (
        16, // EBUSY
        "The interface name is already taken -- something else, or a previous \
         run, is holding it.",
    ),
];

/// Windows fails through wintun rather than through an errno, so there is
/// no table to key on and nothing here to look up.
#[cfg(not(unix))]
pub const INTERFACE_ERRNO: [(i32, &str); 0] = [];

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

/// Everything about the block that can be judged without touching the
/// network or the machine. Called before anything is bound, so a bad
/// config fails `drt start` at the line that names it.
pub fn validate(config: &WireguardConfig) -> Result<Vec<String>, String> {
    // Zero is refused rather than defaulted, because it cannot be
    // reported honestly and cannot be punched to. gotatun answers
    // `listen_port()` with the port it was CONFIGURED with, not the one
    // it bound, so an ephemeral port would be printed as "0" and could
    // never be named to a peer; and a NAT mapping is measured per port,
    // so a peer that wants to be reached must own a stable one.
    if config.listen_port == 0 {
        return Err("wireguard: listen_port must be a real port (51820 is the \
                    convention). Zero would take an ephemeral one, which cannot \
                    be read back to tell a peer, and cannot have its NAT mapping \
                    measured, since a mapping belongs to a port."
            .into());
    }
    if let Some(cidr) = &config.address {
        let net: IpNetwork = cidr.parse().map_err(|e| {
            format!("wireguard.address: '{cidr}' is not a CIDR address like 10.9.0.1/24 ({e})")
        })?;
        // The network address itself is not an address a host holds. A
        // `10.9.0.0/24` here is the commonest transcription slip from a
        // route table, and it produces an interface that answers to
        // nothing rather than an error.
        let host_bits = match net {
            IpNetwork::V4(_) => 32,
            IpNetwork::V6(_) => 128,
        };
        if net.prefix() < host_bits && net.ip() == net.network() {
            return Err(format!(
                "wireguard.address: '{cidr}' is the network address, not a host address \
                 on it. Give the address this device holds, like 10.9.0.1/24."
            ));
        }
    }
    if config.mtu < 576 {
        return Err(format!(
            "wireguard.mtu: {} is below the 576 every IPv4 host must accept",
            config.mtu
        ));
    }
    // One STUN server can report an address; only two can say whether it
    // CHANGED between vantage points, which is the fact that decides
    // whether a punch can work. Refusing one is the same rule
    // `netcheck` applies, for the same reason.
    if config.stun.len() == 1 {
        return Err(
            "wireguard.stun: classifying a NAT mapping needs two servers on \
                    separate addresses; one server can report an address but only two \
                    can say whether it changed, which is what decides whether a punch \
                    is possible. Give two, or none."
                .into(),
        );
    }
    private_key(config)?;
    peers(config)?;
    validate_mode(config)?;
    // Returned rather than printed: `bind` validates too, so printing here
    // said everything twice on every start.
    let mut warnings = unroutable(config);
    warnings.extend(unreachable_forwards(config));
    // A device with no peers is the config a rendezvous starts from, and
    // it is perfectly good -- but only if something can tell it about a
    // peer later. With no `reply_queue` there is no way to, so this one
    // comes up, measures, and waits for a command that can never arrive.
    // Both halves are in the config, so it can be said here.
    if config.peers.is_empty() && config.reply_queue.is_empty() {
        warnings.push(
            "this block names no peers and no `reply_queue`, so nothing can ever \
             tell it about one: it will come up, measure its mapping, and wait. \
             A block that learns its peers at run time needs a `reply_queue` to \
             learn them on."
                .into(),
        );
    }
    Ok(warnings)
}

/// The half of [`validate`] that is about the mode: what a userspace
/// stack needs to be reachable, and what a kernel interface cannot carry.
///
/// Every refusal names the entry, because both numbers are in the config
/// and a wrong one otherwise appears as a connection that times out. The
/// stack answers on the block's `address` and nothing else, so an
/// `expose.tunnel` elsewhere would listen on an address no packet is ever
/// delivered to; a `to` that is the prefix's network address is the same
/// transcription slip `address` refuses; and a name in `to` has no
/// resolver inside a tunnel to answer it.
fn validate_mode(config: &WireguardConfig) -> Result<(), String> {
    match config.mode {
        WireguardMode::Kernel => {
            if !config.forward.is_empty() || !config.expose.is_empty() {
                return Err(format!(
                    "wireguard: `{}` is only reached through the kernel interface, so a \
                     `forward` or `expose` here is a config that believes it is in the \
                     other mode. Set mode = \"userspace\", or remove them.",
                    if config.forward.is_empty() {
                        "expose"
                    } else {
                        "forward"
                    }
                ));
            }
            Ok(())
        }
        WireguardMode::Userspace => {
            if config.forward.is_empty() && config.expose.is_empty() {
                return Err(
                    "wireguard: mode = \"userspace\" with neither `forward` nor \
                            `expose`: a userspace device nothing can reach terminates \
                            traffic nobody can hand it. Name a local port that reaches \
                            the tunnel, an address in it that reaches a local port, or \
                            both."
                        .into(),
                );
            }
            let Some(cidr) = &config.address else {
                return Err("wireguard: mode = \"userspace\" needs an `address`: the \
                            kernel tolerates an interface with none, but a stack cannot \
                            answer from nowhere. Give the one this device holds, like \
                            10.9.0.1/24."
                    .into());
            };
            // Already refused by name above when it does not parse.
            let net: IpNetwork = cidr
                .parse()
                .map_err(|e| format!("wireguard.address: {e}"))?;
            let own = net.ip();
            let host_address = |label: &str, text: &str| -> Result<SocketAddr, String> {
                let addr: SocketAddr = text.parse().map_err(|_| {
                    format!(
                        "wireguard.{label}: '{text}' is not an ip:port. An address and a \
                         port, like 10.9.0.2:22: there is no resolver inside a tunnel."
                    )
                })?;
                if addr.ip().is_unspecified() {
                    return Err(format!(
                        "wireguard.{label}: '{text}' is the unspecified address, which \
                         nothing can dial"
                    ));
                }
                if net.contains(addr.ip()) && addr.ip() == net.network() && net.prefix() < 32 {
                    return Err(format!(
                        "wireguard.{label}: '{text}' is the network address of {cidr}, not \
                         a host on it. Give the address the far machine holds, like \
                         10.9.0.2:22."
                    ));
                }
                if addr.port() == 0 {
                    return Err(format!(
                        "wireguard.{label}: '{text}' has no port; something has to be \
                         listening on one"
                    ));
                }
                Ok(addr)
            };
            let mut binds: Vec<SocketAddr> = Vec::new();
            for (i, entry) in config.forward.iter().enumerate() {
                let bind: SocketAddr = entry.bind.parse().map_err(|_| {
                    format!(
                        "wireguard.forward[{i}].bind: '{}' is not an ip:port to listen \
                         on, like 127.0.0.1:2222 (or :0 for an ephemeral port, reported \
                         as `wireguard_forward`)",
                        entry.bind
                    )
                })?;
                if bind.port() != 0 && binds.contains(&bind) {
                    return Err(format!(
                        "wireguard.forward[{i}].bind: '{bind}' is already the bind of \
                         another forward; one port reaches one place"
                    ));
                }
                binds.push(bind);
                host_address(&format!("forward[{i}].to"), &entry.to)?;
            }
            let mut tunnels: Vec<SocketAddr> = Vec::new();
            for (i, entry) in config.expose.iter().enumerate() {
                let tunnel = host_address(&format!("expose[{i}].tunnel"), &entry.tunnel)?;
                if tunnel.ip() != own {
                    return Err(format!(
                        "wireguard.expose[{i}].tunnel: '{}' is not on this device's own \
                         address {own}, which is the only one the stack answers on",
                        entry.tunnel
                    ));
                }
                if tunnels.contains(&tunnel) {
                    return Err(format!(
                        "wireguard.expose[{i}].tunnel: '{tunnel}' is already exposed by \
                         another entry; one port reaches one place"
                    ));
                }
                tunnels.push(tunnel);
                if entry.to.trim().is_empty() {
                    return Err(format!(
                        "wireguard.expose[{i}].to: the local host:port this reaches is empty"
                    ));
                }
            }
            Ok(())
        }
    }
}

/// Which `forward.to` no peer is allowed, in userspace mode: the
/// counterpart of [`unroutable`]. A forward whose destination sits outside
/// every peer's `allowed_ips` is configured perfectly and reaches nothing --
/// the stack routes it to the device, the device has no peer for it, and
/// the client waits out [`CONNECT_TIMEOUT`]. Both numbers are in the config,
/// so it is a sentence at startup instead of a fifteen second silence.
///
/// A warning and not a refusal, and silent when the block names no peers
/// at all: that is the config a rendezvous writes, and the peer arrives
/// over `reply_queue` with the address the forward is waiting for.
pub fn unreachable_forwards(config: &WireguardConfig) -> Vec<String> {
    let mut said = Vec::new();
    if config.mode != WireguardMode::Userspace || config.peers.is_empty() {
        return said;
    }
    for entry in &config.forward {
        let Ok(to) = entry.to.parse::<SocketAddr>() else {
            continue; // `validate_mode` refuses this by name; not this function's job.
        };
        let allowed = config.peers.iter().any(|peer| {
            peer.allowed_ips
                .iter()
                .filter_map(|cidr| cidr.parse::<IpNetwork>().ok())
                .any(|net| net.contains(to.ip()))
        });
        if !allowed {
            said.push(format!(
                "forward {} -> {}: no peer is allowed {}, so that connection will \
                 time out after {}s and carry nothing. Add it to a peer's allowed_ips, \
                 or fix `to`.",
                entry.bind,
                entry.to,
                to.ip(),
                CONNECT_TIMEOUT.as_secs()
            ));
        }
    }
    said
}

/// Which `allowed_ips` no packet will ever reach, and what to do about it.
///
/// DRT gives the interface an address and the kernel derives exactly one
/// route from it: the on-link one for that address's own prefix. A peer
/// whose `allowed_ips` lies outside that prefix is configured perfectly and
/// carries nothing — WireGuard would encrypt for it happily, and nothing
/// ever hands it a packet, because the routing table sends those addresses
/// somewhere else entirely.
///
/// That is the failure this catches: a tunnel that comes up, reports a
/// handshake, and silently drops the traffic it was built for. Both numbers
/// are in the config, so it can be said at startup instead of discovered
/// with tcpdump.
///
/// A warning and not a refusal, deliberately: the setup works the moment
/// the operator adds the route, and a hub whose whole job is to reach
/// subnets outside its own prefix is a legitimate config, not a mistake.
/// Route management is not DRT's yet (`doc/WireGuard.md` §4), so the honest
/// thing is to name the gap and the command that closes it.
pub fn unroutable(config: &WireguardConfig) -> Vec<String> {
    let mut said = Vec::new();
    // A userspace stack carries a default route, so every allowed_ip is
    // routed to the device and the `ip route add` below would be wrong
    // advice. What can go unreached there is a `forward.to`, which
    // `unreachable_forwards` names instead.
    if config.mode == WireguardMode::Userspace {
        return said;
    }
    let on_link: Option<IpNetwork> = config.address.as_ref().and_then(|a| a.parse().ok());
    for peer in &config.peers {
        for cidr in &peer.allowed_ips {
            let Ok(net) = cidr.parse::<IpNetwork>() else {
                continue; // `peer()` refuses this by name; not this function's job.
            };
            // A default route is the "send everything through the tunnel"
            // case. It needs a route too, but saying so for every VPN-shaped
            // config would be noise where the intent is unmistakable.
            if net.prefix() == 0 {
                continue;
            }
            let covered = match on_link {
                // Same family, and the address's prefix contains this
                // network: the on-link route already reaches it.
                Some(link) => link.is_ipv4() == net.is_ipv4() && link.contains(net.network()),
                None => false,
            };
            if covered {
                continue;
            }
            let reason = match &config.address {
                None => "this device has no `address`, so it has no route at all".to_string(),
                Some(addr) => format!("outside {addr}, the only prefix the interface routes"),
            };
            said.push(format!(
                "peer {} is allowed {cidr}, and nothing will reach it: {reason}. \
                 It will handshake and carry nothing. Add the route yourself \
                 (`ip route add {cidr} dev {}`), or narrow allowed_ips.",
                peer.public_key, config.interface
            ));
        }
    }
    said
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

/// A fresh key pair, base64: the private key this device would use and
/// the public key its peers would name it by.
///
/// Here because `wg genkey | wg pubkey` is the one step of setting up a
/// WireGuard peer that this block otherwise leaves to the tools it exists
/// to not need — and because an operator who has to install `wireguard-
/// tools` to generate a key has learned that the dependency was never
/// really gone.
///
/// The bytes are the platform CSPRNG's, the same source `crypto/random`
/// answers from. `StaticSecret::from` clamps them, which is Curve25519's
/// own requirement and the reason a key is not simply 32 random bytes.
pub fn keygen() -> (String, String) {
    let mut bytes = [0u8; KEY_LEN];
    // Infallible on every platform DRT builds a device for; a key that
    // could not be generated would be a refusal, not a weak key.
    let _ = drt_platform::entropy::fill(&mut bytes);
    let secret = StaticSecret::from(bytes);
    (
        base64::engine::general_purpose::STANDARD.encode(secret.to_bytes()),
        public_key(&secret),
    )
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
    if spec.allowed_ips.is_empty() {
        return Err(format!(
            "{label}.allowed_ips: empty. A peer with no allowed IPs can neither be \
             routed to nor accepted from -- WireGuard's cryptokey routing drops \
             every packet either way -- so this is a peer that does nothing."
        ));
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

/// The transports a deployment's device uses: [`Transport`] — gotatun's own
/// socket, with the option of a TURN allocation beside it — and a kernel
/// tunnel interface for the IP side.
pub type Kernel = (
    Transport,
    gotatun::tun::tun_async_device::TunDevice,
    gotatun::tun::tun_async_device::TunDevice,
);

/// Create the tunnel interface, **with an address, an MTU, and the link
/// up**.
///
/// gotatun's own `TunDevice::from_name` takes the `tun` crate's default
/// configuration, which sets none of those: the interface appears, has no
/// address, and is down. That is an interface nothing can use until the
/// operator runs `ip addr add` and `ip link set up` by hand — which is
/// precisely the `wg-quick` work this block exists to replace. So the
/// configuration is built here instead.
///
/// One address, because that is what the layer beneath takes. A second —
/// an IPv6 address beside an IPv4 one — is still `ip addr add`, and
/// `doc/WireGuard.md` says so rather than pretending otherwise.
fn interface(
    config: &WireguardConfig,
) -> Result<gotatun::tun::tun_async_device::TunDevice, String> {
    let mut tun = gotatun::tun::tun::Configuration::default();
    tun.tun_name(&config.interface).mtu(config.mtu).up();
    if let Some(cidr) = &config.address {
        let net: IpNetwork = cidr
            .parse()
            .map_err(|e| format!("wireguard.address: '{cidr}' is not a CIDR address ({e})"))?;
        tun.address(net.ip()).netmask(net.mask());
    }
    let device = gotatun::tun::tun::create_as_async(&tun).map_err(|e| {
        let advice = interface_advice(errno(&e));
        format!(
            "wireguard: cannot create the interface '{}': {e}{}{advice}",
            config.interface,
            if advice.is_empty() { "" } else { "\n" },
        )
    })?;
    gotatun::tun::tun_async_device::TunDevice::from_tun_device(device).map_err(|e| {
        format!(
            "wireguard: the interface '{}' is unusable: {e}",
            config.interface
        )
    })
}

/// The OS error behind a `tun` failure, when the failure has one.
///
/// Every way the platform layer can fail arrives as `Error::Io`; the rest
/// of that enum is the crate refusing a configuration before it syscalls,
/// and those say what is wrong on their own.
fn errno(error: &gotatun::tun::tun::Error) -> Option<i32> {
    match error {
        gotatun::tun::tun::Error::Io(io) => io.raw_os_error(),
        _ => None,
    }
}

/// The sentence that follows a failed `create_as_async`, looked up in
/// [`INTERFACE_ERRNO`] by what actually failed.
///
/// Nothing when the errno is one this has not measured: an errno printed
/// bare is a thing to look up, and an errno printed under a confident
/// wrong cause is an hour lost. That asymmetry is the whole of issue #21.
///
/// Takes the number rather than the error so the mapping can be tested
/// without a kernel that will produce each one on demand.
pub fn interface_advice(errno: Option<i32>) -> &'static str {
    #[cfg(not(unix))]
    {
        // No errno to key on: wintun's failures are its own. A hedge, not
        // a diagnosis, and hedged on purpose.
        let _ = errno;
        return "On Windows this is most often wintun.dll not being beside the \
                binary.";
    }
    #[cfg(unix)]
    {
        let Some(errno) = errno else { return "" };
        INTERFACE_ERRNO
            .iter()
            .find(|(number, _)| *number == errno)
            .map(|(_, advice)| *advice)
            .unwrap_or("")
    }
}

/// What this machine can be told about creating a tunnel interface,
/// **without creating one**.
///
/// [`validate`] reads the config and nothing else, so a block can be
/// entirely right and `drt start` still fail on the machine it ran on —
/// which is what happened in issue #21, twice, after `drt wg check` said
/// `ok`. These are the cheap environment facts behind three of
/// [`INTERFACE_ERRNO`]'s four entries, asked before anything is created.
///
/// They are kept apart from `validate`'s warnings, and the caller keeps
/// them apart in its output and its exit code, because they are facts
/// about *here*: `check` exists so a config can be written and checked on
/// a laptop and deployed where the privilege is, and a laptop with no tun
/// node is not a bad config.
///
/// Empty on platforms where the equivalent question cannot be asked
/// without doing the thing — macOS allocates a `utun` through a kernel
/// control socket with no node to stat, and Windows would mean loading
/// wintun.dll. Silence here never means "this will work".
pub fn interface_here() -> Vec<String> {
    #[cfg(not(target_os = "linux"))]
    {
        Vec::new()
    }
    #[cfg(target_os = "linux")]
    {
        let mut findings = Vec::new();
        findings.extend(tun_node_here(std::path::Path::new(TUN_NODE)));
        // Asked second, and separately, because it is the failure the
        // other two get mistaken for. A process can hold the capability
        // and still not open the node, which is exactly the hour issue
        // #21 lost.
        if let Some(false) = net_admin_here() {
            findings.push(
                "this process does not hold CAP_NET_ADMIN, so the kernel will \
                 refuse the interface. Run it as root, or `setcap \
                 cap_net_admin=ep` the binary."
                    .into(),
            );
        }
        findings
    }
}

/// Whether the tunnel device node is there and this process can open it,
/// as a sentence or nothing.
///
/// Takes the path rather than reading [`TUN_NODE`] so a test can point it
/// at one that is not there. The case worth holding is the container
/// started without `--device /dev/net/tun`, and a test that moved the real
/// node would be a test that broke the machine it ran on.
///
/// Opening is the whole probe: `open` is what returns ENOENT and EACCES,
/// and the `TUNSETIFF` that would return EPERM is also what would create
/// an interface — which is the one thing `check` promises not to do. So a
/// node that opens is two facts established and the third still open, and
/// [`interface_here`] reads that third from the capability mask instead.
#[cfg(target_os = "linux")]
pub fn tun_node_here(path: &std::path::Path) -> Option<String> {
    match std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
    {
        Ok(_) => None,
        Err(e) => Some(format!(
            "{}: {e}. {}",
            path.display(),
            interface_advice(e.raw_os_error())
        )),
    }
}

/// Whether this process holds CAP_NET_ADMIN, or `None` if the answer could
/// not be read.
///
/// `CapEff` in `/proc/self/status` is the effective set as a hex mask, and
/// it covers root without a separate uid check: root's effective set is
/// full. `None` rather than a guess where `/proc` is not mounted — a
/// deployment that cannot read the mask should be told nothing about it
/// rather than told it is fine.
#[cfg(target_os = "linux")]
pub fn net_admin_here() -> Option<bool> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    let mask = status
        .lines()
        .find_map(|line| line.strip_prefix("CapEff:"))?
        .trim();
    let bits = u64::from_str_radix(mask, 16).ok()?;
    Some(bits & (1 << CAP_NET_ADMIN) != 0)
}

/// What STUN saw of the socket this device is about to bind.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mapping {
    /// `independent`, `symmetric` or `open`, in `netcheck`'s words.
    pub kind: &'static str,
    /// Whether a peer can be punched to at [`Mapping::address`]. False for
    /// a symmetric NAT, where what a STUN server saw says nothing about
    /// what a peer would see.
    pub punchable: bool,
    /// The address to publish to a rendezvous, when there is one.
    pub address: Option<SocketAddr>,
    /// What was measured, in one line, for a log or a refusal.
    pub why: String,
    /// This machine's own addresses, one per family: the host candidates a
    /// same-LAN peer needs, which the mapped address cannot serve (issue
    /// #25 -- two machines behind one router would have to hairpin through
    /// it). Raw and unfiltered; whether to publish one is the program's
    /// call, since a LAN address discloses topology. Empty offline.
    pub local: Vec<std::net::IpAddr>,
}

/// Measure this device's own mapping, on `listen_port`, **before** the
/// device binds it.
///
/// This is the piece that makes a punch measurable rather than hoped for,
/// and it exists here rather than as `netcheck --udp-port` because
/// `netcheck` cannot do this job once a deployment is running: the device
/// holds the port, so a second bind refuses, and measuring a *different*
/// port measures a different mapping. Measuring here — same local port,
/// microseconds before the device takes it — is as close as a userspace
/// program gets to asking about the socket it is going to use.
///
/// **It is still not the same socket.** The mapping measured belongs to
/// the probe's socket on that port; the device then binds the same local
/// port and gets its own. On an endpoint-independent NAT the external
/// mapping is a function of the internal port, so the answer holds; on a
/// symmetric NAT it does not, which is exactly what `punchable: false`
/// reports. `doc/WireGuard.md` §2 states the limit rather than burying it.
#[cfg(feature = "netcheck")]
pub async fn measure(config: &WireguardConfig) -> Result<Mapping, String> {
    let servers: Vec<&str> = config.stun.iter().map(String::as_str).collect();
    let (mapping, ports, address) =
        crate::netcheck::gather::udp_mapping(&servers, Some(config.listen_port)).await?;
    let seen: Vec<String> = ports
        .iter()
        .map(|(server, port)| format!("{server} saw :{port}"))
        .collect();
    let (kind, punchable, why) = match mapping {
        crate::netcheck::UdpMapping::Open => (
            "open",
            true,
            format!("no NAT in the path ({})", seen.join(", ")),
        ),
        crate::netcheck::UdpMapping::Independent => (
            "independent",
            true,
            format!(
                "one mapping for every destination, so a peer can reach it ({})",
                seen.join(", ")
            ),
        ),
        crate::netcheck::UdpMapping::Symmetric => (
            "symmetric",
            false,
            format!(
                "a fresh mapping per destination, so what a STUN server saw says \
                 nothing about what a peer would see; punching cannot work from \
                 here and a relay is the path ({})",
                seen.join(", ")
            ),
        ),
    };
    // The port to publish is the one the servers agreed on, and only when
    // they agreed: a symmetric mapping has no single port to name.
    let port = ports.first().map(|(_, p)| *p);
    let address = match (punchable, address, port) {
        (true, Some(ip), Some(port)) => Some(SocketAddr::new(ip, port)),
        _ => None,
    };
    Ok(Mapping {
        kind,
        punchable,
        address,
        why,
        local: crate::netcheck::gather::local_addresses(),
    })
}

/// Bind the port and add the configured peers, on an interface that is
/// already up.
///
/// Everything that can be refused is refused before this returns, with
/// the thing that was wrong named: a key that is not a key, a CIDR that is
/// not a network, a port already held, an interface that needs a privilege
/// this process has not got. A deployment that gets past this line has a
/// device that is up and addressable.
pub async fn bind(
    config: &WireguardConfig,
) -> Result<(Device<Kernel>, Option<Allocation>), String> {
    validate(config)?;
    let secret = private_key(config)?;
    let peers = peers(config)?;
    let tun = interface(config)?;
    // The handle the `relay` command later installs an allocation into.
    // Absent entirely when `turn_fallback` is off, which is what keeps the
    // batched socket read for every device that will never relay.
    let (transport, allocation) = Transport::new(config.turn_fallback);
    let device = DeviceBuilder::new()
        .with_udp(transport)
        .with_ip(tun)
        .with_listen_port(config.listen_port)
        .with_private_key(secret)
        .with_peers(peers)
        .build()
        .await
        .map_err(|e| {
            format!(
                "wireguard: cannot bind UDP port {}: {e}",
                config.listen_port
            )
        })?;
    Ok((device, allocation))
}

/// `drt wg`: bring the device up and hold it up, foreground.
///
/// Prints the public key, because a peer cannot be configured without it
/// and deriving it by hand means running `wg pubkey` against a secret on
/// a terminal — the tool this block exists to not need.
pub async fn serve(config: &WireguardConfig) -> Result<(), String> {
    validate(config)?;
    let secret = private_key(config)?;
    #[cfg(feature = "netcheck")]
    if !config.stun.is_empty() {
        match measure(config).await {
            Ok(m) => eprintln!(
                "drt wg: mapping {} on port {} -- {}{}",
                m.kind,
                config.listen_port,
                m.why,
                m.address
                    .map(|a| format!("; publish {a}"))
                    .unwrap_or_default()
            ),
            // A measurement that fails is a measurement, not a tunnel: the
            // device still comes up, and a peer with a configured endpoint
            // still works. Refusing to start over it would trade a working
            // tunnel for an unanswered question.
            Err(e) => eprintln!("drt wg: could not measure the mapping: {e}"),
        }
    }
    let (mut device, _allocation) = bind(config).await?;
    eprintln!(
        "drt wg: {} up on port {}, mtu {}{}, public key {}",
        config.interface,
        config.listen_port,
        config.mtu,
        config
            .address
            .as_deref()
            .map(|a| format!(", address {a}"))
            .unwrap_or_else(|| ", no address (set `address`, or ip addr add)".into()),
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
    /// Point a peer at an address. This is the punch: the rendezvous
    /// learned where the far side actually is, and this is how the device
    /// is told. `keepalive` rides along because it is the other half of
    /// the same job — a punched mapping with no traffic through it closes
    /// again in tens of seconds — and two commands to open one hole is
    /// one more chance to send only the first.
    Endpoint {
        public_key: [u8; KEY_LEN],
        endpoint: SocketAddr,
        keepalive: Option<u16>,
    },
    /// Forget where a peer is, so nothing is sent to a stale address.
    /// Spelled `{command = "endpoint", clear = true}`, never as an absent
    /// field: see [`command_from`].
    ClearEndpoint { public_key: [u8; KEY_LEN] },
    /// Set or clear a peer's keepalive interval on its own.
    Keepalive {
        public_key: [u8; KEY_LEN],
        seconds: Option<u16>,
    },
    /// Add a peer the config never named. A rendezvous *discovers* peers —
    /// that is what makes it a rendezvous — so a device that can only be
    /// told about peers it already knows cannot serve one.
    Add {
        public_key: [u8; KEY_LEN],
        allowed_ips: Vec<IpNetwork>,
        endpoint: Option<SocketAddr>,
        keepalive: Option<u16>,
    },
    /// Remove a peer, and with it any route to it.
    Remove { public_key: [u8; KEY_LEN] },
    /// Send through a TURN allocation from now on, taking one first.
    ///
    /// The fallback for a NAT `stun` says cannot be punched: the program
    /// reads `punchable: false` off the mapping, mints a credential with
    /// `crypto/turn_credential`, and hands it here. What comes back is the
    /// relayed address to publish to the rendezvous **instead of** the
    /// measured one. Names no peer: it changes how this device sends to all
    /// of them.
    Relay {
        server: SocketAddr,
        username: String,
        password: String,
        realm: String,
    },
    /// Give the allocation up and send straight out the socket again.
    ClearRelay,
    /// Measure this device's own mapping again, and say what it is now.
    ///
    /// For the machine that moves. `wireguard_endpoint` reports a *peer*
    /// roaming; nothing reported us roaming, so a laptop that went from
    /// home Wi-Fi to a phone hotspot had a published endpoint that was
    /// wrong and no event saying so. This re-runs the same measurement
    /// `start` runs and re-emits `wireguard_mapping`, so a program can
    /// re-join its rendezvous with an address that is true.
    ///
    /// **It costs a rehandshake**, because measuring needs the port back:
    /// see [`drive`]. That is the right trade for the case it exists for
    /// — a device whose network just changed has dead sessions anyway.
    Remap,
}

impl Command {
    /// The peer every command names.
    pub fn public_key(&self) -> [u8; KEY_LEN] {
        match self {
            Command::Endpoint { public_key, .. }
            | Command::ClearEndpoint { public_key }
            | Command::Keepalive { public_key, .. }
            | Command::Add { public_key, .. }
            | Command::Remove { public_key } => *public_key,
            // Not a peer's command: these change this device's own path.
            Command::Relay { .. } | Command::ClearRelay | Command::Remap => [0u8; KEY_LEN],
        }
    }

    /// Whether this command is about the device's own path rather than one
    /// peer, which is what decides where it is carried out.
    pub fn is_relay(&self) -> bool {
        matches!(self, Command::Relay { .. } | Command::ClearRelay)
    }
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
    /// What STUN saw of this device's own port, measured once before the
    /// device bound it. The address a rendezvous should publish, and
    /// whether publishing it is worth anything.
    Mapping(Mapping),
    /// This device is sending through a TURN allocation, and here is the
    /// address to publish. The rendezvous should be told this instead of
    /// whatever `wireguard_mapping` measured, because the measured one is
    /// what could not be punched to.
    ///
    /// `address` is nil when a `relay` command cleared the allocation.
    Relaying {
        address: Option<SocketAddr>,
        server: Option<SocketAddr>,
    },
    /// A command the device would not carry out, and why.
    ///
    /// Reported rather than only logged because the program on the other
    /// end of the reply queue is the one that wrote it, and a supervisor
    /// waiting for a handshake that will never come should learn that its
    /// command was malformed rather than conclude the network is bad.
    Refused { command: String, reason: String },
}

// ---------------------------------------------------------------------------
// depth: the task that owns the device
// ---------------------------------------------------------------------------

/// Own the device: watch it for changes worth reporting, send a full
/// snapshot on the timer, and apply what the deployment asks.
///
/// Two cadences on purpose. Changes — a peer roaming, a handshake landing
/// — are polled every [`WATCH_MS`] and reported as they are seen, because
/// watching a punch means watching for a handshake that either lands
/// within a second or two or never lands at all, and a supervisor that
/// learns at the next ten-second snapshot cannot tell "it worked" from
/// "it worked eventually". Full snapshots stay on `report_ms`, so a quiet
/// tunnel does not fill a queue with the same numbers.
///
/// Generic over the transports so the deployment's kernel-tun device and
/// the tests' channel-tun devices run the *same* loop. Returns when the
/// command channel closes, which is what dropping the bridge does.
pub async fn drive<T: DeviceTransports>(
    device: Device<T>,
    every: Duration,
    reports: mpsc::UnboundedSender<Report>,
    mut commands: mpsc::UnboundedReceiver<Command>,
    allocation: Option<Allocation>,
    config: Option<WireguardConfig>,
) {
    let mut last: Vec<PeerReport> = Vec::new();
    // Whether an allocation is installed, as of the last look. The
    // transport drops one that fails rather than propagating the error
    // (which would end gotatun's receive task for good), so this loop is
    // what notices and tells the deployment it is sending direct again.
    let mut relaying = false;
    let mut watch = tokio::time::interval(Duration::from_millis(WATCH_MS).min(every));
    watch.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut next_snapshot = tokio::time::Instant::now();
    loop {
        // Whether this pass was woken by a command, which is the one case
        // where the program on the other end is waiting to hear back.
        let asked = tokio::select! {
            command = commands.recv() => {
                let Some(command) = command else { return };
                // `remap` asks the device about itself, and it needs the
                // port back to do it, so it happens here where the device
                // can be suspended rather than in `apply`.
                if matches!(command, Command::Remap) {
                    let _ = reports.send(remap(&device, config.as_ref()).await);
                    continue;
                }
                // A relay command is the device's own path, not a peer's, so
                // it is carried out here where the allocation lives rather
                // than in `apply`, which only knows about peers.
                if command.is_relay() {
                    match relay(&allocation, &command).await {
                        Ok(report) => {
                            let _ = reports.send(report);
                        }
                        Err(refusal) => {
                            let _ = reports.send(refusal);
                        }
                    }
                    // Take the new state as the baseline before going
                    // round again. Without this, a `clear` the deployment
                    // ASKED for was reported as a failure on the very next
                    // tick: this branch skips the check below, which then
                    // found `relaying` still true, the allocation gone,
                    // and nothing marking the pass as asked -- so a
                    // program that deliberately stopped relaying was told
                    // its relay had died.
                    if let Some(handle) = &allocation {
                        relaying = handle.borrow().is_some();
                    }
                    continue;
                }
                if let Err(refusal) = apply(&device, &command).await {
                    let _ = reports.send(refusal);
                    continue;
                }
                true
            }
            _ = watch.tick() => false,
        };

        // Before anything else: did the relay go away without being asked?
        // A program waiting on a rendezvous it published a relayed address
        // to should hear that the address is dead, not keep waiting.
        if let Some(handle) = &allocation {
            let installed = handle.borrow().is_some();
            if relaying && !installed && !asked {
                let _ = reports.send(Report::Refused {
                    command: "relay".into(),
                    reason: "the turn allocation failed and was dropped; this device \
                             is sending direct again. Allocate again if the mapping \
                             still cannot be punched."
                        .into(),
                });
            }
            relaying = installed;
        }

        let now = snapshot(&device).await;
        // Roaming carries `previous`, which a snapshot cannot, so it is
        // its own event either way.
        let moved = announce(&reports, &last, &now);
        let due = tokio::time::Instant::now() >= next_snapshot;

        // Three reasons to send the peers: the timer, something notable
        // changed, or a command was just carried out.
        //
        // The timer is unconditional, and that is the whole point of it:
        // `report_ms` is a clock a program can wait on, so it has to tick
        // even when there is nothing to say. A device with no peers at
        // all -- which is exactly the config a rendezvous writes, since
        // it learns its peers later -- has an empty snapshot that never
        // differs from the last one, and gating the timer on a change
        // meant such a device never reported anything at all and a
        // program using `report_ms` as its clock waited forever (issue
        // #15).
        //
        // The other two send EARLY, before the timer is due. `asked` is
        // not a nicety: without it a command that added a peer or set an
        // endpoint would update this loop's idea of the state and emit
        // nothing -- and the next tick, finding nothing changed since,
        // would emit nothing either. The change would stay invisible
        // until something ELSE moved. That is exactly the bug
        // `a_peer_the_config_never_named_can_be_added_and_then_reached`
        // caught: a rendezvous told the device about a peer and the
        // supervisor was never told it had worked.
        if due || (now != last && (moved || asked)) {
            let _ = reports.send(Report::Peers(now.clone()));
        }
        if due {
            next_snapshot = tokio::time::Instant::now() + every;
        }
        last = now;
    }
}

/// depth: wait for the suspended device to actually let its port go.
///
/// Binds and drops, which asks the kernel the only question that matters:
/// is this port free? `measure` then takes it microseconds later — the
/// same hand-off `start` already makes between measuring a port and the
/// device binding it, with the same caveat about it not being the same
/// socket (`doc/WireGuard.md` §2).
#[cfg(feature = "netcheck")]
async fn port_released(port: u16, within_ms: u64) -> bool {
    let deadline = tokio::time::Instant::now() + Duration::from_millis(within_ms);
    loop {
        if std::net::UdpSocket::bind((std::net::Ipv4Addr::UNSPECIFIED, port)).is_ok() {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// depth: give the port back, measure, take it again.
///
/// A mapping belongs to a port, and measuring one means binding that port
/// to ask about it — which the running device is holding. gotatun's
/// `suspend` stops the connection and releases the socket, and `resume`
/// rebinds it, so the sequence is: suspend, measure the freed port,
/// resume. The measurement is therefore exactly the one `start` takes,
/// with exactly the same caveat: it is the probe's socket on that port,
/// not the device's own (`doc/WireGuard.md` §2).
///
/// **It costs a rehandshake.** `resume` resets every peer's session on
/// purpose, so the tunnel comes back with a fresh handshake rather than a
/// stale one. That is the right trade for the case this exists for — a
/// machine whose network just changed has dead sessions already — and the
/// wrong one on a timer, which is why this is a command and not an
/// interval.
///
/// The device is resumed whatever the measurement did. A measurement that
/// failed is a fact to report; a device left suspended over one would be a
/// tunnel lost because a STUN server was down.
async fn remap<T: DeviceTransports>(
    device: &Device<T>,
    config: Option<&WireguardConfig>,
) -> Report {
    let refuse = |reason: String| Report::Refused {
        command: "remap".into(),
        reason,
    };
    let Some(config) = config else {
        return refuse(
            "this device was not started from a config, so there is nothing to \
             measure against."
                .into(),
        );
    };
    if config.stun.is_empty() {
        return refuse(
            "wireguard.stun names no servers, so there is nothing to ask what \
             this device's mapping is. Give two, on separate addresses."
                .into(),
        );
    }
    #[cfg(not(feature = "netcheck"))]
    {
        let _ = device;
        refuse(
            "this build carries no `netcheck`, which is what measures a mapping. \
             A build with `wireguard` and without it can still be told where a \
             peer is; it cannot say where it is itself."
                .into(),
        )
    }
    #[cfg(feature = "netcheck")]
    {
        device.suspend().await;
        // Suspending is not the same as having let go: see
        // [`PORT_RELEASE_MS`]. Measuring before the socket is actually
        // gone fails with "address already in use" every time, which is
        // how this was found.
        let released = port_released(config.listen_port, PORT_RELEASE_MS).await;
        let measured = if released {
            measure(config).await
        } else {
            Err(format!(
                "the device did not let go of port {} within {PORT_RELEASE_MS} ms, \
                 so there was nothing to measure on",
                config.listen_port
            ))
        };
        let resumed = device.resume().await;
        // Resume first in the reporting too: a device that did not come
        // back is a worse fact than a measurement that did not land, and
        // it is the one the program has to act on.
        if let Err(e) = resumed {
            return refuse(format!(
                "the device did not come back up after remeasuring, so this \
                 tunnel is down: {e}. The mapping was {}.",
                match &measured {
                    Ok(m) => format!("measured as {}", m.kind),
                    Err(e) => format!("not measured either: {e}"),
                }
            ));
        }
        match measured {
            Ok(mapping) => Report::Mapping(mapping),
            Err(e) => refuse(format!("could not measure the mapping: {e}")),
        }
    }
}

/// depth: take or drop the TURN allocation every send goes through.
async fn relay(allocation: &Option<Allocation>, command: &Command) -> Result<Report, Report> {
    let refuse = |reason: String| {
        Err(Report::Refused {
            command: "relay".into(),
            reason,
        })
    };
    let Some(handle) = allocation else {
        return refuse(
            "this device cannot relay: set `turn_fallback = true` on the wireguard \
             block. It is off by default because a device that may relay gives up \
             the batched socket read to keep the relayed path from starving."
                .into(),
        );
    };
    match command {
        Command::ClearRelay => {
            handle.send_replace(None);
            Ok(Report::Relaying {
                address: None,
                server: None,
            })
        }
        Command::Relay {
            server,
            username,
            password,
            realm,
        } => match allocate(*server, username, password, realm).await {
            Ok(relayed) => {
                let address = relayed.address;
                handle.send_replace(Some(Arc::new(relayed)));
                Ok(Report::Relaying {
                    address: Some(address),
                    server: Some(*server),
                })
            }
            Err(e) => refuse(e),
        },
        // `is_relay` gates this arm; anything else reaching it is a bug.
        other => refuse(format!("{other:?} is not a relay command; this is a bug")),
    }
}

/// depth: what changed between two snapshots that a program should hear
/// about at once rather than at the next full report. Returns whether
/// anything did.
fn announce(
    reports: &mpsc::UnboundedSender<Report>,
    last: &[PeerReport],
    now: &[PeerReport],
) -> bool {
    let mut notable = false;
    for peer in now {
        let Some(before) = last.iter().find(|p| p.public_key == peer.public_key) else {
            // A peer seen for the first time has not roamed, it has
            // arrived, and the snapshot beside this carries it.
            notable = true;
            continue;
        };
        if before.endpoint != peer.endpoint {
            let _ = reports.send(Report::Roamed {
                public_key: peer.public_key.clone(),
                endpoint: peer.endpoint,
                previous: before.endpoint,
            });
            notable = true;
        }
        // The first handshake with a peer is the moment a punch either
        // worked or did not, so it is an event and not a statistic.
        if before.last_handshake_ms.is_none() && peer.last_handshake_ms.is_some() {
            notable = true;
        }
    }
    // A peer that went away is notable too: something removed it.
    notable || now.len() != last.len()
}

/// depth: one command, applied, or the refusal to report back.
///
/// A command naming a peer the device does not have is a refusal the
/// program hears about — it wrote the command, and a supervisor waiting on
/// a handshake should not have to guess that its key was wrong.
async fn apply<T: DeviceTransports>(device: &Device<T>, command: &Command) -> Result<(), Report> {
    let key = PublicKey::from(command.public_key());
    let named = base64::engine::general_purpose::STANDARD.encode(key.as_bytes());
    let refuse = |what: &str, reason: String| {
        Err(Report::Refused {
            command: what.to_string(),
            reason,
        })
    };
    let unknown = |what: &str| {
        refuse(
            what,
            format!("no peer {named}; add it with `command = \"add\"` first"),
        )
    };
    match command {
        Command::Endpoint {
            endpoint,
            keepalive,
            ..
        } => {
            let known = device
                .write(async |d| {
                    d.modify_peer(&key, |p| {
                        p.set_endpoint(Some(*endpoint));
                        if let Some(seconds) = keepalive {
                            p.set_keepalive(Some(*seconds));
                        }
                    })
                    .await
                })
                .await;
            match known {
                Ok(true) => Ok(()),
                Ok(false) => unknown("endpoint"),
                Err(e) => refuse("endpoint", e.to_string()),
            }
        }
        Command::ClearEndpoint { .. } => {
            let known = device
                .write(async |d| d.modify_peer(&key, |p| p.set_endpoint(None)).await)
                .await;
            match known {
                Ok(true) => Ok(()),
                Ok(false) => unknown("endpoint"),
                Err(e) => refuse("endpoint", e.to_string()),
            }
        }
        Command::Keepalive { seconds, .. } => {
            let known = device
                .write(async |d| d.modify_peer(&key, |p| p.set_keepalive(*seconds)).await)
                .await;
            match known {
                Ok(true) => Ok(()),
                Ok(false) => unknown("keepalive"),
                Err(e) => refuse("keepalive", e.to_string()),
            }
        }
        Command::Add {
            allowed_ips,
            endpoint,
            keepalive,
            ..
        } => {
            let mut peer = Peer::new(key);
            peer.allowed_ips = allowed_ips.clone();
            peer.endpoint = *endpoint;
            peer.keepalive = *keepalive;
            // A device that had NO peers does not route what is added to
            // it later, measured on gotatun 0.9.2: build one with an empty
            // peer list, add a peer, send to its allowed IP, and nothing
            // leaves. Add any peer at build time and the same runtime add
            // works. That is exactly the config a rendezvous writes --
            // `peers = {}`, learn them later -- so it cannot be left to
            // the operator to discover.
            //
            // `suspend`/`resume` is the documented way to have the device
            // rebuild its connection, and it costs nothing here: a device
            // with no peers has no session to tear down.
            let was_empty = device.read(async |d| d.peers().await.is_empty()).await;
            match device.add_peer(peer).await {
                Ok(true) => {
                    if was_empty {
                        device.suspend().await;
                        if let Err(e) = device.resume().await {
                            return refuse("add", format!("the device would not resume: {e}"));
                        }
                    }
                    Ok(())
                }
                // Already present, so make it match what was asked rather
                // than refusing: a rendezvous that re-announces a peer is
                // doing its job, not making a mistake.
                Ok(false) => {
                    let allowed = allowed_ips.clone();
                    let known = device
                        .write(async |d| {
                            d.modify_peer(&key, |p| {
                                p.clear_allowed_ips();
                                p.add_allowed_ips(allowed);
                                if let Some(e) = endpoint {
                                    p.set_endpoint(Some(*e));
                                }
                                if let Some(k) = keepalive {
                                    p.set_keepalive(Some(*k));
                                }
                            })
                            .await
                        })
                        .await;
                    match known {
                        Ok(_) => Ok(()),
                        Err(e) => refuse("add", e.to_string()),
                    }
                }
                Err(e) => refuse("add", e.to_string()),
            }
        }
        Command::Remove { .. } => match device.remove_peer(&key).await {
            Ok(true) => Ok(()),
            Ok(false) => unknown("remove"),
            Err(e) => refuse("remove", e.to_string()),
        },
        // The device's own path, not a peer's. `drive` routes these to
        // `relay` before they reach here, where the allocation lives.
        Command::Relay { .. } | Command::ClearRelay => refuse(
            "relay",
            "a relay command reached the peer handler; this is a bug".into(),
        ),
        Command::Remap => refuse(
            "remap",
            "a remap command reached the peer handler; this is a bug".into(),
        ),
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
    /// Refusals raised while READING the reply queue, which happens on the
    /// drive loop rather than on the device's runtime, so they cannot
    /// travel back through `reports`. Held for the same pass's `report`.
    refusals: Vec<Report>,
    /// One-shot reports the queue would not take, retried on the next
    /// pass ahead of anything newer. See [`WireguardBridge::report`] for
    /// which reports are one-shot and why dropping them was wrong.
    held: Vec<Vec<u8>>,
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
        validate(config)?;
        let secret = private_key(config)?;
        let (report_tx, reports) = mpsc::unbounded_channel();

        // Measured BEFORE the device binds the port, and queued before
        // anything else, because it is what a rendezvous publishes and a
        // program should have it in hand before it trades addresses.
        #[cfg(feature = "netcheck")]
        if !config.stun.is_empty() {
            match rt.block_on(measure(config)) {
                Ok(m) => {
                    eprintln!("drt wg: mapping {} -- {}", m.kind, m.why);
                    let _ = report_tx.send(Report::Mapping(m));
                }
                // A measurement that fails is a measurement, not a tunnel:
                // the device still comes up, and a peer with a configured
                // endpoint still works. Refusing to start over an
                // unanswered question would trade a working tunnel for it.
                Err(e) => {
                    eprintln!("drt wg: could not measure the mapping: {e}");
                    let _ = report_tx.send(Report::Refused {
                        command: "measure".into(),
                        reason: e,
                    });
                }
            }
        }

        let (device, allocation) = rt.block_on(bind(config))?;
        let (commands, command_rx) = mpsc::unbounded_channel();
        let every = Duration::from_millis(config.report_ms.max(1));
        let remap_config = config.clone();
        let runtime = std::thread::spawn(move || {
            rt.block_on(drive(
                device,
                every,
                report_tx,
                command_rx,
                allocation,
                // The config is kept so `remap` can measure against it
                // later: the servers to ask and the port to ask about.
                Some(remap_config),
            ));
            // Leaked, not dropped. `drive` RETURNS when the bridge is
            // dropped, so unlike `stun`'s server this thread reaches the
            // end of its runtime's life -- straight into FM-1
            // (doc/Failure-Modes.md), the tokio 1.53.1 use-after-free in
            // runtime teardown that every foreground verb here leaks
            // around. The deployment is on its way down; the OS reclaims
            // what drop would have.
            std::mem::forget(rt);
        });
        Ok(WireguardBridge {
            reports,
            commands,
            refusals: Vec::new(),
            held: Vec::new(),
            queue: config.queue.clone(),
            reply_queue: config.reply_queue.clone(),
            interface: config.interface.clone(),
            listen_port: config.listen_port,
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
    ///
    /// A refused push means an undeclared or a full queue, and the two
    /// kinds of report want opposite things from it:
    ///
    /// - A **snapshot** (`Peers`) is dropped. The next one carries the
    ///   running totals, so a panel loses an interval and nothing else,
    ///   and holding a stream of them for a queue nobody drains would
    ///   move the deployment's sizing problem into this process.
    /// - Everything else is said **once** -- the mapping, a roam, a relay
    ///   taken or lost, a refusal -- so a drop is that fact gone for
    ///   good. These are held and retried on the next pass, oldest
    ///   first, ahead of anything newer.
    ///
    /// The mapping is what makes this more than tidiness: it is sent on
    /// the bridge's first pass, which is *before* a root program has
    /// reached the line that declares its queue. Under the old blanket
    /// drop, the one report a rendezvous cannot start without was the one
    /// report guaranteed to be lost -- found by running the rendezvous
    /// behind two NATs (issue #15). Same shape as the listener's fix in
    /// issue #11: a fact delivered before the program can hear it is
    /// held, not dropped.
    pub fn report(&mut self, push: &mut dyn FnMut(&str, &[u8]) -> bool) {
        push_reports(
            &self.queue,
            &mut self.held,
            &mut self.refusals,
            &mut self.reports,
            push,
        );
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
                Err(reason) => {
                    eprintln!("drt start: wireguard: {reason}");
                    self.refusals.push(Report::Refused {
                        command: "parse".into(),
                        reason,
                    });
                }
            }
        }
    }
}

/// The push-or-hold decision behind [`WireguardBridge::report`], as a free
/// function so a test can drive it with a channel it owns rather than a
/// bound device and a kernel interface.
///
/// `held` carries the one-shot reports a previous pass could not deliver;
/// they go out first, ahead of anything newer, and go back on the end if
/// they are refused again.
pub fn push_reports(
    queue: &str,
    held: &mut Vec<Vec<u8>>,
    refusals: &mut Vec<Report>,
    reports: &mut mpsc::UnboundedReceiver<Report>,
    push: &mut dyn FnMut(&str, &[u8]) -> bool,
) {
    let mut waiting = std::mem::take(held);
    waiting.extend(
        std::mem::take(refusals)
            .iter()
            .map(|refusal| encode(&report_value(refusal))),
    );
    for msg in waiting {
        if !push(queue, &msg) && held.len() < HELD_MAX {
            held.push(msg);
        }
    }
    while let Ok(report) = reports.try_recv() {
        let msg = encode(&report_value(&report));
        let once = !matches!(report, Report::Peers(_));
        if !push(queue, &msg) && once && held.len() < HELD_MAX {
            held.push(msg);
        }
    }
}

// ---------------------------------------------------------------------------
// The UDP transport, and the TURN allocation it can fall back to
// ---------------------------------------------------------------------------

/// A TURN allocation, live, shared between the device's sender, its receiver
/// and the command that installed it.
///
/// `None` inside the lock means "allowed to relay, not relaying"; the whole
/// thing absent (see [`Transport`]) means the device was not built to relay
/// at all.
/// A `watch` and not a lock, for a reason the first version got wrong: a
/// receiver parked on the direct socket must be *woken* when an allocation
/// appears, or it waits there forever while every packet arrives on the
/// path it is not watching. `watch::Receiver::changed()` is that wake, and
/// it is cancel-safe, which a select arm has to be.
type Allocation = Arc<tokio::sync::watch::Sender<Option<Arc<Relayed>>>>;

/// One TURN allocation and the address it answers at.
///
/// Public only because [`Allocation`] is — the handle has to be nameable by
/// whoever builds a [`Transport`]. Its fields are not: what a caller does
/// with an allocation is install it, and nothing else.
pub struct Relayed {
    conn: Arc<dyn webrtc_util::Conn + Send + Sync>,
    /// The address to publish to a rendezvous. Packets sent here reach this
    /// device through the TURN server.
    address: SocketAddr,
    /// Kept alive because dropping it drops the allocation: the client owns
    /// the refresh loop that keeps the server from reclaiming it.
    _client: turn::client::Client,
}

/// The device's UDP side: gotatun's own socket, plus the option of a TURN
/// allocation beside it.
///
/// **Why wrap rather than replace.** gotatun's `UdpSocketFactory` does
/// `recvmmsg` and GRO on Linux, and a device that gave that up to gain a
/// fallback it never uses would be paying for nothing. So the direct socket
/// is still gotatun's, and a device built without `turn_fallback` delegates
/// every call to it — including the batched receive, which the relaying path
/// cannot use.
///
/// **Why both paths stay live.** With an allocation installed, sends go
/// through it and receives listen on *both* it and the direct socket. That
/// is ICE's own shape: the direct path is not torn down when a relayed one
/// appears, so a peer that later becomes reachable directly — it roamed, or
/// its NAT let go — is still heard.
pub struct Transport {
    allocation: Option<Allocation>,
}

impl Transport {
    /// A transport for a device that may be asked to relay, or one that may
    /// not. The handle is what the command later installs an allocation
    /// into; there is nothing to install into for a device built without.
    pub fn new(relayable: bool) -> (Transport, Option<Allocation>) {
        let allocation = relayable.then(|| Arc::new(tokio::sync::watch::Sender::new(None)));
        (
            Transport {
                allocation: allocation.clone(),
            },
            allocation,
        )
    }
}

type DirectSend = <gotatun::udp::socket::UdpSocketFactory as UdpTransportFactory>::Send;
type DirectRecv = <gotatun::udp::socket::UdpSocketFactory as UdpTransportFactory>::Recv;

impl UdpTransportFactory for Transport {
    type Send = Sender;
    type Recv = Receiver;

    async fn bind(
        &mut self,
        params: &gotatun::udp::UdpTransportFactoryParams,
    ) -> std::io::Result<(Sender, Receiver)> {
        let (direct_tx, direct_rx) = gotatun::udp::socket::UdpSocketFactory::default()
            .bind(params)
            .await?;
        Ok((
            Sender {
                direct: direct_tx,
                allocation: self.allocation.clone(),
            },
            Receiver {
                direct: direct_rx,
                allocation: self.allocation.as_ref().map(|a| a.subscribe()),
            },
        ))
    }
}

/// Outbound. Through the allocation when there is one, straight out the
/// socket when there is not.
#[derive(Clone)]
pub struct Sender {
    direct: DirectSend,
    allocation: Option<Allocation>,
}

impl UdpSend for Sender {
    type SendManyBuf = <DirectSend as UdpSend>::SendManyBuf;

    async fn send_to(&self, packet: Packet, destination: SocketAddr) -> std::io::Result<()> {
        if let Some(handle) = &self.allocation {
            // `borrow` and not a lock await: the send path should not queue
            // behind whoever is installing an allocation.
            let relayed = handle.borrow().clone();
            if let Some(relayed) = relayed {
                // The TURN client installs the permission for `destination`
                // on the way past, so the peer's reply is allowed back.
                if let Err(e) = relayed.conn.send_to(&packet, destination).await {
                    // Drop the allocation, and drop this packet rather than
                    // report it: WireGuard retransmits, and by the time it
                    // does the sends are direct again. Returning an error
                    // would be worse than useless -- gotatun ignores most
                    // send errors and breaks a loop on the rest.
                    eprintln!("drt wg: the turn allocation failed ({e}); sending direct again");
                    handle.send_replace(None);
                }
                return Ok(());
            }
        }
        self.direct.send_to(packet, destination).await
    }

    fn local_addr(&self) -> std::io::Result<Option<SocketAddr>> {
        // The DIRECT address, always, even while relaying: this is what the
        // device reports as its own port, and the relayed address is a
        // separate fact the deployment is told about by name
        // (`wireguard_relay`) precisely because it is not this.
        self.direct.local_addr().map(Some)
    }
}

/// Inbound, from either path.
pub struct Receiver {
    direct: DirectRecv,
    allocation: Option<tokio::sync::watch::Receiver<Option<Arc<Relayed>>>>,
}

impl UdpRecv for Receiver {
    type RecvManyBuf = <DirectRecv as UdpRecv>::RecvManyBuf;

    async fn recv_from(
        &mut self,
        pool: &mut PacketBufPool,
    ) -> std::io::Result<(Packet, SocketAddr)> {
        let Some(watch) = self.allocation.as_mut() else {
            return self.direct.recv_from(pool).await;
        };
        loop {
            // Re-read on every pass, and watch for a change on every pass.
            // The first version read once and then parked on whichever path
            // existed at that moment: with no allocation yet it waited on
            // the direct socket, an allocation arrived, and every packet
            // after that came back through the relay — to a receiver that
            // was not listening to it and never would be. Measured as a
            // handshake B answered and A never heard.
            let relayed = watch.borrow_and_update().clone();
            let mut relayed_buf = pool.get();
            match relayed {
                None => {
                    tokio::select! {
                        direct = self.direct.recv_from(pool) => return direct,
                        // An allocation appeared: go round and listen to it.
                        _ = watch.changed() => continue,
                    }
                }
                Some(relayed) => {
                    tokio::select! {
                        direct = self.direct.recv_from(pool) => return direct,
                        got = relayed.conn.recv_from(&mut relayed_buf) => {
                            let (n, from) = got.map_err(|e| {
                                std::io::Error::other(format!("turn relay: {e}"))
                            })?;
                            relayed_buf.truncate(n);
                            return Ok((relayed_buf, from));
                        }
                        // Cleared, or replaced: re-read rather than keep
                        // reading from an allocation that is gone.
                        _ = watch.changed() => continue,
                    }
                }
            }
        }
    }

    async fn recv_many_from(
        &mut self,
        recv_buf: &mut Self::RecvManyBuf,
        pool: &mut PacketBufPool,
        packets: &mut Vec<(Packet, SocketAddr)>,
    ) -> std::io::Result<()> {
        // A device that cannot relay keeps gotatun's batched read, which is
        // the whole reason this wraps the socket instead of replacing it.
        // One that can gives it up: a `recvmmsg` parked on the socket would
        // starve the relayed path for as long as it waits.
        if self.allocation.is_none() {
            return self.direct.recv_many_from(recv_buf, pool, packets).await;
        }
        let (packet, from) = self.recv_from(pool).await?;
        packets.push((packet, from));
        Ok(())
    }

    fn enable_udp_gro(&self) -> std::io::Result<()> {
        self.direct.enable_udp_gro()
    }
}

/// Take an allocation on `server` with the credential the deployment minted,
/// and answer with the address to publish.
///
/// The credential is not this block's to invent: `crypto/turn_credential`
/// mints one under a secret the guest cannot read, and the program hands it
/// here. So a deployment that relays through someone else's TURN server
/// needs no secret in its `wireguard` block, and one that relays through its
/// own shares exactly the secret it already shares.
async fn allocate(
    server: SocketAddr,
    username: &str,
    password: &str,
    realm: &str,
) -> Result<Relayed, String> {
    let socket = tokio::net::UdpSocket::bind("0.0.0.0:0")
        .await
        .map_err(|e| format!("a socket for the turn client: {e}"))?;
    let client = turn::client::Client::new(turn::client::ClientConfig {
        stun_serv_addr: server.to_string(),
        turn_serv_addr: server.to_string(),
        username: username.to_string(),
        password: password.to_string(),
        realm: realm.to_string(),
        software: String::new(),
        rto_in_ms: 0,
        conn: Arc::new(socket),
        vnet: None,
    })
    .await
    .map_err(|e| format!("turn client for {server}: {e}"))?;
    client
        .listen()
        .await
        .map_err(|e| format!("turn client for {server}: {e}"))?;
    let conn = client
        .allocate()
        .await
        .map_err(|e| format!("turn allocation on {server}: {e}"))?;
    let address = conn
        .local_addr()
        .map_err(|e| format!("turn allocation on {server} has no address: {e}"))?;
    Ok(Relayed {
        conn: Arc::new(conn),
        address,
        _client: client,
    })
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
        Report::Mapping(m) => rmpv::Value::Map(vec![
            ("event".into(), "wireguard_mapping".into()),
            ("mapping".into(), m.kind.into()),
            // The one field a rendezvous needs, and nil when there is
            // nothing worth publishing -- a symmetric mapping has no
            // address a peer could use.
            ("address".into(), addr_value(m.address)),
            ("punchable".into(), rmpv::Value::Boolean(m.punchable)),
            ("why".into(), m.why.as_str().into()),
            // Always a list, never nil, so a program iterates it without
            // a guard: empty is "this machine has no route", which is a
            // fact and not an absence.
            (
                "local".into(),
                rmpv::Value::Array(m.local.iter().map(|a| a.to_string().into()).collect()),
            ),
        ]),
        Report::Relaying { address, server } => rmpv::Value::Map(vec![
            ("event".into(), "wireguard_relay".into()),
            // The address to publish, in place of the measured one.
            ("address".into(), addr_value(*address)),
            ("server".into(), addr_value(*server)),
        ]),
        Report::Refused { command, reason } => rmpv::Value::Map(vec![
            ("event".into(), "wireguard_error".into()),
            ("command".into(), command.as_str().into()),
            ("reason".into(), reason.as_str().into()),
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
///
/// **An absent `endpoint` is an error, not "clear it".** The first version
/// of this read a missing field as a request to forget where the peer is,
/// on the reasoning that a Lua table cannot hold a nil value. That made a
/// typo in the field name — `endpiont`, `Endpoint` — tear down a working
/// tunnel silently, which is a far worse failure than the one it avoided.
/// Clearing is now `{command = "endpoint", clear = true}`, said out loud.
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
    // `remap` names no peer and carries no field at all: it asks this
    // device about itself.
    if name == "remap" {
        return Ok(Command::Remap);
    }
    // `relay` is the other command that names no peer -- it changes how
    // this device sends to all of them -- so it is read before the key
    // every other command requires.
    if name == "relay" {
        if field("clear").and_then(|v| v.as_bool()).unwrap_or(false) {
            return Ok(Command::ClearRelay);
        }
        let text = field("server")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "relay: no `server`; name the TURN server as host:port".to_string())?;
        let server = text.parse::<SocketAddr>().map_err(|e| {
            format!(
                "relay.server: '{text}' is not a host:port ({e}). An address, not \
                     a hostname: this is the fallback path and it should not depend \
                     on a resolver that may be what is broken."
            )
        })?;
        let want = |k: &str| {
            field(k)
                .and_then(|v| v.as_str())
                .map(str::to_string)
                .ok_or_else(|| format!("relay: no `{k}`; mint one with crypto/turn_credential"))
        };
        return Ok(Command::Relay {
            server,
            username: want("username")?,
            password: want("password")?,
            realm: field("realm")
                .and_then(|v| v.as_str())
                .unwrap_or("drt")
                .to_string(),
        });
    }
    let key_text = field("public_key")
        .and_then(|v| v.as_str())
        .ok_or_else(|| format!("{name}: no `public_key`; it is the only name a peer has"))?;
    let public_key = parse_key(&format!("{name}.public_key"), key_text)?;
    let keepalive = field("keepalive")
        .and_then(|v| v.as_u64())
        .map(|s| s as u16);
    let endpoint = match field("endpoint").and_then(|v| v.as_str()) {
        Some(text) => Some(text.parse::<SocketAddr>().map_err(|e| {
            format!(
                "{name}.endpoint: '{text}' is not a host:port ({e}). An address, \
                     never a hostname: this is the far side of a punch, and the \
                     rendezvous measured a number."
            )
        })?),
        None => None,
    };
    let clearing = field("clear").and_then(|v| v.as_bool()).unwrap_or(false);
    match name {
        "endpoint" => match (clearing, endpoint) {
            (true, _) => Ok(Command::ClearEndpoint { public_key }),
            (false, Some(endpoint)) => Ok(Command::Endpoint {
                public_key,
                endpoint,
                keepalive,
            }),
            (false, None) => Err("endpoint: no `endpoint`. To forget where a peer is, say \
                                  so: {command = \"endpoint\", public_key = ..., \
                                  clear = true}. An absent field is not read as a \
                                  request to clear, because that makes a mistyped \
                                  field name tear down a working tunnel."
                .to_string()),
        },
        "keepalive" => Ok(Command::Keepalive {
            public_key,
            seconds: keepalive,
        }),
        "add" => {
            let mut allowed_ips = Vec::new();
            for cidr in field("allowed_ips")
                .and_then(|v| v.as_array())
                .ok_or_else(|| {
                    "add: no `allowed_ips`. A peer with none can neither be routed \
                     to nor accepted from, so adding one would add nothing."
                        .to_string()
                })?
            {
                let text = cidr
                    .as_str()
                    .ok_or_else(|| "add.allowed_ips: each entry is a CIDR string".to_string())?;
                allowed_ips
                    .push(text.parse::<IpNetwork>().map_err(|e| {
                        format!("add.allowed_ips: '{text}' is not a network ({e})")
                    })?);
            }
            if allowed_ips.is_empty() {
                return Err("add.allowed_ips: empty; a peer with no allowed IPs does \
                            nothing"
                    .into());
            }
            Ok(Command::Add {
                public_key,
                allowed_ips,
                endpoint,
                keepalive,
            })
        }
        "remove" => Ok(Command::Remove { public_key }),
        // Unreachable: the name was checked against COMMAND above, and
        // this match covers it. Kept as a refusal rather than a panic so
        // that adding a name to COMMAND and forgetting an arm is a message
        // on a queue and not a dead deployment.
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
