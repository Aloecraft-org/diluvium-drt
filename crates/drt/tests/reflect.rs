//! `drt netcheck --reflect` against a local edge, through the real binary.
//!
//! The live edge cannot be reached from CI, and the TLS half is
//! `connectors/rest`'s stack unchanged. What is new and worth pinning is the
//! parsing, the keying by `edge`, the address cross-check, and the refusal
//! to call two vantages a comparison.

#![cfg(feature = "netcheck")]

use std::io::{Read, Write};
use std::net::TcpListener;
use std::process::Command;

/// An edge that answers one request with the shape `api/supervisor.lua`
/// builds, then stops. `port`/`edge`/`address` are `Option` so a test can
/// leave one unobserved, which is the case the spec is loudest about.
fn edge(edge_name: Option<&str>, address: Option<&str>, seen_port: Option<u16>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/", listener.local_addr().unwrap());
    let mut observed = Vec::new();
    if let Some(a) = address {
        observed.push(format!("\"address\":\"{a}\""));
    }
    if let Some(p) = seen_port {
        observed.push(format!("\"port\":{p}"));
    }
    if let Some(e) = edge_name {
        observed.push(format!("\"edge\":\"{e}\""));
    }
    let body = format!(
        "{{\"service\":\"reflect\",\"label\":\"reflect\",\"observed\":{{{}}}}}",
        observed.join(",")
    );
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut scratch = [0u8; 2048];
            let _ = stream.read(&mut scratch);
            let _ = stream.write_all(
                format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\n\
                     content-length: {}\r\n\r\n{body}",
                    body.len()
                )
                .as_bytes(),
            );
        }
    });
    url
}

fn netcheck(args: &[&str]) -> String {
    let out = Command::new(env!("CARGO_BIN_EXE_drt"))
        .arg("netcheck")
        .args(args)
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// One edge fills the address and one vantage of the TCP line.
#[test]
fn one_edge_fills_the_address_and_one_vantage() {
    let url = edge(Some("gate1"), Some("203.0.113.7"), Some(51823));
    let text = netcheck(&["--reflect", &url]);
    assert!(text.contains("address    203.0.113.7"), "{text}");
    assert!(
        text.contains("tcp map    51823 (gate1)  (one vantage; not a comparison)"),
        "{text}"
    );
}

/// Two edges are two vantages and **not** a comparison, because each fetch
/// is its own connection with its own source port. Measured against the
/// live edge: two fetches answered 3075 and 56304.
#[test]
fn two_edges_render_both_and_refuse_to_compare_them() {
    let a = edge(Some("gate1"), Some("203.0.113.7"), Some(51823));
    let b = edge(Some("gate2"), Some("203.0.113.7"), Some(51999));
    let text = netcheck(&["--reflect", &a, "--reflect", &b]);
    assert!(text.contains("51823 (gate1), 51999 (gate2)"), "{text}");
    assert!(
        text.contains("separate source ports; not a comparison"),
        "{text}"
    );
    assert!(!text.contains("per-destination"), "{text}");
}

/// An unobserved port is not measured, never zero — the spec is explicit,
/// and a zero here would read as a real port and produce a wrong comparison.
#[test]
fn an_edge_that_observes_no_port_is_not_a_zero() {
    let url = edge(Some("gate1"), Some("203.0.113.7"), None);
    let text = netcheck(&["--reflect", &url]);
    assert!(text.contains("address    203.0.113.7"), "{text}");
    assert!(!text.contains(" 0 (gate1)"), "a zero port: {text}");
    assert!(
        text.contains("(gate1)"),
        "the vantage is still named: {text}"
    );
}

/// An edge that names no `edge` is keyed by its URL. Inventing a name would
/// make two anonymous edges look like one.
#[test]
fn an_edge_that_does_not_name_itself_is_keyed_by_its_url() {
    let url = edge(None, Some("203.0.113.7"), Some(51823));
    let text = netcheck(&["--reflect", &url]);
    assert!(text.contains(&format!("({url})")), "{text}");
}

/// An edge that is not there says so, and does not become a finding about
/// the network.
#[test]
fn an_edge_that_does_not_answer_says_why_and_changes_no_verdict() {
    let port = {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let p = l.local_addr().unwrap().port();
        drop(l);
        p
    };
    let text = netcheck(&["--reflect", &format!("http://127.0.0.1:{port}/")]);
    assert!(text.contains("tcp map    not measured ("), "{text}");
    assert!(text.contains("connect:"), "the reason, not a shrug: {text}");
    assert!(
        text.starts_with("relay"),
        "silence is not a finding: {text}"
    );
}

/// A reflect address that disagrees with STUN's is recorded rather than
/// resolved: they measure different protocols, and a network may egress
/// differently for each.
#[test]
fn a_reflect_address_never_silently_replaces_one_already_measured() {
    // No STUN server is reachable here, so this pins the simpler half: the
    // first edge's address is taken, and a second edge disagreeing with it
    // is reported instead of overwriting it.
    let a = edge(Some("gate1"), Some("203.0.113.7"), Some(51823));
    let b = edge(Some("gate2"), Some("198.51.100.9"), Some(51999));
    let text = netcheck(&["--reflect", &a, "--reflect", &b]);
    assert!(text.contains("address    203.0.113.7"), "{text}");
    assert!(
        text.contains("address    203.0.113.7 (gate2 saw 198.51.100.9, over TCP)"),
        "a disagreement recorded and not rendered is a disagreement not recorded: {text}"
    );
}
