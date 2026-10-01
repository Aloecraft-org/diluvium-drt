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

#![cfg(all(feature = "p2p", unix))]

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
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
