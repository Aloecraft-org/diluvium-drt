//! The wire between a host and a plugin: length-prefixed msgpack frames.
//!
//! # Surface
//!
//! Entry points:
//! - [`encode`] — one frame's bytes, length prefix included.
//! - [`decode`] — one frame off the front of a buffer, or `Ok(None)` when
//!   the buffer does not hold a whole one yet.
//! - [`next_wire_id`] — the next id, host-global (see below).
//! - [`Request`], [`Reply`], [`ReplyBody`], [`PluginError`] — the frames.
//!
//! Configurable values:
//! - [`PROTOCOL_VERSION`] — the `version` field every frame carries.
//! - [`MAX_FRAME_BYTES`] — the refusal ceiling on a declared length.
//! - [`LENGTH_PREFIX`] — four, and the reason it is not a varint.
//!
//! Fan-out:
//! - [`ErrorClass`] — `transport`, `plugin`, `capability`, and who may say
//!   each one. `denied` is deliberately absent; see the type.
//! - [`ReplyBody`] — a value or an error, the only two a reply can be.
//!
//! # Why these choices
//!
//! **The id is host-global, not per instance.** `doc/Plugins.md` §2 made
//! this call for the C host and it holds here for the same reason: a guest
//! token is unique only within its own instance, so two instances calling
//! one plugin would collide on the wire. The counter here is process-wide
//! and the guest's own token never reaches the plugin.
//!
//! **The length prefix is a fixed four bytes, big-endian.** A varint would
//! save three bytes on a small frame and cost a plugin author an afternoon;
//! every language can read four bytes and call `ntohl`. It is the C host's
//! framing and a plugin written for that host works here unchanged.
//!
//! **A declared length is checked before anything is allocated.** A plugin
//! is a subprocess the operator wired, not an attacker, but a desynced
//! stream produces a garbage length exactly as readily as a hostile one
//! does, and `Vec::with_capacity` on a garbage `u32` is a four-gigabyte
//! request either way.

use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicU64, Ordering};

/// The `version` field on every frame. A plugin declaring a version this
/// host does not know is refused at the hello, not mid-call.
pub const PROTOCOL_VERSION: u32 = 1;

/// The ceiling on a declared frame length. A plugin with a legitimate
/// reason to exceed it wants a different channel, not a bigger number:
/// the frame is held whole in memory on both sides.
pub const MAX_FRAME_BYTES: u32 = 16 * 1024 * 1024;

/// Four bytes, big-endian, ahead of every frame body.
pub const LENGTH_PREFIX: usize = 4;

/// A call, host to plugin.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Request {
    pub version: u32,
    pub id: u64,
    /// The full call name (`"webauthn/assert"`), as the guest spelled it
    /// and after the dispatcher gated it.
    pub target: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub args: Option<rmpv::Value>,
}

/// An answer, plugin to host.
///
/// `final` says whether more frames for this id are coming. This host
/// currently treats a non-final reply as a protocol error rather than
/// silently dropping it — streaming is in the wire's shape so that adding
/// it later is not a format change, but nothing consumes it yet, and a
/// plugin that streams into a host that ignores the intermediate frames
/// would look like it worked.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Reply {
    pub version: u32,
    pub id: u64,
    #[serde(rename = "final")]
    pub is_final: bool,
    #[serde(flatten)]
    pub body: ReplyBody,
}

/// The two things a reply can be. Untagged because the wire distinguishes
/// them by which key is present, which is the C host's shape.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ReplyBody {
    Ok { value: rmpv::Value },
    Err { error: PluginError },
}

/// A failure the plugin is reporting about a call it accepted.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PluginError {
    pub class: ErrorClass,
    pub code: String,
    pub message: String,
}

/// Who is at fault, in the plugin's own words.
///
/// **`denied` is not here, and that is load-bearing.** Whether a guest
/// holds a capability is the dispatcher's finding, made before a plugin is
/// ever reached. A plugin that could answer `denied` could report a
/// refusal that never happened, and a reader of the log could not tell the
/// two apart. A plugin asked for something it will not do says
/// [`ErrorClass::Capability`] with its own reason.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ErrorClass {
    /// The stream, the framing, the process: the channel itself failed.
    Transport,
    /// The plugin ran and the operation failed on its own terms.
    Plugin,
    /// The plugin declines: not wired for this target, or its own scope
    /// forbids it. Never a statement about the guest's caps.
    Capability,
}

/// What a frame can be wrong about.
#[derive(Debug, thiserror::Error)]
pub enum FrameError {
    #[error(
        "plugin frame declares {len} bytes, over the {MAX_FRAME_BYTES}-byte ceiling; \
         the stream is desynced or the plugin is misframing"
    )]
    TooLarge { len: u32 },

    #[error("plugin frame is not valid msgpack: {0}")]
    Malformed(#[from] rmp_serde::decode::Error),

    #[error("plugin frame could not be encoded: {0}")]
    Unencodable(#[from] rmp_serde::encode::Error),

    #[error(
        "plugin frame declares protocol version {found}, and this host speaks \
         {PROTOCOL_VERSION}"
    )]
    Version { found: u32 },
}

// depth: the counter and the two codec halves.

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

/// The next wire id. Starts at 1 so that a zero read off a desynced stream
/// is never mistaken for a live call.
pub fn next_wire_id() -> u64 {
    NEXT_ID.fetch_add(1, Ordering::Relaxed)
}

/// One frame's bytes: four-byte big-endian length, then the msgpack body.
///
/// Named-map encoding (`to_vec_named`), as everything in this workspace
/// does, so a plugin reads `{"id": …}` and not a positional array.
pub fn encode<T: Serialize>(frame: &T) -> Result<Vec<u8>, FrameError> {
    let body = rmp_serde::to_vec_named(frame)?;
    let mut out = Vec::with_capacity(LENGTH_PREFIX + body.len());
    out.extend_from_slice(&(body.len() as u32).to_be_bytes());
    out.extend_from_slice(&body);
    Ok(out)
}

/// One frame off the front of `buf`, and how many bytes it consumed.
///
/// `Ok(None)` means the buffer does not hold a whole frame yet and the
/// caller should read more — the ordinary case on a polled socket, not a
/// failure. An error here is terminal for the channel: a stream that
/// misframed once cannot be resynchronised, because the next length is
/// read from whatever the previous frame's body happened to contain.
pub fn decode<'a, T: Deserialize<'a>>(buf: &'a [u8]) -> Result<Option<(T, usize)>, FrameError> {
    if buf.len() < LENGTH_PREFIX {
        return Ok(None);
    }
    let len = u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]);
    if len > MAX_FRAME_BYTES {
        return Err(FrameError::TooLarge { len });
    }
    let len = len as usize;
    let end = LENGTH_PREFIX + len;
    if buf.len() < end {
        return Ok(None);
    }
    let frame = rmp_serde::from_slice(&buf[LENGTH_PREFIX..end])?;
    Ok(Some((frame, end)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn a_request() -> Request {
        Request {
            version: PROTOCOL_VERSION,
            id: 7,
            target: "webauthn/assert".into(),
            args: Some(rmpv::Value::from("challenge")),
        }
    }

    #[test]
    fn a_request_survives_the_round_trip() {
        let bytes = encode(&a_request()).unwrap();
        let (back, used) = decode::<Request>(&bytes).unwrap().unwrap();
        assert_eq!(back, a_request());
        assert_eq!(used, bytes.len(), "the whole frame was consumed");
    }

    #[test]
    fn both_reply_shapes_survive_the_round_trip() {
        for body in [
            ReplyBody::Ok {
                value: rmpv::Value::from(42),
            },
            ReplyBody::Err {
                error: PluginError {
                    class: ErrorClass::Plugin,
                    code: "no_credential".into(),
                    message: "no credential for that handle".into(),
                },
            },
        ] {
            let reply = Reply {
                version: PROTOCOL_VERSION,
                id: 9,
                is_final: true,
                body: body.clone(),
            };
            let bytes = encode(&reply).unwrap();
            let (back, _) = decode::<Reply>(&bytes).unwrap().unwrap();
            assert_eq!(back, reply, "{body:?} came back as it went out");
        }
    }

    /// The ordinary case on a polled socket: a frame arrives in pieces.
    #[test]
    fn a_partial_frame_asks_for_more_rather_than_failing() {
        let bytes = encode(&a_request()).unwrap();
        for cut in 0..bytes.len() {
            assert!(
                decode::<Request>(&bytes[..cut]).unwrap().is_none(),
                "{cut} of {} bytes is not yet a frame",
                bytes.len()
            );
        }
        assert!(decode::<Request>(&bytes).unwrap().is_some(), "all of it is");
    }

    /// Two frames back to back: `decode` takes the first and says where it
    /// ended, so the caller can keep the tail.
    #[test]
    fn a_buffer_of_two_frames_yields_them_in_order() {
        let mut buf = encode(&a_request()).unwrap();
        let second = Request {
            id: 8,
            ..a_request()
        };
        buf.extend_from_slice(&encode(&second).unwrap());

        let (first, used) = decode::<Request>(&buf).unwrap().unwrap();
        assert_eq!(first.id, 7);
        let (next, used2) = decode::<Request>(&buf[used..]).unwrap().unwrap();
        assert_eq!(next.id, 8);
        assert_eq!(used + used2, buf.len(), "nothing left over");
    }

    #[test]
    fn an_oversized_length_refuses_before_allocating() {
        let mut bytes = (MAX_FRAME_BYTES + 1).to_be_bytes().to_vec();
        bytes.extend_from_slice(b"never read");
        match decode::<Request>(&bytes) {
            Err(FrameError::TooLarge { len }) => assert_eq!(len, MAX_FRAME_BYTES + 1),
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    /// A length that fits the ceiling but not the body must not be read as
    /// a short frame -- that is how a desync becomes silent corruption.
    #[test]
    fn a_length_longer_than_the_body_waits_rather_than_truncating() {
        let mut bytes = 64u32.to_be_bytes().to_vec();
        bytes.extend_from_slice(b"only ten b");
        assert!(decode::<Request>(&bytes).unwrap().is_none());
    }

    #[test]
    fn a_body_that_is_not_msgpack_is_a_named_failure() {
        let body = b"\xc1\xc1\xc1\xc1";
        let mut bytes = (body.len() as u32).to_be_bytes().to_vec();
        bytes.extend_from_slice(body);
        assert!(matches!(
            decode::<Request>(&bytes),
            Err(FrameError::Malformed(_))
        ));
    }

    #[test]
    fn wire_ids_are_unique_and_never_zero() {
        let ids: Vec<u64> = (0..64).map(|_| next_wire_id()).collect();
        let mut sorted = ids.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), ids.len(), "every id handed out once");
        assert!(ids.iter().all(|&id| id != 0), "zero is never a live call");
    }

    /// The class names on the wire are the C host's, lowercase. A rename
    /// here would make `host.try` print a different sentence on each host.
    #[test]
    fn the_error_classes_spell_themselves_as_the_c_host_does() {
        for (class, spelling) in [
            (ErrorClass::Transport, "transport"),
            (ErrorClass::Plugin, "plugin"),
            (ErrorClass::Capability, "capability"),
        ] {
            let bytes = rmp_serde::to_vec(&class).unwrap();
            let on_the_wire: String = rmp_serde::from_slice(&bytes).unwrap();
            assert_eq!(on_the_wire, spelling);
        }
    }
}
