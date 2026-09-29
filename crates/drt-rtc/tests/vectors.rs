//! The browser access test vectors (`doc/BrowserAccess.md`): records, the
//! answers built from them, refusals, and Wisp packets, as the browser
//! client's suite loads them.
//!
//! The file is generated from this implementation and checked against it:
//! this test fails when the two disagree, and `DRT_WRITE_VECTORS=1` rewrites
//! the file instead. Nobody edits it by hand; a change is made here, the
//! file regenerated, and the diff reviewed as the wire change it is.
//!
//! ## surface block
//!
//! - Entry point: [`the_vectors_file_is_what_this_implementation_says`].
//! - Configurable: [`FILE`], [`WRITE`].
//! - Fan-out: [`records`], [`refusals`], [`packets`], one per section of the
//!   file.

use drt_rtc::record::{answer_sdp, fingerprint_hex, RecordError};
use drt_rtc::wisp::{self, reason};
use drt_rtc::Record;
use serde_json::{json, Value};

const FILE: &str = "vectors/browser-access-v1.json";
const WRITE: &str = "DRT_WRITE_VECTORS";

/// A digest nobody will mistake for a real certificate's: bytes 0..32.
fn digest(seed: u8) -> [u8; 32] {
    std::array::from_fn(|i| seed.wrapping_add(i as u8))
}

fn records() -> Vec<Value> {
    let cases = [
        (
            "host with a host and a server-reflexive candidate",
            Record {
                ufrag: "Xk3fQ9aBc2Dd7eFg".into(),
                pwd: "8bqS0lK1vT6YpR2eWm4nHc".into(),
                fingerprint: digest(0x10),
                candidates: vec![
                    "candidate:1 1 udp 2130706431 192.168.1.20 50212 typ host".into(),
                    "candidate:2 1 udp 1694498815 203.0.113.7 50212 typ srflx raddr 0.0.0.0 rport 0".into(),
                ],
            },
            "0",
        ),
        (
            "host publishing only its public candidate",
            Record {
                ufrag: "HostOnlyPub1".into(),
                pwd: "p/+0123456789abcdefghijklmnop".into(),
                fingerprint: digest(0xA0),
                candidates: vec![
                    "candidate:2 1 udp 1694498815 203.0.113.7 50212 typ srflx raddr 0.0.0.0 rport 0".into(),
                ],
            },
            "data",
        ),
        (
            "browser record: srflx only, extensions stripped, no mDNS",
            Record {
                ufrag: "bRwS".into(),
                pwd: "Zm9vYmFyYmF6cXV4cXV1eA".into(),
                fingerprint: digest(0x42),
                candidates: vec![
                    "candidate:842163049 1 udp 1677729535 198.51.100.23 61234 typ srflx raddr 0.0.0.0 rport 0".into(),
                ],
            },
            "0",
        ),
        (
            "a reader skips a TCP and a relay line, and keeps the UDP one (§2.1)",
            Record {
                ufrag: "SkipTcpRelay".into(),
                pwd: "0123456789abcdefghijKL".into(),
                fingerprint: digest(0x60),
                candidates: vec![
                    "candidate:1 1 udp 2130706430 192.0.2.1 50001 typ host".into(),
                    "candidate:4 1 udp 41885439 198.51.100.1 3478 typ relay raddr 0.0.0.0 rport 0".into(),
                    "candidate:5 1 tcp 1518280447 192.0.2.5 9 typ host tcptype active".into(),
                ],
            },
            "0",
        ),
        (
            "a reader skips an mDNS name and a line that does not read as a candidate",
            Record {
                ufrag: "SkipMdns".into(),
                pwd: "0123456789abcdefghijKL".into(),
                fingerprint: digest(0x70),
                candidates: vec![
                    "candidate:1 1 udp 2113937151 3f1a7c2e-0000-4000-8000-000000000000.local 61234 typ host".into(),
                    "candidate:not a candidate at all".into(),
                    "candidate:2 1 UDP 1694498815 203.0.113.7 50212 typ srflx raddr 0.0.0.0 rport 0".into(),
                ],
            },
            "0",
        ),
        (
            "no candidates at all (the browser's checks still reach the host)",
            Record {
                ufrag: "Empty0000".into(),
                pwd: "AAAAAAAAAAAAAAAAAAAAAA".into(),
                fingerprint: digest(0xFF),
                candidates: vec![],
            },
            "0",
        ),
    ];
    cases
        .into_iter()
        .map(|(name, r, mid)| {
            // `rtc` is what was published; `decoded` and `answer_sdp` are
            // what a reader makes of it, so a line §2.1 skips is in the
            // first and in neither of the others.
            let rtc = r.encode().expect("a vector record encodes");
            let r = Record::decode(&rtc).expect("a vector record decodes");
            json!({
                "name": name,
                "rtc": rtc,
                "bytes": rtc.len(),
                "decoded": {
                    "u": r.ufrag,
                    "p": r.pwd,
                    "f_hex": fingerprint_hex(&r.fingerprint),
                    "c": r.candidates,
                },
                "mid": mid,
                "answer_sdp": answer_sdp(&r, mid),
            })
        })
        .collect()
}

/// Records a reader must refuse, with the rule each breaks.
fn refusals() -> Vec<Value> {
    let f = "EBESExQVFhcYGRobHB0eHyAhIiMkJSYnKCkqKywtLi8=";
    let cases: Vec<(&str, String, RecordError)> = vec![
        (
            "version 2",
            format!(r#"{{"v":2,"u":"abcd","p":"0123456789012345678901","f":"{f}","c":[]}}"#),
            RecordError::Version,
        ),
        (
            "ufrag shorter than 4",
            format!(r#"{{"v":1,"u":"abc","p":"0123456789012345678901","f":"{f}","c":[]}}"#),
            RecordError::Ufrag,
        ),
        (
            "ufrag with a character outside ice-char",
            format!(r#"{{"v":1,"u":"ab-cd","p":"0123456789012345678901","f":"{f}","c":[]}}"#),
            RecordError::Ufrag,
        ),
        (
            "password shorter than 22",
            format!(r#"{{"v":1,"u":"abcd","p":"012345678901234567890","f":"{f}","c":[]}}"#),
            RecordError::Pwd,
        ),
        (
            "fingerprint of 20 bytes (sha-1)",
            r#"{"v":1,"u":"abcd","p":"0123456789012345678901","f":"EBESExQVFhcYGRobHB0eHyAhIiM=","c":[]}"#.into(),
            RecordError::Fingerprint,
        ),
        (
            "fingerprint as hex, not base64",
            r#"{"v":1,"u":"abcd","p":"0123456789012345678901","f":"10:11:12:13:14:15:16:17:18:19:1A:1B:1C:1D:1E","c":[]}"#.into(),
            RecordError::Fingerprint,
        ),
        (
            "candidate with an a= prefix",
            format!(
                r#"{{"v":1,"u":"abcd","p":"0123456789012345678901","f":"{f}","c":["a=candidate:1 1 udp 2130706431 10.0.0.1 5000 typ host"]}}"#
            ),
            RecordError::Candidate("a=candidate:1 1 udp 2130706431 10.0.0.1 5000 typ host".into()),
        ),
        (
            "missing c",
            format!(r#"{{"v":1,"u":"abcd","p":"0123456789012345678901","f":"{f}"}}"#),
            RecordError::Missing("c"),
        ),
        (
            "not an object",
            r#"["v",1]"#.into(),
            RecordError::NotAnObject,
        ),
    ];
    cases
        .into_iter()
        .map(|(name, rtc, want)| {
            assert_eq!(Record::decode(&rtc), Err(want.clone()), "{name}");
            json!({"name": name, "rtc": rtc, "error": format!("{want:?}").split('(').next().unwrap()})
        })
        .collect()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn packets() -> Vec<Value> {
    let cases: Vec<(&str, Vec<u8>)> = vec![
        (
            "CONTINUE on stream 0: initial credit 128, and the v1 marker",
            wisp::cont(0, 128),
        ),
        (
            "CONNECT stream 1, TCP, 127.0.0.1:8123",
            wisp::connect(1, wisp::STREAM_TCP, 8123, "127.0.0.1"),
        ),
        (
            "CONNECT stream 0x01020304, TCP, [fd00::1]:22",
            wisp::connect(0x0102_0304, wisp::STREAM_TCP, 22, "fd00::1"),
        ),
        (
            "DATA stream 1, 'GET / HTTP/1.1\\r\\n'",
            wisp::data(1, b"GET / HTTP/1.1\r\n"),
        ),
        ("CONTINUE stream 1, 64 remaining", wisp::cont(1, 64)),
        (
            "CLOSE stream 1, voluntary (0x02)",
            wisp::close(1, reason::VOLUNTARY),
        ),
        (
            "CLOSE stream 2, blocked by scope (0x48)",
            wisp::close(2, reason::BLOCKED),
        ),
    ];
    cases
        .into_iter()
        .map(|(name, bytes)| json!({"name": name, "hex": hex(&bytes)}))
        .collect()
}

#[test]
fn the_vectors_file_is_what_this_implementation_says() {
    let want = json!({
        "v": 1,
        "about": "Test vectors for doc/BrowserAccess.md. Generated by crates/drt-rtc/tests/vectors.rs; \
                  do not edit. `rtc` is the presence string; `answer_sdp` is what a browser builds \
                  from that record and the offer's a=mid (lines end CRLF); `hex` is one data channel \
                  message.",
        "records": records(),
        "invalid_records": refusals(),
        "wisp": packets(),
    });
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(FILE);
    let text = serde_json::to_string_pretty(&want).unwrap() + "\n";
    if std::env::var_os(WRITE).is_some() {
        std::fs::write(&path, &text).unwrap();
        return;
    }
    let have = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("{}: {e}; run with {WRITE}=1 to write it", path.display()));
    // Compared as values, not text: an `--all-features` build unifies
    // `serde_json/preserve_order` in (drt-config's `_preserve-order-test`),
    // which changes the order `json!` writes keys in and nothing else.
    let have: Value = serde_json::from_str(&have).expect("the vectors file is JSON");
    assert!(
        have == want,
        "{} disagrees with this implementation; rerun with {WRITE}=1 and review the diff",
        path.display()
    );
    // And every record in the file decodes to the lines it says a reader
    // keeps, and a record that has been read reads the same again: the
    // §2.1 skip happens once, on the way in.
    for r in want["records"].as_array().unwrap() {
        let rtc = r["rtc"].as_str().unwrap();
        assert!(rtc.len() <= drt_rtc::record::MAX_BYTES);
        let decoded = Record::decode(rtc).unwrap();
        assert_eq!(serde_json::json!(decoded.candidates), r["decoded"]["c"]);
        let again = decoded.encode().unwrap();
        assert_eq!(Record::decode(&again).unwrap(), decoded);
    }
}
