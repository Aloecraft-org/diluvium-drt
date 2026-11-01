//! LDAP groups deciding who may park, call, read and write in a topic, end
//! to end and through the real binary, as a person would set it up:
//!
//! 1. A real OpenLDAP `slapd` holds the users and their groups.
//! 2. A dlua program under `drt run` logs each user in with `ldap/bind`
//!    and signs a JWT carrying the groups it answered, with an EdDSA key
//!    that stays in the host (`crypto/jwt_sign`), and prints the key's
//!    public half (`crypto/public_key`). That program is the session
//!    framework's job in miniature.
//! 3. `drt p2p --match --topics --kv` admits those JWTs, mapping groups to
//!    roles, and checks them against the JWKS kept in its own kv.
//! 4. `drt p2p --park` and `drt p2p` park and call with them, and `drt kv`
//!    and plain HTTP read and write kv with them, each allowed and refused
//!    by group.
//!
//! Without `slapd` (Debian/Ubuntu: `apt install slapd`) the tests say
//! `skipped` and pass, unless `DRT_TEST_REQUIRE_LDAP=1` is set, as CI sets
//! it after installing one.
//!
//! ## surface block
//!
//! - Entry points: the tests; [`Slapd::start`]; [`issue`], the stand-in
//!   session issuer; [`Match::start`], the server.
//! - Configurable values: [`SLAPD`], [`SCHEMA_DIRS`], [`MODULE_DIRS`];
//!   the directory in [`LDIF`]; [`ISSUER`] and [`AUDIENCE`].
//! - Fan-out: none.

#![cfg(all(unix, feature = "connector-ldap", feature = "connector-kv"))]

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const SLAPD: &[&str] = &[
    "/usr/sbin/slapd",
    "/usr/local/libexec/slapd",
    "/usr/libexec/slapd",
];
const SCHEMA_DIRS: &[&str] = &[
    "/etc/ldap/schema",
    "/etc/openldap/schema",
    "/usr/local/etc/openldap/schema",
];
const MODULE_DIRS: &[&str] = &["/usr/lib/ldap", "/usr/lib64/openldap", "/usr/lib/openldap"];
const BASE: &str = "dc=example,dc=com";
const ISSUER: &str = "drt-session";
const AUDIENCE: &str = "match-test";
const WAIT: Duration = Duration::from_secs(20);

/// alice parks, bob calls, carol reads, dave reads and writes, erin is
/// staff and nothing else. Every user is in `staff`, which no topic maps.
const LDIF: &str = "\
dn: dc=example,dc=com
objectClass: dcObject
objectClass: organization
dc: example
o: Example

dn: ou=people,dc=example,dc=com
objectClass: organizationalUnit
ou: people

dn: ou=services,dc=example,dc=com
objectClass: organizationalUnit
ou: services

dn: ou=groups,dc=example,dc=com
objectClass: organizationalUnit
ou: groups

dn: cn=svc,ou=services,dc=example,dc=com
objectClass: inetOrgPerson
cn: svc
sn: svc
userPassword: service-secret

dn: uid=alice,ou=people,dc=example,dc=com
objectClass: inetOrgPerson
uid: alice
cn: Alice
sn: A
userPassword: alice-pw

dn: uid=bob,ou=people,dc=example,dc=com
objectClass: inetOrgPerson
uid: bob
cn: Bob
sn: B
userPassword: bob-pw

dn: uid=carol,ou=people,dc=example,dc=com
objectClass: inetOrgPerson
uid: carol
cn: Carol
sn: C
userPassword: carol-pw

dn: uid=dave,ou=people,dc=example,dc=com
objectClass: inetOrgPerson
uid: dave
cn: Dave
sn: D
userPassword: dave-pw

dn: uid=erin,ou=people,dc=example,dc=com
objectClass: inetOrgPerson
uid: erin
cn: Erin
sn: E
userPassword: erin-pw

dn: cn=parkers,ou=groups,dc=example,dc=com
objectClass: groupOfNames
cn: parkers
member: uid=alice,ou=people,dc=example,dc=com

dn: cn=callers,ou=groups,dc=example,dc=com
objectClass: groupOfNames
cn: callers
member: uid=bob,ou=people,dc=example,dc=com

dn: cn=readers,ou=groups,dc=example,dc=com
objectClass: groupOfNames
cn: readers
member: uid=carol,ou=people,dc=example,dc=com

dn: cn=writers,ou=groups,dc=example,dc=com
objectClass: groupOfNames
cn: writers
member: uid=dave,ou=people,dc=example,dc=com

dn: cn=staff,ou=groups,dc=example,dc=com
objectClass: groupOfNames
cn: staff
member: uid=alice,ou=people,dc=example,dc=com
member: uid=bob,ou=people,dc=example,dc=com
member: uid=carol,ou=people,dc=example,dc=com
member: uid=dave,ou=people,dc=example,dc=com
member: uid=erin,ou=people,dc=example,dc=com
";

/// The topics file's group map for `home`, shared by both tests.
const HOME_GROUPS: &str = r#"{"parkers": ["park"], "callers": ["call"],
    "readers": ["read"], "writers": ["read", "write"]}"#;

macro_rules! skipped {
    ($($arg:tt)*) => {{
        if std::env::var_os("DRT_TEST_REQUIRE_LDAP").is_some() {
            panic!("DRT_TEST_REQUIRE_LDAP is set, and this test would have skipped: {}", format!($($arg)*));
        }
        eprintln!("skipped: {}", format!($($arg)*));
    }};
}

// depth: slapd, plaintext on loopback

struct Slapd {
    child: Child,
    dir: PathBuf,
    port: u16,
}

impl Drop for Slapd {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// One copy of slapd per process, at a path of its own. Ubuntu's AppArmor
/// profile attaches to /usr/sbin/slapd and confines it to its own
/// directories, so the installed binary cannot read this test's config
/// and database; the same program at another path is not confined
/// (`connectors/ldap/tests/openldap.rs` found this in CI).
fn unconfined_copy(slapd: &Path) -> PathBuf {
    static COPY: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    COPY.get_or_init(|| {
        let dir = std::env::temp_dir().join(format!("drt-topics-jwt-bin-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let copy = dir.join("slapd");
        std::fs::copy(slapd, &copy).unwrap();
        copy
    })
    .clone()
}

fn first_existing(candidates: &[&str]) -> Option<PathBuf> {
    candidates.iter().map(PathBuf::from).find(|p| p.exists())
}

fn free_tcp_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

impl Slapd {
    /// A fresh slapd, or `None` (having said why) when this machine has none.
    fn start(name: &str) -> Option<Slapd> {
        let Some(slapd) = first_existing(SLAPD) else {
            skipped!("no slapd in {SLAPD:?}");
            return None;
        };
        let Some(schema) =
            first_existing(SCHEMA_DIRS).filter(|d| d.join("inetorgperson.schema").exists())
        else {
            skipped!("no slapd schema in {SCHEMA_DIRS:?}");
            return None;
        };
        let dir =
            std::env::temp_dir().join(format!("drt-topics-jwt-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("db")).unwrap();
        let module = match first_existing(MODULE_DIRS) {
            Some(m) if m.join("back_mdb.so").exists() || m.join("back_mdb.la").exists() => {
                format!("modulepath {}\nmoduleload back_mdb\n", m.display())
            }
            _ => String::new(),
        };
        let (d, s) = (dir.display(), schema.display());
        let conf = format!(
            "include {s}/core.schema\ninclude {s}/cosine.schema\ninclude {s}/inetorgperson.schema\n\
             pidfile {d}/slapd.pid\n{module}\
             database mdb\nmaxsize 10485760\nsuffix \"{BASE}\"\nrootdn \"cn=admin,{BASE}\"\n\
             rootpw admin-pw\ndirectory {d}/db\n\
             access to attrs=userPassword by self read by anonymous auth by * none\n\
             access to * by * read\n"
        );
        std::fs::write(dir.join("slapd.conf"), conf).unwrap();
        std::fs::write(dir.join("data.ldif"), LDIF).unwrap();
        let added = Command::new(slapd.with_file_name("slapadd"))
            .arg("-f")
            .arg(dir.join("slapd.conf"))
            .arg("-l")
            .arg(dir.join("data.ldif"))
            .output()
            .unwrap();
        assert!(
            added.status.success(),
            "slapadd: {}",
            String::from_utf8_lossy(&added.stderr)
        );
        let port = free_tcp_port();
        let copy = unconfined_copy(&slapd);
        // Another test's fork can briefly hold the copy's write descriptor
        // while it is made, and exec then fails with ETXTBSY; try again.
        let mut tries = 0;
        let child = loop {
            let spawned = Command::new(&copy)
                .arg("-f")
                .arg(dir.join("slapd.conf"))
                .arg("-h")
                .arg(format!("ldap://127.0.0.1:{port}/"))
                .args(["-d", "0"])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn();
            match spawned {
                Err(e) if e.raw_os_error() == Some(26) && tries < 20 => {
                    tries += 1;
                    std::thread::sleep(Duration::from_millis(50));
                }
                other => break other.unwrap(),
            }
        };
        let server = Slapd { child, dir, port };
        let deadline = Instant::now() + Duration::from_secs(10);
        while TcpStream::connect(("127.0.0.1", port)).is_err() {
            assert!(Instant::now() < deadline, "slapd did not listen on {port}");
            std::thread::sleep(Duration::from_millis(50));
        }
        Some(server)
    }
}

// depth: the stand-in session issuer, a dlua program under `drt run`

/// Each process gets a home of its own, as `drt p2p` keeps state there.
fn drt() -> Command {
    static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let home = std::env::temp_dir().join(format!("drt-topics-jwt-home-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&home).unwrap();
    let mut c = Command::new(env!("CARGO_BIN_EXE_drt"));
    c.env("HOME", home);
    c
}

/// A PKCS#8 PEM for a raw Ed25519 secret (RFC 8410's fixed prefix), the
/// shape `openssl genpkey -algorithm ed25519` writes.
fn pem(secret: u8) -> String {
    use base64::Engine as _;
    let mut der = vec![
        0x30, 0x2e, 0x02, 0x01, 0x00, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x04, 0x22, 0x04,
        0x20,
    ];
    der.extend_from_slice(&[secret; 32]);
    let b64 = base64::engine::general_purpose::STANDARD.encode(der);
    format!("-----BEGIN PRIVATE KEY-----\n{b64}\n-----END PRIVATE KEY-----\n")
}

/// The login program: bind each user, and for each that binds sign a JWT
/// with their groups. `SPECS` is a Lua list of `{label, user, password,
/// iss?, aud?, ttl?}`.
const ISSUE: &str = r#"
local jwk = host.call("crypto/public_key", {key = "session-1"})
print("jwks " .. json.encode({keys = {jwk}}))
for _, s in ipairs(SPECS) do
  local who = host.call("ldap/bind", {user = s.user, password = s.password})
  if who.ok then
    local claims = {iss = s.iss or ISSUER, aud = s.aud or AUDIENCE, sub = s.user, groups = who.groups}
    local token = host.call("crypto/jwt_sign", {key = "session-1", ttl = s.ttl or 600, claims = claims})
    print("token " .. s.label .. " " .. token)
  else
    print("refused " .. s.label)
  end
end
"#;

/// What [`issue`] printed: the issuer's JWKS, a token per label that bound,
/// and the labels that did not.
struct Issued {
    jwks: String,
    tokens: BTreeMap<String, String>,
    refused: Vec<String>,
}

impl Issued {
    fn token(&self, label: &str) -> &str {
        self.tokens
            .get(label)
            .unwrap_or_else(|| panic!("no token for {label}"))
    }
}

/// Run the login program against `slapd`, signing with an EdDSA key named
/// `session-1` made from `secret` (a second secret makes a second issuer
/// with the same key name: a forger's tokens).
fn issue(slapd: &Slapd, secret: u8, specs: &str) -> Issued {
    let dir = tempfile::tempdir().unwrap();
    let pem_path = dir.path().join("session-1.pem");
    std::fs::write(&pem_path, pem(secret)).unwrap();
    let config = serde_json::json!({
        "caps": [
            {"capability": "host:ldap/bind"},
            {"capability": "host:crypto/jwt_sign"},
            {"capability": "host:crypto/public_key"},
        ],
        "connectors": {
            "ldap": {"scope": {
                "url": format!("ldap://127.0.0.1:{}", slapd.port), "starttls": false,
                "base_dn": BASE, "bind_dn": format!("cn=svc,ou=services,{BASE}"),
                "bind_password": "service-secret", "group_base": format!("ou=groups,{BASE}"),
            }},
            "crypto": {"scope": {
                "key": "the-issuers-master-key-0123456789",
                "signing_keys": [{"name": "session-1", "alg": "EdDSA",
                                  "private_key": {"file": pem_path}}],
            }},
        },
    });
    std::fs::write(dir.path().join("drt.json"), config.to_string()).unwrap();
    let program = format!(
        "local ISSUER, AUDIENCE = {ISSUER:?}, {AUDIENCE:?}\nlocal SPECS = {specs}\n{ISSUE}"
    );
    std::fs::write(dir.path().join("login.dlua"), program).unwrap();
    let out = drt()
        .arg("--config")
        .arg(dir.path().join("drt.json"))
        .arg("run")
        .arg(dir.path().join("login.dlua"))
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(
        out.status.success(),
        "{stdout}{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let mut issued = Issued {
        jwks: String::new(),
        tokens: BTreeMap::new(),
        refused: Vec::new(),
    };
    for line in stdout.lines() {
        if let Some(j) = line.strip_prefix("jwks ") {
            issued.jwks = j.to_string();
        } else if let Some(rest) = line.strip_prefix("token ") {
            let (label, token) = rest.split_once(' ').unwrap();
            issued.tokens.insert(label.to_string(), token.to_string());
        } else if let Some(label) = line.strip_prefix("refused ") {
            issued.refused.push(label.to_string());
        }
    }
    issued
}

const EVERYONE: &str = r#"{
  {label = "alice", user = "alice", password = "alice-pw"},
  {label = "alice-renewed", user = "alice", password = "alice-pw"},
  {label = "bob", user = "bob", password = "bob-pw"},
  {label = "carol", user = "carol", password = "carol-pw"},
  {label = "dave", user = "dave", password = "dave-pw"},
  {label = "erin", user = "erin", password = "erin-pw"},
  {label = "bob-wrong-password", user = "bob", password = "guess"},
  {label = "nobody", user = "nobody", password = "x"},
}"#;

// depth: the match server, a parked peer, a caller

struct Match {
    child: Child,
    port: u16,
    base: String,
    out: std::sync::mpsc::Receiver<String>,
}

impl Match {
    /// `drt p2p --match` with `args`, read until it has said what it
    /// serves: one line, one more for kv and one more for a jwt block.
    fn start(dir: &Path, args: &[&str], lines: usize) -> Match {
        let port = std::net::UdpSocket::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let mut child = drt()
            .current_dir(dir)
            .arg("p2p")
            .arg("--match")
            .arg(port.to_string())
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let stdout = child.stdout.take().unwrap();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                eprintln!("match: {line}");
                let _ = tx.send(line);
            }
        });
        let stderr = child.stderr.take().unwrap();
        std::thread::spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                eprintln!("match: {line}");
            }
        });
        let server = Match {
            child,
            port,
            base: format!("http://127.0.0.1:{port}"),
            out: rx,
        };
        for _ in 0..lines {
            server.line();
        }
        server
    }

    fn line(&self) -> String {
        self.out
            .recv_timeout(WAIT)
            .expect("the server printed a line")
    }

    /// One request with `token` as a bearer (or none).
    fn request(&self, method: &str, path: &str, token: Option<&str>, body: &str) -> (u16, String) {
        let mut conn = TcpStream::connect(("127.0.0.1", self.port)).unwrap();
        conn.set_read_timeout(Some(WAIT)).unwrap();
        let auth = token
            .map(|t| format!("Authorization: Bearer {t}\r\n"))
            .unwrap_or_default();
        conn.write_all(
            format!(
                "{method} {path} HTTP/1.1\r\nHost: t\r\nConnection: close\r\n{auth}\
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

    fn get(&self, path: &str, token: Option<&str>) -> (u16, String) {
        self.request("GET", path, token, "")
    }

    fn kv_url(&self, topic: &str, key: &str) -> String {
        format!("drt://127.0.0.1:{}/v1/{topic}/kv/{key}", self.port)
    }
}

impl Drop for Match {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Echoes every connection until it closes: the parked side's service.
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

/// `drt p2p --park` at `name` with `token`, read until present.
struct Parked {
    child: Child,
}

impl Parked {
    fn start(server: &Match, name: &str, token: &str) -> Parked {
        let mut child = drt()
            .arg("p2p")
            .arg("--park")
            .arg(format!("{}/v1/{name}", server.base))
            .arg("--H")
            .arg(format!("auth={token}"))
            .arg("--forward")
            .arg(format!("127.0.0.1:{}", echo()))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut err = BufReader::new(child.stderr.take().unwrap());
        let start = Instant::now();
        let mut seen = String::new();
        loop {
            let mut line = String::new();
            assert!(
                err.read_line(&mut line).unwrap() > 0,
                "the parked peer ended:\n{seen}"
            );
            seen.push_str(&line);
            if line.contains("present at ") {
                break;
            }
            assert!(start.elapsed() < WAIT, "never present:\n{seen}");
        }
        std::thread::spawn(move || for _ in err.lines() {});
        Parked { child }
    }
}

impl Drop for Parked {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// `drt p2p <peer>` with `token`, `input` on stdin: (stdout, stderr).
fn call(peer: &str, token: Option<&str>, input: &[u8]) -> (String, String) {
    let mut cmd = drt();
    cmd.arg("p2p").arg(peer);
    if let Some(t) = token {
        cmd.arg("--H").arg(format!("auth={t}"));
    }
    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let input = input.to_vec();
    std::thread::spawn(move || {
        let _ = stdin.write_all(&input);
        std::thread::sleep(Duration::from_millis(1500));
    });
    let out = child.wait_with_output().unwrap();
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// `drt kv <args> --H auth=<token>`, `stdin` written: (status, stdout, stderr).
fn kv(args: &[&str], token: &str, stdin: &str) -> (i32, String, String) {
    let mut child = drt()
        .arg("kv")
        .args(args)
        .arg("--H")
        .arg(format!("auth={token}"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(stdin.as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

// depth: the tests

/// The whole chain with the JWKS kept in the server's own kv, where the
/// issuer publishes it: directory groups decide park, call, read and
/// write, each allowed for its group and refused for the others; static
/// tokens and bare names keep working beside them; and replacing the
/// published JWKS changes which tokens verify, with no restart.
#[test]
fn ldap_groups_decide_who_parks_calls_reads_and_writes() {
    let Some(slapd) = Slapd::start("roles") else {
        return;
    };
    let issued = issue(&slapd, 7, EVERYONE);
    let forged = issue(&slapd, 9, EVERYONE);
    // A wrong password and an unknown user get no token at all.
    assert_eq!(issued.refused, ["bob-wrong-password", "nobody"]);

    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("topics.json"),
        format!(
            r#"{{
              "jwt": {{"issuer": "{ISSUER}", "audience": "{AUDIENCE}", "jwks": {{"kv": "auth/jwks"}}}},
              "topics": {{
                "home": {{"tokens": [{{"token": "home-static", "roles": ["park", "call"]}}],
                         "groups": {HOME_GROUPS}}},
                "auth": {{"tokens": [{{"token": "jwks-publisher", "roles": ["write"]}}]}}
              }}
            }}"#
        ),
    )
    .unwrap();
    let server = Match::start(dir.path(), &["--topics", "topics.json", "--kv", "kv"], 3);
    let names = "/v1/home/names";

    // Until the issuer publishes its keys, a JWT verifies against nothing.
    let (status, body) = server.get(names, Some(issued.token("carol")));
    assert_eq!(status, 401, "{body}");
    assert!(body.contains("no signing keys are published"), "{body}");

    // The issuer publishes its JWKS with the auth topic's write token.
    let jwks_url = server.kv_url("auth", "jwks");
    let (code, _, err) = kv(&["put", &jwks_url, "-"], "jwks-publisher", &issued.jwks);
    assert_eq!(code, 0, "{err}");

    // park: alice (parkers) parks; bob, carol, dave and erin may not.
    let box_url = format!("{}/v1/home/box", server.base);
    let _box = Parked::start(&server, "home/box", issued.token("alice"));
    for who in ["bob", "carol", "dave", "erin"] {
        let (status, body) = server.get("/v1/home/desk/calls", Some(issued.token(who)));
        assert_eq!(status, 403, "{who} parked: {body}");
    }

    // call: bob (callers) reaches the parked service through a real
    // WebRTC session; everyone else is refused before any session.
    let (out, errs) = call(&box_url, Some(issued.token("bob")), b"hello from bob\n");
    assert_eq!(out, "hello from bob\n", "{errs}");
    for who in ["alice", "carol", "dave", "erin"] {
        let (out, errs) = call(&box_url, Some(issued.token(who)), b"x\n");
        assert!(
            out.is_empty() && errs.contains("403"),
            "{who} called: {errs}"
        );
    }
    let (_, errs) = call(&box_url, None, b"x\n");
    assert!(errs.contains("401"), "{errs}");
    // A token signed by another key under the same kid, and one with the
    // signature of another token, do not verify.
    let (_, errs) = call(&box_url, Some(forged.token("bob")), b"x\n");
    assert!(errs.contains("401"), "a forged token called: {errs}");
    let (status, body) = server.get(names, Some(forged.token("carol")));
    assert_eq!(status, 401, "{body}");
    assert!(body.contains("signature"), "{body}");

    // read: carol (readers) and dave (writers) list the topic; the others
    // may not.
    for who in ["carol", "dave"] {
        assert_eq!(
            server.get(names, Some(issued.token(who))),
            (200, r#"{"topic":"home","names":["box"]}"#.to_string()),
            "{who}"
        );
    }
    for who in ["alice", "bob", "erin"] {
        assert_eq!(
            server.get(names, Some(issued.token(who))).0,
            403,
            "{who} listed"
        );
    }

    // kv: dave (writers) writes with `drt kv`; carol (readers) reads it and
    // may not write; alice, bob and erin may do neither.
    let note = server.kv_url("home", "note");
    let (code, out, err) = kv(
        &["put", &note, "the box is in the hall"],
        issued.token("dave"),
        "",
    );
    assert_eq!((code, out.as_str()), (0, "2\n"), "{err}");
    let (code, out, err) = kv(&["get", &note], issued.token("carol"), "");
    assert_eq!((code, out.as_str()), (0, "the box is in the hall"), "{err}");
    let (code, _, err) = kv(&["put", &note, "carol was here"], issued.token("carol"), "");
    assert_eq!(code, 1, "carol wrote: {err}");
    assert!(err.contains("403"), "{err}");
    for who in ["alice", "bob", "erin"] {
        let path = "/v1/home/kv/note";
        assert_eq!(
            server.get(path, Some(issued.token(who))).0,
            403,
            "{who} read"
        );
        assert_eq!(
            server.request("PUT", path, Some(issued.token(who)), "x").0,
            403,
            "{who} wrote"
        );
    }
    assert_eq!(
        server.get("/v1/home/kv/note", Some(issued.token("dave"))).1,
        "the box is in the hall"
    );

    // A JWT holds a name by its subject: alice's renewed token keeps the
    // box, and the static park token may not take it.
    assert_eq!(
        server
            .get("/v1/home/box/calls", Some(issued.token("alice-renewed")))
            .0,
        200
    );
    assert_eq!(server.get("/v1/home/box/calls", Some("home-static")).0, 403);

    // Beside the JWTs, a static token of the topic and a bare name work as
    // they did; a valid JWT in an undeclared topic is refused the same way
    // as a token that may not.
    assert_eq!(
        server.get("/v1/home/shelf/calls", Some("home-static")).0,
        200
    );
    assert_eq!(server.get("/v1/mypc/calls?k=anyone", None).0, 200);
    assert_eq!(
        server
            .get("/v1/elsewhere/names", Some(issued.token("carol")))
            .0,
        403
    );

    // Rotation is a kv write: once the forger's key is the published one,
    // the issuer's tokens stop verifying and the forger's start.
    let (code, _, err) = kv(&["put", &jwks_url, "-"], "jwks-publisher", &forged.jwks);
    assert_eq!(code, 0, "{err}");
    assert_eq!(server.get(names, Some(issued.token("carol"))).0, 401);
    assert_eq!(server.get(names, Some(forged.token("carol"))).0, 200);
    // A value that is not a JWKS admits nobody, and says so.
    let (code, _, err) = kv(&["put", &jwks_url, "not a key set"], "jwks-publisher", "");
    assert_eq!(code, 0, "{err}");
    let (status, body) = server.get(names, Some(forged.token("carol")));
    assert_eq!(status, 401, "{body}");
    assert!(body.contains("do not read as a JWKS"), "{body}");
    // Only the auth topic's write token publishes keys: a writer of home
    // may not.
    let (code, _, err) = kv(&["put", &jwks_url, "-"], issued.token("dave"), &forged.jwks);
    assert_eq!(code, 1, "{err}");
    // A published set with a TTL admits while it lives and fails closed
    // the moment it lapses: a publisher that stops refreshing it takes
    // every JWT with it.
    let (code, _, err) = kv(
        &["put", &jwks_url, "-", "--ttl", "1"],
        "jwks-publisher",
        &issued.jwks,
    );
    assert_eq!(code, 0, "{err}");
    assert_eq!(server.get(names, Some(issued.token("carol"))).0, 200);
    std::thread::sleep(Duration::from_millis(1100));
    let (status, body) = server.get(names, Some(issued.token("carol")));
    assert_eq!(status, 401, "{body}");
    assert!(body.contains("no signing keys are published"), "{body}");
}

/// A JWKS written into the topics file, and the claims the server checks
/// beyond the signature: the issuer, the audience, the expiry, and a
/// groups claim, missing or holding nothing the topic maps.
#[test]
fn a_jwks_in_the_topics_file_and_the_claims_it_checks() {
    let Some(slapd) = Slapd::start("claims") else {
        return;
    };
    let issued = issue(
        &slapd,
        7,
        r#"{
          {label = "bob", user = "bob", password = "bob-pw"},
          {label = "carol", user = "carol", password = "carol-pw"},
          {label = "erin", user = "erin", password = "erin-pw"},
          {label = "carol-other-issuer", user = "carol", password = "carol-pw", iss = "someone-else"},
          {label = "carol-other-audience", user = "carol", password = "carol-pw", aud = "another-server"},
          {label = "carol-short", user = "carol", password = "carol-pw", ttl = 1},
        }"#,
    );
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("topics.json"),
        format!(
            r#"{{"jwt": {{"issuer": "{ISSUER}", "audience": "{AUDIENCE}", "jwks": {}}},
                 "topics": {{"home": {{"groups": {HOME_GROUPS}}}}}}}"#,
            issued.jwks
        ),
    )
    .unwrap();
    let server = Match::start(dir.path(), &["--topics", "topics.json"], 2);
    let names = "/v1/home/names";

    assert_eq!(server.get(names, Some(issued.token("carol"))).0, 200);
    for (label, why) in [
        ("carol-other-issuer", "issuer"),
        ("carol-other-audience", "audience"),
    ] {
        let (status, body) = server.get(names, Some(issued.token(label)));
        assert_eq!(status, 401, "{label}: {body}");
        assert!(body.contains(why), "{label}: {body}");
    }
    // erin's groups map to nothing in home: a valid token with no role.
    assert_eq!(server.get(names, Some(issued.token("erin"))).0, 403);
    // A parked peer with bob's token, which holds call only, is refused,
    // and the server keeps serving.
    let (status, _) = server.get("/v1/home/box/calls", Some(issued.token("bob")));
    assert_eq!(status, 403);
    // A tampered token: the payload changed, the signature kept.
    let carol = issued.token("carol");
    let mut parts: Vec<&str> = carol.split('.').collect();
    let erin = issued.token("erin");
    parts[1] = erin.split('.').nth(1).unwrap();
    let (status, body) = server.get(names, Some(&parts.join(".")));
    assert_eq!(status, 401, "{body}");
    assert!(body.contains("signature"), "{body}");
    // The host owns the time: a one-second token is refused once it ends.
    std::thread::sleep(Duration::from_millis(2100));
    let (status, body) = server.get(names, Some(issued.token("carol-short")));
    assert_eq!(status, 401, "{body}");
    assert!(body.contains("expired"), "{body}");
}

/// The file is checked before the server starts: a kv JWKS needs --kv.
#[test]
fn a_jwks_kept_in_kv_needs_a_server_that_keeps_kv() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("topics.json"),
        r#"{"jwt": {"issuer": "i", "jwks": {"kv": "auth/jwks"}},
            "topics": {"home": {"groups": {"g": ["call"]}}}}"#,
    )
    .unwrap();
    let out = drt()
        .current_dir(dir.path())
        .args(["p2p", "--match", "0", "--topics", "topics.json"])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success());
    assert!(err.contains("add --kv"), "{err}");
}
