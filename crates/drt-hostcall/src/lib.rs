//! The hostcall encoding as serde types.
//!
//! `doc/Hostcall.md` is the normative text; this crate is its implementation
//! and must never drift from it. The two-sentence version: a hostcall is not
//! an ABI, it is a message on a queue the host drains and an answer on a queue
//! the host pushes to, correlated by a token the guest chooses and the host
//! echoes verbatim.
//!
//! Field names cross the boundary: everything encodes as msgpack *maps*, so
//! serialization goes through [`to_bytes`]/rmp-serde's `to_vec_named`, the
//! same convention as the safe `diluvium` crate.

use serde::{Deserialize, Serialize};

/// A token value. Chosen by the guest, echoed verbatim by the host, never
/// interpreted by it. An integer rather than a string because it is compared,
/// not read.
pub type Token = u64;

/// The `host` guest library allocates its tokens from `2^30` upward, so a
/// program that also pushes raw requests on the same queue pair keeps its own
/// tokens below this and the spaces never meet.
pub const GUEST_LIB_TOKEN_BASE: Token = 1 << 30;

/// The request: a msgpack map pushed by the guest onto its request queue.
///
/// Nothing else is reserved. A call that needs more invents fields inside
/// `args`, not beside it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Request {
    /// The correlation token. Required. Unique among this guest's
    /// *outstanding* requests; reuse after the reply arrives is fine.
    pub tok: Token,
    /// What is being asked: `"time"`, `"fs/read"`, `"js/invoke"`. Namespaced
    /// with `/` like queue names — structural, so a capability grant can
    /// cover a family.
    pub call: String,
    /// The call's arguments, in whatever shape the call defines. Absent means
    /// no arguments.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub args: Option<rmpv::Value>,
}

/// A reply status. **The set will grow**; a consumer switches on the values
/// it knows and treats an unknown status as an error, which is what keeps
/// growth from being a version break — hence [`Status::Other`], which
/// preserves the unknown string rather than failing to decode.
///
/// There is deliberately no `"pending"`: under the queue shape every hostcall
/// is already asynchronous, and "the answer has not arrived" is an empty
/// queue, not a status.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    /// The call succeeded; the reply's `value` is the answer.
    Ok,
    /// A call the guest is not granted, or the host does not connect.
    Denied,
    /// A connected call that failed.
    Error,
    /// A request the host could not read.
    Malformed,
    /// A status this build does not know. Treat as an error.
    #[serde(untagged)]
    Other(String),
}

impl Status {
    /// Whether a correct guest treats this reply as a failure. Everything but
    /// `ok` — unknown statuses included, which is the growth rule.
    pub fn is_failure(&self) -> bool {
        *self != Status::Ok
    }
}

/// The reply: a msgpack map pushed by the host onto the guest's reply queue.
///
/// **Every drained request is answered.** A host that drops requests on the
/// floor has made backpressure invisible. The constructors below are the four
/// legal shapes; they keep `value`/`detail` presence tied to `status` the way
/// the encoding requires.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Reply {
    /// The request's token, echoed verbatim. Omitted only in a `malformed`
    /// reply where no token was readable — an uncorrelatable reply is the
    /// sender's own diagnostic rather than silence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tok: Option<Token>,
    pub status: Status,
    /// Present when `status == "ok"`: the answer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<rmpv::Value>,
    /// Present otherwise: why, worded for the program to read. The same field
    /// name the lifecycle events use, on purpose.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

impl Reply {
    pub fn ok(tok: Token, value: rmpv::Value) -> Self {
        Reply {
            tok: Some(tok),
            status: Status::Ok,
            value: Some(value),
            detail: None,
        }
    }

    pub fn denied(tok: Token, detail: impl Into<String>) -> Self {
        Reply {
            tok: Some(tok),
            status: Status::Denied,
            value: None,
            detail: Some(detail.into()),
        }
    }

    pub fn error(tok: Token, detail: impl Into<String>) -> Self {
        Reply {
            tok: Some(tok),
            status: Status::Error,
            value: None,
            detail: Some(detail.into()),
        }
    }

    /// `tok` is whatever was readable from the unreadable request, or `None`
    /// when none was.
    pub fn malformed(tok: Option<Token>, detail: impl Into<String>) -> Self {
        Reply {
            tok,
            status: Status::Malformed,
            value: None,
            detail: Some(detail.into()),
        }
    }
}

/// Encode any of the types here as a msgpack map with field names — the only
/// encoding that crosses the boundary.
pub fn to_bytes<T: Serialize>(v: &T) -> Result<Vec<u8>, rmp_serde::encode::Error> {
    rmp_serde::to_vec_named(v)
}

pub fn from_bytes<'a, T: Deserialize<'a>>(bytes: &'a [u8]) -> Result<T, rmp_serde::decode::Error> {
    rmp_serde::from_slice(bytes)
}

/// Salvage a token from request bytes that failed to decode as a [`Request`],
/// for the `malformed` reply's echo: a readable msgpack map with an integer
/// `tok` yields that token; anything else yields `None`.
pub fn salvage_token(bytes: &[u8]) -> Option<Token> {
    let value: rmpv::Value = rmp_serde::from_slice(bytes).ok()?;
    let map = value.as_map()?;
    map.iter()
        .find(|(k, _)| k.as_str() == Some("tok"))
        .and_then(|(_, v)| v.as_u64())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_round_trips_as_named_map() {
        let req = Request {
            tok: 7,
            call: "fs/read".into(),
            args: Some(rmpv::Value::from("a.txt")),
        };
        let bytes = to_bytes(&req).unwrap();
        // A map with field names, not an array: the guest-visible encoding.
        let raw: rmpv::Value = rmp_serde::from_slice(&bytes).unwrap();
        assert!(raw.is_map());
        assert_eq!(from_bytes::<Request>(&bytes).unwrap(), req);
    }

    #[test]
    fn absent_args_is_absent_not_nil() {
        let req = Request {
            tok: 1,
            call: "time".into(),
            args: None,
        };
        let raw: rmpv::Value = rmp_serde::from_slice(&to_bytes(&req).unwrap()).unwrap();
        assert_eq!(raw.as_map().unwrap().len(), 2);
    }

    #[test]
    fn statuses_are_the_documented_strings() {
        for (s, name) in [
            (Status::Ok, "ok"),
            (Status::Denied, "denied"),
            (Status::Error, "error"),
            (Status::Malformed, "malformed"),
        ] {
            let bytes = to_bytes(&s).unwrap();
            let as_str: String = from_bytes(&bytes).unwrap();
            assert_eq!(as_str, name);
            assert_eq!(from_bytes::<Status>(&bytes).unwrap(), s);
        }
    }

    #[test]
    fn unknown_status_decodes_and_is_a_failure() {
        let bytes = to_bytes(&"backpressure").unwrap();
        let s: Status = from_bytes(&bytes).unwrap();
        assert_eq!(s, Status::Other("backpressure".into()));
        assert!(s.is_failure());
        assert!(!Status::Ok.is_failure());
        // And it re-encodes as the same string: a relay does not eat growth.
        let out: String = from_bytes(&to_bytes(&s).unwrap()).unwrap();
        assert_eq!(out, "backpressure");
    }

    #[test]
    fn malformed_without_token_omits_the_field() {
        let reply = Reply::malformed(None, "not a map");
        let raw: rmpv::Value = rmp_serde::from_slice(&to_bytes(&reply).unwrap()).unwrap();
        let keys: Vec<_> = raw
            .as_map()
            .unwrap()
            .iter()
            .map(|(k, _)| k.as_str().unwrap())
            .collect();
        assert_eq!(keys, ["status", "detail"]);
        assert_eq!(
            from_bytes::<Reply>(&to_bytes(&reply).unwrap()).unwrap(),
            reply
        );
    }

    #[test]
    fn ok_reply_echoes_token_verbatim() {
        let reply = Reply::ok(GUEST_LIB_TOKEN_BASE + 3, rmpv::Value::from(1234u64));
        let back: Reply = from_bytes(&to_bytes(&reply).unwrap()).unwrap();
        assert_eq!(back.tok, Some(GUEST_LIB_TOKEN_BASE + 3));
        assert_eq!(back.status, Status::Ok);
        assert_eq!(back.value, Some(rmpv::Value::from(1234u64)));
        assert_eq!(back.detail, None);
    }

    #[test]
    fn salvage_finds_a_readable_token_and_nothing_else() {
        // A map with a tok but a missing required field: salvageable.
        let partial = rmpv::Value::Map(vec![("tok".into(), rmpv::Value::from(9u64))]);
        let bytes = rmp_serde::to_vec(&partial).unwrap();
        assert!(from_bytes::<Request>(&bytes).is_err());
        assert_eq!(salvage_token(&bytes), Some(9));
        // Not a map at all: nothing readable.
        assert_eq!(salvage_token(&to_bytes(&"junk").unwrap()), None);
        assert_eq!(salvage_token(&[0xc1]), None);
    }
}
