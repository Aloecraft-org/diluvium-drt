//! Endpoint refs — the distribution seam (SPEC.md §10).
//!
//! Refs are opaque to guests: msgpack ext type `0x02`, which guests cannot
//! parse, so any change to this encoding is invisible to every guest ever
//! written. What DRT minted process-locally in a prototype would have been an
//! index; it is instead this small tagged encoding, resolved at bind time,
//! because **refs are captured inside snapshots** — a durable agent's
//! snapshot restored in another process or machine next week must still say
//! which endpoint it held, and a process-local index would make every stored
//! snapshot untranslatable.
//!
//! `local` is the only scheme implemented in v1. Non-local schemes resolve
//! through ego-transport when distribution lands — additive, no format break:
//! the payload is a named msgpack map, so fields grow without re-tagging.

use serde::{Deserialize, Serialize};

/// The msgpack ext type tag for an endpoint ref, from the dv ext registry
/// (covered by `DV_ABI_VERSION`).
pub const ENDPOINT_EXT_TYPE: i8 = 0x02;

/// The one scheme v1 resolves.
pub const SCHEME_LOCAL: &str = "local";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EndpointRef {
    /// `local` in v1; `ssh` and friends resolve through ego-transport later.
    pub scheme: String,
    /// Scheme-specific: for `local`, the instance/endpoint name the swarm's
    /// table resolves.
    pub address: String,
    /// The minting node's identity stamp — the same identity `dv_snapshot`'s
    /// `host` argument carries, derivable from the SSH host key. Absent means
    /// minted without one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity: Option<String>,
}

#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum RefError {
    #[error("not an endpoint ref: msgpack ext {found}, want {ENDPOINT_EXT_TYPE}")]
    WrongExtType { found: i8 },
    #[error("not an endpoint ref: not a msgpack ext value")]
    NotExt,
    #[error("unreadable endpoint ref payload: {0}")]
    Payload(String),
}

impl EndpointRef {
    pub fn local(address: impl Into<String>, identity: Option<String>) -> Self {
        EndpointRef {
            scheme: SCHEME_LOCAL.into(),
            address: address.into(),
            identity,
        }
    }

    /// The value a guest sees in a message: opaque ext bytes.
    pub fn to_value(&self) -> rmpv::Value {
        let payload = rmp_serde::to_vec_named(self).expect("a ref of three strings serializes");
        rmpv::Value::Ext(ENDPOINT_EXT_TYPE, payload)
    }

    pub fn from_value(value: &rmpv::Value) -> Result<Self, RefError> {
        match value {
            rmpv::Value::Ext(tag, payload) if *tag == ENDPOINT_EXT_TYPE => {
                rmp_serde::from_slice(payload).map_err(|e| RefError::Payload(e.to_string()))
            }
            rmpv::Value::Ext(tag, _) => Err(RefError::WrongExtType { found: *tag }),
            _ => Err(RefError::NotExt),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_through_the_ext_value() {
        let r = EndpointRef::local("agent-7/inbox", Some("node-key-fingerprint".into()));
        let v = r.to_value();
        assert!(matches!(v, rmpv::Value::Ext(ENDPOINT_EXT_TYPE, _)));
        assert_eq!(EndpointRef::from_value(&v).unwrap(), r);
    }

    #[test]
    fn refuses_what_is_not_a_ref() {
        assert_eq!(
            EndpointRef::from_value(&rmpv::Value::from(7)).unwrap_err(),
            RefError::NotExt
        );
        assert_eq!(
            EndpointRef::from_value(&rmpv::Value::Ext(0x01, vec![])).unwrap_err(),
            RefError::WrongExtType { found: 0x01 }
        );
        assert!(matches!(
            EndpointRef::from_value(&rmpv::Value::Ext(ENDPOINT_EXT_TYPE, vec![0xc1])),
            Err(RefError::Payload(_))
        ));
    }

    #[test]
    fn growth_is_additive_unknown_fields_read_fine() {
        // A future ref with an extra field still decodes: no format break.
        let future = rmpv::Value::Map(vec![
            ("scheme".into(), "local".into()),
            ("address".into(), "a/inbox".into()),
            ("hops".into(), rmpv::Value::from(3)),
        ]);
        let payload = rmp_serde::to_vec_named(&future).unwrap();
        let v = rmpv::Value::Ext(ENDPOINT_EXT_TYPE, payload);
        let r = EndpointRef::from_value(&v).unwrap();
        assert_eq!(r.address, "a/inbox");
        assert_eq!(r.identity, None);
    }
}
