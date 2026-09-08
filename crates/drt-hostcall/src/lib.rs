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
//!
//! ## The blob lane
//!
//! One thing deliberately does *not* cross as a msgpack value: a column.
//! A million-row `f64` column is eight megabytes that would be walked,
//! copied and re-tagged by the encoder for no purpose, and base64 would be
//! worse again. So [`Reply`] carries a side channel, [`Reply::blobs`],
//! which serde never touches, and the payload references a column by index
//! -- `{dtype, len, blob: <i>}` (`doc/Plan-2026-09.md` §3.2).
//!
//! Three steps, in three places, which is what keeps each of them small:
//!
//! 1. A connector answers with [`column`], wrapping the bytes in a msgpack
//!    ext value whose tag is the dtype. Nothing else in this crate's
//!    encoding uses ext, so the wrapper is unambiguous, and it means the
//!    [`Connector`](../drt_connector/trait.Connector.html) trait needs no
//!    second method and no shipped connector changes.
//! 2. The dispatcher calls [`lift_columns`] on that answer, moving each
//!    column's bytes into `blobs` and leaving the descriptor behind. Every
//!    [`Reply`] a pump sees is already in §3.2's shape.
//! 3. The pump encodes with [`to_wire`], which is where the lane's two
//!    deliveries diverge -- see that function's TODO(A0).

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
    /// The blob lane: raw column bytes, referenced from `value` by index
    /// and **never serialized** (`doc/Plan-2026-09.md` §3.2).
    ///
    /// `serde(skip)` is the whole point rather than an optimisation: these
    /// bytes must not pass through the encoding. A reply decoded from the
    /// wire has an empty lane, which is correct -- the bytes reached the
    /// guest by the other route, and a decoder holding a descriptor whose
    /// blob it cannot see is a decoder that should say so rather than
    /// invent one.
    #[serde(skip)]
    pub blobs: Vec<Vec<u8>>,
}

impl Reply {
    pub fn ok(tok: Token, value: rmpv::Value) -> Self {
        Reply {
            tok: Some(tok),
            status: Status::Ok,
            value: Some(value),
            detail: None,
            blobs: Vec::new(),
        }
    }

    pub fn denied(tok: Token, detail: impl Into<String>) -> Self {
        Reply {
            tok: Some(tok),
            status: Status::Denied,
            value: None,
            detail: Some(detail.into()),
            blobs: Vec::new(),
        }
    }

    pub fn error(tok: Token, detail: impl Into<String>) -> Self {
        Reply {
            tok: Some(tok),
            status: Status::Error,
            value: None,
            detail: Some(detail.into()),
            blobs: Vec::new(),
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
            blobs: Vec::new(),
        }
    }
}

// ---------------------------------------------------------------------------
// The blob lane (doc/Plan-2026-09.md §3.2)
// ---------------------------------------------------------------------------

/// The element types a column can have, with the codes `dv.h` fixes:
/// `0=f64 1=i64 2=u8` (§3.1). Not an open set -- `dv_array_adopt` takes
/// exactly these three, so a fourth is a change to `dv.h` and not to this.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Dtype {
    F64,
    I64,
    U8,
}

/// The first of the three msgpack ext tags a column travels under between a
/// connector and the dispatcher, one per dtype, contiguous from here.
///
/// Application-defined ext types are `0..=127`; nothing else in this
/// encoding uses ext at all, which is why a column can be recognised by its
/// wrapper alone and no connector needs a second trait method to answer
/// one.
pub const COLUMN_EXT_BASE: i8 = 0x70;

impl Dtype {
    /// The code that goes in a descriptor's `dtype`, and into
    /// `dv_array_adopt`.
    pub fn code(self) -> u8 {
        match self {
            Dtype::F64 => 0,
            Dtype::I64 => 1,
            Dtype::U8 => 2,
        }
    }

    pub fn from_code(code: u8) -> Option<Dtype> {
        match code {
            0 => Some(Dtype::F64),
            1 => Some(Dtype::I64),
            2 => Some(Dtype::U8),
            _ => None,
        }
    }

    /// Bytes per element, which is what turns a byte count into the `len` a
    /// descriptor states and a guest allocates against.
    pub fn width(self) -> usize {
        match self {
            Dtype::F64 | Dtype::I64 => 8,
            Dtype::U8 => 1,
        }
    }

    fn ext_tag(self) -> i8 {
        COLUMN_EXT_BASE + self.code() as i8
    }

    fn from_ext_tag(tag: i8) -> Option<Dtype> {
        u8::try_from(tag - COLUMN_EXT_BASE)
            .ok()
            .and_then(Dtype::from_code)
    }
}

/// Wrap a column for a connector's answer: the bytes, tagged with their
/// dtype, in a value the dispatcher will lift out.
///
/// A connector builds its answer as an ordinary [`rmpv::Value`] and puts
/// one of these wherever a column belongs -- a field of a map, an element
/// of a list. It never sees `blobs` or an index; [`lift_columns`] assigns
/// those, because only the thing walking the whole answer can number them.
///
/// `bytes` is the column's native little-endian representation, the same
/// bytes `dv_array_adopt` takes. A length that is not a whole number of
/// elements is the caller's bug and is left to show as one: the descriptor
/// rounds down and the trailing bytes are still carried, so the mismatch is
/// visible rather than silently trimmed.
pub fn column(dtype: Dtype, bytes: Vec<u8>) -> rmpv::Value {
    rmpv::Value::Ext(dtype.ext_tag(), bytes)
}

/// Move every column in `value` into `blobs`, leaving `{dtype, len, blob}`
/// where each was.
///
/// Takes `value` by value and moves each column's `Vec<u8>` out of it, so a
/// column is never copied on this path however large it is -- which is the
/// reason the lane exists. Returns the answer in §3.2's shape.
///
/// Recursive over maps and arrays, since a connector may answer several
/// columns and will usually answer them inside a table.
pub fn lift_columns(value: rmpv::Value, blobs: &mut Vec<Vec<u8>>) -> rmpv::Value {
    match value {
        rmpv::Value::Ext(tag, bytes) => match Dtype::from_ext_tag(tag) {
            Some(dtype) => {
                let len = bytes.len() / dtype.width();
                let index = blobs.len();
                blobs.push(bytes);
                rmpv::Value::Map(vec![
                    ("dtype".into(), rmpv::Value::from(dtype.code())),
                    ("len".into(), rmpv::Value::from(len as u64)),
                    ("blob".into(), rmpv::Value::from(index as u64)),
                ])
            }
            // An ext this crate did not write. Carried through untouched:
            // it is somebody's value, not a column.
            None => rmpv::Value::Ext(tag, bytes),
        },
        rmpv::Value::Array(items) => {
            rmpv::Value::Array(items.into_iter().map(|v| lift_columns(v, blobs)).collect())
        }
        rmpv::Value::Map(fields) => rmpv::Value::Map(
            fields
                .into_iter()
                .map(|(k, v)| (k, lift_columns(v, blobs)))
                .collect(),
        ),
        other => other,
    }
}

/// Encode a reply for the guest's reply queue: the one encode on the
/// hostcall path, because it is the one that knows about the lane.
///
/// TODO(A0): **the lane has two deliveries and only one exists yet.**
///
/// The one this round's `dv.h` describes: the descriptors go out as they
/// are, the host stages `blobs` on the instance, and the guest reads each
/// with `dv_reply_blob(inst, index, &ptr, &len)` and hands it to
/// `dv_array_adopt`, so the bytes never enter the encoding at all. That
/// needs `dv_reply_blob`, which arrives with session A's A0 milestone
/// (`doc/Plan-2026-09.md` §3.1); DRT reaches the core through the safe
/// `diluvium` crate, so it becomes callable here when that pin lands.
///
/// The one below, until then: each descriptor is replaced by its blob's
/// bytes as a msgpack `bin`, which a guest reads as a Lua string. That is
/// **not a stopgap shape** -- it is exactly what §3.1 specifies a build
/// without `numeric` to do, where `dv_array_adopt` "pushes a Lua string
/// copy instead". So the guest-visible behaviour here is already the
/// documented no-`numeric` behaviour, and A0 does not change what a guest
/// without arrays sees; it adds the path for a guest with them, and takes
/// away the one copy this makes.
///
/// Never base64, on either path (§3.2).
///
/// Takes the reply **by value** so the inlining below moves each blob
/// rather than copying it: the numeric spec's Stage 4 acceptance is that a
/// ten-million-row column loads with one buffer copy, and a `&Reply` here
/// would have made that two.
pub fn to_wire(reply: Reply) -> Result<Vec<u8>, rmp_serde::encode::Error> {
    if reply.blobs.is_empty() {
        return to_bytes(&reply);
    }
    let Reply {
        tok,
        status,
        value,
        detail,
        blobs,
    } = reply;
    let inlined = Reply {
        tok,
        status,
        value: value.map(|v| inline_blobs(v, blobs)),
        detail,
        blobs: Vec::new(),
    };
    to_bytes(&inlined)
}

// depth: the pre-A0 inlining, which is `lift_columns` run backwards
//
// A descriptor whose `blob` names no blob is left as it is rather than
// dropped or faked. It cannot happen through `lift_columns`, and if it ever
// does the guest should receive the descriptor and be able to say so.
//
// Each blob is moved out of the lane rather than cloned, so a column that
// arrived here without being copied leaves the same way. An index named
// twice yields an empty second buffer -- a bug made visible, rather than
// one column silently aliased onto two names.
fn inline_blobs(value: rmpv::Value, mut blobs: Vec<Vec<u8>>) -> rmpv::Value {
    fn walk(value: rmpv::Value, blobs: &mut [Vec<u8>]) -> rmpv::Value {
        match value {
            rmpv::Value::Map(fields) => {
                if let Some(index) = blob_index(&fields) {
                    if let Some(bytes) = blobs.get_mut(index) {
                        return rmpv::Value::Binary(std::mem::take(bytes));
                    }
                }
                rmpv::Value::Map(
                    fields
                        .into_iter()
                        .map(|(k, v)| (k, walk(v, blobs)))
                        .collect(),
                )
            }
            rmpv::Value::Array(items) => {
                rmpv::Value::Array(items.into_iter().map(|v| walk(v, blobs)).collect())
            }
            other => other,
        }
    }
    walk(value, &mut blobs)
}

/// The `blob` index of a map that is a column descriptor, or `None` for a
/// map that is an ordinary answer. All three fields, because a guest map
/// that happens to carry a `blob` key is not a descriptor.
fn blob_index(fields: &[(rmpv::Value, rmpv::Value)]) -> Option<usize> {
    let field = |name: &str| {
        fields
            .iter()
            .find(|(k, _)| k.as_str() == Some(name))
            .map(|(_, v)| v)
    };
    if fields.len() != 3 || field("dtype").is_none() || field("len").is_none() {
        return None;
    }
    field("blob")?.as_u64().map(|i| i as usize)
}

/// Encode any of the types here as a msgpack map with field names — the only
/// encoding that crosses the boundary.
///
/// A [`Reply`] that may carry columns goes through [`to_wire`] instead:
/// this one drops the lane on the floor, because serde cannot see it.
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

    // --- the blob lane -------------------------------------------------

    /// Three f64 as their little-endian bytes: 1.0, 2.0, 3.0.
    fn three_doubles() -> Vec<u8> {
        [1.0f64, 2.0, 3.0]
            .iter()
            .flat_map(|d| d.to_le_bytes())
            .collect()
    }

    #[test]
    fn lifting_a_column_leaves_a_descriptor_and_moves_the_bytes() {
        let answer = rmpv::Value::Map(vec![
            ("rows".into(), rmpv::Value::from(3u64)),
            ("price".into(), column(Dtype::F64, three_doubles())),
        ]);
        let mut blobs = Vec::new();
        let lifted = lift_columns(answer, &mut blobs);

        assert_eq!(blobs.len(), 1);
        assert_eq!(blobs[0], three_doubles());
        let fields = lifted.as_map().unwrap();
        let price = &fields
            .iter()
            .find(|(k, _)| k.as_str() == Some("price"))
            .unwrap()
            .1;
        let d = price.as_map().unwrap();
        let get = |n: &str| {
            d.iter()
                .find(|(k, _)| k.as_str() == Some(n))
                .unwrap()
                .1
                .as_u64()
                .unwrap()
        };
        // `{dtype, len, blob}` -- len in ELEMENTS, not bytes, which is what
        // a guest allocates against.
        assert_eq!(get("dtype"), Dtype::F64.code() as u64);
        assert_eq!(get("len"), 3);
        assert_eq!(get("blob"), 0);
    }

    #[test]
    fn columns_are_numbered_in_the_order_they_are_walked() {
        let answer = rmpv::Value::Array(vec![
            column(Dtype::U8, vec![9, 9]),
            rmpv::Value::Map(vec![("inner".into(), column(Dtype::I64, vec![0; 16]))]),
        ]);
        let mut blobs = Vec::new();
        let lifted = lift_columns(answer, &mut blobs);
        assert_eq!(blobs.len(), 2);
        assert_eq!(blobs[0], vec![9, 9]);
        assert_eq!(blobs[1].len(), 16);
        // Nested and top-level alike, and an i64 column's len is bytes/8.
        let items = lifted.as_array().unwrap();
        let first = items[0].as_map().unwrap();
        assert_eq!(
            first
                .iter()
                .find(|(k, _)| k.as_str() == Some("len"))
                .unwrap()
                .1
                .as_u64(),
            Some(2)
        );
    }

    #[test]
    fn a_reply_with_no_column_encodes_exactly_as_it_always_did() {
        let reply = Reply::ok(1, rmpv::Value::from("plain"));
        assert_eq!(to_wire(reply.clone()).unwrap(), to_bytes(&reply).unwrap());
    }

    /// The pre-A0 delivery: the descriptor is replaced by the bytes, as a
    /// msgpack `bin`, which is a Lua string to a guest without `numeric`.
    /// Never base64, and never a nested map the guest would have to walk.
    #[test]
    fn to_wire_puts_the_bytes_back_where_the_descriptor_was() {
        let mut blobs = Vec::new();
        let value = lift_columns(
            rmpv::Value::Map(vec![("price".into(), column(Dtype::F64, three_doubles()))]),
            &mut blobs,
        );
        let mut reply = Reply::ok(4, value);
        reply.blobs = blobs;

        let decoded: rmpv::Value = rmp_serde::from_slice(&to_wire(reply).unwrap()).unwrap();
        let fields = decoded.as_map().unwrap();
        let value = &fields
            .iter()
            .find(|(k, _)| k.as_str() == Some("value"))
            .unwrap()
            .1;
        let price = &value
            .as_map()
            .unwrap()
            .iter()
            .find(|(k, _)| k.as_str() == Some("price"))
            .unwrap()
            .1;
        assert_eq!(price.as_slice(), Some(three_doubles().as_slice()));

        // And the lane itself never appears on the wire, under any name.
        let keys: Vec<_> = fields.iter().filter_map(|(k, _)| k.as_str()).collect();
        assert!(
            !keys.contains(&"blobs"),
            "the lane is not encoded: {keys:?}"
        );
    }

    /// A guest map that happens to carry a `blob` key is a guest map. All
    /// three fields and nothing else, or it is not a descriptor.
    #[test]
    fn an_ordinary_map_is_not_mistaken_for_a_descriptor() {
        let value = rmpv::Value::Map(vec![
            ("blob".into(), rmpv::Value::from(0u64)),
            ("note".into(), rmpv::Value::from("mine")),
        ]);
        let mut reply = Reply::ok(1, value.clone());
        reply.blobs = vec![vec![1, 2, 3]];
        let decoded: rmpv::Value = rmp_serde::from_slice(&to_wire(reply).unwrap()).unwrap();
        let fields = decoded.as_map().unwrap();
        let out = &fields
            .iter()
            .find(|(k, _)| k.as_str() == Some("value"))
            .unwrap()
            .1;
        assert_eq!(out, &value);
    }

    /// An ext value this crate did not write is somebody's value, carried
    /// through rather than swallowed as a column.
    #[test]
    fn an_unknown_ext_is_not_a_column() {
        let mine = rmpv::Value::Ext(3, vec![1, 2]);
        let mut blobs = Vec::new();
        assert_eq!(lift_columns(mine.clone(), &mut blobs), mine);
        assert!(blobs.is_empty());
    }

    #[test]
    fn dtype_codes_are_the_ones_dv_h_fixes() {
        for (dtype, code, width) in [
            (Dtype::F64, 0u8, 8usize),
            (Dtype::I64, 1, 8),
            (Dtype::U8, 2, 1),
        ] {
            assert_eq!(dtype.code(), code);
            assert_eq!(dtype.width(), width);
            assert_eq!(Dtype::from_code(code), Some(dtype));
            assert_eq!(Dtype::from_ext_tag(dtype.ext_tag()), Some(dtype));
        }
        assert_eq!(Dtype::from_code(3), None);
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
