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

/// An edge that reports the source port the connection actually came from,
/// which is what an edge's `x-real-port` carries. Anything testing the
/// pinning needs the real port, not a canned one.
fn echoing_edge(edge_name: &str) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/", listener.local_addr().unwrap());
    let name = edge_name.to_string();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let seen = stream.peer_addr().unwrap();
            let mut scratch = [0u8; 2048];
            let _ = stream.read(&mut scratch);
            let body = format!(
                "{{\"observed\":{{\"address\":\"{}\",\"port\":{},\"edge\":\"{name}\"}}}}",
                seen.ip(),
                seen.port()
            );
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

/// Two vantages are pinned and compared by default.
///
/// The flag used to be what turned two vantages into a measurement, and
/// a caller who did not know it got two unrelated observations from a run
/// that could plainly have compared them. Pinning happens whenever more
/// than one fetch is planned now, which is the only time it measures
/// anything; the flag is kept and changes nothing (issue #25).
///
/// `tokio::net::TcpSocket` binds and sets `SO_REUSEADDR` natively, so the
/// `socket2` the work was sized against was never needed.
#[test]
fn two_edges_are_pinned_and_compared_by_default() {
    let a = echoing_edge("gate1");
    let b = echoing_edge("gate2");

    let bare = netcheck(&["--reflect", &a, "--reflect", &b]);
    assert!(
        bare.contains("independent (pinned source port, sequential)"),
        "{bare}"
    );
    // Both vantages observed ONE port, which is the whole measurement.
    let ports: Vec<&str> = bare
        .lines()
        .find(|l| l.contains("tcp map"))
        .unwrap()
        .split_whitespace()
        .filter(|t| t.chars().all(|c| c.is_ascii_digit()) && t.len() > 3)
        .collect();
    assert_eq!(ports.len(), 2, "{bare}");
    assert_eq!(ports[0], ports[1], "one source port, seen twice: {bare}");

    // The flag is a no-op on a run that pins anyway.
    let forced = netcheck(&["--pin-source-port", "--reflect", &a, "--reflect", &b]);
    assert!(
        forced.contains("independent (pinned source port, sequential)"),
        "{forced}"
    );
}

/// One edge named twice is not a comparison, and used to say it was.
///
/// Endpoint-independent means the same external port regardless of
/// *destination*. Two connections to one destination reusing a mapping is
/// what every NAT does, symmetric ones included — so `--reflect URL
/// --reflect URL`, one name typed twice, answered `independent` and would
/// have told a symmetric NAT that it punches. That is the most consequential
/// wrong answer this tool can give, reached by an obvious command.
///
/// What the run really measures is whether the mapping held, which is worth
/// knowing on its own: if it did not, no two-edge comparison can ever
/// succeed on this network and standing up a second vantage buys nothing.
/// Pinned by default now, since two fetches are planned, so the bare
/// command gets the stability check.
#[test]
fn one_edge_asked_twice_is_a_stability_check_not_a_comparison() {
    let a = echoing_edge("gate1");
    let text = netcheck(&["--reflect", &a, "--reflect", &a]);
    assert!(
        !text.contains("independent (pinned"),
        "two views of ONE destination say nothing about endpoint-independence: {text}"
    );
    assert!(!text.contains("per-destination"), "{text}");
    assert!(
        text.contains("one edge twice: the mapping held"),
        "and it is a real measurement, so it says what it found: {text}"
    );

    // Two genuinely different edges still compare, so the guard is about
    // distinct destinations rather than refusing everything.
    let b = echoing_edge("gate2");
    let both = netcheck(&["--reflect", &a, "--reflect", &b]);
    assert!(
        both.contains("independent (pinned source port, sequential)"),
        "{both}"
    );
}

/// Pinning with one edge measures nothing, and must not claim otherwise.
#[test]
fn pinning_one_edge_is_still_one_vantage() {
    let a = echoing_edge("gate1");
    let text = netcheck(&["--pin-source-port", "--reflect", &a]);
    assert!(text.contains("(one vantage; not a comparison)"), "{text}");
}

/// An edge that does not answer breaks the pinning run, and the remaining
/// view is one vantage again rather than half a comparison.
#[test]
fn a_failed_edge_in_a_pinned_run_is_not_half_a_comparison() {
    let a = echoing_edge("gate1");
    let dead = {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let p = l.local_addr().unwrap().port();
        drop(l);
        format!("http://127.0.0.1:{p}/")
    };
    let text = netcheck(&["--pin-source-port", "--reflect", &a, "--reflect", &dead]);
    assert!(!text.contains("per-destination"), "{text}");
    assert!(!text.contains("independent"), "{text}");
    assert!(text.contains("(one vantage; not a comparison)"), "{text}");
}

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

/// Two edges that report different ports from one pinned source port are
/// a per-destination mapping, and the run says so rather than calling it a
/// comparison it declined to make.
///
/// The canned edges answer fixed, differing ports, which is what a NAT
/// that maps per destination looks like from outside. Before pin-by-default
/// this run left from two source ports and could only refuse to compare;
/// now it is the comparison, and the honest label is the unwelcome one.
#[test]
fn two_edges_reporting_different_ports_are_per_destination() {
    let a = edge(Some("gate1"), Some("203.0.113.7"), Some(51823));
    let b = edge(Some("gate2"), Some("203.0.113.7"), Some(51999));
    let text = netcheck(&["--reflect", &a, "--reflect", &b]);
    assert!(text.contains("51823 (gate1), 51999 (gate2)"), "{text}");
    assert!(
        text.contains("per-destination (pinned source port, sequential)"),
        "{text}"
    );
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

/// `--reflect-at` is how the second vantage is reached before the second A
/// record lands.
///
/// The design is one name discriminated by `observed.edge`, and discofetch
/// is deliberately holding the second A record until the measurement is
/// trusted — so today `reflect.discofetch.link` resolves to gate1 alone and
/// gate2 is reached by naming its address. `curl --resolve` by another name.
#[test]
fn reflect_at_names_the_vantage_when_dns_names_only_one() {
    // Two addresses, one port, one name: the gate1/gate2 shape exactly.
    let bind = |ip: &str, edge: &str| {
        let listener = TcpListener::bind(format!("{ip}:0")).unwrap();
        let port = listener.local_addr().unwrap().port();
        let name = edge.to_string();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let seen = stream.peer_addr().unwrap();
                let mut scratch = [0u8; 2048];
                let _ = stream.read(&mut scratch);
                let body = format!(
                    "{{\"observed\":{{\"address\":\"{}\",\"port\":{},\"edge\":\"{name}\"}}}}",
                    seen.ip(),
                    seen.port()
                );
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
        port
    };
    // Same port on both, which a real deployment also has; the loopback
    // range gives two addresses without a second machine.
    let port = bind("127.0.0.1", "gate1");
    let _ = bind("127.0.0.2", "gate2");
    // A second listener on 127.0.0.2 needs the same port to model this, and
    // binding :0 twice cannot guarantee it — so ask only for what we got.
    let text = netcheck(&[
        "--pin-source-port",
        "--reflect",
        &format!("http://reflect.test:{port}/"),
        "--reflect-at",
        "127.0.0.1",
    ]);
    // The Host stayed the name and the address was ours: one vantage, named
    // by what the edge called itself rather than by the URL.
    assert!(text.contains("(gate1)"), "{text}");
    assert!(
        text.contains("(one vantage; not a comparison)"),
        "one address is one vantage: {text}"
    );

    // And an address that is not one is refused by name rather than
    // silently resolving the URL instead.
    let bad = netcheck(&[
        "--reflect",
        &format!("http://reflect.test:{port}/"),
        "--reflect-at",
        "gate2.example",
    ]);
    assert!(bad.contains("is not an address"), "{bad}");
}

// --- the inbound probe -------------------------------------------------

/// An echoing reflect edge on a chosen address.
fn echoing_edge_on(ip: &str, edge_name: &str) -> String {
    let listener = TcpListener::bind(format!("{ip}:0")).unwrap();
    let url = format!("http://{}/", listener.local_addr().unwrap());
    let name = edge_name.to_string();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let seen = stream.peer_addr().unwrap();
            let mut scratch = [0u8; 2048];
            let _ = stream.read(&mut scratch);
            let body = format!(
                "{{\"observed\":{{\"address\":\"{}\",\"port\":{},\"edge\":\"{name}\"}}}}",
                seen.ip(),
                seen.port()
            );
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

/// A prober on a chosen address AND port, so one URL derives both legs.
fn prober_on(ip: &str, port: u16, result: &'static str) -> std::net::SocketAddr {
    let listener = TcpListener::bind(format!("{ip}:{port}")).unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let seen = stream.peer_addr().unwrap();
            let mut scratch = [0u8; 2048];
            let n = stream.read(&mut scratch).unwrap_or(0);
            let req = String::from_utf8_lossy(&scratch[..n]).into_owned();
            if result == "429" {
                let _ = stream
                    .write_all(b"HTTP/1.1 429 Too Many Requests\r\ncontent-length: 0\r\n\r\n");
                continue;
            }
            let p: u16 = req
                .split("port=")
                .nth(1)
                .and_then(|t| t.split(|c: char| !c.is_ascii_digit()).next())
                .and_then(|t| t.parse().ok())
                .unwrap_or(0);
            let body = format!(
                "{{\"service\":\"probe\",\"edge\":\"gate2\",\"address\":\"{}\",\
                 \"port\":{p},\"result\":\"{result}\"}}",
                seen.ip()
            );
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
    addr
}

/// **The client obligation, enforced rather than documented.**
///
/// `NETCHECK-SPEC.md` §3: with a prober on both gates the asymmetry that
/// made the probe safe becomes the client's job. A SYN from an address the
/// caller just contacted can traverse the mapping the caller's own request
/// created and answer `connected` when nothing out there can reach them —
/// and `connected` is the only result that reaches `direct`, whose advice is
/// "forward the port". The most expensive wrong answer available here.
#[test]
fn a_probe_from_an_edge_we_already_contacted_is_refused() {
    // Reflect and probe must share a port for one URL to derive both, so
    // bind the prober on a different address at the reflect port.
    let a = echoing_edge_on("127.0.0.1", "gate1");
    let port: u16 = a
        .rsplit(':')
        .next()
        .unwrap()
        .trim_end_matches('/')
        .parse()
        .unwrap();
    let _ = prober_on("127.0.0.3", port, "connected");
    let url = format!("http://reflect.test:{port}/");

    // Same vantage for both legs: refused, with the reason.
    let same = netcheck(&[
        "--reflect",
        &url,
        "--reflect-at",
        "127.0.0.1",
        "--port",
        "22",
        "--probe-at",
        "127.0.0.1",
    ]);
    assert!(
        same.contains("already contacted for reflect"),
        "the obligation must be enforced, not documented: {same}"
    );
    assert!(!same.contains("port 22: connected"), "{same}");

    // No probe vantage named at all.
    let none = netcheck(&[
        "--reflect",
        &url,
        "--reflect-at",
        "127.0.0.1",
        "--port",
        "22",
    ]);
    assert!(none.contains("needs --probe-at"), "{none}");

    // A distinct vantage is the legal shape, and measures.
    let ok = netcheck(&[
        "--reflect",
        &url,
        "--reflect-at",
        "127.0.0.1",
        "--port",
        "22",
        "--probe-at",
        "127.0.0.3",
    ]);
    assert!(ok.contains("port 22: connected"), "{ok}");
}

/// A rate limit is silence, never a finding about the network. The prober
/// limits per observed address (30/min by default), and rendering a 429 as
/// `refused` would be a confidently wrong answer about someone's firewall.
#[test]
fn a_rate_limited_probe_is_not_measured_never_refused() {
    let a = echoing_edge_on("127.0.0.1", "gate1");
    let port: u16 = a
        .rsplit(':')
        .next()
        .unwrap()
        .trim_end_matches('/')
        .parse()
        .unwrap();
    let _ = prober_on("127.0.0.3", port, "429");
    let url = format!("http://reflect.test:{port}/");

    let text = netcheck(&[
        "--reflect",
        &url,
        "--reflect-at",
        "127.0.0.1",
        "--port",
        "22",
        "--probe-at",
        "127.0.0.3",
    ]);
    assert!(text.contains("inbound    not measured"), "{text}");
    assert!(
        text.contains("rate limited"),
        "and it says which silence: {text}"
    );
    assert!(!text.contains("refused"), "never a finding: {text}");
}

// ---------------------------------------------------------------------------
// `--extra-root`: the flag that makes netcheck usable behind an intercepting
// proxy. depth: a TLS terminator in front of the plain edge above.
// ---------------------------------------------------------------------------

/// A TLS terminator in front of an [`echoing_edge`], plus the self-signed
/// certificate a client has to be told to trust.
///
/// The same twenty lines as the tunnel suite's `tls_gate`, and for the same
/// reason: no public CA will vouch for a loopback edge, so the only way to
/// exercise the trust path at all is to stand up a CA nobody trusts and
/// name it. That is also exactly the shape of the case in the field — an
/// egress proxy re-signing with a CA that is private to one company.
///
/// Returns the `https://` URL, the PEM to trust, and the directory holding
/// it, which the caller must keep alive.
fn tls_edge(edge_name: &str) -> (String, std::path::PathBuf, tempfile::TempDir) {
    use std::sync::Arc;
    use tokio_rustls::rustls::pki_types::{CertificateDer, PrivatePkcs8KeyDer};

    // The upstream this terminates onto: the plain edge the rest of this
    // file already drives, unchanged.
    let upstream = echoing_edge(edge_name)
        .trim_start_matches("http://")
        .trim_end_matches('/')
        .to_string();

    // `localhost` rather than the address, so the name in the URL is the
    // name in the certificate; `--reflect-at` is what actually points the
    // connection at loopback.
    let issued = rcgen::generate_simple_self_signed(vec!["localhost".to_string()])
        .expect("a self-signed certificate for the edge");
    let cert = CertificateDer::from(issued.cert.der().to_vec());
    let key = PrivatePkcs8KeyDer::from(issued.key_pair.serialize_der());

    let dir = tempfile::tempdir().unwrap();
    let pem = dir.path().join("edge-ca.pem");
    std::fs::write(&pem, issued.cert.pem()).unwrap();

    let config = tokio_rustls::rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![cert], key.into())
        .expect("the edge's certificate and key agree");

    // Bound on this thread so the port is known before the test proceeds;
    // served on another, which owns the runtime.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    listener.set_nonblocking(true).unwrap();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async move {
            let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));
            let listener = tokio::net::TcpListener::from_std(listener).unwrap();
            while let Ok((sock, _)) = listener.accept().await {
                let acceptor = acceptor.clone();
                let upstream = upstream.clone();
                tokio::spawn(async move {
                    let Ok(mut tls) = acceptor.accept(sock).await else {
                        return;
                    };
                    let Ok(mut up) = tokio::net::TcpStream::connect(&upstream).await else {
                        return;
                    };
                    let _ = tokio::io::copy_bidirectional(&mut tls, &mut up).await;
                });
            }
        });
    });

    (format!("https://localhost:{port}/"), pem, dir)
}

/// An edge behind a CA the public roots do not know is unreachable, and
/// `--extra-root` is what reaches it.
///
/// This is the whole of discofetch's ask: `netcheck` is what we tell people
/// to run when they do not know their own network, and a corporate network
/// — the one case where that question is hardest to answer — is exactly
/// where an intercepting proxy re-signs the fetch with a CA no public root
/// vouches for. Before the flag the answer was `not measured`, which is
/// honest and useless.
///
/// Both halves are asserted, because only the pair proves the flag did the
/// work: the same edge, the same run, unreachable without it and measured
/// with it.
#[test]
fn an_intercepted_edge_is_unreachable_until_extra_root_names_its_ca() {
    let (url, pem, _dir) = tls_edge("gate1");

    let without = netcheck(&["--reflect", &url, "--reflect-at", "127.0.0.1"]);
    assert!(
        without.contains("address    not measured"),
        "an untrusted CA is a silence, not an address: {without}"
    );
    assert!(
        without.contains("tls:"),
        "and it names TLS as the reason rather than a bare failure: {without}"
    );

    let with = netcheck(&[
        "--reflect",
        &url,
        "--reflect-at",
        "127.0.0.1",
        "--extra-root",
        pem.to_str().unwrap(),
    ]);
    assert!(
        with.contains("address    127.0.0.1"),
        "named, the CA verifies and the edge answers: {with}"
    );
    assert!(
        with.contains("(gate1)"),
        "and it is the edge we stood up: {with}"
    );
}

/// A path that is not a certificate is refused by name, before anything is
/// measured.
///
/// The promise `load_roots` makes in its first sentence. A file accepted at
/// load and refused at dial is the late, obscure failure the flag exists to
/// prevent, so the refusal has to come first and has to say which file.
#[test]
fn a_file_that_is_not_a_certificate_is_refused_by_name() {
    let dir = tempfile::tempdir().unwrap();
    let junk = dir.path().join("not-a-cert.pem");
    std::fs::write(&junk, b"this is not a certificate\n").unwrap();

    let out = Command::new(env!("CARGO_BIN_EXE_drt"))
        .arg("netcheck")
        .args(["--extra-root", junk.to_str().unwrap()])
        .output()
        .unwrap();
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "a bad root is a refusal: {err}");
    assert!(err.contains("--extra-root"), "named by flag: {err}");
    assert!(err.contains("not-a-cert.pem"), "and by file: {err}");
}

// ---------------------------------------------------------------------------
// Issue #25: one flag, and the edge supplies the rest.
// depth: an edge that answers a `measure` block, on every loopback address.
// ---------------------------------------------------------------------------

/// A reflect edge that answers the given `measure` block beside what it
/// observed, bound on every address so the vantages it names -- two
/// loopback addresses -- both reach it. The seen port is echoed, so a
/// pinned pair of fetches reads as one port twice.
fn configuring_edge(measure: &str) -> String {
    let listener = TcpListener::bind("0.0.0.0:0").unwrap();
    let url = format!(
        "http://127.0.0.1:{}/",
        listener.local_addr().unwrap().port()
    );
    let measure = measure.to_string();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let seen = stream.peer_addr().unwrap();
            let local = stream.local_addr().unwrap();
            let mut scratch = [0u8; 2048];
            let _ = stream.read(&mut scratch);
            // The edge names itself by the address it was reached at, so
            // two vantages of one listener are two names.
            let body = format!(
                "{{\"observed\":{{\"address\":\"{}\",\"port\":{},\"edge\":\"edge-{}\"}},\
                 \"measure\":{measure}}}",
                seen.ip(),
                seen.port(),
                local.ip()
            );
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

/// The whole of the ask: one `--reflect`, and the answer configures the
/// run. The vantages it names are both fetched and compared, and the STUN
/// pair it names is the pair asked -- which the `udp map` line proves by
/// naming a server nothing on this command line ever typed.
#[test]
fn one_reflect_flag_and_the_answer_configures_the_run() {
    let url = configuring_edge(
        r#"{"stun":["answered-a.invalid:3478","answered-b.invalid:3478"],
            "vantages":["127.0.0.1","127.0.0.2"]}"#,
    );
    let text = netcheck(&["--reflect", &url]);
    assert!(
        text.contains(&format!(
            "config     {url}: stun from the answer (2), vantages from the answer (2)"
        )),
        "{text}"
    );
    assert!(text.contains("(edge-127.0.0.1)"), "{text}");
    assert!(text.contains("(edge-127.0.0.2)"), "{text}");
    assert!(
        text.contains("independent (pinned source port, sequential)"),
        "two answered vantages are pinned and compared: {text}"
    );
    assert!(
        text.contains("answered-a.invalid"),
        "the servers asked were the answer's: {text}"
    );
}

/// A typed flag wins over the answer, one key at a time.
#[test]
fn a_typed_flag_wins_over_the_answer_per_key() {
    let url = configuring_edge(
        r#"{"stun":["answered-a.invalid:3478","answered-b.invalid:3478"],
            "vantages":["127.0.0.1","127.0.0.2"]}"#,
    );
    let text = netcheck(&["--reflect", &url, "--reflect-at", "127.0.0.1"]);
    assert!(
        text.contains("stun from the answer (2), vantages from --reflect-at"),
        "{text}"
    );
    assert!(text.contains("(one vantage; not a comparison)"), "{text}");
    assert!(!text.contains("edge-127.0.0.2"), "{text}");
}

/// The answer cannot weaken the rules the flags live under.
///
/// One server is still "1 given", and two entries that are one address are
/// still one destination -- the stability check, not a comparison.
#[test]
fn an_answer_cannot_weaken_the_two_server_rule_or_fake_a_second_vantage() {
    let one = configuring_edge(r#"{"stun":["only.invalid:3478"],"vantages":["127.0.0.1"]}"#);
    let text = netcheck(&["--reflect", &one]);
    assert!(text.contains("1 given"), "{text}");

    let twice = configuring_edge(r#"{"vantages":["127.0.0.1","127.0.0.1"]}"#);
    let text = netcheck(&["--reflect", &twice]);
    assert!(text.contains("one edge twice: the mapping held"), "{text}");
    assert!(!text.contains("independent (pinned"), "{text}");
}

/// An answer with no `measure` block, or a malformed one, is the old run.
#[test]
fn an_answer_without_a_measure_block_leaves_the_run_to_the_flags() {
    let plain = echoing_edge("gate1");
    let text = netcheck(&["--reflect", &plain]);
    assert!(
        text.contains(&format!("config     {plain}: no stun, no vantages")),
        "{text}"
    );
    assert!(text.contains("0 given"), "{text}");

    let malformed = configuring_edge(r#"{"stun":"not-a-list","vantages":42}"#);
    let text = netcheck(&["--reflect", &malformed, "--stun", "typed.invalid:3478"]);
    assert!(
        text.contains("stun from --stun, no vantages"),
        "a malformed key reads as absent: {text}"
    );
}

/// A bare run names what it is missing, and invents nobody's infrastructure.
#[test]
fn a_bare_run_says_it_has_nothing_to_measure_against() {
    let text = netcheck(&[]);
    assert!(
        text.contains(
            "config     nothing named to measure against: --reflect <url> supplies the rest"
        ),
        "{text}"
    );
    assert!(!text.contains("discofetch"), "{text}");
}
