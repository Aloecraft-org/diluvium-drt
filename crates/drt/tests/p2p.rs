//! `drt p2p` end to end, on loopback (`doc/P2P.md`): a listening peer
//! serving a TCP forward, called by its record and by its own signalling
//! port, with stdio and with a mapped port; the REPL default's two
//! services; and what the alias prints for a `drt tunnel` line.
//!
//! ## surface block
//!
//! - Entry points: the `#[test]`s below.
//! - Configurable: [`WAIT`], how long any one step may take.
//! - Fan-out: [`Listening`] is the serving peer, a `drt p2p --listen`
//!   subprocess; [`Room`] is `examples/30-signaling-room`, the signalling
//!   server a parked peer answers at; [`echo`] is its target.

#![cfg(feature = "p2p")]

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const WAIT: Duration = Duration::from_secs(20);

/// Each process gets a home of its own: a listening peer keeps its identity
/// under `~/.drt/p2p`, and two starting at once must not race for one file.
fn drt() -> Command {
    static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let home = std::env::temp_dir().join(format!("drt-p2p-home-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&home).unwrap();
    let mut c = Command::new(env!("CARGO_BIN_EXE_drt"));
    c.env("HOME", home);
    c
}

/// Echoes every connection until it closes.
/// `call`, with stdin closed the moment it is written: what a pipe does.
/// The answer arrives because the two sides half-close (doc/P2P.md §7.2).
fn call_at_once(peer: &str, args: &[&str], input: &[u8]) -> (String, String) {
    let mut child = drt()
        .arg("p2p")
        .arg(peer)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    stdin.write_all(input).unwrap();
    drop(stdin);
    let out = child.wait_with_output().unwrap();
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

fn echo() -> u16 {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for conn in l.incoming().flatten() {
            std::thread::spawn(move || {
                let mut c = conn;
                let mut buf = [0u8; 4096];
                while let Ok(n) = c.read(&mut buf) {
                    if n == 0 || c.write_all(&buf[..n]).is_err() {
                        break;
                    }
                }
            });
        }
    });
    port
}

fn free_port() -> u16 {
    std::net::UdpSocket::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// A `drt p2p --listen` process, read until it has printed its record.
struct Listening {
    child: Child,
    record: String,
    signal: Option<u16>,
    lines: Vec<String>,
}

impl Listening {
    fn start(forward: Option<&str>, signal: bool) -> Listening {
        let udp = free_port();
        let signal_port = signal.then(free_port);
        let mut cmd = drt();
        cmd.arg("p2p").arg("--listen").arg(udp.to_string());
        if let Some(f) = forward {
            cmd.arg("--forward").arg(f);
        }
        if let Some(p) = signal_port {
            cmd.arg("--signal").arg(p.to_string());
        }
        let mut child = cmd
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut err = BufReader::new(child.stderr.take().unwrap());
        let mut lines = Vec::new();
        let mut record = None;
        let start = Instant::now();
        while start.elapsed() < WAIT {
            let mut line = String::new();
            if err.read_line(&mut line).unwrap() == 0 {
                break;
            }
            if let Some(r) = line.strip_prefix("drt p2p: record ") {
                record = Some(r.trim().to_string());
            }
            let done = line.contains("call it with: drt p2p drt://")
                || (!signal && line.contains("call it with: drt p2p '"));
            lines.push(line);
            if done {
                break;
            }
        }
        // Keep draining so the child never blocks on a full pipe.
        std::thread::spawn(move || {
            let mut line = String::new();
            while err.read_line(&mut line).unwrap_or(0) > 0 {
                line.clear();
            }
        });
        let record = record.unwrap_or_else(|| panic!("no record in {lines:?}"));
        Listening {
            child,
            record,
            signal: signal_port,
            lines,
        }
    }
}

impl Drop for Listening {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// `drt p2p <peer> [args]` with `input` on stdin; what came out, and stderr.
fn call(peer: &str, args: &[&str], input: &[u8]) -> (String, String) {
    let mut child = drt()
        .arg("p2p")
        .arg(peer)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let input = input.to_vec();
    std::thread::spawn(move || {
        let _ = stdin.write_all(&input);
        // Give the echo time to come back before stdin's EOF ends the session.
        std::thread::sleep(Duration::from_millis(1500));
    });
    let out = child.wait_with_output().unwrap();
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

#[test]
fn a_pipe_that_closes_at_once_still_gets_its_answer() {
    let port = echo();
    let l = Listening::start(Some(&format!("127.0.0.1:{port}")), false);
    let started = Instant::now();
    let (out, err) = call_at_once(&l.record, &[], b"piped\n");
    assert_eq!(out, "piped\n", "{err}");
    // Not the old two-second grace: the far side's END ended it.
    assert!(
        started.elapsed() < Duration::from_secs(8),
        "{:?}",
        started.elapsed()
    );
}

#[test]
fn a_record_in_hand_reaches_the_one_forward_over_stdio() {
    let port = echo();
    let l = Listening::start(Some(&format!("127.0.0.1:{port}")), false);
    assert!(
        l.lines.iter().any(|x| x.contains("fingerprint SHA256:")),
        "{:?}",
        l.lines
    );
    let (out, err) = call(&l.record, &[], b"hello over p2p\n");
    assert_eq!(out, "hello over p2p\n", "{err}");
}

#[test]
fn a_signal_port_takes_the_callers_request_and_a_mapped_port_carries_streams() {
    let port = echo();
    let l = Listening::start(Some(&format!("127.0.0.1:{port}")), true);
    let signal = l.signal.unwrap();
    let local = free_port();
    let mut child = drt()
        .arg("p2p")
        .arg(format!("drt://127.0.0.1:{signal}"))
        .arg("-p")
        .arg(format!("{local}:1"))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut err = BufReader::new(child.stderr.take().unwrap());
    let mut line = String::new();
    err.read_line(&mut line).unwrap();
    assert!(
        line.contains(&format!("127.0.0.1:{local} reaches")),
        "{line}"
    );
    let start = Instant::now();
    let mut conn = loop {
        match TcpStream::connect(("127.0.0.1", local)) {
            Ok(c) => break c,
            Err(_) if start.elapsed() < WAIT => std::thread::sleep(Duration::from_millis(50)),
            Err(e) => panic!("{e}"),
        }
    };
    conn.set_read_timeout(Some(WAIT)).unwrap();
    conn.write_all(b"ports mapped\n").unwrap();
    let mut got = vec![0u8; 13];
    conn.read_exact(&mut got).unwrap();
    assert_eq!(&got, b"ports mapped\n");
    let _ = child.kill();
    let _ = child.wait();
}

#[test]
fn the_repl_default_serves_ssh_and_raw_repl_by_name() {
    let l = Listening::start(None, false);
    assert!(
        l.lines.iter().any(|x| x.contains("serving the REPL")),
        "{:?}",
        l.lines
    );
    // The default target and the service `ssh` are the SSH server.
    let (out, _) = call(&l.record, &[], b"");
    assert!(out.starts_with("SSH-2.0-"), "{out:?}");
    let (out, _) = call(&l.record, &["-p", ":ssh"], b"");
    assert!(out.starts_with("SSH-2.0-"), "{out:?}");
    // The service `repl` is the REPL itself, raw.
    let (out, err) = call(&l.record, &["-p", ":repl"], b"1+1\n");
    assert!(
        out.contains("drt repl") && out.contains('2'),
        "{out:?} {err}"
    );
}

#[test]
fn a_fingerprint_that_differs_refuses_the_record_before_any_session() {
    let port = echo();
    let l = Listening::start(Some(&format!("127.0.0.1:{port}")), false);
    let wrong = "SHA256:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
    let (out, err) = call(&l.record, &["--fingerprint", wrong], b"x\n");
    assert!(out.is_empty());
    assert!(
        err.contains("fingerprint is SHA256:") && err.contains("not the"),
        "{err}"
    );
    let (out, err) = call(&l.record, &["--fingerp", wrong], b"x\n");
    assert!(out.is_empty() && err.contains("not the"), "{err}");
    assert!(
        !err.contains("this side's network"),
        "a refusal before any session is not a question about the network: {err}"
    );
}

/// No path, and the failure ends with what this side's network is
/// (doc/P2P.md §1). With no --stun named there is nothing to measure it
/// against, and the clause says that rather than guessing.
#[test]
fn a_call_with_no_path_says_what_this_sides_network_is() {
    let record = {
        let l = Listening::start(Some("127.0.0.1:9"), false);
        l.record.clone()
    };
    let (out, err) = call(&record, &[], b"x\n");
    assert!(out.is_empty(), "{out}");
    assert!(err.contains("no session within"), "{err}");
    // `slim,p2p` (the Windows row) has no STUN client and says that
    // instead; either way the clause is there and claims nothing.
    let why = if cfg!(feature = "stun") {
        "this side's network: not measured (no --stun server named"
    } else {
        "this side's network: not measured (this build has no STUN client)"
    };
    assert!(err.contains(why), "{err}");
}

#[test]
fn drt_tunnel_prints_its_p2p_form_and_runs_it() {
    let port = echo();
    let l = Listening::start(Some(&format!("127.0.0.1:{port}")), false);
    let mut child = drt()
        .arg("tunnel")
        .arg(format!("rtc:{}", l.record))
        .arg("--to")
        .arg(format!("127.0.0.1:{port}"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    std::thread::spawn(move || {
        let _ = stdin.write_all(b"as before\n");
        std::thread::sleep(Duration::from_millis(1500));
    });
    let out = child.wait_with_output().unwrap();
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains(&format!(
            "this is `drt p2p <record> -p :127.0.0.1:{port}` now"
        )),
        "{err}"
    );
    // `--to host:port` named an address in scope, which this forward is not.
    assert!(err.contains("blocked"), "{err}");
}

// depth: the park role, against the reference signalling server

/// `drt start` on example 30's config, on a free port.
struct Room {
    child: Child,
    base: String,
    answerer: String,
    caller: String,
    _dir: tempfile::TempDir,
}

impl Room {
    fn start() -> Room {
        let example = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../examples/30-signaling-room")
            .canonicalize()
            .unwrap();
        let mut config: serde_json::Value =
            serde_json::from_slice(&std::fs::read(example.join("app.json")).unwrap()).unwrap();
        config["program"]["path"] = example.join("app.dlua").to_string_lossy().into();
        config["listeners"][0]["address"] = "127.0.0.1:0".into();
        let args = config["args"].clone();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("room.json");
        std::fs::write(&path, config.to_string()).unwrap();
        let mut child = Command::new(env!("CARGO_BIN_EXE_drt"))
            .arg("--config")
            .arg(&path)
            .arg("start")
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut lines = BufReader::new(child.stderr.take().unwrap()).lines();
        let addr = loop {
            let line = lines.next().expect("drt start ended first").unwrap();
            if let Some(at) = line.strip_prefix("drt start: http listening on ") {
                break at.trim().to_string();
            }
        };
        std::thread::spawn(move || for _ in lines {});
        let text = |k: &str| args[k].as_str().unwrap().to_string();
        Room {
            child,
            base: format!("http://{addr}/v1/{}", text("name")),
            answerer: text("answerer_token"),
            caller: text("caller_token"),
            _dir: dir,
        }
    }
}

impl Drop for Room {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn a_parked_peer_answers_calls_at_a_signalling_server() {
    let room = Room::start();
    let port = echo();
    let mut parked = drt()
        .arg("p2p")
        .arg("--park")
        .arg(&room.base)
        .arg("--H")
        .arg(format!("auth={}", room.answerer))
        .arg("--forward")
        .arg(format!("127.0.0.1:{port}"))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut err = BufReader::new(parked.stderr.take().unwrap());
    let start = Instant::now();
    loop {
        let mut line = String::new();
        assert!(
            err.read_line(&mut line).unwrap() > 0,
            "the parked peer ended"
        );
        if line.contains("present at ") {
            break;
        }
        assert!(start.elapsed() < WAIT, "{line}");
    }
    std::thread::spawn(move || {
        let mut line = String::new();
        while err.read_line(&mut line).unwrap_or(0) > 0 {
            line.clear();
        }
    });
    // A caller with the caller token, by the name's drt:// address.
    let (out, errs) = call(
        &room.base.replacen("http://", "drt://", 1),
        &["--H", &format!("auth={}", room.caller)],
        b"through the room\n",
    );
    assert_eq!(out, "through the room\n", "{errs}");
    // A caller with no token is refused by the server, before any session.
    let (out, errs) = call(&room.base, &[], b"x\n");
    assert!(out.is_empty());
    assert!(errs.contains("401"), "{errs}");
    let _ = parked.kill();
    let _ = parked.wait();
}

// depth: the match role

/// `drt p2p --match` on a free port, read until it listens.
struct Match {
    child: Child,
    base: String,
}

impl Match {
    fn start(capacity: usize) -> Match {
        let port = free_port();
        let mut child = drt()
            .arg("p2p")
            .arg("--match")
            .arg(port.to_string())
            .arg("--capacity")
            .arg(capacity.to_string())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut out = BufReader::new(child.stdout.take().unwrap()).lines();
        let line = out.next().expect("the server says what it serves").unwrap();
        assert!(line.contains("admission is by token"), "{line}");
        std::thread::spawn(move || for _ in out {});
        let stderr = child.stderr.take().unwrap();
        std::thread::spawn(move || for _ in BufReader::new(stderr).lines() {});
        Match {
            child,
            base: format!("http://127.0.0.1:{port}"),
        }
    }

    fn get(&self, path: &str) -> (u16, String) {
        self.request("GET", path, "")
    }

    fn post(&self, path: &str, body: &str) -> (u16, String) {
        self.request("POST", path, body)
    }

    fn request(&self, method: &str, path: &str, body: &str) -> (u16, String) {
        let mut conn = TcpStream::connect(self.base.trim_start_matches("http://")).unwrap();
        conn.set_read_timeout(Some(WAIT)).unwrap();
        conn.write_all(
            format!(
                "{method} {path} HTTP/1.1\r\nHost: t\r\nConnection: close\r\n\
                 Content-Type: text/plain;charset=utf-8\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            )
            .as_bytes(),
        )
        .unwrap();
        let mut reply = String::new();
        conn.read_to_string(&mut reply).unwrap();
        let status = reply.split_whitespace().nth(1).unwrap().parse().unwrap();
        let body = reply.split("\r\n\r\n").nth(1).unwrap_or("").to_string();
        (status, body)
    }
}

impl Drop for Match {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn the_match_server_claims_names_by_token_and_holds_a_capacity() {
    let server = Match::start(2);
    let port = echo();
    let name = format!("{}/v1/mypc", server.base);
    let mut parked = drt()
        .arg("p2p")
        .arg("--park")
        .arg(&name)
        .arg("--H")
        .arg("auth=answerer-token")
        .arg("--H")
        .arg("DRT-Caller-Token=caller-token")
        .arg("--forward")
        .arg(format!("127.0.0.1:{port}"))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut err = BufReader::new(parked.stderr.take().unwrap());
    loop {
        let mut line = String::new();
        assert!(
            err.read_line(&mut line).unwrap() > 0,
            "the parked peer ended"
        );
        if line.contains("present at ") {
            break;
        }
    }
    std::thread::spawn(move || {
        let mut line = String::new();
        while err.read_line(&mut line).unwrap_or(0) > 0 {
            line.clear();
        }
    });
    // The caller token set at the claim admits a caller, and nothing else.
    let (out, errs) = call(&name, &["--H", "auth=caller-token"], b"matched\n");
    assert_eq!(out, "matched\n", "{errs}");
    let (_, errs) = call(&name, &[], b"x\n");
    assert!(errs.contains("401"), "{errs}");
    let (_, errs) = call(&name, &["--H", "auth=wrong"], b"x\n");
    assert!(errs.contains("403"), "{errs}");
    // Another answerer token may not take the held name; two more names fit
    // one, and the capacity refuses the third.
    assert_eq!(server.get("/v1/mypc/calls?k=other").0, 403);
    assert_eq!(server.get("/v1/second/calls?k=t").0, 200);
    let (status, body) = server.get("/v1/third/calls?k=t");
    assert_eq!((status, body.contains("no room")), (429, true), "{body}");
    // A name nobody holds has no answerer present.
    assert_eq!(server.get("/v1/third/calls?k=t").0, 429);
    let _ = parked.kill();
    let _ = parked.wait();
}

/// `drt p2p --park` at `server`, read until it is present there; its
/// stderr is kept for the lines a test waits on.
struct Parked {
    child: Child,
    err: BufReader<std::process::ChildStderr>,
}

impl Parked {
    fn start(server: &Match, name: &str, args: &[&str]) -> Parked {
        let port = echo();
        let mut child = drt()
            .arg("p2p")
            .arg("--park")
            .arg(format!("{}/v1/{name}", server.base))
            .args(args)
            .arg("--forward")
            .arg(format!("127.0.0.1:{port}"))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let err = BufReader::new(child.stderr.take().unwrap());
        let mut parked = Parked { child, err };
        parked.until("present at ");
        parked
    }

    /// Read stderr until a line contains `what`; the lines before it.
    fn until(&mut self, what: &str) -> String {
        let start = Instant::now();
        let mut seen = String::new();
        loop {
            let mut line = String::new();
            assert!(
                self.err.read_line(&mut line).unwrap() > 0,
                "the parked peer ended before {what:?}:\n{seen}"
            );
            seen.push_str(&line);
            if line.contains(what) {
                return seen;
            }
            assert!(
                start.elapsed() < WAIT,
                "no {what:?} within {WAIT:?}:\n{seen}"
            );
        }
    }
}

impl Drop for Parked {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// One answerer parks several names (`doc/P2P.md` §2.2): each is answered
/// by the same host, at once, and a name the server refuses stops being
/// answered while the others carry on.
#[test]
fn one_answerer_parks_several_names_and_loses_only_the_refused_one() {
    let server = Match::start(4);
    // Another answerer token holds `taken` first.
    assert_eq!(server.get("/v1/taken/calls?k=other").0, 200);
    let second = format!("{}/v1/room-2", server.base);
    let taken = format!("{}/v1/taken", server.base);
    let mut parked = Parked::start(
        &server,
        "room-1",
        &["--park", &second, "--park", &taken, "--H", "auth=t"],
    );
    let lines = parked.until("no longer parked at ");
    assert!(lines.contains("/v1/taken"), "{lines}");
    // Both names at once: their calls may share an id at the server, and
    // the host keeps the two sessions apart.
    let calls: Vec<_> = ["room-1", "room-2"]
        .into_iter()
        .map(|name| {
            let peer = format!("{}/v1/{name}", server.base);
            std::thread::spawn(move || (name, call(&peer, &[], name.as_bytes())))
        })
        .collect();
    for handle in calls {
        let (name, (out, errs)) = handle.join().unwrap();
        assert_eq!(out, name, "{errs}");
    }
}

/// Several `--park` are names at signalling servers: a name twice, or a
/// `wss://` leg beside a name, is refused by name before anything is served.
#[test]
fn several_parks_are_distinct_names_at_signalling_servers() {
    let refused = |parks: &[&str]| {
        let mut cmd = drt();
        cmd.arg("p2p");
        for p in parks {
            cmd.arg("--park").arg(p);
        }
        let out = cmd.stdin(Stdio::null()).output().unwrap();
        assert!(!out.status.success());
        String::from_utf8_lossy(&out.stderr).into_owned()
    };
    let err = refused(&["drt://127.0.0.1:1/v1/a", "drt://127.0.0.1:1/v1/a"]);
    assert!(err.contains("127.0.0.1:1/v1/a twice"), "{err}");
    let err = refused(&["wss://127.0.0.1:1/park/x?k=y", "drt://127.0.0.1:1/v1/a"]);
    assert!(err.contains("parks alone"), "{err}");
}

/// Pairing (doc/DRT-Signalling.md §6.2): a parked peer asks the server for
/// a call from another parked peer; the server tells that peer, which
/// calls with the asker's caller token and keeps serving, and reports the
/// outcome. A peer without `--pair` declines and says so.
#[test]
fn the_match_server_pairs_two_parked_peers_when_one_asks() {
    let server = Match::start(4);
    let mut told = Parked::start(&server, "told", &["--H", "auth=told-token", "--pair", "*"]);
    let mut asker = Parked::start(
        &server,
        "asker",
        &[
            "--H",
            "auth=asker-token",
            "--H",
            "DRT-Caller-Token=asker-caller",
        ],
    );
    let mut deaf = Parked::start(&server, "deaf", &["--H", "auth=deaf-token"]);

    // The asker holds its name; the server tells `told` to call it.
    let (status, body) = server.post("/v1/asker/pair?k=asker-token", r#"{"name":"told"}"#);
    assert_eq!((status, body.as_str()), (202, r#"{"id":"p1"}"#));
    let lines = told.until("pair p1: connected");
    assert!(lines.contains("pair p1: calling asker at "), "{lines}");
    assert!(lines.contains("pair:asker: connected"), "{lines}");
    asker.until(": connected");

    // One without consent declines, and the server hears it.
    let (status, _) = server.post("/v1/asker/pair?k=asker-token", r#"{"name":"deaf"}"#);
    assert_eq!(status, 202);
    let lines = deaf.until("pair p1: declined");
    assert!(lines.contains("no --pair"), "{lines}");

    // Asking for a name nobody holds, or without the asker's token, fails
    // by name.
    assert_eq!(
        server
            .post("/v1/asker/pair?k=asker-token", r#"{"name":"nobody"}"#)
            .0,
        503
    );
    assert_eq!(server.post("/v1/asker/pair", r#"{"name":"told"}"#).0, 401);
    assert_eq!(
        server.post("/v1/asker/pair?k=asker-token", "not json").0,
        400
    );
}

/// The answerer's `--accept` travels as DRT-Accept, and the match server
/// refuses a caller outside it by the address its listener saw, before
/// the parked side ever hears of the call.
#[test]
fn the_match_server_refuses_a_caller_outside_the_answerers_accept_range() {
    let server = Match::start(2);
    let port = echo();
    let name = format!("{}/v1/fenced", server.base);
    let mut parked = drt()
        .arg("p2p")
        .arg("--park")
        .arg(&name)
        .arg("--H")
        .arg("auth=answerer-token")
        .arg("--accept")
        .arg("10.0.0.0/8")
        .arg("--forward")
        .arg(format!("127.0.0.1:{port}"))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut err = BufReader::new(parked.stderr.take().unwrap());
    loop {
        let mut line = String::new();
        assert!(
            err.read_line(&mut line).unwrap() > 0,
            "the parked peer ended"
        );
        if line.contains("present at ") {
            break;
        }
    }
    std::thread::spawn(move || {
        let mut line = String::new();
        while err.read_line(&mut line).unwrap_or(0) > 0 {
            line.clear();
        }
    });
    let (out, errs) = call(&name, &[], b"x\n");
    assert!(
        out.is_empty() && errs.contains("403") && errs.contains("address"),
        "{errs}"
    );
    let _ = parked.kill();
    let _ = parked.wait();
}

// depth: the carriers

#[test]
fn a_relay_joins_two_sessions_and_the_caller_is_told() {
    let port = echo();
    let destination = Listening::start(Some(&format!("127.0.0.1:{port}")), false);
    let relay = Listening::start(Some(""), false);
    assert!(
        relay
            .lines
            .iter()
            .any(|x| x.contains("whatever the caller names")),
        "{:?}",
        relay.lines
    );
    // The destination's record is what the relay is told to call; the
    // caller's own session is with the relay.
    let (out, err) = call(
        &destination.record,
        &["--relay", &relay.record],
        b"relayed\n",
    );
    assert_eq!(out, "relayed\n", "{err}");
    assert!(err.contains("via relay"), "{err}");
    // --fallback: direct first, so a reachable destination never touches
    // the relay.
    let (out, err) = call(
        &destination.record,
        &["--fallback", &relay.record],
        b"direct\n",
    );
    assert_eq!(out, "direct\n", "{err}");
    assert!(!err.contains("via relay"), "{err}");
    // A peer that is not a relay refuses to be used as one.
    let (out, err) = call(
        &destination.record,
        &["--relay", &destination.record],
        b"x\n",
    );
    assert!(out.is_empty() && err.contains("not a relay"), "{err}");
}

/// Inside a project, `--listen` writes its record under .drt_root/live so
/// a launcher on this machine calls it in direct mode with nothing sent.
#[test]
fn a_listener_inside_a_project_writes_its_record_under_live() {
    let project = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(project.path().join(".drt_root")).unwrap();
    let port = free_port();
    let mut cmd = drt();
    let mut child = cmd
        .args([
            "p2p",
            "--listen",
            &port.to_string(),
            "--forward",
            "127.0.0.1:1",
        ])
        .current_dir(project.path())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut err = BufReader::new(child.stderr.take().unwrap());
    let (mut record, mut written) = (None, None);
    let start = Instant::now();
    while written.is_none() && start.elapsed() < WAIT {
        let mut line = String::new();
        if err.read_line(&mut line).unwrap() == 0 {
            break;
        }
        if let Some(p) = line.strip_prefix("drt p2p: record written to ") {
            written = Some(PathBuf::from(p.trim()));
        } else if let Some(r) = line.strip_prefix("drt p2p: record ") {
            record = Some(r.trim().to_string());
        }
    }
    let _ = child.kill();
    let _ = child.wait();
    let written = written.expect("the record's path");
    assert_eq!(
        written,
        project
            .path()
            .join(".drt_root/live")
            .join(format!("p2p-{port}.record.json"))
    );
    assert_eq!(
        std::fs::read_to_string(&written).unwrap().trim(),
        record.unwrap()
    );
}
