//! `drt p2p --reflect`: a STUN server on one port over UDP and TCP, and
//! with a peer gate the checks that need a second address
//! (`doc/Reflect.md`).
//!
//! A STUN answer is "this is the address and port your packet came from".
//! Over UDP that is the mapping a hole punch lands on; over TCP it is the
//! mapping of that connection. With `--reflect-peer`, the other gate
//! answers what one address cannot: whether a reply from an address the
//! client never sent to gets in (filtering, RFC 5780 CHANGE-REQUEST), and
//! whether a connection to the client's observed address on a port it
//! names gets through (the cross request).
//!
//! **The client may name a port, never an address.** Everything the other
//! gate sends goes to the address this gate observed, so neither gate can
//! be pointed at a third party.
//!
//! ## surface block
//!
//! - Entry points: [`serve::run`], the server; [`ReflectRole`], what it is
//!   given; [`ask::run`], the asking side, `drt+stun://` and
//!   `drt+reflect://`.
//! - Configurable: [`DEFAULT_PORT`], [`DEFAULT_RATE`], and the timeouts in
//!   [`serve`] and [`wire`].
//! - Fan-out: [`Capability`], what a server offers; [`Code`], every
//!   result and failure by its stable code; [`wire`], the attributes and
//!   the signed request between gates.

pub mod ask;
pub mod serve;
pub mod wire;

use crate::p2p::peer::HostSpec;

/// STUN's port (RFC 5389), for UDP and TCP alike.
pub const DEFAULT_PORT: u16 = 3478;
/// Cross requests a minute from one source address, `--reflect-rate`'s
/// default. A change request counts as one.
pub const DEFAULT_RATE: u32 = 30;

/// What `drt p2p --reflect` serves.
#[derive(Debug, Clone)]
pub struct ReflectRole {
    pub port: u16,
    /// `--host`, each one bound on its own: who may ask.
    pub hosts: Vec<HostSpec>,
    /// `--reflect-peer`, `host[:port]`: the other gate, tried in order.
    pub peers: Vec<String>,
    /// `--reflect-key`, resolved: signs and checks every request between
    /// gates. A gate with a key and no peer answers the other gate's
    /// requests and offers no cross checks of its own.
    pub key: Option<Vec<u8>>,
    /// `--reflect-rate`: cross requests a minute per source address.
    pub rate: u32,
}

/// `drt p2p drt+stun://…` or `drt+reflect://…`: what to ask, and how to
/// print it.
#[derive(Debug, Clone)]
pub struct AskRole {
    pub location: ask::Location,
    /// `--port`, repeatable: ports to ask the other gate to connect to.
    /// None: one the system picks, which this side listens on.
    pub ports: Vec<u16>,
    pub json: bool,
}

/// The two service names a reflect location takes, which no peer's
/// service may take.
pub use drt_rtc::scope::RESERVED_SERVICES;

/// What a server offers, listed on every answer it gives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Capability {
    /// STUN over UDP: the mapped address and port.
    Udp,
    /// STUN over TCP: the observed address and port of the connection.
    Tcp,
    /// A peer gate is named, and its address is OTHER-ADDRESS.
    Peer,
    /// CHANGE-REQUEST with change-IP, answered by the peer gate.
    Filtering,
    /// The TCP cross request, carried out by the peer gate.
    Cross,
}

impl Capability {
    pub const ALL: [Capability; 5] = [
        Capability::Udp,
        Capability::Tcp,
        Capability::Peer,
        Capability::Filtering,
        Capability::Cross,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Capability::Udp => "udp",
            Capability::Tcp => "tcp",
            Capability::Peer => "peer",
            Capability::Filtering => "filtering",
            Capability::Cross => "cross",
        }
    }

    pub fn parse(name: &str) -> Option<Capability> {
        Capability::ALL.into_iter().find(|c| c.name() == name)
    }
}

/// Every result and failure, by the stable code a script matches on. The
/// table in `doc/Reflect.md`, Codes, is [`Code::ALL`] with [`Code::meaning`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Code {
    Ok,
    Connected,
    Refused,
    Timeout,
    Unreachable,
    Sent,
    NoPeer,
    NoKey,
    RateLimited,
    PeerUnreachable,
    PeerRefused,
    BadRequest,
    NotOffered,
    UdpBlocked,
    TcpBlocked,
    Unresolved,
    SameAddress,
}

impl Code {
    pub const ALL: [Code; 17] = [
        Code::Ok,
        Code::Connected,
        Code::Refused,
        Code::Timeout,
        Code::Unreachable,
        Code::Sent,
        Code::NoPeer,
        Code::NoKey,
        Code::RateLimited,
        Code::PeerUnreachable,
        Code::PeerRefused,
        Code::BadRequest,
        Code::NotOffered,
        Code::UdpBlocked,
        Code::TcpBlocked,
        Code::Unresolved,
        Code::SameAddress,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Code::Ok => "ok",
            Code::Connected => "connected",
            Code::Refused => "refused",
            Code::Timeout => "timeout",
            Code::Unreachable => "unreachable",
            Code::Sent => "sent",
            Code::NoPeer => "no_peer",
            Code::NoKey => "no_key",
            Code::RateLimited => "rate_limited",
            Code::PeerUnreachable => "peer_unreachable",
            Code::PeerRefused => "peer_refused",
            Code::BadRequest => "bad_request",
            Code::NotOffered => "not_offered",
            Code::UdpBlocked => "udp_blocked",
            Code::TcpBlocked => "tcp_blocked",
            Code::Unresolved => "unresolved",
            Code::SameAddress => "same_address",
        }
    }

    pub fn meaning(self) -> &'static str {
        match self {
            Code::Ok => "the check ran; its answer is beside the code",
            Code::Connected => "the other gate's connection to the port was accepted",
            Code::Refused => "the other gate's connection to the port was refused",
            Code::Timeout => "the other gate's connection to the port got no answer",
            Code::Unreachable => "the other gate has no route to the observed address",
            Code::Sent => "the other gate sent the change request's answer",
            Code::NoPeer => "the server names no peer gate, and the check needs one",
            Code::NoKey => "the gate asked holds no --reflect-key, so it takes no request from a gate",
            Code::RateLimited => "over --reflect-rate requests a minute from this address",
            Code::PeerUnreachable => "the peer gate did not answer",
            Code::PeerRefused => {
                "the peer gate refused the signed request: another key, a clock too far off, or a nonce it has seen"
            }
            Code::BadRequest => "the request was malformed, or named port 0",
            Code::NotOffered => "the server does not offer this check, as its capabilities say",
            Code::UdpBlocked => "no answer over UDP: the server is down, or UDP does not get out",
            Code::TcpBlocked => "no answer over TCP: the server is down, or TCP to its port does not get out",
            Code::Unresolved => "the location names no address",
            Code::SameAddress => {
                "the change request's answer came from the server's own address, as with two gates on one address, so it says nothing about filtering"
            }
        }
    }

    pub fn parse(name: &str) -> Option<Code> {
        Code::ALL.into_iter().find(|c| c.name() == name)
    }

    /// The STUN error code an error answer carrying this one has.
    pub fn stun_error(self) -> u16 {
        match self {
            Code::BadRequest => 400,
            Code::NoKey | Code::PeerRefused => 401,
            Code::RateLimited => 403,
            Code::NoPeer | Code::NotOffered => 420,
            _ => 500,
        }
    }
}

/// `--reflect-key`'s value: the key itself, or `env:NAME` for the
/// environment variable that holds it.
pub fn resolve_key(spec: &str) -> Result<Vec<u8>, String> {
    let key = match spec.strip_prefix("env:") {
        Some(name) => std::env::var(name)
            .map_err(|_| format!("--reflect-key env:{name}: the variable is not set"))?,
        None => spec.to_string(),
    };
    if key.is_empty() {
        return Err("--reflect-key is empty".into());
    }
    Ok(key.into_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_capability_reads_back_by_name() {
        for c in Capability::ALL {
            assert_eq!(Capability::parse(c.name()), Some(c));
        }
    }

    /// The table in `doc/Reflect.md` is this one: each code, its meaning.
    #[test]
    fn the_documented_table_is_the_code_table() {
        let doc = include_str!("../../../../../doc/Reflect.md");
        for c in Code::ALL {
            let row = format!("| `{}` |", c.name());
            assert!(
                doc.contains(&row),
                "doc/Reflect.md has no row for {}",
                c.name()
            );
        }
    }

    #[test]
    fn codes_are_snake_case_unique_and_read_back() {
        let mut seen = std::collections::BTreeSet::new();
        for c in Code::ALL {
            let n = c.name();
            assert!(
                n.bytes().all(|b| b.is_ascii_lowercase() || b == b'_'),
                "{n}"
            );
            assert!(seen.insert(n), "{n} twice");
            assert_eq!(Code::parse(n), Some(c));
        }
    }
}
