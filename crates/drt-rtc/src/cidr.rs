//! An address range, for who may connect: `--host 10.9.0.0/24` on a
//! listening peer and `--accept 203.0.113.0/24` on a parked one
//! (`doc/P2P.md` §6) are each a list of these, checked against the address
//! a session's packets come from.
//!
//! ## surface block
//!
//! - Entry points: [`Cidr::parse`], [`Cidr::contains`].
//! - Configurable: none.
//! - Fan-out: none; the two address families are one match.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

/// `address/prefix`. An address alone is a range of one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cidr {
    pub ip: IpAddr,
    pub prefix: u8,
}

impl Cidr {
    /// `10.9.0.0/24`, `fd00::/64`, or a bare address.
    pub fn parse(s: &str) -> Result<Cidr, String> {
        let (ip, prefix) = match s.split_once('/') {
            Some((ip, prefix)) => (ip, Some(prefix)),
            None => (s, None),
        };
        let ip: IpAddr = ip
            .trim_start_matches('[')
            .trim_end_matches(']')
            .parse()
            .map_err(|_| format!("'{s}' is not an address or an address/prefix range"))?;
        let bits = if ip.is_ipv4() { 32 } else { 128 };
        let prefix = match prefix {
            None => bits,
            Some(p) => p
                .parse::<u8>()
                .ok()
                .filter(|p| *p <= bits)
                .ok_or_else(|| format!("'{s}': the prefix is not 0..={bits}"))?,
        };
        Ok(Cidr { ip, prefix })
    }

    /// Whether `ip` is in the range. A v4 address mapped into v6 is its v4
    /// self, so a range written in one family admits the other's spelling.
    pub fn contains(&self, ip: IpAddr) -> bool {
        match (self.ip, unmap(ip)) {
            (IpAddr::V4(net), IpAddr::V4(ip)) => {
                let mask = mask32(self.prefix);
                u32::from(net) & mask == u32::from(ip) & mask
            }
            (IpAddr::V6(net), IpAddr::V6(ip)) => {
                let mask = mask128(self.prefix);
                u128::from(net) & mask == u128::from(ip) & mask
            }
            _ => false,
        }
    }

    /// A range of exactly one address.
    pub fn is_single(&self) -> bool {
        self.prefix == if self.ip.is_ipv4() { 32 } else { 128 }
    }
}

impl std::fmt::Display for Cidr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}", self.ip, self.prefix)
    }
}

fn unmap(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => IpAddr::V4(v4),
            None => IpAddr::V6(v6),
        },
        v4 => v4,
    }
}

fn mask32(prefix: u8) -> u32 {
    if prefix == 0 {
        0
    } else {
        u32::MAX << (32 - u32::from(prefix))
    }
}

fn mask128(prefix: u8) -> u128 {
    if prefix == 0 {
        0
    } else {
        u128::MAX << (128 - u32::from(prefix))
    }
}

#[allow(dead_code)]
const _: (Ipv4Addr, Ipv6Addr) = (Ipv4Addr::UNSPECIFIED, Ipv6Addr::UNSPECIFIED);

#[cfg(test)]
mod tests {
    use super::Cidr;

    #[test]
    fn a_range_admits_its_addresses_and_no_others() {
        let lan = Cidr::parse("10.9.0.0/24").unwrap();
        assert!(lan.contains("10.9.0.7".parse().unwrap()));
        assert!(!lan.contains("10.9.1.7".parse().unwrap()));
        assert!(
            lan.contains("::ffff:10.9.0.7".parse().unwrap()),
            "a mapped v4 is a v4"
        );
        assert!(!lan.is_single());
        let one = Cidr::parse("192.0.2.1").unwrap();
        assert!(one.is_single());
        assert!(one.contains("192.0.2.1".parse().unwrap()));
        assert!(!one.contains("192.0.2.2".parse().unwrap()));
        let v6 = Cidr::parse("fd00::/16").unwrap();
        assert!(v6.contains("fd00:1::1".parse().unwrap()));
        assert!(!v6.contains("fe80::1".parse().unwrap()));
        assert_eq!(Cidr::parse("0.0.0.0/0").unwrap().to_string(), "0.0.0.0/0");
        assert!(Cidr::parse("0.0.0.0/0")
            .unwrap()
            .contains("8.8.8.8".parse().unwrap()));
    }

    #[test]
    fn what_is_not_a_range_is_refused_by_name() {
        for bad in ["10.9.0.0/33", "box.lan", "fd00::/129", "10.9/8", ""] {
            assert!(Cidr::parse(bad).is_err(), "{bad}");
        }
    }
}
