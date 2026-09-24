//! Wisp v1 packets (`MercuryWorkshop/wisp-protocol`, protocol.md 1.2), with
//! a data channel message in place of a WebSocket frame. The profile the
//! host serves is `doc/BrowserAccess.md` §6; this module is only the bytes.
//!
//! ## surface block
//!
//! - Entry points: [`parse`], [`connect`], [`data`], [`cont`], [`close`].
//! - Configurable: [`MAX_PAYLOAD`], derived from the wire's 16 KiB message
//!   cap; not a knob.
//! - Fan-out: [`Packet`], one variant per packet type, and [`reason`], the
//!   close codes.

/// Every message on either channel is at most 16 KiB (§4).
pub const MAX_MESSAGE: usize = 16 * 1024;
/// Type byte plus a little-endian `u32` stream id.
pub const HEADER: usize = 5;
/// The most `DATA` one packet carries.
pub const MAX_PAYLOAD: usize = MAX_MESSAGE - HEADER;

pub const CONNECT: u8 = 0x01;
pub const DATA: u8 = 0x02;
pub const CONTINUE: u8 = 0x03;
pub const CLOSE: u8 = 0x04;

pub const STREAM_TCP: u8 = 0x01;
pub const STREAM_UDP: u8 = 0x02;

/// `CLOSE` reasons, by the names protocol.md gives them.
pub mod reason {
    pub const UNSPECIFIED: u8 = 0x01;
    pub const VOLUNTARY: u8 = 0x02;
    pub const NETWORK_ERROR: u8 = 0x03;
    pub const INVALID: u8 = 0x41;
    pub const UNREACHABLE: u8 = 0x42;
    pub const TIMEOUT: u8 = 0x43;
    pub const REFUSED: u8 = 0x44;
    pub const IDLE: u8 = 0x47;
    pub const BLOCKED: u8 = 0x48;
    pub const THROTTLED: u8 = 0x49;
    pub const CLIENT_ERROR: u8 = 0x81;

    /// The word the host's reports use for a reason code.
    pub fn name(code: u8) -> &'static str {
        match code {
            UNSPECIFIED => "unspecified",
            VOLUNTARY => "voluntary",
            NETWORK_ERROR => "network_error",
            INVALID => "invalid",
            UNREACHABLE => "unreachable",
            TIMEOUT => "timeout",
            REFUSED => "refused",
            IDLE => "idle",
            BLOCKED => "blocked",
            THROTTLED => "throttled",
            CLIENT_ERROR => "client_error",
            _ => "unknown",
        }
    }
}

/// One packet, borrowed from the message it arrived in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Packet<'a> {
    Connect {
        stream: u32,
        kind: u8,
        port: u16,
        host: &'a [u8],
    },
    Data {
        stream: u32,
        payload: &'a [u8],
    },
    Continue {
        stream: u32,
        remaining: u32,
    },
    Close {
        stream: u32,
        reason: u8,
    },
    /// A known type whose payload is too short to be one. The host answers
    /// a malformed `CONNECT` with `CLOSE 0x41` and ignores the rest.
    Malformed {
        kind: u8,
        stream: u32,
    },
    /// A type this profile does not know. Ignored (§6).
    Unknown {
        kind: u8,
        stream: u32,
    },
}

/// Read one message. `None` for a message shorter than the header, which
/// §6 says is ignored.
pub fn parse(msg: &[u8]) -> Option<Packet<'_>> {
    if msg.len() < HEADER {
        return None;
    }
    let kind = msg[0];
    let stream = u32::from_le_bytes([msg[1], msg[2], msg[3], msg[4]]);
    let body = &msg[HEADER..];
    Some(match kind {
        CONNECT if body.len() >= 3 => Packet::Connect {
            stream,
            kind: body[0],
            port: u16::from_le_bytes([body[1], body[2]]),
            host: &body[3..],
        },
        DATA => Packet::Data {
            stream,
            payload: body,
        },
        CONTINUE if body.len() >= 4 => Packet::Continue {
            stream,
            remaining: u32::from_le_bytes([body[0], body[1], body[2], body[3]]),
        },
        CLOSE if !body.is_empty() => Packet::Close {
            stream,
            reason: body[0],
        },
        CONNECT | CONTINUE | CLOSE => Packet::Malformed { kind, stream },
        _ => Packet::Unknown { kind, stream },
    })
}

fn header(kind: u8, stream: u32, extra: usize) -> Vec<u8> {
    let mut v = Vec::with_capacity(HEADER + extra);
    v.push(kind);
    v.extend_from_slice(&stream.to_le_bytes());
    v
}

pub fn connect(stream: u32, kind: u8, port: u16, host: &str) -> Vec<u8> {
    let mut v = header(CONNECT, stream, 3 + host.len());
    v.push(kind);
    v.extend_from_slice(&port.to_le_bytes());
    v.extend_from_slice(host.as_bytes());
    v
}

/// One `DATA` packet. The caller keeps `payload` within [`MAX_PAYLOAD`].
pub fn data(stream: u32, payload: &[u8]) -> Vec<u8> {
    debug_assert!(payload.len() <= MAX_PAYLOAD);
    let mut v = header(DATA, stream, payload.len());
    v.extend_from_slice(payload);
    v
}

pub fn cont(stream: u32, remaining: u32) -> Vec<u8> {
    let mut v = header(CONTINUE, stream, 4);
    v.extend_from_slice(&remaining.to_le_bytes());
    v
}

pub fn close(stream: u32, reason: u8) -> Vec<u8> {
    let mut v = header(CLOSE, stream, 1);
    v.push(reason);
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_packet_round_trips() {
        let c = connect(7, STREAM_TCP, 8123, "127.0.0.1");
        assert_eq!(
            parse(&c),
            Some(Packet::Connect {
                stream: 7,
                kind: STREAM_TCP,
                port: 8123,
                host: b"127.0.0.1"
            })
        );
        assert_eq!(
            parse(&data(7, b"GET /")),
            Some(Packet::Data {
                stream: 7,
                payload: b"GET /"
            })
        );
        assert_eq!(
            parse(&cont(0, 128)),
            Some(Packet::Continue {
                stream: 0,
                remaining: 128
            })
        );
        assert_eq!(
            parse(&close(7, reason::BLOCKED)),
            Some(Packet::Close {
                stream: 7,
                reason: reason::BLOCKED
            })
        );
    }

    #[test]
    fn short_and_unknown_messages() {
        assert_eq!(parse(&[CONNECT, 1, 0, 0]), None);
        assert_eq!(
            parse(&[CONNECT, 1, 0, 0, 0, STREAM_TCP]),
            Some(Packet::Malformed {
                kind: CONNECT,
                stream: 1
            })
        );
        assert_eq!(
            parse(&[0x05, 1, 0, 0, 0]),
            Some(Packet::Unknown { kind: 5, stream: 1 })
        );
    }

    #[test]
    fn little_endian_as_the_spec_says() {
        assert_eq!(
            cont(0x0102_0304, 0x0A0B_0C0D),
            vec![3, 4, 3, 2, 1, 0x0D, 0x0C, 0x0B, 0x0A]
        );
        assert_eq!(
            &connect(1, STREAM_TCP, 0x1F90, "h")[5..8],
            &[STREAM_TCP, 0x90, 0x1F]
        );
    }
}
