//! `drt netcheck`: what can this network do, and what should you do about it.
//!
//! Implements discofetch's `doc/NETCHECK-SPEC.md`. The design constraint
//! from that spec governs everything here: **the output is a verdict, not a
//! report.** A tool that prints observations makes the user the expert. So
//! every measurement below exists to select one of four verdicts and to
//! justify it, and a measurement that cannot change the verdict is not
//! taken.
//!
//! ## The split, and why it is the whole file
//!
//! [`Measurements`] is data. [`decide`] is a pure function over it, driven
//! by [`RULES`] — a table, not nested branches. The network lives in
//! [`gather`] and nowhere else.
//!
//! That split is not tidiness. The verdict tree is the part that will be
//! wrong first: real home networks will surprise us, and being wrong here
//! means confidently telling someone to forward a port that will never
//! answer. A table is edited and re-tested; control flow is argued about.
//! It also keeps the eventual move of the tree into `.dlua` (discofetch's
//! `doc/NETCHECK.md` proposes it, so the tree can be corrected without
//! cutting a DRT release) a port rather than a rewrite. We do not build
//! for that move; we decline to build against it.
//!
//! ## The one place this is easy to get wrong
//!
//! **Punchability is a UDP question.** A NAT can be endpoint-independent
//! for TCP and symmetric for UDP, and it is the UDP behaviour that decides
//! whether a hole punch lands. So the UDP mapping — two STUN servers, one
//! socket, via `ego_transport::stun::detect_mapping` — is decisive, and the
//! TCP mapping read from two reflect edges is informational only. A verdict
//! built on the TCP half would be confidently wrong on exactly the networks
//! where this matters most. `tcp_independent_udp_symmetric` in the tests
//! below is that case, and it is real.

use std::fmt;
use std::net::IpAddr;

/// What this network is, and what to do about it. Exactly one is emitted.
///
/// Ordered best-day-first, which is also the order the rule table is
/// searched: the first rule that matches wins, so a more specific verdict
/// must sit above a more general one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// A public address reaches this machine inbound.
    Direct,
    /// A routable v6 address answers and v4 is hopeless. Different advice,
    /// and an increasingly common situation.
    V6Direct,
    /// No inbound, but the UDP mapping is endpoint-independent, so a
    /// rendezvous gets peers a direct path.
    Punchable,
    /// Symmetric mapping or CGNAT. Nothing punches; traffic is relayed.
    Relay,
}

impl Verdict {
    /// The short name, which is also what `--json` emits and what a script
    /// matches on.
    pub fn name(self) -> &'static str {
        match self {
            Verdict::Direct => "direct",
            Verdict::V6Direct => "v6-direct",
            Verdict::Punchable => "punchable",
            Verdict::Relay => "relay",
        }
    }

    /// The sentence of advice. One sentence, in the second person, naming
    /// the next action rather than the network's taxonomy — the person
    /// reading this wants to know what to do.
    pub fn advice(self) -> &'static str {
        match self {
            Verdict::Direct => "point a name at this address and forward the port",
            Verdict::V6Direct => "use the IPv6 address; this network's IPv4 has no inbound path",
            Verdict::Punchable => "use a rendezvous; peers will connect to you directly",
            Verdict::Relay => "use a tunnel",
        }
    }
}

impl fmt::Display for Verdict {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// How a NAT assigns UDP mappings, as observed across two STUN servers.
///
/// Mirrors `ego_transport::stun::NatMapping` rather than re-using it so
/// that this module — and every fixture in the tests — stays compilable
/// without the `stun` feature. [`gather`] converts.
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

/// The result of asking an edge to connect back to the caller's own
/// observed address.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Inbound {
    Connected,
    Refused,
    Timeout,
}

/// One edge's view of the caller: which vantage answered, and the source
/// port it terminated.
///
/// `port` is `Option` on purpose. `x-real-port` may be absent — an edge
/// that has not rolled the header out, or a path that never had one — and
/// the spec is explicit that absence means *not measured*, never zero. A
/// zero here would read as a real port and produce a wrong comparison.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EdgeView {
    pub edge: String,
    pub port: Option<u16>,
}

/// Everything measured, and nothing derived. `decide` reads only this.
///
/// Every field is optional or explicitly-absent-able because a diagnostic
/// that refuses to answer when one probe fails is a diagnostic nobody runs.
/// The rules below are written to degrade rather than abstain.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Measurements {
    /// The v4 address the edges observed, if any answered.
    pub observed_address: Option<IpAddr>,
    /// A routable v6 address on a local interface — not link-local
    /// (`fe80::/10`), not ULA (`fc00::/7`).
    pub routable_v6: Option<IpAddr>,
    /// UDP mapping across two STUN servers. The decisive measurement.
    pub udp_mapping: Option<UdpMapping>,
    /// The mapped port each STUN server reported, for the evidence block.
    pub udp_ports: Vec<(String, u16)>,
    /// What each reflect edge saw. Informational: the TCP half.
    pub tcp_views: Vec<EdgeView>,
    /// The inbound test. Filled by a caller that can arrange an
    /// unsolicited inbound connect; nothing in this build can, so it is
    /// always `None` here and the renderer says "not measured" rather
    /// than naming a flag the binary does not accept.
    pub inbound: Option<(u16, Inbound)>,
}

impl Measurements {
    /// Whether the observed address is inside CGNAT space (`100.64.0.0/10`).
    ///
    /// This short-circuits to `relay` regardless of mapping behaviour: a
    /// carrier-grade NAT can present an endpoint-independent mapping and
    /// still give you no way to be reached, because the address is shared
    /// and you do not control the box that owns it.
    pub fn is_cgnat(&self) -> bool {
        match self.observed_address {
            Some(IpAddr::V4(v4)) => {
                let o = v4.octets();
                o[0] == 100 && (64..=127).contains(&o[1])
            }
            _ => false,
        }
    }

    /// Whether IPv4 was **measured** and found to have no inbound path —
    /// the precondition for advising v6 instead.
    ///
    /// The distinction this exists to make: an address that came back and
    /// is not publicly reachable is a finding; an address that never came
    /// back is not. `has_public_v4()` cannot tell them apart, because
    /// `None` and "a private address" both answer `false`, and reading the
    /// first as the second is how an unmeasured network got the tree's most
    /// specific verdict.
    pub fn v4_ruled_out(&self) -> bool {
        // Either an address came back and is unreachable, or the mapping
        // came back symmetric — both are measurements. Neither being
        // present means nothing is known and nothing should be claimed.
        (self.observed_address.is_some() && !self.has_public_v4())
            || matches!(self.udp_mapping, Some(UdpMapping::Symmetric))
    }

    /// Whether the observed v4 address is a public one — not CGNAT, not
    /// RFC1918, not loopback or link-local. **`false` also when nothing was
    /// observed**, which is why [`v4_ruled_out`](Self::v4_ruled_out) exists
    /// and why a rule must not ask this question on its own.
    pub fn has_public_v4(&self) -> bool {
        match self.observed_address {
            Some(IpAddr::V4(v4)) => {
                !self.is_cgnat()
                    && !v4.is_private()
                    && !v4.is_loopback()
                    && !v4.is_link_local()
                    && !v4.is_unspecified()
            }
            _ => false,
        }
    }

    /// Whether the TCP mapping agrees across the edges that answered with a
    /// port. `None` when fewer than two did — which is the state until a
    /// second gate exists, and is reported as "not measured" rather than
    /// guessed.
    pub fn tcp_agrees(&self) -> Option<bool> {
        let ports: Vec<u16> = self.tcp_views.iter().filter_map(|v| v.port).collect();
        if ports.len() < 2 {
            return None;
        }
        Some(ports.windows(2).all(|w| w[0] == w[1]))
    }
}

/// One row of the verdict table: a predicate, the verdict it selects, and
/// the sentence that justifies it.
///
/// `why` is not decoration. The day someone disputes the answer, "symmetric
/// NAT: 51823→51823 via gate1, 51823→40119 via gate2" is the sentence that
/// ends the argument, and a verdict that cannot say why it was chosen is a
/// verdict nobody trusts twice.
pub struct Rule {
    pub verdict: Verdict,
    pub why: &'static str,
    pub matches: fn(&Measurements) -> bool,
}

/// The verdict tree, as a table. **First match wins**, so order is part of
/// the logic and each rule may assume every rule above it failed.
pub static RULES: &[Rule] = &[
    // CGNAT first, above everything. A carrier NAT can look
    // endpoint-independent and still be unreachable and unforwardable, so
    // this must outrank both the inbound test and the mapping.
    Rule {
        verdict: Verdict::Relay,
        why: "the observed address is inside CGNAT space (100.64.0.0/10), so there is no address to be reached at and no router of yours to forward it",
        matches: |m| m.is_cgnat(),
    },
    // A connection that actually arrived outranks every inference about
    // what might arrive. Guarded on a public address so that a
    // `connected` observed against an RFC1918 address — which means the
    // prober and the caller share a network — cannot be read as reachable
    // from the internet.
    Rule {
        verdict: Verdict::Direct,
        why: "an edge connected inbound to the observed address",
        matches: |m| {
            m.has_public_v4() && matches!(m.inbound, Some((_, Inbound::Connected)))
        },
    },
    // The decisive UDP read, and it outranks v6. Symmetric before
    // independent, because symmetric is the one that forecloses on hole
    // punching.
    Rule {
        verdict: Verdict::Punchable,
        why: "the UDP mapping is endpoint-independent, so the address a STUN server sees is the address a peer can reach",
        matches: |m| {
            matches!(
                m.udp_mapping,
                Some(UdpMapping::Independent) | Some(UdpMapping::Open)
            )
        },
    },
    // v6 sits BELOW punchable and ABOVE relay-by-symmetric, which is the
    // only place it belongs, and it was above both until a real network
    // said otherwise.
    //
    // On a machine with a routable v6 and no reflect edge, the old rule
    // answered `v6-direct` while the UDP mapping said `independent` --
    // an inference overriding a measurement, in a module whose first
    // premise is that it does not do that. `routable_v6` reads the routing
    // table and sends nothing (see `gather`), so v6 reachability is never
    // measured here at all; a routable address behind a v6 firewall is
    // common on consumer gear. Punching over v4 was *measured* and works.
    //
    // The second half is `v4_ruled_out`. The old rule asked
    // `!has_public_v4()`, which is false when `observed_address` is `None`
    // -- so "no edge answered" was read as "v4 is hopeless", and the module
    // gave its most specific verdict on a network it had measured nothing
    // about. Not measured is not a finding. With nothing measured this now
    // falls through to relay, which is the answer that works everywhere.
    Rule {
        verdict: Verdict::V6Direct,
        why: "a routable IPv6 address is present and IPv4 has no inbound path",
        matches: |m| {
            m.routable_v6.is_some()
                && m.v4_ruled_out()
                && !matches!(m.inbound, Some((_, Inbound::Connected)))
        },
    },
    Rule {
        verdict: Verdict::Relay,
        why: "the UDP mapping is per-destination (symmetric), so what a STUN server sees says nothing about what a peer would see",
        matches: |m| matches!(m.udp_mapping, Some(UdpMapping::Symmetric)),
    },
];

/// The fallback when no rule matches — which happens when the UDP mapping
/// could not be measured at all.
///
/// `relay` is the right default because it is the verdict that always
/// works: following it on a network that could have punched costs a relay
/// hop, while the opposite mistake costs a connection that never forms. The
/// reason says the measurement is missing rather than claiming a finding.
const UNMEASURED: (Verdict, &str) = (
    Verdict::Relay,
    "the UDP mapping could not be measured, and relay is the answer that works on every network",
);

/// The verdict and the sentence that justifies it. Pure: no clock, no
/// network, no environment.
pub fn decide(m: &Measurements) -> (Verdict, &'static str) {
    for rule in RULES {
        if (rule.matches)(m) {
            return (rule.verdict, rule.why);
        }
    }
    UNMEASURED
}

/// The human rendering: verdict, one sentence of advice, then the evidence.
///
/// Evidence lines are emitted for measurements that were *not* taken too —
/// "not measured" is a finding, and a silently missing line reads as a
/// measurement that passed.
pub fn render_text(m: &Measurements, verdict: Verdict, why: &'static str) -> String {
    let mut out = String::new();
    out.push_str(&format!("{} — {}\n", verdict, why));
    out.push_str(&format!("  use: {}\n\n", verdict.advice()));
    out.push_str("evidence\n");

    match m.observed_address {
        Some(a) => out.push_str(&format!(
            "  address    {}{}\n",
            a,
            if m.is_cgnat() { " (CGNAT)" } else { "" }
        )),
        None => out.push_str("  address    not measured (no reflect edge answered)\n"),
    }

    match m.routable_v6 {
        Some(a) => out.push_str(&format!("  v6         {a}\n")),
        None => out.push_str("  v6         none routable\n"),
    }

    if m.udp_ports.is_empty() {
        out.push_str("  udp map    not measured\n");
    } else {
        let pairs: Vec<String> = m
            .udp_ports
            .iter()
            .map(|(s, p)| format!("{p} ({s})"))
            .collect();
        let label = match m.udp_mapping {
            Some(UdpMapping::Symmetric) => "  SYMMETRIC",
            Some(UdpMapping::Independent) => "  independent",
            Some(UdpMapping::Open) => "  open",
            None => "",
        };
        out.push_str(&format!("  udp map    {}{}\n", pairs.join(", "), label));
    }

    if m.tcp_views.is_empty() {
        out.push_str("  tcp map    not measured\n");
    } else {
        let pairs: Vec<String> = m
            .tcp_views
            .iter()
            .map(|v| match v.port {
                Some(p) => format!("{p} ({})", v.edge),
                None => format!("absent ({})", v.edge),
            })
            .collect();
        let label = match m.tcp_agrees() {
            Some(true) => "  independent",
            Some(false) => "  per-destination",
            None => "  (one vantage; not a comparison)",
        };
        out.push_str(&format!("  tcp map    {}{}\n", pairs.join(", "), label));
    }

    match m.inbound {
        Some((port, r)) => out.push_str(&format!(
            "  inbound    port {}: {}\n",
            port,
            match r {
                Inbound::Connected => "connected",
                Inbound::Refused => "refused",
                Inbound::Timeout => "timeout",
            }
        )),
        None => out.push_str("  inbound    not measured (no inbound test in this build)\n"),
    }

    out
}

/// Taking the measurements. The only part of this module that touches a
/// network or the machine, which is what keeps [`decide`] testable.
///
/// Gated on `stun` because the decisive measurement is
/// `ego_transport::stun::detect_mapping`. The verdict table above compiles
/// and is tested without it.
#[cfg(feature = "stun")]
pub mod gather {
    use super::{Measurements, UdpMapping};
    use ego_transport::stun::{detect_mapping, NatMapping, ProbeConfig};

    /// Ask two or more STUN servers what they see of **one** socket, and
    /// classify the mapping.
    ///
    /// One socket for every probe is the whole point: two sockets would
    /// have different mappings under any NAT and the comparison would mean
    /// nothing. `detect_mapping` owns that discipline, and refuses below two
    /// servers rather than guessing — so a caller that supplies one gets an
    /// error here and "not measured" in the evidence, never a confident
    /// wrong answer.
    pub async fn udp_mapping(servers: &[&str]) -> Result<(UdpMapping, Vec<(String, u16)>), String> {
        if servers.len() < 2 {
            return Err(format!(
                "classifying a NAT mapping needs two servers on separate addresses; {} given",
                servers.len()
            ));
        }
        let report = detect_mapping(servers, &ProbeConfig::default())
            .await
            .map_err(|e| e.to_string())?;
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
        Ok((mapping, ports))
    }

    /// A routable IPv6 address on a local interface, if there is one.
    ///
    /// "Routable" excludes link-local (`fe80::/10`), ULA (`fc00::/7`) and
    /// loopback: none of them is an address a peer on the internet can
    /// reach, and counting one would turn `v6-direct` into advice that
    /// cannot work.
    ///
    /// Determined by asking the routing table which source address it would
    /// use for a public destination — a connected UDP socket sends nothing,
    /// so this is a local operation, not a probe.
    pub fn routable_v6() -> Option<std::net::IpAddr> {
        use std::net::{IpAddr, SocketAddr, UdpSocket};
        let sock = UdpSocket::bind("[::]:0").ok()?;
        // 2001:4860:4860::8888 is a well-known public destination. Nothing
        // is sent; connect() only makes the kernel pick a source address.
        sock.connect(SocketAddr::from((
            "2001:4860:4860::8888".parse::<IpAddr>().ok()?,
            53,
        )))
        .ok()?;
        let local = sock.local_addr().ok()?.ip();
        match local {
            IpAddr::V6(v6)
                if !v6.is_loopback()
                    && !v6.is_unspecified()
                    // link-local fe80::/10
                    && (v6.segments()[0] & 0xffc0) != 0xfe80
                    // ULA fc00::/7
                    && (v6.octets()[0] & 0xfe) != 0xfc =>
            {
                Some(IpAddr::V6(v6))
            }
            _ => None,
        }
    }

    /// Fill in the measurements this build can take without an edge:
    /// the UDP mapping and the local v6 fact. Reflect and the prober are
    /// the edges' half and are supplied by the caller.
    ///
    /// A failed STUN probe leaves `udp_mapping` as `None`, which `decide`
    /// reads as "not measured" and answers `relay` — the verdict that works
    /// on every network — rather than abstaining.
    pub async fn local_and_udp(m: &mut Measurements, stun_servers: &[&str]) {
        m.routable_v6 = routable_v6();
        if let Ok((mapping, ports)) = udp_mapping(stun_servers).await {
            m.udp_mapping = Some(mapping);
            m.udp_ports = ports;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    fn public_v4() -> Option<IpAddr> {
        Some(IpAddr::V4(Ipv4Addr::new(203, 0, 113, 7)))
    }

    fn cgnat_v4() -> Option<IpAddr> {
        Some(IpAddr::V4(Ipv4Addr::new(100, 90, 1, 2)))
    }

    #[test]
    fn a_public_address_answering_inbound_is_direct() {
        let m = Measurements {
            observed_address: public_v4(),
            inbound: Some((22, Inbound::Connected)),
            udp_mapping: Some(UdpMapping::Independent),
            ..Default::default()
        };
        assert_eq!(decide(&m).0, Verdict::Direct);
    }

    #[test]
    fn cgnat_is_relay_whatever_else_is_true() {
        // Deliberately stacked in favour of a better verdict: an
        // independent mapping AND a successful inbound connect. CGNAT still
        // wins, because the address is not yours to be reached at.
        let m = Measurements {
            observed_address: cgnat_v4(),
            udp_mapping: Some(UdpMapping::Independent),
            inbound: Some((22, Inbound::Connected)),
            ..Default::default()
        };
        let (v, _) = decide(&m);
        assert_eq!(v, Verdict::Relay);
    }

    #[test]
    fn tcp_independent_udp_symmetric_is_relay() {
        // The trap the spec singles out, and the reason the UDP half is
        // decisive. A verdict read off the TCP columns would say punchable
        // here and be wrong.
        let m = Measurements {
            observed_address: public_v4(),
            udp_mapping: Some(UdpMapping::Symmetric),
            udp_ports: vec![("stun1".into(), 51823), ("stun2".into(), 40119)],
            tcp_views: vec![
                EdgeView {
                    edge: "gate1".into(),
                    port: Some(51823),
                },
                EdgeView {
                    edge: "gate2".into(),
                    port: Some(51823),
                },
            ],
            ..Default::default()
        };
        assert_eq!(m.tcp_agrees(), Some(true));
        assert_eq!(decide(&m).0, Verdict::Relay);
    }

    #[test]
    fn an_independent_udp_mapping_with_no_inbound_is_punchable() {
        let m = Measurements {
            observed_address: public_v4(),
            udp_mapping: Some(UdpMapping::Independent),
            inbound: Some((22, Inbound::Timeout)),
            ..Default::default()
        };
        assert_eq!(decide(&m).0, Verdict::Punchable);
    }

    #[test]
    fn hopeless_v4_with_routable_v6_is_v6_direct() {
        let m = Measurements {
            observed_address: Some(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 5))),
            routable_v6: Some("2001:db8::1".parse().unwrap()),
            udp_mapping: Some(UdpMapping::Symmetric),
            ..Default::default()
        };
        assert_eq!(decide(&m).0, Verdict::V6Direct);
    }

    #[test]
    fn an_absent_edge_port_is_not_measured_never_zero() {
        // The spec's degradation case. One edge rolled the header out and
        // one did not: that is one vantage, which is not a comparison, and
        // must not be reported as agreement.
        let m = Measurements {
            observed_address: public_v4(),
            udp_mapping: Some(UdpMapping::Independent),
            tcp_views: vec![
                EdgeView {
                    edge: "gate1".into(),
                    port: Some(51823),
                },
                EdgeView {
                    edge: "gate2".into(),
                    port: None,
                },
            ],
            ..Default::default()
        };
        assert_eq!(m.tcp_agrees(), None, "one port is not a comparison");
        let text = render_text(&m, Verdict::Punchable, "x");
        assert!(text.contains("absent (gate2)"), "{text}");
        assert!(text.contains("not a comparison"), "{text}");
    }

    #[test]
    fn an_unmeasurable_network_falls_back_to_relay_and_says_so() {
        let (v, why) = decide(&Measurements::default());
        assert_eq!(v, Verdict::Relay);
        assert!(why.contains("could not be measured"), "{why}");
    }

    #[test]
    fn inbound_connected_from_a_private_address_is_not_direct() {
        // A `connected` against an RFC1918 address means the prober shares
        // a network with the caller, which says nothing about the internet.
        let m = Measurements {
            observed_address: Some(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 4))),
            inbound: Some((22, Inbound::Connected)),
            udp_mapping: Some(UdpMapping::Independent),
            ..Default::default()
        };
        assert_ne!(decide(&m).0, Verdict::Direct);
    }

    #[test]
    /// Both shapes a real machine with routable IPv6 produced, and both
    /// were wrong before. This is the tree being corrected by a network
    /// rather than by an argument, which is what the module header says
    /// will happen first.
    #[test]
    fn a_routable_v6_does_not_override_what_v4_actually_measured() {
        // What `examples/13` runs: a local STUN pair measures the UDP
        // mapping as independent, and no reflect edge answers. The old tree
        // said `v6-direct` — an inference about v6, which is never
        // measured here, beating a measurement of v4 that says punching
        // works.
        let punchable_with_v6 = Measurements {
            routable_v6: Some("2605:59ca:6632:a610::1".parse().unwrap()),
            udp_mapping: Some(UdpMapping::Independent),
            ..Default::default()
        };
        assert_eq!(decide(&punchable_with_v6).0, Verdict::Punchable);

        // What `examples/09` runs: nothing measured at all, on a machine
        // that happens to have v6. The old tree read "no edge answered" as
        // "v4 is hopeless" — because `has_public_v4()` answers false for
        // `None` — and gave its most specific verdict about a network it
        // knew nothing about. Relay is the answer that works everywhere,
        // and not measuring something is not a finding about it.
        let nothing_measured_with_v6 = Measurements {
            routable_v6: Some("2605:59ca:6632:a610::1".parse().unwrap()),
            ..Default::default()
        };
        assert_eq!(decide(&nothing_measured_with_v6).0, Verdict::Relay);

        // But a measured symmetric mapping *is* v4 ruled out, so v6 is the
        // better advice there and still wins over relay.
        let symmetric_with_v6 = Measurements {
            routable_v6: Some("2605:59ca:6632:a610::1".parse().unwrap()),
            udp_mapping: Some(UdpMapping::Symmetric),
            ..Default::default()
        };
        assert_eq!(decide(&symmetric_with_v6).0, Verdict::V6Direct);

        // And CGNAT still outranks everything, v6 included: the address is
        // shared and no v6 finding changes that.
        let cgnat_with_v6 = Measurements {
            observed_address: cgnat_v4(),
            routable_v6: Some("2605:59ca:6632:a610::1".parse().unwrap()),
            ..Default::default()
        };
        assert_eq!(decide(&cgnat_with_v6).0, Verdict::Relay);
    }

    fn every_rule_is_reachable() {
        // A table whose row never fires is a row nobody maintains. Each
        // verdict must be selectable by some measurement set; this fails
        // loudly if a reordering shadows one entirely.
        let cases = [
            Measurements {
                observed_address: cgnat_v4(),
                ..Default::default()
            },
            Measurements {
                observed_address: public_v4(),
                inbound: Some((22, Inbound::Connected)),
                ..Default::default()
            },
            Measurements {
                observed_address: public_v4(),
                udp_mapping: Some(UdpMapping::Independent),
                ..Default::default()
            },
            // v6-direct needs v4 *measured* and ruled out, not merely
            // unmeasured — a private observed address is a measurement.
            Measurements {
                observed_address: Some(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 5))),
                routable_v6: Some("2001:db8::1".parse().unwrap()),
                ..Default::default()
            },
            Measurements {
                observed_address: public_v4(),
                udp_mapping: Some(UdpMapping::Symmetric),
                ..Default::default()
            },
        ];
        assert_eq!(cases.len(), RULES.len(), "one case per rule");
        for (i, m) in cases.iter().enumerate() {
            let (v, why) = decide(m);
            assert_eq!(v, RULES[i].verdict, "case {i} selected the wrong verdict");
            assert_eq!(why, RULES[i].why, "case {i} matched a different rule");
        }
    }
}
