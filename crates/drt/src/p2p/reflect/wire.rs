//! The reflect server's additions to STUN: the attributes it adds to a
//! binding message, and the signed request one gate sends the other
//! (`doc/Reflect.md`, Wire).
//!
//! Every message is an RFC 5389 binding request, success or error, built
//! with `ego_transport::stun`'s codec; the attributes here are appended
//! after it. Their types are in the comprehension-optional range, so a
//! plain STUN client ignores them and still reads its mapped address.
//!
//! ## surface block
//!
//! - Entry points: [`append`], [`attributes`], [`find`]; [`encode_address`],
//!   [`decode_address`]; [`sign`] and [`verify`] for the link between
//!   gates.
//! - Configurable: [`CLOCK_SKEW`].
//! - Fan-out: the attribute types, [`CAPABILITIES`] to [`PEER_AUTH`].

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use ego_transport::stun::HEADER_LEN;
use hmac::{Mac, SimpleHmac};
use sha2::Sha256;

/// How far a signed request's timestamp may be from this gate's clock.
pub const CLOCK_SKEW: Duration = Duration::from_secs(30);

/// On every answer: what this server offers, comma-separated
/// ([`super::Capability`]).
pub const CAPABILITIES: u16 = 0xC0D0;
/// On a TCP request: the port the other gate should connect to, as two
/// bytes and two zero bytes.
pub const CROSS_PORT: u16 = 0xC0D1;
/// On a TCP cross request, and on the request between gates: the 16 bytes
/// the other gate writes down the connection it makes.
pub const CROSS_TOKEN: u16 = 0xC0D2;
/// On an answer: a [`super::Code`]'s name.
pub const RESULT: u16 = 0xC0D3;
/// Between gates: connect to this address, which is the requester's
/// observed address with the port it asked for.
pub const PEER_CROSS: u16 = 0xC0D4;
/// Between gates: answer this client's binding request, whose transaction
/// id is the message's own, from here. The address, then the
/// CHANGE-REQUEST flags as four bytes.
pub const PEER_CHANGE: u16 = 0xC0D5;
/// Between gates, and always last: a millisecond timestamp (8 bytes), a
/// nonce (16), and an HMAC-SHA256 (32) over the whole message with these 32
/// bytes zero.
pub const PEER_AUTH: u16 = 0xC0D6;

const AUTH_LEN: usize = 8 + 16 + 32;

// depth: attributes

/// `message` with these attributes after its own, padded to four bytes,
/// and its header's length updated.
pub fn append(mut message: Vec<u8>, attrs: &[(u16, &[u8])]) -> Vec<u8> {
    for (kind, value) in attrs {
        message.extend_from_slice(&kind.to_be_bytes());
        message.extend_from_slice(&(value.len() as u16).to_be_bytes());
        message.extend_from_slice(value);
        message.resize(message.len() + (4 - value.len() % 4) % 4, 0);
    }
    let len = (message.len() - HEADER_LEN) as u16;
    message[2..4].copy_from_slice(&len.to_be_bytes());
    message
}

/// Every attribute of a well-formed message, in order, with its offset in
/// `message`. Stops at the first one that runs past the end.
pub fn attributes(message: &[u8]) -> Vec<(u16, usize, &[u8])> {
    let mut out = Vec::new();
    let mut at = HEADER_LEN;
    while at + 4 <= message.len() {
        let kind = u16::from_be_bytes([message[at], message[at + 1]]);
        let len = u16::from_be_bytes([message[at + 2], message[at + 3]]) as usize;
        let start = at + 4;
        if start + len > message.len() {
            break;
        }
        out.push((kind, at, &message[start..start + len]));
        at = start + len + (4 - len % 4) % 4;
    }
    out
}

/// The first attribute of this type.
pub fn find(message: &[u8], kind: u16) -> Option<&[u8]> {
    attributes(message)
        .into_iter()
        .find(|(k, _, _)| *k == kind)
        .map(|(_, _, v)| v)
}

/// The address form of RFC 5389 MAPPED-ADDRESS: a zero byte, the family,
/// the port, the address.
pub fn encode_address(addr: SocketAddr) -> Vec<u8> {
    let mut out = vec![0, if addr.is_ipv4() { 1 } else { 2 }];
    out.extend_from_slice(&addr.port().to_be_bytes());
    match addr.ip() {
        IpAddr::V4(ip) => out.extend_from_slice(&ip.octets()),
        IpAddr::V6(ip) => out.extend_from_slice(&ip.octets()),
    }
    out
}

pub fn decode_address(value: &[u8]) -> Option<SocketAddr> {
    let port = u16::from_be_bytes([*value.get(2)?, *value.get(3)?]);
    let ip = match (value.get(1)?, value.len()) {
        (1, 8) => IpAddr::V4(Ipv4Addr::from(<[u8; 4]>::try_from(&value[4..8]).ok()?)),
        (2, 20) => IpAddr::V6(Ipv6Addr::from(<[u8; 16]>::try_from(&value[4..20]).ok()?)),
        _ => return None,
    };
    Some(SocketAddr::new(ip, port))
}

// depth: the signed request between gates

/// `message`, which must not yet carry [`PEER_AUTH`], signed with `key`.
pub fn sign(message: Vec<u8>, key: &[u8], nonce: [u8; 16]) -> Vec<u8> {
    let mut value = [0u8; AUTH_LEN];
    value[..8].copy_from_slice(&now_ms().to_be_bytes());
    value[8..24].copy_from_slice(&nonce);
    let mut message = append(message, &[(PEER_AUTH, &value)]);
    let mac_at = message.len() - 32;
    let mac = mac(key, &message);
    message[mac_at..].copy_from_slice(&mac);
    message
}

/// Why a signed request was not taken.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refused {
    /// No [`PEER_AUTH`], or not the last attribute.
    Unsigned,
    /// The HMAC does not match: another key, or a changed message.
    BadSignature,
    /// The timestamp is further than [`CLOCK_SKEW`] from this clock.
    Stale,
}

/// Check a signed request; on success, its nonce, which the caller checks
/// it has not seen.
pub fn verify(message: &[u8], key: &[u8]) -> Result<[u8; 16], Refused> {
    let (_, at, value) = attributes(message)
        .into_iter()
        .rfind(|(k, _, _)| *k == PEER_AUTH)
        .ok_or(Refused::Unsigned)?;
    if value.len() != AUTH_LEN || at + 4 + AUTH_LEN != message.len() {
        return Err(Refused::Unsigned);
    }
    let stamp = u64::from_be_bytes(value[..8].try_into().expect("8 bytes"));
    let nonce: [u8; 16] = value[8..24].try_into().expect("16 bytes");
    let given = &value[24..];
    let mut zeroed = message.to_vec();
    let mac_at = zeroed.len() - 32;
    zeroed[mac_at..].fill(0);
    let mut check = SimpleHmac::<Sha256>::new_from_slice(key).expect("any key length");
    check.update(&zeroed);
    check
        .verify_slice(given)
        .map_err(|_| Refused::BadSignature)?;
    if now_ms().abs_diff(stamp) > CLOCK_SKEW.as_millis() as u64 {
        return Err(Refused::Stale);
    }
    Ok(nonce)
}

fn mac(key: &[u8], message: &[u8]) -> [u8; 32] {
    let mut m = SimpleHmac::<Sha256>::new_from_slice(key).expect("any key length");
    m.update(message);
    m.finalize().into_bytes().into()
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Sixteen bytes from the system's CSPRNG, by way of the transaction ids
/// the STUN codec already draws from it.
pub fn random16() -> [u8; 16] {
    let a = *ego_transport::stun::TransactionId::random().as_bytes();
    let b = *ego_transport::stun::TransactionId::random().as_bytes();
    let mut out = [0u8; 16];
    out[..12].copy_from_slice(&a);
    out[12..].copy_from_slice(&b[..4]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use ego_transport::stun::{encode_binding_request, TransactionId};

    fn request() -> Vec<u8> {
        encode_binding_request(&TransactionId::random()).to_vec()
    }

    #[test]
    fn appended_attributes_read_back_padded() {
        let m = append(
            request(),
            &[(RESULT, b"refused"), (CROSS_PORT, &[0, 22, 0, 0])],
        );
        assert_eq!((m.len() - HEADER_LEN) % 4, 0);
        assert_eq!(find(&m, RESULT), Some(&b"refused"[..]));
        assert_eq!(find(&m, CROSS_PORT), Some(&[0, 22, 0, 0][..]));
        // Still a binding request to the codec.
        assert!(ego_transport::stun::decode(&m).is_ok());
    }

    #[test]
    fn addresses_round_trip_in_both_families() {
        for a in ["203.0.113.7:3478", "[2001:db8::7]:22"] {
            let a: SocketAddr = a.parse().unwrap();
            assert_eq!(decode_address(&encode_address(a)), Some(a));
        }
    }

    #[test]
    fn a_signed_request_verifies_only_with_its_key_and_unchanged() {
        let signed = sign(append(request(), &[(RESULT, b"x")]), b"key", [7; 16]);
        assert_eq!(verify(&signed, b"key"), Ok([7; 16]));
        assert_eq!(verify(&signed, b"other"), Err(Refused::BadSignature));
        let mut changed = signed.clone();
        changed[HEADER_LEN + 4] ^= 1;
        assert_eq!(verify(&changed, b"key"), Err(Refused::BadSignature));
        assert_eq!(verify(&request(), b"key"), Err(Refused::Unsigned));
    }
}
