//! `stdlib:jwks` (`crates/drt/src/stdlib/jwks.dlua`), end to end on loopback: a `drt p2p
//! --match --kv --topics` server, the JWKS deployment publishing to one of
//! its topics with a write token, and tokens signed by a separate `drt run`
//! program on the same master. Every token is verified the way a service
//! holding only the served set would: `jsonwebtoken`, which shares no code
//! with the crypto connector, picking the key by `kid`.
//!
//! ## surface block
//!
//! - Entry points: the `#[test]`s below.
//! - Configurable: [`WAIT`], how long any one step may take; [`MASTER`],
//!   the crypto master the publisher and the signer share; [`WRITER`] and
//!   [`READER`], the topic's tokens.
//! - Fan-out: [`Match`] is the signalling server; [`Jwks`] the JWKS
//!   deployment (`drt start`, restartable with other keys); [`sign`] the
//!   signer, one `drt run`; [`verify`] a token checked against a fetched
//!   set.

#![cfg(all(feature = "p2p", feature = "connector-kv"))]

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use jsonwebtoken::jwk::JwkSet;

const WAIT: Duration = Duration::from_secs(20);
const MASTER: &str = "jwks-test-master-key-0123456789";
const WRITER: &str = "publisher-token";
const READER: &str = "reader-token";
const TOPIC: &str = "auth";

fn drt(home: &Path) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_drt"));
    c.env("HOME", home);
    c
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// Drain a child's pipe on a thread, keeping what it said.
fn collect(pipe: impl Read + Send + 'static) -> std::sync::Arc<std::sync::Mutex<String>> {
    let out = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
    let keep = out.clone();
    std::thread::spawn(move || {
        for line in BufReader::new(pipe).lines().map_while(Result::ok) {
            let mut k = keep.lock().unwrap();
            k.push_str(&line);
            k.push('\n');
        }
    });
    out
}

// depth: one HTTP request

#[derive(Debug)]
struct Reply {
    status: u16,
    headers: Vec<(String, String)>,
    body: String,
}

impl Reply {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }
}

fn send(
    port: u16,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: &str,
) -> Option<Reply> {
    let mut conn = TcpStream::connect(("127.0.0.1", port)).ok()?;
    conn.set_read_timeout(Some(WAIT)).unwrap();
    let mut head = format!(
        "{method} {path} HTTP/1.1\r\nHost: t\r\nConnection: close\r\nContent-Length: {}\r\n",
        body.len()
    );
    for (n, v) in headers {
        head.push_str(&format!("{n}: {v}\r\n"));
    }
    head.push_str("\r\n");
    conn.write_all(head.as_bytes()).ok()?;
    conn.write_all(body.as_bytes()).ok()?;
    let mut raw = Vec::new();
    conn.read_to_end(&mut raw).ok()?;
    let raw = String::from_utf8_lossy(&raw).into_owned();
    let (head, body) = raw.split_once("\r\n\r\n")?;
    let mut lines = head.lines();
    let status = lines.next()?.split_whitespace().nth(1)?.parse().ok()?;
    let headers = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(n, v)| (n.trim().to_ascii_lowercase(), v.trim().to_string()))
        .collect();
    Some(Reply {
        status,
        headers,
        body: body.to_string(),
    })
}

// depth: the signalling server

struct Match {
    child: Child,
    port: u16,
    dir: tempfile::TempDir,
}

impl Match {
    fn start() -> Match {
        let dir = tempfile::tempdir().unwrap();
        let topics = dir.path().join("topics.json");
        std::fs::write(
            &topics,
            format!(
                r#"{{"topics": {{"{TOPIC}": {{"tokens": [
                    {{"token": "{WRITER}", "roles": ["read", "write"]}},
                    {{"token": "{READER}", "roles": ["read"]}}]}}}}}}"#
            ),
        )
        .unwrap();
        let port = free_port();
        let mut child = drt(dir.path())
            .args(["p2p", "--match", &port.to_string(), "--topics"])
            .arg(&topics)
            .arg("--kv")
            .arg(dir.path().join("kv"))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut out = BufReader::new(child.stdout.take().unwrap()).lines();
        let line = out.next().expect("the server says what it serves").unwrap();
        assert!(line.contains("1 topic(s)"), "{line}");
        let line = out.next().expect("the server says it keeps kv").unwrap();
        // Only what every wording of this line shares: kv is on, and it is
        // not the no-token warning a server without topics prints.
        assert!(line.contains("kv in "), "{line}");
        assert!(!line.contains("no token"), "{line}");
        std::thread::spawn(move || for _ in out {});
        collect(child.stderr.take().unwrap());
        Match { child, port, dir }
    }

    fn kv(&self, method: &str, key: &str, token: Option<&str>, body: &str) -> Reply {
        let bearer = token.map(|t| format!("Bearer {t}"));
        let headers: Vec<(&str, &str)> = bearer
            .iter()
            .map(|b| ("authorization", b.as_str()))
            .collect();
        send(
            self.port,
            method,
            &format!("/v1/{TOPIC}/kv/{key}"),
            &headers,
            body,
        )
        .expect("the server answers")
    }

    fn stop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for Match {
    fn drop(&mut self) {
        self.stop();
    }
}

// depth: the JWKS deployment

struct Jwks {
    child: Child,
    port: u16,
    _dir: tempfile::TempDir,
    stdout: std::sync::Arc<std::sync::Mutex<String>>,
    stderr: std::sync::Arc<std::sync::Mutex<String>>,
}

impl Jwks {
    /// `drt start` on `stdlib:jwks`, publishing `keys` with `ttl`, its kv
    /// scope the topic with `token`. Does not wait for it to serve.
    fn spawn(m: &Match, keys: &str, ttl: u32, token: &str, port: u16) -> Jwks {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("app.json"),
            format!(
                r#"{{"entry": "stdlib:jwks",
                    "args": {{"keys": "{keys}", "ttl": {ttl}, "max_age": 60}},
                    "caps": [
                      {{"capability": "host:crypto/derive", "scope": ["issuer:*"]}},
                      {{"capability": "host:crypto/public_key"}},
                      {{"capability": "host:kv/put"}},
                      {{"capability": "host:kv/list"}},
                      {{"capability": "host:time/monotonic"}}],
                    "connectors": {{
                      "crypto": {{"scope": {{"key": "{MASTER}"}}}},
                      "kv": {{"scope": {{"url": "drt://127.0.0.1:{}/v1/{TOPIC}", "token": "{token}"}}}},
                      "time": {{}}}},
                    "listeners": [{{"scheme": "http", "address": "127.0.0.1:{port}",
                      "queue": "http_in", "reply_queue": "http_out",
                      "resp_headers": ["cache-control", "access-control-allow-origin"]}}]}}"#,
                m.port
            ),
        )
        .unwrap();
        let mut child = drt(dir.path())
            .current_dir(dir.path())
            .args(["start", "--config", "app.json"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let stdout = collect(child.stdout.take().unwrap());
        let stderr = collect(child.stderr.take().unwrap());
        Jwks {
            child,
            port,
            _dir: dir,
            stdout,
            stderr,
        }
    }

    /// Spawned, and serving a set holding `want` keys.
    fn start(m: &Match, keys: &str, ttl: u32, port: u16) -> Jwks {
        let j = Jwks::spawn(m, keys, ttl, WRITER, port);
        let want = keys.split_whitespace().count();
        j.wait_for(|set| set.keys.len() >= want, "the published keys");
        j
    }

    fn get(&self) -> Option<Reply> {
        send(self.port, "GET", "/.well-known/jwks.json", &[], "")
    }

    fn set(&self) -> JwkSet {
        let r = self.get().expect("the deployment answers");
        assert_eq!(r.status, 200, "{r:?}");
        serde_json::from_str(&r.body).unwrap_or_else(|e| panic!("{e}: {}", r.body))
    }

    fn wait_for(&self, ok: impl Fn(&JwkSet) -> bool, what: &str) -> JwkSet {
        let end = Instant::now() + WAIT;
        loop {
            if let Some(r) = self.get() {
                if r.status == 200 {
                    let set: JwkSet = serde_json::from_str(&r.body).unwrap();
                    if ok(&set) {
                        return set;
                    }
                }
            }
            assert!(
                Instant::now() < end,
                "no set with {what}; the deployment said:\n{}{}",
                self.stdout.lock().unwrap(),
                self.stderr.lock().unwrap()
            );
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    fn stop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for Jwks {
    fn drop(&mut self) {
        self.stop();
    }
}

// depth: the signer and the verifier

/// One token per kid, signed by a `drt run` program that shares only the
/// master with the publisher: a separate process, as an issuer would be.
fn sign(kids: &[&str]) -> Vec<String> {
    let dir = tempfile::tempdir().unwrap();
    let mut src = String::new();
    for kid in kids {
        src.push_str(&format!(
            "host.call('crypto/derive', {{label = '{kid}', alg = 'EdDSA'}})\n\
             print(host.call('crypto/jwt_sign', {{key = '{kid}', ttl = 600, claims = {{sub = 'alice', by = '{kid}'}}}}))\n"
        ));
    }
    std::fs::write(dir.path().join("sign.dlua"), src).unwrap();
    std::fs::write(
        dir.path().join("sign.json"),
        format!(
            r#"{{"program": {{"path": "sign.dlua"}},
                "caps": [{{"capability": "host:crypto/derive", "scope": ["issuer:*"]}},
                         {{"capability": "host:crypto/jwt_sign"}}],
                "connectors": {{"crypto": {{"scope": {{"key": "{MASTER}"}}}}}}}}"#
        ),
    )
    .unwrap();
    let out = drt(dir.path())
        .current_dir(dir.path())
        .args(["run", "--config", "sign.json"])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(
        out.status.success(),
        "{stdout}\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let tokens: Vec<String> = stdout.lines().map(str::to_string).collect();
    assert_eq!(tokens.len(), kids.len(), "{stdout}");
    tokens
}

/// What a relying service does: read `kid` from the header, find that key
/// in the set, and verify as EdDSA. `Err` says which step refused.
fn verify(set: &JwkSet, token: &str) -> Result<serde_json::Value, String> {
    let header = jsonwebtoken::decode_header(token).map_err(|e| format!("header: {e}"))?;
    let kid = header.kid.ok_or("no kid")?;
    let jwk = set.find(&kid).ok_or(format!("no key {kid} in the set"))?;
    let key = jsonwebtoken::DecodingKey::from_jwk(jwk).map_err(|e| format!("jwk: {e}"))?;
    let v = jsonwebtoken::Validation::new(jsonwebtoken::Algorithm::EdDSA);
    jsonwebtoken::decode::<serde_json::Value>(token, &key, &v)
        .map(|d| d.claims)
        .map_err(|e| format!("verify: {e}"))
}

fn kids(set: &JwkSet) -> Vec<String> {
    let mut k: Vec<String> = set
        .keys
        .iter()
        .filter_map(|j| j.common.key_id.clone())
        .collect();
    k.sort();
    k
}

// depth: the tests

/// Old and new keys are served together, each token finds its own by
/// `kid`, and a token under a key nobody published finds nothing.
#[test]
fn a_service_verifies_old_and_new_tokens_by_kid_from_the_served_set() {
    let m = Match::start();
    let j = Jwks::start(&m, "issuer:2 issuer:1", 60, free_port());
    let tokens = sign(&["issuer:1", "issuer:2", "issuer:3"]);

    let r = j.get().unwrap();
    assert_eq!(r.header("content-type"), Some("application/json"));
    assert_eq!(r.header("cache-control"), Some("max-age=60"));
    assert_eq!(r.header("access-control-allow-origin"), Some("*"));

    let set = j.set();
    assert_eq!(kids(&set), ["issuer:1", "issuer:2"]);
    for (token, kid) in tokens.iter().zip(["issuer:1", "issuer:2"]) {
        let claims = verify(&set, token).unwrap_or_else(|e| panic!("{kid}: {e}"));
        assert_eq!(claims["sub"], "alice");
        assert_eq!(claims["by"], kid);
    }
    assert_eq!(
        verify(&set, &tokens[2]).unwrap_err(),
        "no key issuer:3 in the set"
    );

    // A token's signature under the wrong key of the set does not pass:
    // issuer:1's token with issuer:2's kid.
    let mut parts: Vec<String> = tokens[0].split('.').map(str::to_string).collect();
    use base64::Engine as _;
    let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;
    parts[0] = b64.encode(br#"{"alg":"EdDSA","typ":"JWT","kid":"issuer:2"}"#);
    assert!(verify(&set, &parts.join("."))
        .unwrap_err()
        .starts_with("verify:"));
}

/// The set carries public members only, as served and as kept in kv, and
/// the kv entries stay behind the topic's tokens.
#[test]
fn only_public_members_leave_the_host_and_kv_keeps_its_tokens() {
    let m = Match::start();
    let j = Jwks::start(&m, "issuer:1", 60, free_port());
    let r = j.get().unwrap();
    let raw: serde_json::Value = serde_json::from_str(&r.body).unwrap();
    let key = raw["keys"][0].as_object().unwrap();
    let mut members: Vec<&str> = key.keys().map(String::as_str).collect();
    members.sort();
    assert_eq!(members, ["alg", "crv", "kid", "kty", "use", "x"]);
    assert_eq!(key["kty"], "OKP");
    assert_eq!(key["crv"], "Ed25519");
    assert_eq!(key["alg"], "EdDSA");
    assert_eq!(key["use"], "sig");

    // In kv under the kid's escaped key, the same public members.
    let kept = m.kv("GET", "jwks/issuer~3A1", Some(READER), "");
    assert_eq!(kept.status, 200, "{kept:?}");
    let kept: serde_json::Value = serde_json::from_str(&kept.body).unwrap();
    assert_eq!(&kept, &raw["keys"][0]);
    // The whole set is also one kv value, byte for byte what is served,
    // for a reader that wants one key (the match server's JWT tokens).
    let whole = m.kv("GET", "jwks", Some(READER), "");
    assert_eq!(whole.status, 200, "{whole:?}");
    assert_eq!(whole.body, r.body);
    // No token, 401; a read token may not write.
    assert_eq!(m.kv("GET", "jwks/issuer~3A1", None, "").status, 401);
    assert_eq!(
        m.kv("PUT", "jwks/issuer~3A1", Some(READER), "{}").status,
        403
    );
}

/// A key taken out of `keys` stays while its last TTL runs, so tokens it
/// signed keep verifying, and then leaves; a key still listed is refreshed
/// past its TTL and stays.
#[test]
fn a_retired_key_leaves_the_set_when_its_ttl_runs_out() {
    let m = Match::start();
    let port = free_port();
    let mut j = Jwks::start(&m, "issuer:2 issuer:1", 3, port);
    let tokens = sign(&["issuer:1", "issuer:2"]);
    j.stop();

    let j = Jwks::start(&m, "issuer:2", 3, port);
    let at = Instant::now();
    let set = j.set();
    assert_eq!(
        kids(&set),
        ["issuer:1", "issuer:2"],
        "the old key is still within its TTL"
    );
    verify(&set, &tokens[0]).expect("an old token verifies while its key is served");

    let set = j.wait_for(|s| s.keys.len() == 1, "issuer:1 gone");
    assert!(at.elapsed() <= Duration::from_secs(5), "{:?}", at.elapsed());
    assert_eq!(kids(&set), ["issuer:2"]);
    // The one-value set follows within SYNC_MS (5 s) of the key leaving.
    let end = Instant::now() + Duration::from_secs(8);
    loop {
        let whole = m.kv("GET", "jwks", Some(READER), "");
        let whole: JwkSet = serde_json::from_str(&whole.body).unwrap();
        if kids(&whole) == ["issuer:2"] {
            break;
        }
        assert!(
            Instant::now() < end,
            "the set key still holds {:?}",
            kids(&whole)
        );
        std::thread::sleep(Duration::from_millis(200));
    }
    assert_eq!(
        verify(&set, &tokens[0]).unwrap_err(),
        "no key issuer:1 in the set"
    );

    // Two TTLs later the listed key is still there: it is refreshed.
    std::thread::sleep(Duration::from_secs(6));
    let set = j.set();
    assert_eq!(kids(&set), ["issuer:2"]);
    verify(&set, &tokens[1]).expect("the current key verifies");
}

/// Whatever else a writer put under `jwks/` is filtered: a symmetric key,
/// an entry holding a private member, one kept under another kid's key,
/// one without a kid, and one that is not JSON are not served.
#[test]
fn what_the_topic_holds_is_filtered_before_it_is_served() {
    let m = Match::start();
    let j = Jwks::start(&m, "issuer:1", 60, free_port());
    let good = j.set();
    let x = good.keys[0].clone();
    let x = serde_json::to_value(&x).unwrap()["x"]
        .as_str()
        .unwrap()
        .to_string();
    let put = |key: &str, value: String| {
        let r = m.kv("PUT", key, Some(WRITER), &value);
        assert!(r.status == 200 || r.status == 201, "{key}: {r:?}");
    };
    put(
        "jwks/oct",
        r#"{"kty":"oct","k":"c2VjcmV0","kid":"oct"}"#.into(),
    );
    put(
        "jwks/leak",
        format!(r#"{{"kty":"OKP","crv":"Ed25519","x":"{x}","d":"{x}","kid":"leak"}}"#),
    );
    put(
        "jwks/elsewhere",
        format!(r#"{{"kty":"OKP","crv":"Ed25519","x":"{x}","kid":"issuer:1"}}"#),
    );
    put(
        "jwks/nokid",
        format!(r#"{{"kty":"OKP","crv":"Ed25519","x":"{x}"}}"#),
    );
    put("jwks/junk", "not json".into());
    put(
        "jwks/other",
        format!(r#"{{"kty":"OKP","crv":"Ed25519","x":"{x}","kid":"other","extra":"dropped"}}"#),
    );

    let set = j.wait_for(|s| s.keys.len() == 2, "the one acceptable extra");
    assert_eq!(kids(&set), ["issuer:1", "other"]);
    let raw: serde_json::Value = serde_json::from_str(&j.get().unwrap().body).unwrap();
    let other = raw["keys"]
        .as_array()
        .unwrap()
        .iter()
        .find(|k| k["kid"] == "other")
        .unwrap();
    assert!(other.get("extra").is_none(), "{other}");
    // Each refusal is said once, by key and reason.
    let said = j.stdout.lock().unwrap().clone();
    for line in [
        "not serving jwks/oct: kty oct is not a public key type",
        "not serving jwks/leak: it holds the private member d",
        "not serving jwks/elsewhere: kept under jwks/elsewhere, not its kid's key",
        "not serving jwks/nokid: no kid",
        "not serving jwks/junk: not JSON",
    ] {
        assert_eq!(said.matches(line).count(), 1, "{line}\n{said}");
    }
}

/// A publisher whose token may not write stops at start and says why,
/// rather than serving an empty set.
#[test]
fn a_publisher_whose_token_may_not_write_stops_and_says_why() {
    let m = Match::start();
    let mut j = Jwks::spawn(&m, "issuer:1", 60, READER, free_port());
    let end = Instant::now() + WAIT;
    // The exit status is not checked: `drt start` with a listener exits 0
    // when its root instance faults (start.rs, `alive == 0`), which is
    // `drt start`'s behaviour for every program, not this one's.
    while j.child.try_wait().unwrap().is_none() {
        assert!(Instant::now() < end, "the publisher kept running");
        std::thread::sleep(Duration::from_millis(100));
    }
    let said = format!("{}{}", j.stdout.lock().unwrap(), j.stderr.lock().unwrap());
    assert!(
        said.contains("faulted") && said.contains("kv/put") && said.contains("403"),
        "{said}"
    );
    j.stop();
}

/// With the kv server gone the set is 503 and not cached, never an empty
/// set a verifier would take as "no keys".
#[test]
fn a_store_that_does_not_answer_is_503_not_an_empty_set() {
    let mut m = Match::start();
    let j = Jwks::start(&m, "issuer:1", 60, free_port());
    m.stop();
    let r = j.get().unwrap();
    assert_eq!(r.status, 503, "{r:?}");
    assert_eq!(r.header("cache-control"), Some("no-store"));
    assert!(m.dir.path().exists());
}
