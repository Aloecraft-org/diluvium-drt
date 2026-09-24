//! The presence record: what each side publishes in a room's `rtc` field,
//! and the answer a browser builds from the host's (`doc/BrowserAccess.md`
//! §2, §3.2).
//!
//! ## surface block
//!
//! - Entry points: [`Record::decode`], [`Record::encode`], [`answer_sdp`],
//!   [`fingerprint_hex`].
//! - Configurable: [`MAX_BYTES`], [`MAX_CANDIDATES`] and the ICE length
//!   bounds below. They are the wire's, so changing one is a `v` bump, not
//!   a tuning knob.
//! - Fan-out: [`RecordError`], one variant per rule a record can break.

use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use serde::Serialize;

/// The record's budget, in UTF-8 bytes of the whole string.
pub const MAX_BYTES: usize = 512;
/// Candidate lines a record may carry.
pub const MAX_CANDIDATES: usize = 8;
/// RFC 8839's floor on a ufrag, and a ceiling that keeps a record in budget.
pub const UFRAG_LEN: std::ops::RangeInclusive<usize> = 4..=32;
/// RFC 8839's floor on a password (128 bits of base64-ish), and a ceiling.
pub const PWD_LEN: std::ops::RangeInclusive<usize> = 22..=64;
/// The one version this module reads and writes.
pub const VERSION: u64 = 1;

/// One peer's ICE credentials, DTLS fingerprint and candidates.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    pub ufrag: String,
    pub pwd: String,
    /// SHA-256 of the peer's DTLS certificate (DER).
    pub fingerprint: [u8; 32],
    /// `candidate:` lines, exactly as RFC 8839 §5.1 spells them.
    pub candidates: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecordError {
    TooLong(usize),
    NotJson(String),
    NotAnObject,
    Version,
    Missing(&'static str),
    WrongType(&'static str),
    Ufrag,
    Pwd,
    Fingerprint,
    TooManyCandidates(usize),
    Candidate(String),
}

impl std::fmt::Display for RecordError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RecordError::TooLong(n) => write!(f, "record is {n} bytes, over {MAX_BYTES}"),
            RecordError::NotJson(e) => write!(f, "record is not JSON: {e}"),
            RecordError::NotAnObject => write!(f, "record is not a JSON object"),
            RecordError::Version => write!(f, "record version is not {VERSION}"),
            RecordError::Missing(k) => write!(f, "record has no `{k}`"),
            RecordError::WrongType(k) => write!(f, "record `{k}` has the wrong type"),
            RecordError::Ufrag => write!(
                f,
                "record `u` is not {}..={} ice-chars",
                UFRAG_LEN.start(),
                UFRAG_LEN.end()
            ),
            RecordError::Pwd => write!(
                f,
                "record `p` is not {}..={} ice-chars",
                PWD_LEN.start(),
                PWD_LEN.end()
            ),
            RecordError::Fingerprint => {
                write!(f, "record `f` is not padded base64 of a 32-byte digest")
            }
            RecordError::TooManyCandidates(n) => {
                write!(f, "record carries {n} candidates, over {MAX_CANDIDATES}")
            }
            RecordError::Candidate(c) => {
                write!(f, "record candidate is not a `candidate:` line: {c}")
            }
        }
    }
}

impl std::error::Error for RecordError {}

/// The field order the host writes. Readers accept any order.
#[derive(Serialize)]
struct Wire<'a> {
    v: u64,
    u: &'a str,
    p: &'a str,
    f: String,
    c: &'a [String],
}

impl Record {
    /// The record as it goes into presence: compact JSON, `v u p f c`.
    /// Refused, like a decode, when it breaks a rule, so a host never
    /// publishes something a browser would refuse.
    pub fn encode(&self) -> Result<String, RecordError> {
        self.check()?;
        let s = serde_json::to_string(&Wire {
            v: VERSION,
            u: &self.ufrag,
            p: &self.pwd,
            f: STANDARD.encode(self.fingerprint),
            c: &self.candidates,
        })
        .expect("a record serializes");
        if s.len() > MAX_BYTES {
            return Err(RecordError::TooLong(s.len()));
        }
        Ok(s)
    }

    /// Read a record from a presence `rtc` string. Unknown keys are ignored;
    /// anything else that breaks §2 refuses the whole record.
    pub fn decode(s: &str) -> Result<Record, RecordError> {
        if s.len() > MAX_BYTES {
            return Err(RecordError::TooLong(s.len()));
        }
        let value: serde_json::Value =
            serde_json::from_str(s).map_err(|e| RecordError::NotJson(e.to_string()))?;
        let obj = value.as_object().ok_or(RecordError::NotAnObject)?;
        let field = |k: &'static str| obj.get(k).ok_or(RecordError::Missing(k));
        let text = |k: &'static str| -> Result<String, RecordError> {
            field(k)?
                .as_str()
                .map(str::to_string)
                .ok_or(RecordError::WrongType(k))
        };

        if field("v")?.as_u64() != Some(VERSION) {
            return Err(RecordError::Version);
        }
        let ufrag = text("u")?;
        let pwd = text("p")?;
        let f = text("f")?;
        let c = field("c")?.as_array().ok_or(RecordError::WrongType("c"))?;
        let mut candidates = Vec::with_capacity(c.len());
        for line in c {
            candidates.push(
                line.as_str()
                    .ok_or(RecordError::WrongType("c"))?
                    .to_string(),
            );
        }
        let fingerprint = decode_fingerprint(&f)?;
        let record = Record {
            ufrag,
            pwd,
            fingerprint,
            candidates,
        };
        record.check()?;
        Ok(record)
    }

    fn check(&self) -> Result<(), RecordError> {
        if !UFRAG_LEN.contains(&self.ufrag.len()) || !self.ufrag.bytes().all(ice_char) {
            return Err(RecordError::Ufrag);
        }
        if !PWD_LEN.contains(&self.pwd.len()) || !self.pwd.bytes().all(ice_char) {
            return Err(RecordError::Pwd);
        }
        if self.candidates.len() > MAX_CANDIDATES {
            return Err(RecordError::TooManyCandidates(self.candidates.len()));
        }
        for c in &self.candidates {
            if !c.starts_with("candidate:") || c.contains(['\r', '\n']) {
                return Err(RecordError::Candidate(c.clone()));
            }
        }
        Ok(())
    }
}

/// RFC 8839 `ice-char`: `ALPHA / DIGIT / "+" / "/"`.
fn ice_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'+' || b == b'/'
}

fn decode_fingerprint(f: &str) -> Result<[u8; 32], RecordError> {
    if f.len() != 44 {
        return Err(RecordError::Fingerprint);
    }
    let bytes = STANDARD.decode(f).map_err(|_| RecordError::Fingerprint)?;
    bytes.try_into().map_err(|_| RecordError::Fingerprint)
}

/// A digest as SDP's `a=fingerprint` spells it: upper-case hex pairs
/// joined by `:`.
pub fn fingerprint_hex(digest: &[u8; 32]) -> String {
    let mut s = String::with_capacity(95);
    for (i, b) in digest.iter().enumerate() {
        if i > 0 {
            s.push(':');
        }
        s.push_str(&format!("{b:02X}"));
    }
    s
}

/// The answer a browser builds from the host's record and its own offer's
/// `a=mid` (§3.2). The reference the vectors carry, byte for byte; the host
/// itself never uses it.
pub fn answer_sdp(host: &Record, mid: &str) -> String {
    let mut lines = vec![
        "v=0".to_string(),
        "o=- 0 2 IN IP4 127.0.0.1".to_string(),
        "s=-".to_string(),
        "t=0 0".to_string(),
        format!("a=group:BUNDLE {mid}"),
        "m=application 9 UDP/DTLS/SCTP webrtc-datachannel".to_string(),
        "c=IN IP4 0.0.0.0".to_string(),
        format!("a=mid:{mid}"),
        format!("a=ice-ufrag:{}", host.ufrag),
        format!("a=ice-pwd:{}", host.pwd),
        format!(
            "a=fingerprint:sha-256 {}",
            fingerprint_hex(&host.fingerprint)
        ),
        "a=setup:passive".to_string(),
        "a=sctp-port:5000".to_string(),
        "a=max-message-size:262144".to_string(),
    ];
    for c in &host.candidates {
        lines.push(format!("a={c}"));
    }
    lines.push("a=end-of-candidates".to_string());
    let mut sdp = lines.join("\r\n");
    sdp.push_str("\r\n");
    sdp
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Record {
        Record {
            ufrag: "Xk3fQ9aB".into(),
            pwd: "8bqS0lK1vT6YpR2eWm4nHc7J".into(),
            fingerprint: [7; 32],
            candidates: vec!["candidate:1 1 udp 2130706431 192.168.1.20 50212 typ host".into()],
        }
    }

    #[test]
    fn a_record_round_trips() {
        let r = sample();
        assert_eq!(Record::decode(&r.encode().unwrap()).unwrap(), r);
    }

    #[test]
    fn keys_in_any_order_and_unknown_keys_are_read() {
        let r = sample();
        let s = format!(
            r#"{{"c":["{}"],"extra":true,"f":"{}","p":"{}","u":"{}","v":1}}"#,
            r.candidates[0],
            STANDARD.encode(r.fingerprint),
            r.pwd,
            r.ufrag
        );
        assert_eq!(Record::decode(&s).unwrap(), r);
    }

    #[test]
    fn each_rule_refuses_by_name() {
        let good = sample().encode().unwrap();
        let cases = [
            (good.replace(r#""v":1"#, r#""v":2"#), RecordError::Version),
            (good.replace("Xk3fQ9aB", "Xk3"), RecordError::Ufrag),
            (good.replace("Xk3fQ9aB", "Xk3f-9aB"), RecordError::Ufrag),
            (
                good.replace("8bqS0lK1vT6YpR2eWm4nHc7J", "short"),
                RecordError::Pwd,
            ),
            (
                good.replace(&STANDARD.encode([7u8; 32]), &STANDARD.encode([7u8; 20])),
                RecordError::Fingerprint,
            ),
            (
                good.replace("candidate:1", "a=candidate:1"),
                RecordError::Candidate(
                    "a=candidate:1 1 udp 2130706431 192.168.1.20 50212 typ host".into(),
                ),
            ),
            ("[]".to_string(), RecordError::NotAnObject),
        ];
        for (s, want) in cases {
            assert_eq!(Record::decode(&s), Err(want), "{s}");
        }
        let long = format!("{}{}", good, " ".repeat(MAX_BYTES));
        assert!(matches!(
            Record::decode(&long),
            Err(RecordError::TooLong(_))
        ));
    }

    #[test]
    fn fingerprint_hex_is_sdp_shaped() {
        let mut d = [0u8; 32];
        d[0] = 0xAB;
        d[31] = 0x01;
        let hex = fingerprint_hex(&d);
        assert!(hex.starts_with("AB:00:"));
        assert!(hex.ends_with(":01"));
        assert_eq!(hex.len(), 95);
    }
}
