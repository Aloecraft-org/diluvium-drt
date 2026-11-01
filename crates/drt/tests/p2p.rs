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

/// Says `tag` on every connection, then echoes: a target that tells the
/// caller which of several it reached.
fn tagged(tag: &'static str) -> u16 {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for conn in l.incoming().flatten() {
            std::thread::spawn(move || {
                let mut c = conn;
                if c.write_all(tag.as_bytes()).is_err() {
                    return;
                }
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
        Listening::from_command(cmd, signal_port)
    }

    /// A listening peer however it was started, read until its record.
    fn from_command(mut cmd: Command, signal_port: Option<u16>) -> Listening {
        let signal = signal_port.is_some();
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

/// `commands.p2p.forward` as a list, from a config file: a caller names one
/// target by `host:port`, or by port alone when every target is on one
/// host, and a port no target names is refused.
#[test]
fn a_forward_list_from_the_config_serves_the_target_the_caller_names() {
    let (a, b) = (tagged("A:"), tagged("B:"));
    let udp = free_port();
    let dir = std::env::temp_dir().join(format!("drt-p2p-list-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("listen.json");
    std::fs::write(
        &file,
        format!(
            r#"{{"commands": {{"p2p": {{"listen": {udp}, "forward": ["127.0.0.1:{a}", "127.0.0.1:{b}"]}}}}}}"#
        ),
    )
    .unwrap();
    let mut cmd = drt();
    cmd.arg("--config").arg(&file).arg("p2p");
    let l = Listening::from_command(cmd, None);

    let (out, err) = call(&l.record, &["-p", &format!(":127.0.0.1:{b}")], b"x\n");
    assert_eq!(out, "B:x\n", "by host:port: {err}");
    let (out, err) = call(&l.record, &["-p", &format!(":{a}")], b"y\n");
    assert_eq!(out, "A:y\n", "by port alone: {err}");
    let (out, _) = call(&l.record, &["-p", &format!(":{}", free_port())], b"z\n");
    assert_eq!(out, "", "a port no target names reaches nothing");
    let _ = std::fs::remove_dir_all(&dir);
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

/// `-- <cmd>` (doc/P2P.md §2.1): the command runs once the port is bound,
/// reaches the far side through it, and its exit status is drt's.
#[cfg(unix)]
#[test]
fn a_command_after_the_peer_runs_against_the_bound_port_and_its_status_is_drts() {
    let port = echo();
    let l = Listening::start(Some(&format!("127.0.0.1:{port}")), false);
    let local = free_port();
    // bash's /dev/tcp: a TCP client every Unix CI runner has.
    let script = format!(
        "exec 3<>/dev/tcp/127.0.0.1/{local} && printf 'via the command\\n' >&3 && \
         head -n1 <&3 && exit 7"
    );
    let started = Instant::now();
    let out = drt()
        .arg("p2p")
        .arg(&l.record)
        .arg("-p")
        .arg(local.to_string())
        .arg("--")
        .args(["bash", "-c", &script])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    let err = String::from_utf8_lossy(&out.stderr);
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "via the command\n",
        "{err}"
    );
    assert_eq!(out.status.code(), Some(7), "{err}");
    assert!(started.elapsed() < WAIT, "{:?}", started.elapsed());
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
        Match::start_with(capacity, &[])
    }

    fn start_with(capacity: usize, args: &[&str]) -> Match {
        let port = free_port();
        let mut child = drt()
            .arg("p2p")
            .arg("--match")
            .arg(port.to_string())
            .arg("--capacity")
            .arg(capacity.to_string())
            .args(args)
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
        request_at(&self.base, method, path, body)
    }
}

/// One plain HTTP request to `base`, answered whole.
fn request_at(base: &str, method: &str, path: &str, body: &str) -> (u16, String) {
    request_with(base, method, path, "", body)
}

/// As [`request_at`], with `headers` (`Name: value\r\n` lines) added.
fn request_with(base: &str, method: &str, path: &str, headers: &str, body: &str) -> (u16, String) {
    let mut conn = TcpStream::connect(base.trim_start_matches("http://")).unwrap();
    conn.set_read_timeout(Some(WAIT)).unwrap();
    conn.write_all(
        format!(
            "{method} {path} HTTP/1.1\r\nHost: t\r\nConnection: close\r\n{headers}\
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

/// Topics (doc/P2P.md §2.4): a name inside a topic is parked, called,
/// listed and paired only with that topic's tokens, each by its role, while
/// a bare name beside it works as it always has.
#[test]
fn the_match_server_admits_a_topic_by_its_tokens_roles() {
    let dir = tempfile::tempdir().unwrap();
    let topics = dir.path().join("topics.json");
    std::fs::write(
        &topics,
        r#"{"topics": {"home": {"tokens": [
            {"token": "home-park", "roles": ["park", "call"]},
            {"token": "home-call", "roles": ["call"]},
            {"token": "home-read", "roles": ["read"]},
            {"token": "other-park", "roles": ["park"]}
        ]}}}"#,
    )
    .unwrap();
    let server = Match::start_with(8, &["--topics", topics.to_str().unwrap()]);
    let laptop = format!("{}/v1/home/laptop", server.base);
    let _parked = Parked::start(&server, "home/laptop", &["--H", "auth=home-park"]);

    // A call token of the topic calls; no token, or one without call, does not.
    let (out, errs) = call(&laptop, &["--H", "auth=home-call"], b"in a topic\n");
    assert_eq!(out, "in a topic\n", "{errs}");
    let (_, errs) = call(&laptop, &[], b"x\n");
    assert!(errs.contains("401"), "{errs}");
    let (_, errs) = call(&laptop, &["--H", "auth=home-read"], b"x\n");
    assert!(errs.contains("403"), "{errs}");

    // The listing is for read tokens of the topic only, and an undeclared
    // topic looks like a token that may not.
    assert_eq!(
        server.get("/v1/home/names?k=home-read"),
        (200, r#"{"topic":"home","names":["laptop"]}"#.to_string())
    );
    assert_eq!(server.get("/v1/home/names?k=home-call").0, 403);
    assert_eq!(server.get("/v1/home/names").0, 401);
    assert_eq!(server.get("/v1/elsewhere/names?k=home-read").0, 403);

    // Another park token may not take the held name; a token without park
    // may not park at all; a reserved word is not a name. `kv` after a
    // topic is the topic's kv store, which this server, without --kv, does
    // not keep; `names` takes only GET.
    assert_eq!(server.get("/v1/home/laptop/calls?k=other-park").0, 403);
    assert_eq!(server.get("/v1/home/desk/calls?k=home-call").0, 403);
    assert_eq!(server.get("/v1/home/kv/calls?k=home-park").0, 404);
    assert_eq!(server.get("/v1/home/names/calls?k=home-park").0, 400);

    // Pairing stays inside the topic: the told side calls with its own
    // token, which holds call.
    let mut desk = Parked::start(
        &server,
        "home/desk",
        &["--H", "auth=home-park", "--pair", "*"],
    );
    let (status, body) = server.post("/v1/home/laptop/pair?k=home-park", r#"{"name":"desk"}"#);
    assert_eq!((status, body.as_str()), (202, r#"{"id":"p1"}"#));
    let lines = desk.until("pair p1: connected");
    assert!(
        lines.contains("pair p1: calling home/laptop at "),
        "{lines}"
    );

    // A bare name beside the topics is claimed and called as before.
    assert_eq!(server.get("/v1/mypc/calls?k=anyone").0, 200);
}

/// Every role of a topic token, allowed where it applies and refused where
/// it does not, over the profile's own requests: park answers, call calls
/// and withdraws only its own call, read lists, and write (kv's role)
/// opens none of them.
#[test]
fn each_topic_role_opens_only_its_own_requests() {
    let dir = tempfile::tempdir().unwrap();
    let topics = dir.path().join("topics.json");
    std::fs::write(
        &topics,
        r#"{"topics": {
            "home": {"tokens": [
                {"token": "p", "roles": ["park"]},
                {"token": "c", "roles": ["call"]},
                {"token": "c2", "roles": ["call"]},
                {"token": "r", "roles": ["read"]},
                {"token": "w", "roles": ["write"]}
            ]},
            "work": {"tokens": [{"token": "wp", "roles": ["park", "call", "read"]}]}
        }}"#,
    )
    .unwrap();
    let server = Match::start_with(8, &["--topics", topics.to_str().unwrap()]);

    // park: only a park token claims a name and polls it.
    for token in ["c", "r", "w", "wp"] {
        let path = format!("/v1/home/box/calls?k={token}");
        assert_eq!(server.get(&path).0, 403, "{token} parked");
    }
    assert_eq!(server.get("/v1/home/box/calls").0, 401);
    let (status, body) = server.get("/v1/home/box/calls?k=p");
    assert_eq!(status, 200, "{body}");
    assert!(body.contains(r#""calls":[]"#), "{body}");
    // The topic is a namespace: the same name in another topic, and the
    // bare name, are other names, free to claim.
    assert_eq!(server.get("/v1/work/box/calls?k=wp").0, 200);
    assert_eq!(server.get("/v1/box/calls?k=bare").0, 200);

    // read: only a read token lists, and sees only its own topic's names.
    for token in ["p", "c", "w", "wp"] {
        let path = format!("/v1/home/names?k={token}");
        assert_eq!(server.get(&path).0, 403, "{token} listed");
    }
    assert_eq!(
        server.get("/v1/home/names?k=r"),
        (200, r#"{"topic":"home","names":["box"]}"#.to_string())
    );
    assert_eq!(
        server.get("/v1/work/names?k=wp"),
        (200, r#"{"topic":"work","names":["box"]}"#.to_string())
    );

    // call: only a call token calls; it is held until answered or withdrawn.
    for token in ["p", "r", "w", "wp"] {
        let path = format!("/v1/home/box/calls?k={token}");
        assert_eq!(server.post(&path, "{}").0, 403, "{token} called");
    }
    let base = server.base.clone();
    let held =
        std::thread::spawn(move || request_at(&base, "POST", "/v1/home/box/calls?k=c", "{}"));
    let start = Instant::now();
    loop {
        let (_, body) = server.get("/v1/home/box/calls?k=p");
        if body.contains(r#""id":"c1""#) {
            break;
        }
        assert!(start.elapsed() < WAIT, "the call never arrived: {body}");
        std::thread::sleep(Duration::from_millis(50));
    }
    // Another call token may not withdraw it, nor may a token without call;
    // its own caller may.
    assert_eq!(
        server.request("DELETE", "/v1/home/box/calls/c1?k=c2", "").0,
        404
    );
    assert_eq!(
        server.request("DELETE", "/v1/home/box/calls/c1?k=w", "").0,
        403
    );
    assert_eq!(
        server.request("DELETE", "/v1/home/box/calls/c1?k=c", "").0,
        204
    );
    assert_eq!(held.join().unwrap().0, 410);

    // Pairing inside a topic: the told side calls with its own token, so a
    // told name whose token holds park but not call is refused at the ask,
    // and a name outside the topic is not a name to tell.
    assert_eq!(server.get("/v1/home/other/calls?k=p").0, 200);
    let (status, body) = server.post("/v1/home/box/pair?k=p", r#"{"name":"other"}"#);
    assert_eq!(status, 403, "{body}");
    assert!(body.contains("may not call"), "{body}");
    assert_eq!(
        server
            .post("/v1/home/box/pair?k=p", r#"{"name":"work/box"}"#)
            .0,
        400
    );
    // Nor does a bare name reach into a topic, or learn who is present there.
    assert_eq!(
        server
            .post("/v1/box/pair?k=bare", r#"{"name":"home/box"}"#)
            .0,
        400
    );

    // A DRT-Caller-Token sent at the claim is not read inside a topic: the
    // header's token does not admit a caller, and the topic's call token is
    // still the one that gets through (to a hold no one answers: 504 is
    // past the test's patience, so the withdraw ends it).
    let claimed = request_with(
        &server.base,
        "GET",
        "/v1/home/hdr/calls?k=p",
        "DRT-Caller-Token: side-door\r\n",
        "",
    );
    assert_eq!(claimed.0, 200);
    assert_eq!(server.post("/v1/home/hdr/calls?k=side-door", "{}").0, 403);
    let base = server.base.clone();
    let held =
        std::thread::spawn(move || request_at(&base, "POST", "/v1/home/hdr/calls?k=c", "{}"));
    let start = Instant::now();
    while !server
        .get("/v1/home/hdr/calls?k=p")
        .1
        .contains(r#""id":"c1""#)
    {
        assert!(
            start.elapsed() < WAIT,
            "the topic's caller was not admitted"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(
        server.request("DELETE", "/v1/home/hdr/calls/c1?k=p", "").0,
        204
    );
    assert_eq!(held.join().unwrap().0, 410);
}

/// Without --topics a topic's path is refused by name, and a topics file
/// the server could not honour stops it before it listens.
#[test]
fn a_match_server_without_topics_says_so_and_a_bad_file_stops_it() {
    let server = Match::start(2);
    let (status, body) = server.get("/v1/home/laptop/calls?k=t");
    assert_eq!((status, body.contains("no topics")), (404, true), "{body}");
    drop(server);

    let dir = tempfile::tempdir().unwrap();
    let bad = dir.path().join("topics.json");
    std::fs::write(
        &bad,
        r#"{"topics":{"home":{"tokens":[{"token":"t","roles":["admin"]}]}}}"#,
    )
    .unwrap();
    let out = drt()
        .args(["p2p", "--match", "0", "--topics", bad.to_str().unwrap()])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        !out.status.success() && err.contains("'admin' is not a role"),
        "{err}"
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
