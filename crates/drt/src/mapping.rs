//! The UDP mapping of one local port, measured against two STUN servers,
//! and this machine's own addresses: what `wireguard_mapping` reports and
//! what `drt netcheck`'s deprecated flags measure. Kept apart from both, so
//! WireGuard keeps it when the verdict tree leaves drt.
//!
//! ## surface block
//!
//! - Entry points: [`udp_mapping`], [`local_addresses`],
//!   [`source_toward`].
//! - Configurable: none.
//! - Fan-out: [`UdpMapping`], the three kinds.

#[cfg(feature = "stun")]
use ego_transport::stun::{detect_mapping, NatMapping, ProbeConfig};
#[cfg(feature = "stun")]
use std::net::{IpAddr, Ipv4Addr, SocketAddr};

/// How a NAT assigns UDP mappings, as observed across two STUN servers.
///
/// Mirrors `ego_transport::stun::NatMapping` rather than re-using it so
/// that `netcheck`'s verdict table -- and every fixture in its tests --
/// stays compilable without the `stun` feature. [`udp_mapping`] converts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UdpMapping {
    /// No NAT in the path: the socket's own address is what the world sees.
    Open,
    /// One mapping reused for every destination. Punchable.
    Independent,
    /// A fresh mapping per destination ("symmetric"). What a STUN server
    /// reports says nothing about what a peer would see.
    Symmetric,
}

// depth: measuring

/// The reflexive address every probe agreed on, or `None`.
///
/// STUN's entire job is telling a caller the address the world sees, and
/// this was being thrown away: only `.port()` was kept, `observed_address`
/// stayed `None`, and the evidence line said "no reflect edge answered"
/// while a STUN server had just answered exactly that question.
///
/// That mattered far more than a missing line. [`Measurements::is_cgnat`]
/// reads `observed_address`, and the CGNAT rule is the one that outranks
/// every other — so in the only configuration this build supports, the
/// highest-priority rule in the table could never fire, and a machine
/// behind a carrier NAT was told `punchable`.
///
/// **Only when every probe agrees.** Two servers reporting different
/// addresses means different egress paths, and picking one would be a
/// guess about which. This module does not guess.
#[cfg(feature = "stun")]
fn agreed_address(report: &ego_transport::stun::MappingReport) -> Option<IpAddr> {
    let mut seen = report.probes.iter().map(|p| p.reflexive.ip());
    let first = seen.next()?;
    seen.all(|a| a == first).then_some(first)
}

/// Ask two or more STUN servers what they see of **one** socket, and
/// classify the mapping.
///
/// One socket for every probe is the whole point: two sockets would
/// have different mappings under any NAT and the comparison would mean
/// nothing. `detect_mapping` owns that discipline, and refuses below two
/// servers rather than guessing — so a caller that supplies one gets an
/// error here and "not measured" in the evidence, never a confident
/// wrong answer.
///
/// `udp_port` binds the probe socket to a chosen local port. A mapping
/// is a fact about one flow: measured from an ephemeral port, the
/// mapped port reported is not the one `udp/51820` will get, and on any
/// NAT that is not port-preserving the verdict is right about the
/// network and wrong about the flow that matters (discofetch
/// `DRT_ASKS.md` §2). A port that cannot be bound is a refusal naming
/// it, never a silent fall back to ephemeral -- that would be the same
/// wrong answer with a confident face.
#[cfg(feature = "stun")]
pub async fn udp_mapping(
    servers: &[&str],
    udp_port: Option<u16>,
) -> Result<(UdpMapping, Vec<(String, u16)>, Option<IpAddr>), String> {
    if servers.len() < 2 {
        return Err(format!(
            "classifying a NAT mapping needs two servers on separate addresses; {} given",
            servers.len()
        ));
    }
    let config = ProbeConfig {
        bind_addr: SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), udp_port.unwrap_or(0)),
        ..ProbeConfig::default()
    };
    let report = detect_mapping(servers, &config)
        .await
        .map_err(|e| match udp_port {
            Some(port) => format!("--udp-port {port}: {e}"),
            None => e.to_string(),
        })?;
    let mapping = match report.mapping {
        NatMapping::Open => UdpMapping::Open,
        NatMapping::EndpointIndependent => UdpMapping::Independent,
        NatMapping::EndpointDependent => UdpMapping::Symmetric,
    };
    // Pair each server with the port it reported, in the order supplied,
    // because the evidence line names them and an unlabelled pair of
    // numbers settles no argument.
    let ports = servers
        .iter()
        .zip(report.probes.iter())
        .map(|(s, p)| ((*s).to_string(), p.reflexive.port()))
        .collect();
    Ok((mapping, ports, agreed_address(&report)))
}

/// The source address the routing table would pick toward `dest`.
///
/// A connected UDP socket sends nothing; `connect()` only makes the
/// kernel choose, so this is a local operation and not a probe.
/// `dest` decides the family. `None` when there is no route at all,
/// which an offline machine answers honestly rather than with
/// loopback.
pub fn source_toward(dest: std::net::IpAddr) -> Option<std::net::IpAddr> {
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, UdpSocket};
    let unspecified: IpAddr = match dest {
        IpAddr::V4(_) => Ipv4Addr::UNSPECIFIED.into(),
        IpAddr::V6(_) => Ipv6Addr::UNSPECIFIED.into(),
    };
    let sock = UdpSocket::bind(SocketAddr::new(unspecified, 0)).ok()?;
    // 8.8.8.8 and 2001:4860:4860::8888 are well-known public
    // destinations. Nothing is sent to either.
    sock.connect(SocketAddr::new(dest, 53)).ok()?;
    let local = sock.local_addr().ok()?.ip();
    (!local.is_loopback() && !local.is_unspecified()).then_some(local)
}

/// This machine's own addresses, one per family, as a peer on the same
/// network would reach them (issue #25).
///
/// The one candidate a mapping report could not carry. `wireguard_mapping`
/// publishes the server-reflexive address -- what a STUN server saw --
/// and two machines behind one router then have to hairpin through it,
/// which plenty of routers refuse; so two machines on one LAN could not
/// punch to each other from the report alone. A host candidate is what
/// ICE uses for exactly that, and a guest cannot learn one on its own:
/// no sockets, no `net`, an `fs` scope of one directory.
///
/// Two addresses and not a list, on purpose. The address the routing
/// table picks toward the internet *is* the one a same-LAN peer reaches,
/// so the primary per family covers the case that was filed, and a
/// dual-stack home roughly doubles the coverage for no interface
/// enumeration and no new dependency. Multi-homed machines would want
/// `getifaddrs`; that arrives with evidence they are common, not before.
/// Raw: private, link-scoped, whatever the table answers. Whether to
/// publish a LAN address at all is a program's decision, since it
/// discloses topology.
pub fn local_addresses() -> Vec<std::net::IpAddr> {
    ["8.8.8.8", "2001:4860:4860::8888"]
        .iter()
        .filter_map(|d| source_toward(d.parse().ok()?))
        .collect()
}
