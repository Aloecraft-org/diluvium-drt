//! What a browser may reach: the block's scope, matched as
//! `doc/BrowserAccess.md` §6 says, before any connection and again on the
//! resolved address.
//!
//! ## surface block
//!
//! - Entry points: [`Entry::parse`], [`Scope::new`], [`Scope::allows`],
//!   [`resolved_ok`].
//! - Configurable: [`SCHEMES`], the schemes an entry may name, with their
//!   default ports. The scheme is advice to the browser; the host enforces
//!   host and port only.
//! - Fan-out: [`special_purpose`], the address classes refused after
//!   resolution unless an entry names them outright.

use std::net::IpAddr;

/// The schemes an entry may name, and the port each implies when the entry
/// gives none.
pub const SCHEMES: &[(&str, u16)] = &[("http", 80), ("https", 443), ("ssh", 22)];

/// One reachable target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub scheme: String,
    /// As configured, and as compared: ASCII case-insensitively. An IPv6
    /// literal is kept without its brackets.
    pub host: String,
    pub port: u16,
}

impl Entry {
    /// `scheme://host[:port]`, nothing after it. A path, a query or a user
    /// would be a promise about something the host does not enforce, so
    /// each is refused by name.
    pub fn parse(s: &str) -> Result<Entry, String> {
        let (scheme, rest) = s
            .split_once("://")
            .ok_or_else(|| format!("scope entry '{s}' is not scheme://host[:port]"))?;
        let scheme = scheme.to_ascii_lowercase();
        let default_port = SCHEMES
            .iter()
            .find(|(name, _)| *name == scheme)
            .map(|(_, port)| *port)
            .ok_or_else(|| {
                let names: Vec<&str> = SCHEMES.iter().map(|(n, _)| *n).collect();
                format!(
                    "scope entry '{s}': scheme must be one of {}",
                    names.join(", ")
                )
            })?;
        let authority = rest.strip_suffix('/').unwrap_or(rest);
        if authority.contains(['/', '?', '#', '@']) {
            return Err(format!(
                "scope entry '{s}' names more than a host and port; the host enforces only those"
            ));
        }
        let (host, port) = if let Some(v6) = authority.strip_prefix('[') {
            let (addr, after) = v6
                .split_once(']')
                .ok_or_else(|| format!("scope entry '{s}': unclosed '['"))?;
            let port = match after.strip_prefix(':') {
                Some(p) => parse_port(s, p)?,
                None if after.is_empty() => default_port,
                None => return Err(format!("scope entry '{s}': junk after ']'")),
            };
            (addr.to_string(), port)
        } else {
            match authority.rsplit_once(':') {
                Some((h, p)) => (h.to_string(), parse_port(s, p)?),
                None => (authority.to_string(), default_port),
            }
        };
        if host.is_empty() {
            return Err(format!("scope entry '{s}' has no host"));
        }
        Ok(Entry { scheme, host, port })
    }

    /// The form the entry was written in, for `hello` and for reports.
    pub fn url(&self) -> String {
        if self.host.contains(':') {
            format!("{}://[{}]:{}", self.scheme, self.host, self.port)
        } else {
            format!("{}://{}:{}", self.scheme, self.host, self.port)
        }
    }
}

fn parse_port(entry: &str, p: &str) -> Result<u16, String> {
    match p.parse::<u16>() {
        Ok(0) | Err(_) => Err(format!("scope entry '{entry}': '{p}' is not a port")),
        Ok(n) => Ok(n),
    }
}

/// Every entry the block names, default deny.
#[derive(Debug, Clone, Default)]
pub struct Scope {
    pub entries: Vec<Entry>,
}

impl Scope {
    pub fn new(entries: Vec<Entry>) -> Scope {
        Scope { entries }
    }

    /// The entry a `CONNECT` to `host:port` matches, if any: the host string
    /// compared ASCII case-insensitively, the port exactly. An IP literal
    /// therefore matches only its own spelling, which is the rule.
    pub fn allows(&self, host: &str, port: u16) -> Option<&Entry> {
        let host = host
            .strip_prefix('[')
            .and_then(|h| h.strip_suffix(']'))
            .unwrap_or(host);
        self.entries
            .iter()
            .find(|e| e.port == port && e.host.eq_ignore_ascii_case(host))
    }
}

/// May `entry` connect to `ip`, now that its name has resolved?
///
/// An ordinary address, yes: the entry named it, and a LAN target is the
/// point. A special-purpose one only when the entry *is* that address, or
/// the entry is `localhost` and the address is loopback. A name in scope
/// that resolves to the metadata service is what this check exists for.
pub fn resolved_ok(entry: &Entry, ip: IpAddr) -> bool {
    if !special_purpose(ip) {
        return true;
    }
    if let Ok(literal) = entry.host.parse::<IpAddr>() {
        return literal == ip;
    }
    entry.host.eq_ignore_ascii_case("localhost") && ip.is_loopback()
}

/// Loopback, link-local, unspecified, multicast, broadcast, and the cloud
/// metadata addresses that sit outside link-local.
pub fn special_purpose(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v) => {
            v.is_loopback()
                || v.is_link_local()
                || v.is_unspecified()
                || v.is_multicast()
                || v.is_broadcast()
                // Alibaba Cloud's metadata service.
                || v.octets() == [100, 100, 100, 200]
        }
        IpAddr::V6(v) => {
            if let Some(v4) = v.to_ipv4_mapped() {
                return special_purpose(IpAddr::V4(v4));
            }
            v.is_loopback()
                || v.is_unspecified()
                || v.is_multicast()
                // fe80::/10
                || (v.segments()[0] & 0xffc0) == 0xfe80
                // AWS's IPv6 metadata service, fd00:ec2::254.
                || v.segments() == [0xfd00, 0x0ec2, 0, 0, 0, 0, 0, 0x0254]
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entries_parse_with_default_ports() {
        let e = Entry::parse("http://127.0.0.1:8123").unwrap();
        assert_eq!(
            (e.scheme.as_str(), e.host.as_str(), e.port),
            ("http", "127.0.0.1", 8123)
        );
        assert_eq!(Entry::parse("ssh://box").unwrap().port, 22);
        assert_eq!(Entry::parse("HTTPS://Box.Lan/").unwrap().port, 443);
        let v6 = Entry::parse("http://[fd00::1]:8096").unwrap();
        assert_eq!((v6.host.as_str(), v6.port), ("fd00::1", 8096));
        assert_eq!(v6.url(), "http://[fd00::1]:8096");
    }

    #[test]
    fn entries_that_promise_more_than_host_and_port_are_refused() {
        for bad in [
            "http://h/path",
            "http://u@h",
            "ftp://h",
            "h:80",
            "http://h:0",
            "http://h:99999",
            "http://",
        ] {
            assert!(Entry::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn matching_is_case_insensitive_on_host_and_exact_on_port() {
        let scope = Scope::new(vec![
            Entry::parse("http://HA.lan:8123").unwrap(),
            Entry::parse("http://[fd00::1]:80").unwrap(),
        ]);
        assert!(scope.allows("ha.LAN", 8123).is_some());
        assert!(scope.allows("ha.lan", 8124).is_none());
        assert!(scope.allows("[fd00::1]", 80).is_some());
        assert!(
            scope.allows("fd00:0::1", 80).is_none(),
            "a literal matches its own spelling"
        );
    }

    #[test]
    fn special_addresses_need_to_be_named() {
        let name = Entry::parse("http://ha.lan:80").unwrap();
        let lit = Entry::parse("http://127.0.0.1:80").unwrap();
        let localhost = Entry::parse("http://localhost:80").unwrap();
        let lo: IpAddr = "127.0.0.1".parse().unwrap();
        let meta: IpAddr = "169.254.169.254".parse().unwrap();
        let lan: IpAddr = "192.168.1.20".parse().unwrap();
        assert!(resolved_ok(&name, lan));
        assert!(!resolved_ok(&name, lo), "a name that resolves to loopback");
        assert!(
            !resolved_ok(&name, meta),
            "a name that resolves to metadata"
        );
        assert!(resolved_ok(&lit, lo));
        assert!(!resolved_ok(&lit, "127.0.0.2".parse().unwrap()));
        assert!(resolved_ok(&localhost, lo));
        assert!(!resolved_ok(&localhost, meta));
        assert!(special_purpose("::ffff:169.254.169.254".parse().unwrap()));
        assert!(special_purpose("fd00:ec2::254".parse().unwrap()));
        assert!(!special_purpose("10.0.0.5".parse().unwrap()));
    }
}
