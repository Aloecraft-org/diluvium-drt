//! A match server's topics and capacity changed while it runs, and a parked
//! side swapping its token without letting its name go, through the real
//! binary:
//!
//! 1. `drt p2p --match --topics` reads the file again when it changes: new
//!    tokens and a new `capacity` are in force on the next request, a name
//!    held by a token the file took out is let go, and a file that no
//!    longer checks leaves the settings in force.
//! 2. `drt p2p --park --token-file` reads its token before every request.
//!    A renewed JWT written to the file keeps the name past the first
//!    token's expiry; when the last token expires unrenewed the server lets
//!    the name go, and the park waits for a new file and parks again.
//!
//! ## surface block
//!
//! - Entry points: the tests; [`Server::start`]; [`mint`], JWTs signed by
//!   `crypto/jwt_sign` under `drt run`.
//! - Configurable values: [`WAIT`]; [`ISSUER`].
//! - Fan-out: none.

#![cfg(all(unix, feature = "p2p", feature = "connector-crypto"))]

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{Receiver, Sender};
use std::time::{Duration, Instant};

const WAIT: Duration = Duration::from_secs(20);
const ISSUER: &str = "drt-session";

fn drt() -> Command {
    static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let home =
        std::env::temp_dir().join(format!("drt-topics-reload-home-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&home).unwrap();
    let mut c = Command::new(env!("CARGO_BIN_EXE_drt"));
    c.env("HOME", home);
    c
}

// depth: a process whose stdout and stderr are read as lines

struct Proc {
    child: Child,
    lines: Receiver<String>,
    name: &'static str,
}

impl Proc {
    fn spawn(name: &'static str, mut cmd: Command) -> Proc {
        let mut child = cmd
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        fn pump(name: &'static str, from: impl Read + Send + 'static, tx: Sender<String>) {
            std::thread::spawn(move || {
                for line in BufReader::new(from).lines().map_while(Result::ok) {
                    eprintln!("{name}: {line}");
                    let _ = tx.send(line);
                }
            });
        }
        pump(name, child.stdout.take().unwrap(), tx.clone());
        pump(name, child.stderr.take().unwrap(), tx);
        Proc {
            child,
            lines: rx,
            name,
        }
    }

    /// Read lines until one contains `what`; every line read is returned,
    /// so a caller can check what came before it.
    fn wait_for(&self, what: &str) -> Vec<String> {
        let deadline = Instant::now() + WAIT;
        let mut seen = Vec::new();
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.lines.recv_timeout(left) {
                Ok(line) => {
                    let found = line.contains(what);
                    seen.push(line);
                    if found {
                        return seen;
                    }
                }
                Err(_) => panic!("{} never said {what:?}; it said {seen:#?}", self.name),
            }
        }
    }

    /// Every line said so far.
    fn drain(&self) -> Vec<String> {
        self.lines.try_iter().collect()
    }
}

impl Drop for Proc {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

// depth: the match server

struct Server {
    proc: Proc,
    port: u16,
}

impl Server {
    /// `drt p2p --match --topics topics.json` in `dir`, read until it has
    /// said what it serves.
    fn start(dir: &Path) -> Server {
        let port = std::net::UdpSocket::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let mut cmd = drt();
        cmd.current_dir(dir).args([
            "p2p",
            "--match",
            &port.to_string(),
            "--topics",
            "topics.json",
        ]);
        let proc = Proc::spawn("match", cmd);
        proc.wait_for("p2p match: serving");
        Server { proc, port }
    }

    fn get(&self, path: &str, token: &str) -> (u16, String) {
        let mut conn = TcpStream::connect(("127.0.0.1", self.port)).unwrap();
        conn.set_read_timeout(Some(WAIT)).unwrap();
        conn.write_all(
            format!(
                "GET {path} HTTP/1.1\r\nHost: t\r\nConnection: close\r\n\
                 Authorization: Bearer {token}\r\n\r\n"
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

/// Replace `path` the way an editor or a renewal script does: a new file
/// renamed over the old, so no reader sees half of it.
fn replace(path: &Path, text: &str) {
    let tmp = path.with_extension("new");
    std::fs::write(&tmp, text).unwrap();
    std::fs::rename(&tmp, path).unwrap();
}

#[test]
fn a_changed_topics_file_is_in_force_without_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let topics = dir.path().join("topics.json");
    std::fs::write(
        &topics,
        r#"{"topics": {"home": {"tokens": [{"token": "park-a", "roles": ["park"]}]}},
            "capacity": 1}"#,
    )
    .unwrap();
    let server = Server::start(dir.path());

    assert_eq!(server.get("/v1/home/box/calls", "park-a").0, 200);
    // The file's capacity, not --capacity's 256: one name.
    let (status, body) = server.get("/v1/home/other/calls", "park-a");
    assert_eq!(status, 429, "{body}");

    // park-a taken out, park-b in, room for two.
    replace(
        &topics,
        r#"{"topics": {"home": {"tokens": [{"token": "park-b", "roles": ["park"]}]}},
            "capacity": 2}"#,
    );
    let said = server.proc.wait_for("reloaded the topics file");
    assert!(
        said.iter()
            .any(|l| l.contains("home/box is free: its token no longer parks in home")),
        "{said:#?}"
    );
    assert!(
        said.last()
            .unwrap()
            .contains("2 name(s) at most, 0 held, 1 let go"),
        "{said:#?}"
    );
    assert_eq!(server.get("/v1/home/box/calls", "park-a").0, 403);
    assert_eq!(server.get("/v1/home/box/calls", "park-b").0, 200);
    assert_eq!(server.get("/v1/home/other/calls", "park-b").0, 200);

    // A file that no longer checks is refused by name, and park-b stays.
    replace(
        &topics,
        r#"{"topics": {"home": {"tokens": [{"token": "park-c"}]}}}"#,
    );
    server.proc.wait_for("the topics in force stay");
    assert_eq!(server.get("/v1/home/box/calls", "park-b").0, 200);
    assert_eq!(server.get("/v1/home/box/calls", "park-c").0, 403);
}

/// JWTs for `sub` alice in group `parkers`, one per ttl in seconds, signed
/// by `crypto/jwt_sign` under `drt run`; and the issuer's JWKS.
fn mint(ttls: &[u64]) -> (String, Vec<String>) {
    use base64::Engine as _;
    let dir = tempfile::tempdir().unwrap();
    let mut der = vec![
        0x30, 0x2e, 0x02, 0x01, 0x00, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x04, 0x22, 0x04,
        0x20,
    ];
    der.extend_from_slice(&[9; 32]);
    let pem = format!(
        "-----BEGIN PRIVATE KEY-----\n{}\n-----END PRIVATE KEY-----\n",
        base64::engine::general_purpose::STANDARD.encode(der)
    );
    let pem_path = dir.path().join("session-1.pem");
    std::fs::write(&pem_path, pem).unwrap();
    let config = serde_json::json!({
        "caps": [
            {"capability": "host:crypto/jwt_sign"},
            {"capability": "host:crypto/public_key"},
        ],
        "connectors": {"crypto": {"scope": {
            "key": "the-issuers-master-key-0123456789",
            "signing_keys": [{"name": "session-1", "alg": "EdDSA",
                              "private_key": {"file": pem_path}}],
        }}},
    });
    std::fs::write(dir.path().join("drt.json"), config.to_string()).unwrap();
    let ttls: Vec<String> = ttls.iter().map(u64::to_string).collect();
    let program = format!(
        r#"
print("jwks " .. json.encode({{keys = {{host.call("crypto/public_key", {{key = "session-1"}})}}}}))
for _, ttl in ipairs({{{}}}) do
  local claims = {{iss = "{ISSUER}", sub = "alice", groups = {{"parkers"}}}}
  print("token " .. host.call("crypto/jwt_sign", {{key = "session-1", ttl = ttl, claims = claims}}))
end
"#,
        ttls.join(", ")
    );
    std::fs::write(dir.path().join("mint.dlua"), program).unwrap();
    let out = drt()
        .arg("--config")
        .arg(dir.path().join("drt.json"))
        .arg("run")
        .arg(dir.path().join("mint.dlua"))
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(
        out.status.success(),
        "{stdout}{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let jwks = stdout
        .lines()
        .find_map(|l| l.strip_prefix("jwks "))
        .unwrap()
        .to_string();
    let tokens = stdout
        .lines()
        .filter_map(|l| l.strip_prefix("token "))
        .map(str::to_string)
        .collect();
    (jwks, tokens)
}

#[test]
fn a_parked_side_swaps_its_jwt_from_the_token_file_and_keeps_the_name() {
    // One token that ends soon, its renewal, which also ends, and a third
    // that lasts: alice's, each holding park through `parkers`.
    let (jwks, tokens) = mint(&[10, 20, 600]);
    let started = Instant::now();
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("topics.json"),
        format!(
            r#"{{"jwt": {{"issuer": "{ISSUER}", "jwks": {jwks}}},
                 "topics": {{"home": {{
                   "tokens": [{{"token": "reader-token-1", "roles": ["read"]}}],
                   "groups": {{"parkers": ["park"]}}}}}}}}"#
        ),
    )
    .unwrap();
    let server = Server::start(dir.path());
    let token_file = dir.path().join("token");
    std::fs::write(&token_file, format!("{}\n", tokens[0])).unwrap();
    let mut cmd = drt();
    cmd.current_dir(dir.path()).args([
        "p2p",
        "--park",
        &format!("drt://127.0.0.1:{}/v1/home/box", server.port),
        "--forward",
        "127.0.0.1:9",
        "--token-file",
        "token",
    ]);
    let park = Proc::spawn("park", cmd);
    park.wait_for("drt p2p: present at");
    let listed = || server.get("/v1/home/names", "reader-token-1");
    assert!(listed().1.contains("\"box\""), "{:?}", listed());

    // The renewal, written before the first token ends, carried on the
    // next poll; past the first token's end the name is still held.
    replace(&token_file, &tokens[1]);
    park.wait_for("holds a new token");
    // The park polls every 5 s, so the renewal is in well before 10 s.
    let first_ends = Duration::from_secs(10) + Duration::from_secs(2);
    std::thread::sleep(first_ends.saturating_sub(started.elapsed()));
    assert!(listed().1.contains("\"box\""), "{:?}", listed());
    let said = server.proc.drain();
    assert!(!said.iter().any(|l| l.contains("is free")), "{said:#?}");

    // The renewal ends unrenewed: the server lets the name go, and the park
    // waits for a new token rather than exiting.
    server
        .proc
        .wait_for("home/box is free: the token it was parked with expired");
    park.wait_for("waiting for a new token in");
    assert!(!listed().1.contains("\"box\""), "{:?}", listed());

    // A new token in the file parks again.
    replace(&token_file, &tokens[2]);
    park.wait_for("drt p2p: present at");
    assert!(listed().1.contains("\"box\""), "{:?}", listed());
}

/// One token, one place: a token file beside an auth header is refused,
/// as is a token file on any role but a park.
#[test]
fn a_token_file_is_a_parks_alone() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("token"), "t0ken").unwrap();
    for (args, why) in [
        (
            vec![
                "--park",
                "drt://127.0.0.1:1/v1/box",
                "--forward",
                "127.0.0.1:9",
                "--token-file",
                "token",
                "--H",
                "auth=other",
            ],
            "both name the token",
        ),
        (
            vec!["drt://127.0.0.1:1/v1/box", "--token-file", "token"],
            "`--token-file` belongs with a park",
        ),
        (
            vec!["--match", "0", "--token-file", "token"],
            "`--token-file` belongs with a park",
        ),
    ] {
        let out = drt()
            .current_dir(dir.path())
            .arg("p2p")
            .args(&args)
            .stdin(Stdio::null())
            .output()
            .unwrap();
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(!out.status.success(), "{args:?}: {err}");
        assert!(err.contains(why), "{args:?}: {err}");
    }
}
