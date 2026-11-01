//! The auth stack end to end, through the real binaries, in the order a
//! person sets it up and uses it:
//!
//! 1. A real OpenLDAP `slapd` holds the users, their passwords and their
//!    groups.
//! 2. One `drt p2p --match --kv --topics` server keeps the sessions and
//!    the published keys in topic `auth`, and admits JWTs in topic `home`,
//!    where directory groups decide the roles.
//! 3. `drt start` runs `stdlib:session` (LDAP bind, TOTP enrolled on first
//!    sign-in, a session in kv, `/verify`, `/api/token`) and, beside it,
//!    `stdlib:jwks`, which publishes the session key's public half to the
//!    same topic. Both hold the same crypto master, so they derive the same
//!    key; neither ever sees the other's requests.
//! 4. A browser (a cookie jar here) signs in with a password and a TOTP
//!    code computed here, not by the host, and asks for a JWT.
//! 5. That JWT parks and calls in `home`, and reads and writes its kv, each
//!    allowed or refused by the user's groups, with `drt p2p` and
//!    `drt kv` as a person would type them.
//! 6. PyJWT, sharing no code with DRT, verifies the same token from the
//!    JWKS URL alone.
//!
//! No step is stubbed, and nothing is handed between steps except what a
//! person would carry: the password, the TOTP secret on the enrolment
//! page, and the token `/api/token` answers.
//!
//! Without `slapd` (Debian/Ubuntu: `apt install slapd`) the test says
//! `skipped` and passes, unless `DRT_TEST_REQUIRE_LDAP=1` is set. Without
//! PyJWT (`pip install 'pyjwt[crypto]'`) the last step says `skipped`,
//! unless `DRT_TEST_REQUIRE_PYJWT=1` is set. CI sets both.
//!
//! ## surface block
//!
//! - Entry points: the test; [`Stack::start`], every process it needs.
//! - Configurable values: [`LDIF`], the directory; [`MASTER`], the crypto
//!   master both programs hold; [`AUTH_TOKEN`], the `auth` topic token
//!   their kv scopes hold; [`ISSUER`], [`AUDIENCE`] and [`JWT_KEY`], what
//!   the session signs and the match server checks; [`HOME_GROUPS`], who
//!   may do what in `home`.
//! - Fan-out: none.

#![cfg(all(
    unix,
    feature = "connector-ldap",
    feature = "connector-kv",
    feature = "connector-rest",
    feature = "connector-crypto",
    feature = "p2p"
))]

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const MASTER: &str = "auth-stack-test-master-key-0123456789";
const AUTH_TOKEN: &str = "auth-topic-token";
const ISSUER: &str = "Example";
const AUDIENCE: &str = "home-match";
const JWT_KEY: &str = "session:1";
const BASE: &str = "dc=example,dc=com";
const WAIT: Duration = Duration::from_secs(20);

/// `admins` park, call, read and write in `home`; `staff` call and read.
/// `auth` maps no group, so no signed-in user reaches the sessions.
const HOME_GROUPS: &str =
    r#"{"admins": ["park", "call", "read", "write"], "staff": ["call", "read"]}"#;

/// alice is in admins and staff, bob in staff, carol in no group.
/// Passwords in the clear, which slapd accepts for a simple bind.
const LDIF: &str = "\
dn: dc=example,dc=com
objectClass: dcObject
objectClass: organization
dc: example
o: Example

dn: ou=people,dc=example,dc=com
objectClass: organizationalUnit
ou: people

dn: ou=groups,dc=example,dc=com
objectClass: organizationalUnit
ou: groups

dn: uid=alice,ou=people,dc=example,dc=com
objectClass: inetOrgPerson
uid: alice
cn: Alice Liddell
sn: Liddell
mail: alice@example.com
userPassword: alice-pw

dn: uid=bob,ou=people,dc=example,dc=com
objectClass: inetOrgPerson
uid: bob
cn: Bob
sn: Bob
userPassword: bob-pw

dn: uid=carol,ou=people,dc=example,dc=com
objectClass: inetOrgPerson
uid: carol
cn: Carol
sn: Carol
userPassword: carol-pw

dn: cn=admins,ou=groups,dc=example,dc=com
objectClass: groupOfNames
cn: admins
member: uid=alice,ou=people,dc=example,dc=com

dn: cn=staff,ou=groups,dc=example,dc=com
objectClass: groupOfNames
cn: staff
member: uid=alice,ou=people,dc=example,dc=com
member: uid=bob,ou=people,dc=example,dc=com
";

/// The listener allowlists doc/Session.md gives.
const REQ_HEADERS: &[&str] = &[
    "cookie",
    "content-type",
    "user-agent",
    "x-forwarded-for",
    "x-forwarded-proto",
    "x-forwarded-host",
    "x-forwarded-uri",
    "x-forwarded-method",
];
const RESP_HEADERS: &[&str] = &[
    "set-cookie",
    "location",
    "cache-control",
    "remote-user",
    "remote-groups",
    "remote-name",
    "remote-email",
];

macro_rules! skipped {
    ($var:literal, $($arg:tt)*) => {{
        if std::env::var_os($var).is_some() {
            panic!("{} is set, and this test would have skipped: {}", $var, format!($($arg)*));
        }
        eprintln!("skipped: {}", format!($($arg)*));
    }};
}

// depth: processes

/// A child process, killed on drop, whose output is kept for a failure's
/// message.
struct Proc {
    child: Child,
    log: Arc<Mutex<String>>,
}

impl Drop for Proc {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Proc {
    /// Start `cmd` and wait for a line of its output holding `ready`;
    /// answer that line.
    fn start(mut cmd: Command, ready: &str) -> (Proc, String) {
        let mut child = cmd
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn");
        let log = Arc::new(Mutex::new(String::new()));
        let (tx, rx) = std::sync::mpsc::channel::<String>();
        for out in [
            Box::new(child.stdout.take().unwrap()) as Box<dyn Read + Send>,
            Box::new(child.stderr.take().unwrap()),
        ] {
            let log = log.clone();
            let tx = tx.clone();
            std::thread::spawn(move || {
                for line in BufReader::new(out).lines().map_while(Result::ok) {
                    log.lock().unwrap().push_str(&format!("{line}\n"));
                    let _ = tx.send(line);
                }
            });
        }
        let deadline = Instant::now() + WAIT;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match rx.recv_timeout(left) {
                Ok(line) if line.contains(ready) => return (Proc { child, log }, line),
                Ok(_) => {}
                Err(_) => panic!(
                    "no line holding {ready:?}; output:\n{}",
                    log.lock().unwrap()
                ),
            }
        }
    }

    fn log(&self) -> String {
        self.log.lock().unwrap().clone()
    }

    /// Wait until the output so far holds `text`.
    fn wait_for(&self, text: &str) {
        let deadline = Instant::now() + WAIT;
        while !self.log().contains(text) {
            assert!(
                Instant::now() < deadline,
                "never said {text:?}:\n{}",
                self.log()
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

/// The port in `... listening on 127.0.0.1:<port>`.
fn port_of(line: &str) -> u16 {
    line.rsplit(':').next().unwrap().trim().parse().unwrap()
}

fn drt() -> Command {
    Command::new(env!("CARGO_BIN_EXE_drt"))
}

/// A stock slapd, run from a copy of its binary: Ubuntu's AppArmor profile
/// attaches to /usr/sbin/slapd and would not let it read this directory.
fn slapd(dir: &Path) -> Option<(Proc, u16)> {
    let bin = Path::new("/usr/sbin/slapd");
    let schema = Path::new("/etc/ldap/schema");
    if !bin.exists() || !schema.join("inetorgperson.schema").exists() {
        skipped!(
            "DRT_TEST_REQUIRE_LDAP",
            "no slapd at /usr/sbin/slapd with its schema in /etc/ldap/schema"
        );
        return None;
    }
    std::fs::create_dir_all(dir.join("db")).unwrap();
    let module = if Path::new("/usr/lib/ldap/back_mdb.so").exists() {
        "modulepath /usr/lib/ldap\nmoduleload back_mdb\n"
    } else {
        ""
    };
    let (d, s) = (dir.display(), schema.display());
    std::fs::write(
        dir.join("slapd.conf"),
        format!(
            "include {s}/core.schema\ninclude {s}/cosine.schema\ninclude {s}/inetorgperson.schema\n\
             pidfile {d}/slapd.pid\n{module}database mdb\nmaxsize 10485760\nsuffix \"{BASE}\"\n\
             rootdn \"cn=admin,{BASE}\"\nrootpw admin-pw\ndirectory {d}/db\n\
             access to attrs=userPassword by self read by anonymous auth by * none\n\
             access to * by * read\n"
        ),
    )
    .unwrap();
    std::fs::write(dir.join("data.ldif"), LDIF).unwrap();
    let added = Command::new(bin.with_file_name("slapadd"))
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
    // slapd takes no port 0, so this one is picked and released.
    let port = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let copy = dir.join("slapd");
    std::fs::copy(bin, &copy).unwrap();
    let log = dir.join("slapd.log");
    let child = spawn_written(
        Command::new(&copy)
            .arg("-f")
            .arg(dir.join("slapd.conf"))
            .arg("-h")
            .arg(format!("ldap://127.0.0.1:{port}/"))
            .args(["-d", "0"])
            .stdout(Stdio::null())
            .stderr(std::fs::File::create(&log).unwrap()),
    );
    let mut proc = Proc {
        child,
        log: Arc::default(),
    };
    let deadline = Instant::now() + Duration::from_secs(10);
    while TcpStream::connect(("127.0.0.1", port)).is_err() {
        let exited = proc.child.try_wait().unwrap();
        if exited.is_some() || Instant::now() > deadline {
            let said = std::fs::read_to_string(&log).unwrap_or_default();
            panic!("slapd did not listen on {port} ({exited:?}):\n{said}");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    Some((proc, port))
}

/// Spawns a program this process has just written, waiting out ETXTBSY
/// from another test's fork still holding the copy open.
fn spawn_written(cmd: &mut Command) -> Child {
    const ETXTBSY: i32 = 26;
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match cmd.spawn() {
            Err(e) if e.raw_os_error() == Some(ETXTBSY) && Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(20))
            }
            spawned => return spawned.unwrap(),
        }
    }
}

// depth: HTTP, and a browser's cookie jar

#[derive(Debug)]
struct Resp {
    status: u16,
    headers: Vec<(String, String)>,
    body: String,
}

impl Resp {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }
}

fn request(port: u16, method: &str, target: &str, headers: &[(&str, &str)], body: &str) -> Resp {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    stream.set_read_timeout(Some(WAIT)).unwrap();
    let mut req = format!("{method} {target} HTTP/1.1\r\nhost: 127.0.0.1:{port}\r\n");
    for (k, v) in headers {
        req.push_str(&format!("{k}: {v}\r\n"));
    }
    req.push_str(&format!(
        "content-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    ));
    stream.write_all(req.as_bytes()).unwrap();
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).unwrap();
    let text = String::from_utf8_lossy(&raw).to_string();
    let (head, body) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
    let mut lines = head.lines();
    let status = lines
        .next()
        .unwrap()
        .split(' ')
        .nth(1)
        .unwrap()
        .parse()
        .unwrap();
    let headers = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string()))
        .collect();
    Resp {
        status,
        headers,
        body: body.to_string(),
    }
}

/// A request with `token` as a bearer, or none.
fn bearer(port: u16, method: &str, path: &str, token: Option<&str>, body: &str) -> Resp {
    let auth = token.map(|t| format!("Bearer {t}"));
    let mut headers = vec![("content-type", "text/plain;charset=utf-8")];
    if let Some(a) = &auth {
        headers.push(("authorization", a));
    }
    request(port, method, path, &headers, body)
}

/// A browser, as far as cookies go.
struct Browser {
    port: u16,
    cookie: Option<String>,
}

impl Browser {
    fn send(&mut self, method: &str, target: &str, body: &str) -> Resp {
        let cookie = self.cookie.as_ref().map(|c| format!("drt_session={c}"));
        let mut headers: Vec<(&str, &str)> = Vec::new();
        if let Some(c) = &cookie {
            headers.push(("cookie", c));
        }
        if method == "POST" {
            headers.push(("content-type", "application/x-www-form-urlencoded"));
        }
        let resp = request(self.port, method, target, &headers, body);
        if let Some(set) = resp.header("set-cookie") {
            let value = set
                .strip_prefix("drt_session=")
                .unwrap()
                .split(';')
                .next()
                .unwrap();
            self.cookie = (!value.is_empty()).then(|| value.to_string());
        }
        resp
    }
}

// depth: TOTP, computed here (RFC 6238 over SHA-1), not by the host

fn base32(text: &str) -> Vec<u8> {
    let alphabet = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";
    let (mut bits, mut acc, mut out) = (0u32, 0u64, Vec::new());
    for c in text.bytes() {
        let v = alphabet.iter().position(|&a| a == c).unwrap() as u64;
        acc = (acc << 5) | v;
        bits += 5;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    out
}

fn totp(secret: &str) -> String {
    use hmac::{Hmac, Mac};
    let step = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
        / 30;
    let mut mac = Hmac::<sha1::Sha1>::new_from_slice(&base32(secret)).unwrap();
    mac.update(&step.to_be_bytes());
    let h = mac.finalize().into_bytes();
    let o = (h[19] & 15) as usize;
    let n = u32::from_be_bytes([h[o] & 0x7f, h[o + 1], h[o + 2], h[o + 3]]) % 1_000_000;
    format!("{n:06}")
}

/// The secret on an enrolment page.
fn enrol_secret(page: &str) -> String {
    let at = page
        .find(r#"id="totp-secret">"#)
        .unwrap_or_else(|| panic!("an enrolment page:\n{page}"))
        + 17;
    page[at..].split('<').next().unwrap().to_string()
}

// depth: the deployment

struct Stack {
    dir: PathBuf,
    _slapd: Proc,
    matcher: Proc,
    match_port: u16,
    jwks: Proc,
    jwks_port: u16,
    session: Proc,
    session_port: u16,
}

impl Drop for Stack {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

impl Stack {
    fn start() -> Option<Stack> {
        let dir = std::env::temp_dir().join(format!("drt-auth-stack-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let (slapd, ldap_port) = slapd(&dir.join("ldap"))?;

        // The server: sessions and keys in `auth`, JWTs admitted in `home`
        // against the set `stdlib:jwks` keeps at `auth/jwks`.
        std::fs::write(
            dir.join("topics.json"),
            format!(
                r#"{{
                  "jwt": {{"issuer": "{ISSUER}", "audience": "{AUDIENCE}", "jwks": {{"kv": "auth/jwks"}}}},
                  "topics": {{
                    "auth": {{"tokens": [{{"token": "{AUTH_TOKEN}", "roles": ["read", "write"]}}]}},
                    "home": {{"groups": {HOME_GROUPS}}}
                  }}
                }}"#
            ),
        )
        .unwrap();
        let mut m = drt();
        m.current_dir(&dir).args([
            "p2p",
            "--match",
            "0",
            "--kv",
            "kv",
            "--topics",
            "topics.json",
        ]);
        let (matcher, line) = Proc::start(m, "listening on");
        let match_port = port_of(&line);
        let kv_scope = serde_json::json!({
            "url": format!("http://127.0.0.1:{match_port}/v1/auth"),
            "token": AUTH_TOKEN
        });

        // The JWKS publisher, holding the session's master.
        let jwks = serde_json::json!({
            "entry": "stdlib:jwks",
            "args": {"keys": JWT_KEY, "ttl": 3600, "max_age": 60},
            "caps": [
                {"capability": "host:crypto/derive", "scope": ["session:*"]},
                {"capability": "host:crypto/public_key"},
                {"capability": "host:kv/put"},
                {"capability": "host:kv/list"},
                {"capability": "host:time/monotonic"}
            ],
            "connectors": {
                "time": {},
                "crypto": {"scope": {"key": MASTER}},
                "kv": {"scope": kv_scope}
            },
            "listeners": [{
                "scheme": "http", "address": "127.0.0.1:0",
                "queue": "http_in", "reply_queue": "http_out",
                "resp_headers": ["cache-control", "access-control-allow-origin"]
            }]
        });
        let path = dir.join("jwks.json");
        std::fs::write(&path, jwks.to_string()).unwrap();
        let mut cmd = drt();
        cmd.arg("start").arg("--config").arg(&path);
        let (jwks, line) = Proc::start(cmd, "listening on");
        let jwks_port = port_of(&line);

        // The session framework: password in LDAP, TOTP enrolled on first
        // sign-in, tokens for the match server's `home`.
        let session = serde_json::json!({
            "entry": "stdlib:session",
            "args": {
                "issuer": ISSUER,
                "cookie_secure": false,
                "second_factor": "totp",
                "totp_enroll": "first_login",
                "jwt_key": JWT_KEY,
                "jwt_audience": AUDIENCE,
                "jwt_ttl": 600
            },
            "caps": [
                {"capability": "host:ldap/bind"},
                {"capability": "host:kv/*"},
                {"capability": "host:time"},
                {"capability": "host:crypto/random"},
                {"capability": "host:crypto/hash"},
                {"capability": "host:crypto/derive", "scope": ["totp:*", "session:*"]},
                {"capability": "host:crypto/totp_verify"},
                {"capability": "host:crypto/totp_enroll"},
                {"capability": "host:crypto/jwt_sign"}
            ],
            "connectors": {
                "time": {},
                "crypto": {"scope": {"key": MASTER}},
                "ldap": {"scope": {
                    "url": format!("ldap://127.0.0.1:{ldap_port}"),
                    "starttls": false,
                    "base_dn": BASE,
                    "group_base": format!("ou=groups,{BASE}")
                }},
                "kv": {"scope": kv_scope}
            },
            "listeners": [{
                "scheme": "http",
                "address": "127.0.0.1:0",
                "headers": REQ_HEADERS,
                "resp_headers": RESP_HEADERS
            }]
        });
        let path = dir.join("session.json");
        std::fs::write(&path, session.to_string()).unwrap();
        let mut cmd = drt();
        cmd.arg("start").arg("--config").arg(&path);
        let (session, line) = Proc::start(cmd, "listening on");
        let session_port = port_of(&line);
        session.wait_for("session: serving");

        Some(Stack {
            dir,
            _slapd: slapd,
            matcher,
            match_port,
            jwks,
            jwks_port,
            session,
            session_port,
        })
    }

    fn logs(&self) -> String {
        format!(
            "--- match\n{}--- jwks\n{}--- session\n{}",
            self.matcher.log(),
            self.jwks.log(),
            self.session.log()
        )
    }

    /// Password, the enrolment page's secret, a code from it, and the
    /// token: what a person does the first time. Answers the browser and
    /// the token.
    fn sign_in(&self, user: &str, password: &str) -> (Browser, String) {
        let mut b = Browser {
            port: self.session_port,
            cookie: None,
        };
        let r = b.send(
            "POST",
            "/login",
            &format!("username={user}&password={password}"),
        );
        assert_eq!(r.status, 303, "{user}: {}\n{}", r.body, self.logs());
        assert_eq!(r.header("location"), Some("/2fa"));
        let page = b.send("GET", "/2fa", "");
        let secret = enrol_secret(&page.body);
        let code = totp(&secret);
        let r = b.send("POST", "/2fa/enroll", &format!("code={code}"));
        assert_eq!(r.status, 303, "{user}: {}\n{}", r.body, self.logs());
        // The same code a second time opens nothing.
        let r = request(
            self.session_port,
            "POST",
            "/login",
            &[("content-type", "application/x-www-form-urlencoded")],
            &format!("username={user}&password={password}"),
        );
        let pending = r
            .header("set-cookie")
            .unwrap()
            .strip_prefix("drt_session=")
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_string();
        let mut again = Browser {
            port: self.session_port,
            cookie: Some(pending),
        };
        let r = again.send("POST", "/2fa/totp", &format!("code={code}"));
        assert_eq!(
            r.status, 401,
            "{user}'s code was accepted twice: {}",
            r.body
        );

        let r = b.send("POST", "/api/token", "");
        assert_eq!(r.status, 200, "{}\n{}", r.body, self.logs());
        let v: serde_json::Value = serde_json::from_str(&r.body).unwrap();
        (b, v["token"].as_str().unwrap().to_string())
    }

    fn match_url(&self, path: &str) -> String {
        format!("drt://127.0.0.1:{}{path}", self.match_port)
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

/// `drt p2p <peer> --H auth=<token>`, `input` on stdin: (stdout, stderr).
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

/// `drt kv <args> --H auth=<token>`: (status, stdout, stderr).
fn kv(args: &[&str], token: &str) -> (i32, String, String) {
    let out = drt()
        .arg("kv")
        .args(args)
        .arg("--H")
        .arg(format!("auth={token}"))
        .stdin(Stdio::null())
        .output()
        .unwrap();
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// PyJWT's relying party: the signing key from the JWKS URL by the token's
/// `kid`, then the signature, `iss`, `aud` and `exp`. Prints `kid sub
/// groups`, or `refused <reason>`.
const PYJWT: &str = r#"
import sys, jwt
url, token = sys.argv[1], sys.argv[2]
try:
    key = jwt.PyJWKClient(url).get_signing_key_from_jwt(token)
    claims = jwt.decode(token, key.key, algorithms=["EdDSA"], audience=sys.argv[3], issuer=sys.argv[4])
    print(jwt.get_unverified_header(token)["kid"], claims["sub"], ",".join(sorted(claims["groups"])))
except jwt.PyJWTError as e:
    print("refused", type(e).__name__)
"#;

/// PyJWT on `token`, or None when it is not installed.
fn pyjwt(jwks_url: &str, token: &str) -> Option<String> {
    let have = Command::new("python3")
        .args(["-I", "-c", "import jwt, cryptography"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if !have {
        skipped!(
            "DRT_TEST_REQUIRE_PYJWT",
            "PyJWT with its crypto extra is not installed (pip install 'pyjwt[crypto]')"
        );
        return None;
    }
    let out = Command::new("python3")
        .args(["-I", "-c", PYJWT, jwks_url, token, AUDIENCE, ISSUER])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// A token with one byte of its signature changed.
fn tampered(token: &str) -> String {
    let mut t = token.to_string();
    let last = t.pop().unwrap();
    t.push(if last == 'A' { 'B' } else { 'A' });
    t
}

// depth: the test

#[test]
fn a_person_signs_in_and_the_session_token_opens_topics_kv_and_a_third_party() {
    let Some(stack) = Stack::start() else {
        return;
    };
    let names = "/v1/home/names";

    // 1. Sign in: alice and bob each enrol TOTP on their first sign-in and
    //    get a token. carol has the password but no group.
    let (mut alice_browser, alice) = stack.sign_in("alice", "alice-pw");
    let (mut bob_browser, bob) = stack.sign_in("bob", "bob-pw");
    let (_, carol) = stack.sign_in("carol", "carol-pw");

    // The reverse proxy's view of the same sessions.
    let r = alice_browser.send("GET", "/verify?group=admins", "");
    assert_eq!(r.status, 200, "{}", r.body);
    assert_eq!(r.header("remote-user"), Some("alice"));
    let groups: Vec<&str> = r.header("remote-groups").unwrap().split(',').collect();
    assert!(
        groups.contains(&"admins") && groups.contains(&"staff"),
        "{groups:?}"
    );
    assert_eq!(
        bob_browser.send("GET", "/verify?group=admins", "").status,
        403
    );
    assert_eq!(
        bob_browser.send("GET", "/verify?group=staff", "").status,
        200
    );

    // 2. The match server checks the token against the set stdlib:jwks
    //    publishes; it may take a moment to land.
    let deadline = Instant::now() + WAIT;
    loop {
        let r = bearer(stack.match_port, "GET", names, Some(&alice), "");
        if r.status == 200 {
            assert_eq!(r.body, r#"{"topic":"home","names":[]}"#);
            break;
        }
        assert!(
            r.status == 401 && r.body.contains("no signing keys are published"),
            "before the keys landed: {} {}\n{}",
            r.status,
            r.body,
            stack.logs()
        );
        assert!(
            Instant::now() < deadline,
            "the JWKS never landed\n{}",
            stack.logs()
        );
        std::thread::sleep(Duration::from_millis(200));
    }
    // The set is in kv where the operator can see it, under the session's
    // key name.
    let (code, out, err) = kv(&["ls", &stack.match_url("/v1/auth/kv/jwks")], AUTH_TOKEN);
    assert_eq!(code, 0, "{err}");
    assert!(out.contains("jwks/session~3A1"), "{out}");

    // 3. Park and call in `home`: alice (admins) parks; bob (staff) and
    //    carol (no group) may not. bob calls through a real WebRTC session;
    //    carol, no token, and a tampered token may not.
    let box_url = stack.match_url("/v1/home/box");
    let (_parked, _) = {
        let mut cmd = drt();
        cmd.arg("p2p")
            .arg("--park")
            .arg(&box_url)
            .arg("--H")
            .arg(format!("auth={alice}"))
            .arg("--forward")
            .arg(format!("127.0.0.1:{}", echo()));
        Proc::start(cmd, "present at ")
    };
    for (who, token) in [("bob", &bob), ("carol", &carol)] {
        let r = bearer(
            stack.match_port,
            "GET",
            "/v1/home/desk/calls",
            Some(token),
            "",
        );
        assert_eq!(r.status, 403, "{who} parked: {}", r.body);
    }
    let (out, errs) = call(&box_url, Some(&bob), b"hello from bob\n");
    assert_eq!(out, "hello from bob\n", "{errs}\n{}", stack.logs());
    let (out, errs) = call(&box_url, Some(&carol), b"x\n");
    assert!(
        out.is_empty() && errs.contains("403"),
        "carol called: {errs}"
    );
    let (_, errs) = call(&box_url, None, b"x\n");
    assert!(errs.contains("401"), "{errs}");
    let (_, errs) = call(&box_url, Some(&tampered(&bob)), b"x\n");
    assert!(errs.contains("401"), "a tampered token called: {errs}");
    assert_eq!(
        bearer(stack.match_port, "GET", names, Some(&bob), "").body,
        r#"{"topic":"home","names":["box"]}"#
    );

    // 4. kv in `home`: alice writes, bob reads and may not write, carol
    //    may do neither. No signed-in user reaches `auth`, where the
    //    sessions are.
    let note = stack.match_url("/v1/home/kv/note");
    let (code, _, err) = kv(&["put", &note, "the box is in the hall"], &alice);
    assert_eq!(code, 0, "{err}");
    let (code, out, err) = kv(&["get", &note], &bob);
    assert_eq!((code, out.as_str()), (0, "the box is in the hall"), "{err}");
    let (code, _, err) = kv(&["put", &note, "bob was here"], &bob);
    assert!(code == 1 && err.contains("403"), "bob wrote: {code} {err}");
    let (code, _, err) = kv(&["get", &note], &carol);
    assert!(code == 1 && err.contains("403"), "carol read: {code} {err}");
    for token in [&alice, &bob] {
        let r = bearer(
            stack.match_port,
            "GET",
            "/v1/auth/kv/?prefix=session/",
            Some(token),
            "",
        );
        assert_eq!(r.status, 403, "a user listed the sessions: {}", r.body);
    }

    // 5. A third party holding only the JWKS URL: PyJWT finds the key by
    //    kid and checks the signature, issuer and audience.
    let jwks_url = format!("http://127.0.0.1:{}/.well-known/jwks.json", stack.jwks_port);
    if let Some(said) = pyjwt(&jwks_url, &alice) {
        assert_eq!(said, "session:1 alice admins,staff");
        assert_eq!(pyjwt(&jwks_url, &bob).unwrap(), "session:1 bob staff");
        assert_eq!(
            pyjwt(&jwks_url, &tampered(&alice)).unwrap(),
            "refused InvalidSignatureError"
        );
    }

    // 6. Signing out ends the session for the proxy at once. A token
    //    already issued lives out its own `exp`, as any bearer JWT does:
    //    `jwt_ttl` is how long that is.
    assert_eq!(alice_browser.send("POST", "/logout", "").status, 303);
    assert_eq!(alice_browser.send("GET", "/verify", "").status, 401);
    assert_eq!(
        bearer(stack.match_port, "GET", names, Some(&alice), "").status,
        200
    );
}
