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
    /// The destination this view was gathered from, as `addr:port`.
    ///
    /// **This, and not `edge`, is what makes two views a comparison.**
    /// Endpoint-independence is about the destination, and two vantages can
    /// share an edge name: `NETCHECK-SPEC.md` §2 offers "a second listen
    /// port on gate1 for the same reflect path" as the cheap intermediate
    /// before a second box, and both of those answer `edge: "gate1"`.
    /// Keying on the name would refuse that measurement; keying on the
    /// destination takes it.
    pub dest: String,
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
    /// Why the UDP mapping is absent, when it is. Rendered beside "not
    /// measured", because the decisive measurement failing is the one
    /// failure an operator has to be able to act on: servers down, DNS
    /// unresolvable, UDP blocked on the path and "you gave me one server"
    /// are four different problems with four different fixes, and a bare
    /// "not measured" sends the reader to guess between them.
    pub udp_why: Option<String>,
    /// What each reflect edge saw. Informational: the TCP half.
    pub tcp_views: Vec<EdgeView>,
    /// Whether every entry in `tcp_views` was gathered from the **same
    /// local source port**. Without that, comparing their ports compares
    /// nothing — see [`tcp_agrees`](Self::tcp_agrees).
    pub tcp_same_source_port: bool,
    /// Why an edge is missing from `tcp_views`, when one is. One entry per
    /// edge that was asked and did not answer, so "not measured" says which
    /// and why rather than leaving the reader to guess between a rate limit,
    /// a name that would not resolve, and an edge that is simply down.
    pub reflect_why: Vec<String>,
    /// A vantage that saw a different address from the one already
    /// measured, rendered beside it.
    ///
    /// Its own field rather than an entry in `reflect_why`, because it
    /// belongs on a different line: `reflect_why` explains an absence in
    /// `tcp map`, and this explains a presence in `address`. Recorded in
    /// `reflect_why` instead, it was written down and never shown, which is
    /// the same as not recording it.
    pub address_why: Option<String>,
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

    /// Whether any measurement here involved actually asking the network.
    ///
    /// This is what the exit status turns on, and `routable_v6` is
    /// deliberately not in it. `routable_v6()` reads the routing table and
    /// sends nothing (see [`gather`]), so a machine that holds a v6 address
    /// and could not reach a single STUN server has measured *nothing* —
    /// but the old rule counted the v6 address and exited 0, reporting
    /// success for a run whose decisive probe failed. Every evidence line
    /// but one said `not measured` directly above it.
    ///
    /// A verdict that rests on a real measurement still exits 0, `relay`
    /// included: `v6-direct` requires [`v4_ruled_out`](Self::v4_ruled_out),
    /// which requires an observed address or a symmetric mapping, and both
    /// of those cost a packet.
    pub fn probed_anything(&self) -> bool {
        self.udp_mapping.is_some() || self.observed_address.is_some() || self.inbound.is_some()
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
        // The guard that makes this mean anything. Each reflect fetch is a
        // separate TCP connection with its own ephemeral source port, so two
        // edges asked over two connections report two different ports on
        // *every* network — and reading that as "per-destination" would be a
        // confident statement about a NAT built on nothing.
        //
        // `REFLECT-NAT.md` §5 is explicit that the measurement is "same-
        // local-port connections to reflect through both edges". Until DRT
        // can pin an outbound source port (doc/Next.md; it needs `socket2`),
        // nothing sets this and this method answers `None` — which the
        // renderer prints as the non-comparison it is.
        if !self.tcp_same_source_port {
            return None;
        }
        // And it must be two *destinations*. Endpoint-independent means the
        // same external port regardless of where you are going, so two
        // connections to one destination measure nothing about it -- every
        // NAT, symmetric ones included, ordinarily reuses a mapping for a
        // second connection to a destination it already has one for. That
        // run answered `independent` once, which would have told a symmetric
        // NAT it punches; what it really measures is
        // [`tcp_mapping_stable`](Self::tcp_mapping_stable).
        //
        // Destination and not edge NAME, because two vantages may share a
        // name: `NETCHECK-SPEC.md` §2's cheap intermediate is a second
        // listen port on gate1, and both ports answer `edge: "gate1"`.
        if self.distinct_destinations() < 2 {
            return None;
        }
        let ports: Vec<u16> = self.tcp_views.iter().filter_map(|v| v.port).collect();
        if ports.len() < 2 {
            return None;
        }
        Some(ports.windows(2).all(|w| w[0] == w[1]))
    }

    /// How many distinct destinations the TCP views came from.
    pub fn distinct_destinations(&self) -> usize {
        let mut seen: Vec<&str> = self.tcp_views.iter().map(|v| v.dest.as_str()).collect();
        seen.sort_unstable();
        seen.dedup();
        seen.len()
    }

    /// Whether one edge, asked twice from one pinned source port, saw the
    /// same external port both times.
    ///
    /// A different question from [`tcp_agrees`](Self::tcp_agrees) and a
    /// useful one on its own: it says whether the NAT holds a mapping
    /// across two sequential connections at all. If it does not — a fresh
    /// external port for every connection, even to the same destination —
    /// then the two-edge comparison can *never* answer `independent`
    /// whatever the NAT's real mapping behaviour is, and standing up a
    /// second vantage to run it would buy nothing.
    ///
    /// So it is worth measuring before there is a second edge, with one.
    pub fn tcp_mapping_stable(&self) -> Option<bool> {
        if !self.tcp_same_source_port {
            return None;
        }
        if self.distinct_destinations() != 1 {
            return None;
        }
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
            "  address    {}{}{}\n",
            a,
            if m.is_cgnat() { " (CGNAT)" } else { "" },
            match &m.address_why {
                Some(why) => format!(" ({why})"),
                None => String::new(),
            }
        )),
        None => {
            out.push_str("  address    not measured (no STUN server or reflect edge answered)\n")
        }
    }

    match m.routable_v6 {
        Some(a) => out.push_str(&format!("  v6         {a}\n")),
        None => out.push_str("  v6         none routable\n"),
    }

    if m.udp_ports.is_empty() {
        match &m.udp_why {
            Some(why) => out.push_str(&format!("  udp map    not measured ({why})\n")),
            None => out.push_str("  udp map    not measured\n"),
        }
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
        match m.reflect_why.first() {
            Some(why) => out.push_str(&format!("  tcp map    not measured ({why})\n")),
            None => out.push_str("  tcp map    not measured\n"),
        }
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
            // Named with its caveat, because the two connections were
            // sequential and a NAT can rebind between them. A reader
            // deciding whether to trust this needs to know that.
            Some(true) => "  independent (pinned source port, sequential)",
            Some(false) => "  per-destination (pinned source port, sequential)",
            // Two vantages over two connections is still not a comparison,
            // and saying so is the difference between this line and a wrong
            // answer about the network.
            // One edge asked more than once is a different measurement, and
            // it is the one that says whether a second edge would be worth
            // standing up.
            None if m.tcp_mapping_stable() == Some(true) => {
                "  one edge twice: the mapping held (a second vantage would measure something)"
            }
            None if m.tcp_mapping_stable() == Some(false) => {
                "  one edge twice: the mapping CHANGED, so no two-edge comparison can succeed here"
            }
            None if m.tcp_views.len() > 1 => {
                "  (separate connections, so separate source ports; not a comparison)"
            }
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
    use super::{EdgeView, Measurements, UdpMapping};
    use ego_transport::stun::{detect_mapping, NatMapping, ProbeConfig};
    use std::net::IpAddr;

    /// Ask two or more STUN servers what they see of **one** socket, and
    /// classify the mapping.
    ///
    /// One socket for every probe is the whole point: two sockets would
    /// have different mappings under any NAT and the comparison would mean
    /// nothing. `detect_mapping` owns that discipline, and refuses below two
    /// servers rather than guessing — so a caller that supplies one gets an
    /// error here and "not measured" in the evidence, never a confident
    /// wrong answer.
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
    fn agreed_address(report: &ego_transport::stun::MappingReport) -> Option<IpAddr> {
        let mut seen = report.probes.iter().map(|p| p.reflexive.ip());
        let first = seen.next()?;
        seen.all(|a| a == first).then_some(first)
    }

    pub async fn udp_mapping(
        servers: &[&str],
    ) -> Result<(UdpMapping, Vec<(String, u16)>, Option<IpAddr>), String> {
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
        Ok((mapping, ports, agreed_address(&report)))
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
    /// Ask each reflect edge what it saw, filling [`Measurements::tcp_views`]
    /// and — where STUN did not already answer — the observed address.
    ///
    /// **The JSON form, not `?format=addr-port`.** discofetch's
    /// `doc/REFLECT-NAT.md` §5 says to key these by the `edge` field, and
    /// `addr-port` does not carry one: it is `ADDRESS PORT` and nothing
    /// else. One JSON fetch gives address, port and edge together, so it is
    /// one request rather than two — and two requests would be two TCP
    /// connections reporting two different source ports, which is the
    /// measurement error this whole line is guarded against.
    ///
    /// An edge that does not answer becomes a line in
    /// [`Measurements::reflect_why`] and never a guess. A rate limit
    /// (HAProxy answers 429 here) is "not measured" like any other silence:
    /// rendering it as a closed port would be a confidently wrong answer
    /// about the user's network, which is the one thing this module exists
    /// not to do.
    pub async fn reflect(m: &mut Measurements, edges: &[&str], pin: bool) {
        // The first fetch takes an ephemeral port and reports it; every
        // fetch after it leaves from that same port. Sequential on purpose
        // -- see `reflect::connect_from`.
        let mut pinned: Option<u16> = None;
        let mut all_pinned = true;
        // One name, every address it resolves to. `NETCHECK-SPEC.md` §2:
        // "One name, two A records. The client resolves
        // reflect.discofetch.link, connects to each returned address from
        // the same local port with the same Host, and reads observed.edge
        // to know which vantage answered."
        //
        // So one `--reflect` can be two vantages, and taking only the first
        // address -- which this did -- would ask one vantage and call it the
        // set.
        let mut targets: Vec<(&str, std::net::SocketAddr)> = Vec::new();
        for url in edges {
            match crate::reflect::addresses(url).await {
                Ok(found) => targets.extend(found.into_iter().map(|a| (*url, a))),
                Err(why) => {
                    all_pinned = false;
                    m.reflect_why.push(format!("{url}: {why}"));
                }
            }
        }
        for (url, dest) in targets {
            match one_edge(url, dest, if pin { pinned } else { None }).await {
                Ok((view, used_port)) => {
                    if pin {
                        match pinned {
                            None => pinned = Some(used_port),
                            // A bind that did not take is not a comparison.
                            Some(want) if want != used_port => all_pinned = false,
                            Some(_) => {}
                        }
                    }
                    // The address an edge saw over TCP. STUN's is over UDP,
                    // and a network may egress differently per protocol, so
                    // a disagreement is recorded rather than resolved.
                    if let Some(seen) = view.address {
                        match m.observed_address {
                            None => m.observed_address = Some(seen),
                            // Different protocols may egress differently,
                            // so this is a finding to show and not a
                            // conflict to resolve. First one wins the field;
                            // a second disagreement is the same story.
                            Some(known) if known != seen => {
                                m.address_why.get_or_insert_with(|| {
                                    format!("{} saw {seen}, over TCP", view.edge)
                                });
                            }
                            Some(_) => {}
                        }
                    }
                    m.tcp_views.push(EdgeView {
                        edge: view.edge,
                        port: view.port,
                        dest: dest.to_string(),
                    });
                }
                Err(why) => {
                    all_pinned = false;
                    m.reflect_why.push(format!("{url} via {dest}: {why}"));
                }
            }
        }
        // Only a run where EVERY view left from one port is a comparison.
        // One failed bind, or one edge that did not answer, and these are
        // separate observations again.
        m.tcp_same_source_port = pin && all_pinned && m.tcp_views.len() > 1;
        // A vantage that answered but names itself nothing new is still a
        // vantage; the destination is what counted it.
        let _ = &m.tcp_views;
    }

    /// What one edge answered.
    struct EdgeAnswer {
        edge: String,
        port: Option<u16>,
        address: Option<IpAddr>,
    }

    async fn one_edge(
        url: &str,
        dest: std::net::SocketAddr,
        from_port: Option<u16>,
    ) -> Result<(EdgeAnswer, u16), String> {
        let (body, used_port) = crate::reflect::get(url, dest, from_port).await?;
        let json: serde_json::Value =
            serde_json::from_str(&body).map_err(|_| "the edge did not answer JSON".to_string())?;
        let observed = json
            .get("observed")
            .ok_or("the edge answered JSON with no `observed` block")?;
        // Absent is not measured, never zero -- NETCHECK-SPEC.md is explicit
        // about the port, and the same rule is right for all three.
        let port = observed
            .get("port")
            .and_then(|v| v.as_u64())
            .and_then(|p| u16::try_from(p).ok());
        let address = observed
            .get("address")
            .and_then(|v| v.as_str())
            .and_then(|a| a.parse::<IpAddr>().ok());
        // The edge names itself; where it does not, the URL is the only
        // honest name for the vantage and is better than inventing one.
        let edge = observed
            .get("edge")
            .and_then(|v| v.as_str())
            .map(str::to_string)
            .unwrap_or_else(|| url.to_string());
        Ok((
            EdgeAnswer {
                edge,
                port,
                address,
            },
            used_port,
        ))
    }

    pub async fn local_and_udp(m: &mut Measurements, stun_servers: &[&str]) {
        m.routable_v6 = routable_v6();
        match udp_mapping(stun_servers).await {
            Ok((mapping, ports, address)) => {
                m.udp_mapping = Some(mapping);
                m.udp_ports = ports;
                // Only when a caller has not already supplied one: a reflect
                // edge is the richer source (it sees the TCP path too), so it
                // wins where both exist.
                m.observed_address = m.observed_address.or(address);
            }
            // Kept, not discarded. This used to be `if let Ok(..)`, and the
            // reason the decisive probe failed went nowhere -- so a run
            // against two real STUN servers that answered nothing looked
            // exactly like a run with no servers named.
            Err(why) => m.udp_why = Some(why),
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
        //
        // `tcp_same_source_port` because the fixture is describing a real
        // TCP finding, and endpoint-independent is only a finding when both
        // views came from one pinned source port. Two equal ports from two
        // separate connections would be a coincidence, not a measurement.
        let m = Measurements {
            observed_address: public_v4(),
            udp_mapping: Some(UdpMapping::Symmetric),
            udp_ports: vec![("stun1".into(), 51823), ("stun2".into(), 40119)],
            tcp_same_source_port: true,
            tcp_views: vec![
                EdgeView {
                    edge: "gate1".into(),
                    port: Some(51823),
                    dest: "203.0.113.1:443".into(),
                },
                EdgeView {
                    edge: "gate2".into(),
                    port: Some(51823),
                    dest: "203.0.113.2:443".into(),
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
                    dest: "203.0.113.1:443".into(),
                },
                EdgeView {
                    edge: "gate2".into(),
                    port: None,
                    dest: "203.0.113.2:443".into(),
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

    /// The exit status has to mean what it says, on a machine with IPv6.
    #[test]
    fn holding_a_v6_address_is_not_a_measurement() {
        // The shape a real machine produced: routable v6, and every probe
        // that costs a packet failed. Verdict `relay` from ignorance, and
        // the old exit rule called that a success because v6 was present.
        let v6_only = Measurements {
            routable_v6: Some("2605:59ca:6632:a610::1".parse().unwrap()),
            udp_why: Some("no STUN response".into()),
            ..Default::default()
        };
        assert_eq!(decide(&v6_only).0, Verdict::Relay);
        assert!(
            !v6_only.probed_anything(),
            "reading the routing table is not asking the network"
        );

        // Anything that cost a packet is a measurement, whatever it found.
        for m in [
            Measurements {
                udp_mapping: Some(UdpMapping::Symmetric),
                ..Default::default()
            },
            Measurements {
                observed_address: public_v4(),
                ..Default::default()
            },
            Measurements {
                inbound: Some((22, Inbound::Refused)),
                ..Default::default()
            },
        ] {
            assert!(m.probed_anything(), "{m:?}");
        }

        // And the verdict that *does* rest on v6 still reports success,
        // because reaching it needs v4 measured and ruled out.
        let v6_direct = Measurements {
            routable_v6: Some("2605:59ca:6632:a610::1".parse().unwrap()),
            udp_mapping: Some(UdpMapping::Symmetric),
            ..Default::default()
        };
        assert_eq!(decide(&v6_direct).0, Verdict::V6Direct);
        assert!(v6_direct.probed_anything());
    }

    /// A failed decisive probe says why. Four different problems with four
    /// different fixes used to render identically.
    #[test]
    fn an_unmeasured_udp_mapping_carries_its_reason() {
        let m = Measurements {
            udp_why: Some("could not resolve STUN server address 'stun1.example:3478'".into()),
            ..Default::default()
        };
        let (v, why) = decide(&m);
        let text = render_text(&m, v, why);
        assert!(
            text.contains("udp map    not measured (could not resolve"),
            "{text}"
        );

        // Absent reason, absent parenthetical — no empty "()" to explain.
        let silent = Measurements::default();
        let (v, why) = decide(&silent);
        assert!(render_text(&silent, v, why).contains("udp map    not measured\n"));
    }

    /// The CGNAT rule outranks everything, and until STUN's own answer was
    /// kept it could not fire at all in the only configuration this build
    /// supports — so a machine behind a carrier NAT was told `punchable`,
    /// which is the single most consequential wrong answer this tool can
    /// give. It is what the whole verdict table is ordered around.
    #[test]
    fn a_cgnat_address_from_stun_alone_is_relay_not_punchable() {
        // Exactly the shape a STUN-only run now produces: an endpoint-
        // independent mapping — which on its own reads `punchable` — and
        // the reflexive address the same probes reported.
        let behind_cgnat = Measurements {
            observed_address: cgnat_v4(),
            udp_mapping: Some(UdpMapping::Independent),
            ..Default::default()
        };
        assert_eq!(decide(&behind_cgnat).0, Verdict::Relay);
        assert!(behind_cgnat.is_cgnat());

        // The same mapping on a public address is genuinely punchable, so
        // the assertion above is measuring CGNAT and not refusing all
        // independent mappings.
        let public = Measurements {
            observed_address: public_v4(),
            udp_mapping: Some(UdpMapping::Independent),
            ..Default::default()
        };
        assert_eq!(decide(&public).0, Verdict::Punchable);

        // And the address is now a measurement, so a STUN-only run reports
        // success rather than "nothing could be asked".
        assert!(behind_cgnat.probed_anything());
        let rendered = {
            let (v, why) = decide(&behind_cgnat);
            render_text(&behind_cgnat, v, why)
        };
        assert!(rendered.contains("(CGNAT)"), "{rendered}");
    }

    /// Two vantages are not a comparison, and the line must not pretend
    /// otherwise.
    ///
    /// Each reflect fetch is its own TCP connection with its own ephemeral
    /// source port, so two edges report two different ports on *every*
    /// network. `tcp_agrees` used to read that as `Some(false)` —
    /// "per-destination" — which is a confident statement about a NAT built
    /// on nothing at all, in the module whose first premise is that it does
    /// not do that. Measured live: two fetches to the same edge answered
    /// ports 3075 and 56304.
    #[test]
    fn two_edges_over_two_connections_are_not_a_tcp_comparison() {
        let two_vantages = Measurements {
            tcp_views: vec![
                EdgeView {
                    edge: "gate1".into(),
                    port: Some(51823),
                    dest: "203.0.113.1:443".into(),
                },
                EdgeView {
                    edge: "gate2".into(),
                    port: Some(51999),
                    dest: "203.0.113.2:443".into(),
                },
            ],
            ..Default::default()
        };
        assert_eq!(
            two_vantages.tcp_agrees(),
            None,
            "different ports from different connections say nothing"
        );
        let (v, why) = decide(&two_vantages);
        let text = render_text(&two_vantages, v, why);
        assert!(
            text.contains("separate source ports; not a comparison"),
            "{text}"
        );
        assert!(
            !text.contains("per-destination"),
            "the one answer this must never give here: {text}"
        );

        // Pinned, the same two ports ARE the measurement, and differing
        // ports then genuinely mean per-destination.
        let pinned = Measurements {
            tcp_same_source_port: true,
            ..two_vantages.clone()
        };
        assert_eq!(pinned.tcp_agrees(), Some(false));

        // And pinned with equal ports is endpoint-independent.
        let agreeing = Measurements {
            tcp_same_source_port: true,
            tcp_views: vec![
                EdgeView {
                    edge: "gate1".into(),
                    port: Some(51823),
                    dest: "203.0.113.1:443".into(),
                },
                EdgeView {
                    edge: "gate2".into(),
                    port: Some(51823),
                    dest: "203.0.113.2:443".into(),
                },
            ],
            ..Default::default()
        };
        assert_eq!(agreeing.tcp_agrees(), Some(true));

        // One vantage is never a comparison however it was gathered.
        let one = Measurements {
            tcp_same_source_port: true,
            tcp_views: vec![EdgeView {
                edge: "gate1".into(),
                port: Some(51823),
                dest: "203.0.113.1:443".into(),
            }],
            ..Default::default()
        };
        assert_eq!(one.tcp_agrees(), None);
    }

    /// The guard on `tcp_agrees`: two views must be two *destinations*.
    #[test]
    fn two_views_of_one_edge_are_not_an_endpoint_comparison() {
        let twice = |edge: &str| Measurements {
            tcp_same_source_port: true,
            tcp_views: vec![
                EdgeView {
                    edge: edge.into(),
                    port: Some(51823),
                    dest: "203.0.113.1:443".into(),
                },
                EdgeView {
                    edge: edge.into(),
                    port: Some(51823),
                    dest: "203.0.113.1:443".into(),
                },
            ],
            ..Default::default()
        };
        let same = twice("gate1");
        assert_eq!(
            same.tcp_agrees(),
            None,
            "one destination says nothing about endpoint-independence"
        );
        assert_eq!(
            same.tcp_mapping_stable(),
            Some(true),
            "but it does say the mapping held"
        );

        // A mapping that changed between two connections to one destination
        // means no two-edge comparison can ever succeed here.
        let moved = Measurements {
            tcp_views: vec![
                EdgeView {
                    edge: "gate1".into(),
                    port: Some(51823),
                    dest: "203.0.113.1:443".into(),
                },
                EdgeView {
                    edge: "gate1".into(),
                    port: Some(51999),
                    dest: "203.0.113.1:443".into(),
                },
            ],
            ..twice("gate1")
        };
        assert_eq!(moved.tcp_agrees(), None);
        assert_eq!(moved.tcp_mapping_stable(), Some(false));

        // Two distinct destinations are the measurement, and stability is
        // then not the question being asked.
        let two = Measurements {
            tcp_views: vec![
                EdgeView {
                    edge: "gate1".into(),
                    port: Some(51823),
                    dest: "203.0.113.1:443".into(),
                },
                EdgeView {
                    edge: "gate2".into(),
                    port: Some(51823),
                    dest: "203.0.113.2:443".into(),
                },
            ],
            ..twice("gate1")
        };
        assert_eq!(two.tcp_agrees(), Some(true));
        assert_eq!(two.tcp_mapping_stable(), None);

        // And the case the edge NAME would have refused: `NETCHECK-SPEC.md`
        // §2's cheap intermediate is a second listen port on gate1, so two
        // real vantages that both answer `edge: "gate1"`. Keying on the
        // destination takes the measurement; keying on the name would have
        // called it a stability check and thrown it away.
        let two_ports = Measurements {
            tcp_views: vec![
                EdgeView {
                    edge: "gate1".into(),
                    port: Some(51823),
                    dest: "203.0.113.1:443".into(),
                },
                EdgeView {
                    edge: "gate1".into(),
                    port: Some(51823),
                    dest: "203.0.113.1:8443".into(),
                },
            ],
            ..twice("gate1")
        };
        assert_eq!(two_ports.tcp_agrees(), Some(true));
        assert_eq!(two_ports.tcp_mapping_stable(), None);
    }

    /// An edge that did not answer says why, on the line where its absence
    /// shows — the same rule the udp line already follows.
    #[test]
    fn an_edge_that_did_not_answer_says_why() {
        let m = Measurements {
            reflect_why: vec!["https://reflect.example/: rate limited by the edge".into()],
            ..Default::default()
        };
        let (v, why) = decide(&m);
        let text = render_text(&m, v, why);
        assert!(
            text.contains("tcp map    not measured (https://reflect.example/: rate limited"),
            "{text}"
        );
        // A rate limit is silence, not a finding about the network. It must
        // not reach the verdict.
        assert_eq!(v, Verdict::Relay);
        assert!(
            !m.probed_anything(),
            "an edge that refused measured nothing"
        );
    }

    #[test]
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
