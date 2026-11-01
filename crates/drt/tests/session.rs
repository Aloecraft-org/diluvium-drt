//! `stdlib:session` end to end: a real OpenLDAP `slapd`, a real
//! `drt p2p --match --kv --topics` holding the sessions, a webhook served
//! here, and `drt start` running the program, all spoken to over HTTP the
//! way a browser and a reverse proxy would.
//!
//! Without `slapd` (Debian/Ubuntu: `apt install slapd`) every test says
//! `skipped` and passes, unless `DRT_TEST_REQUIRE_LDAP=1` is set, as CI
//! sets it: there a skip is a broken runner.
//!
//! ## surface block
//!
//! - Entry points: the tests; [`World::start`], every server one test needs.
//! - Configurable values: [`LDIF`], the directory; [`MASTER`], the crypto
//!   master; [`KV_TOKEN`], the topic token the program's kv scope holds.
//! - Fan-out: [`Decision`], what the webhook answers.

#![cfg(all(
    unix,
    feature = "connector-ldap",
    feature = "connector-kv",
    feature = "connector-rest"
))]

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const MASTER: &str = "session-test-master-key-0123456789";
const KV_TOKEN: &str = "auth-kv-token";
const BASE: &str = "dc=example,dc=com";

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
    "x-original-url",
    "x-original-method",
];
const RESP_HEADERS: &[&str] = &[
    "set-cookie",
    "location",
    "cache-control",
    "remote-user",
    "remote-groups",
    "remote-name",
    "remote-email",
    "x-frame-options",
    "content-security-policy",
    "referrer-policy",
];

macro_rules! skipped {
    ($($arg:tt)*) => {{
        if std::env::var_os("DRT_TEST_REQUIRE_LDAP").is_some() {
            panic!("DRT_TEST_REQUIRE_LDAP is set, and this test would have skipped: {}", format!($($arg)*));
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
        let deadline = Instant::now() + Duration::from_secs(20);
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
}

/// The port in `... listening on 127.0.0.1:<port>`.
fn port_of(line: &str) -> u16 {
    line.rsplit(':').next().unwrap().trim().parse().unwrap()
}

fn drt() -> Command {
    Command::new(env!("CARGO_BIN_EXE_drt"))
}

fn slapd(dir: &Path) -> Option<(Proc, u16)> {
    let bin = Path::new("/usr/sbin/slapd");
    let schema = Path::new("/etc/ldap/schema");
    if !bin.exists() || !schema.join("inetorgperson.schema").exists() {
        skipped!("no slapd at /usr/sbin/slapd with its schema in /etc/ldap/schema");
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
    // slapd takes no port 0, so this one is picked and released; the
    // window is the connector suite's own.
    let port = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    // Run a copy. Ubuntu's AppArmor profile for slapd attaches to the path
    // /usr/sbin/slapd and lets it read only its own directories, so the
    // real binary cannot open this test's config or database (slapadd, the
    // same file under another path, is not confined). A copy at another
    // path is the same program without the profile.
    let copy = dir.join("slapd");
    std::fs::copy(bin, &copy).unwrap();
    let log = dir.join("slapd.log");
    let child = spawn_written(
        Command::new(&copy)
            .arg("-f")
            .arg(dir.join("slapd.conf"))
            .arg("-h")
            .arg(format!("ldap://127.0.0.1:{port}/"))
            // -d keeps it in the foreground, so the child is the server; at 0
            // it still says why it refused to start.
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

/// Spawns a program this process has just written. Another test's fork can
/// still hold the copy's write descriptor for the moment before its exec
/// closes it, and exec of a file open for writing fails with ETXTBSY, so
/// that one error is waited out.
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

// depth: the webhook

/// What the webhook answers to each event, as `{"decision": ...}`, or a
/// status that is not 2xx.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Decision {
    Approve,
    Deny,
    Pending,
    Status(u16),
}

/// How the webhook decides, swapped by [`Hook::answer`] mid-test.
type Decide = Box<dyn FnMut(&serde_json::Value) -> Decision + Send>;

/// A webhook on loopback: answers by `decide(event, body)`, keeps every
/// body it was sent.
struct Hook {
    port: u16,
    seen: Arc<Mutex<Vec<serde_json::Value>>>,
    decide: Arc<Mutex<Decide>>,
}

impl Hook {
    fn start() -> Hook {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let seen: Arc<Mutex<Vec<serde_json::Value>>> = Arc::default();
        let decide: Arc<Mutex<Decide>> = Arc::new(Mutex::new(Box::new(|_| Decision::Deny)));
        let (s, d) = (seen.clone(), decide.clone());
        std::thread::spawn(move || {
            for stream in listener.incoming().map_while(Result::ok) {
                let (_, _, body) = read_request(&stream);
                let v: serde_json::Value = serde_json::from_slice(&body).unwrap_or_default();
                let decision = (d.lock().unwrap())(&v);
                s.lock().unwrap().push(v);
                let (code, text) = match decision {
                    Decision::Approve => (200, r#"{"decision":"approve"}"#.to_string()),
                    Decision::Deny => (200, r#"{"decision":"deny"}"#.to_string()),
                    Decision::Pending => (200, r#"{"decision":"pending"}"#.to_string()),
                    Decision::Status(c) => (c, "{}".to_string()),
                };
                let mut stream = stream;
                let _ = write!(
                    stream,
                    "HTTP/1.1 {code} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{text}",
                    text.len()
                );
            }
        });
        Hook { port, seen, decide }
    }

    fn answer(&self, f: impl FnMut(&serde_json::Value) -> Decision + Send + 'static) {
        *self.decide.lock().unwrap() = Box::new(f);
    }

    fn seen(&self) -> Vec<serde_json::Value> {
        self.seen.lock().unwrap().clone()
    }
}

/// One HTTP/1.1 request off `stream`: its request line, headers, body.
fn read_request(stream: &TcpStream) -> (String, BTreeMap<String, String>, Vec<u8>) {
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    let mut headers = BTreeMap::new();
    loop {
        let mut h = String::new();
        reader.read_line(&mut h).unwrap();
        let h = h.trim_end();
        if h.is_empty() {
            break;
        }
        if let Some((k, v)) = h.split_once(':') {
            headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
        }
    }
    let n: usize = headers
        .get("content-length")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let mut body = vec![0; n];
    reader.read_exact(&mut body).unwrap();
    (line, headers, body)
}

// depth: an HTTP client with a cookie jar

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
    stream
        .set_read_timeout(Some(Duration::from_secs(30)))
        .unwrap();
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

/// A browser, as far as cookies go: it keeps the session cookie the
/// program sets and sends it back.
struct Browser {
    port: u16,
    cookie: Option<String>,
}

impl Browser {
    fn new(port: u16) -> Browser {
        Browser { port, cookie: None }
    }

    fn send(&mut self, method: &str, target: &str, extra: &[(&str, &str)], body: &str) -> Resp {
        let cookie = self.cookie.as_ref().map(|c| format!("drt_session={c}"));
        let mut headers: Vec<(&str, &str)> = extra.to_vec();
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

    fn get(&mut self, target: &str) -> Resp {
        self.send("GET", target, &[], "")
    }

    fn post(&mut self, target: &str, body: &str) -> Resp {
        self.send("POST", target, &[], body)
    }

    /// Password, then the pending session's second-factor page.
    fn login(&mut self, user: &str, password: &str) -> Resp {
        self.post("/login", &format!("username={user}&password={password}"))
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

fn totp(secret: &str, offset: i64) -> String {
    use hmac::{Hmac, Mac};
    let step = (std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
        / 30)
        + offset;
    let mut mac = Hmac::<sha1::Sha1>::new_from_slice(&base32(secret)).unwrap();
    mac.update(&(step as u64).to_be_bytes());
    let h = mac.finalize().into_bytes();
    let o = (h[19] & 15) as usize;
    let n = u32::from_be_bytes([h[o] & 0x7f, h[o + 1], h[o + 2], h[o + 3]]) % 1_000_000;
    format!("{n:06}")
}

/// The secret on an enrolment page.
fn enrol_secret(page: &str) -> String {
    let at = page
        .find(r#"id="totp-secret">"#)
        .expect("an enrolment page")
        + 17;
    page[at..].split('<').next().unwrap().to_string()
}

// depth: one deployment

struct World {
    dir: PathBuf,
    _slapd: Proc,
    ldap_port: u16,
    _matcher: Proc,
    kv_port: u16,
    hook: Hook,
    session: Option<Proc>,
    port: u16,
}

impl Drop for World {
    fn drop(&mut self) {
        self.session = None;
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

impl World {
    /// slapd, the match server with kv and topics, the webhook, and
    /// `stdlib:session` with `args` over the defaults below.
    fn start(name: &str, args: serde_json::Value) -> Option<World> {
        let dir = std::env::temp_dir().join(format!("drt-session-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let (slapd, ldap_port) = slapd(&dir.join("ldap"))?;
        std::fs::write(
            dir.join("topics.json"),
            format!(
                r#"{{"topics":{{"auth":{{"tokens":[{{"token":"{KV_TOKEN}","roles":["read","write"]}}]}}}}}}"#
            ),
        )
        .unwrap();
        let mut m = drt();
        m.args(["p2p", "--match", "0", "--kv"])
            .arg(dir.join("kv"))
            .arg("--topics")
            .arg(dir.join("topics.json"));
        let (matcher, line) = Proc::start(m, "listening on");
        let kv_port = port_of(&line);
        let hook = Hook::start();
        let mut world = World {
            dir,
            _slapd: slapd,
            ldap_port,
            _matcher: matcher,
            kv_port,
            hook,
            session: None,
            port: 0,
        };
        world.restart(args);
        Some(world)
    }

    fn config(&self, args: serde_json::Value) -> PathBuf {
        let mut all = serde_json::json!({
            "issuer": "Example",
            "cookie_secure": false,
            "cookie_domain": "example.test",
            "second_factor": "totp,webhook",
            "totp_enroll": "first_login",
            "webhook_url": format!("http://127.0.0.1:{}/2fa", self.hook.port),
            "jwt_key": "session:1",
            "login_url": "https://auth.example.test/",
        });
        for (k, v) in args.as_object().unwrap() {
            all[k] = v.clone();
        }
        let config = serde_json::json!({
            "entry": "stdlib:session",
            "args": all,
            "caps": [
                {"capability": "host:ldap/bind"},
                {"capability": "host:kv/*"},
                {"capability": "host:rest/post"},
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
                    "url": format!("ldap://127.0.0.1:{}", self.ldap_port),
                    "starttls": false,
                    "base_dn": BASE,
                    "group_base": format!("ou=groups,{BASE}")
                }},
                "kv": {"scope": {
                    "url": format!("http://127.0.0.1:{}/v1/auth", self.kv_port),
                    "token": KV_TOKEN
                }},
                "rest": {"scope": {
                    "allow": [format!("http://127.0.0.1:{}", self.hook.port)],
                    "allow_private": true
                }}
            },
            "listeners": [{
                "scheme": "http",
                "address": "127.0.0.1:0",
                "headers": REQ_HEADERS,
                "resp_headers": RESP_HEADERS
            }]
        });
        let path = self.dir.join("session.json");
        std::fs::write(&path, serde_json::to_string_pretty(&config).unwrap()).unwrap();
        path
    }

    /// The program again, with other arguments, on the same kv.
    fn restart(&mut self, args: serde_json::Value) {
        self.session = None;
        let mut cmd = drt();
        cmd.arg("start").arg("--config").arg(self.config(args));
        let (session, line) = Proc::start(cmd, "listening on");
        self.port = port_of(&line);
        // The program reads its arguments after the listener binds.
        let deadline = Instant::now() + Duration::from_secs(10);
        while !session.log().contains("session: serving") {
            assert!(
                Instant::now() < deadline,
                "the program did not start:\n{}",
                session.log()
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        self.session = Some(session);
    }

    fn browser(&self) -> Browser {
        Browser::new(self.port)
    }

    fn log(&self) -> String {
        self.session.as_ref().unwrap().log()
    }

    /// The kv topic's keys, as the operator's `drt kv ls` lists them.
    fn kv_keys(&self) -> String {
        let out = drt()
            .args(["kv", "ls"])
            .arg(format!("drt://127.0.0.1:{}/v1/auth/kv/", self.kv_port))
            .args(["--H", &format!("auth={KV_TOKEN}")])
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).to_string() + &String::from_utf8_lossy(&out.stderr)
    }

    /// Alice, enrolled and signed in through TOTP: the browser holds a
    /// signed-in cookie. Answers the enrolment secret.
    fn enrolled_alice(&self) -> (Browser, String) {
        let mut b = self.browser();
        assert_eq!(b.login("alice", "alice-pw").status, 303, "{}", self.log());
        let page = b.get("/2fa");
        let secret = enrol_secret(&page.body);
        let r = b.post("/2fa/enroll", &format!("code={}", totp(&secret, 0)));
        assert_eq!(r.status, 303, "{}\n{}", r.body, self.log());
        (b, secret)
    }
}

fn verify(b: &mut Browser, query: &str) -> Resp {
    b.send("GET", &format!("/verify{query}"), &[], "")
}

// depth: the tests

#[test]
fn a_password_and_an_enrolled_totp_code_open_a_session_the_proxy_can_verify() {
    let Some(w) = World::start("totp", serde_json::json!({})) else {
        return;
    };
    let mut b = w.browser();

    // Nobody signed in: a proxied page load is sent to the login, carrying
    // where it was going; anything else is a bare 401.
    let r = b.send(
        "GET",
        "/verify",
        &[
            ("x-forwarded-method", "GET"),
            ("x-forwarded-proto", "https"),
            ("x-forwarded-host", "app.example.test"),
            ("x-forwarded-uri", "/reports?q=1"),
        ],
        "",
    );
    assert_eq!(r.status, 302);
    assert_eq!(
        r.header("location"),
        Some("https://auth.example.test/?rd=https%3A%2F%2Fapp.example.test%2Freports%3Fq%3D1")
    );
    assert_eq!(
        b.send("GET", "/verify", &[("x-forwarded-method", "POST")], "")
            .status,
        401
    );

    // A wrong password is refused and sets no cookie.
    let r = b.login("alice", "wrong");
    assert_eq!(r.status, 401);
    assert!(r.body.contains("Wrong username or password"), "{}", r.body);
    assert!(b.cookie.is_none());

    // The right one opens a pending session, which the proxy does not admit.
    let r = b.post(
        "/login",
        "username=alice&password=alice-pw&rd=https%3A%2F%2Fapp.example.test%2Freports",
    );
    assert_eq!(r.status, 303, "{}", w.log());
    assert_eq!(r.header("location"), Some("/2fa"));
    let set = r.header("set-cookie").unwrap();
    for attr in [
        "HttpOnly",
        "SameSite=Lax",
        "Path=/",
        "Domain=example.test",
        "Max-Age=300",
    ] {
        assert!(set.contains(attr), "{set}");
    }
    let pending = b.cookie.clone().unwrap();
    assert_eq!(verify(&mut b, "").status, 401);

    // No TOTP on file and first_login enrolment: the page offers one.
    let page = b.get("/2fa");
    assert_eq!(page.status, 200);
    let secret = enrol_secret(&page.body);
    assert!(
        page.body.contains(&format!(
            "otpauth://totp/Example:alice?secret={secret}&amp;issuer=Example"
        )),
        "{}",
        page.body
    );
    let r = b.post("/2fa/enroll", "code=000000");
    assert_eq!(r.status, 401);

    // The code from the app signs in under a new cookie, and lands where
    // the user was going.
    let used = totp(&secret, 0);
    let r = b.post("/2fa/enroll", &format!("code={used}"));
    assert_eq!(r.status, 303, "{}", w.log());
    assert_eq!(
        r.header("location"),
        Some("https://app.example.test/reports")
    );
    assert!(r.header("set-cookie").unwrap().contains("Max-Age=43200"));
    let signed_in = b.cookie.clone().unwrap();
    assert_ne!(
        pending, signed_in,
        "the session id is new after the second factor"
    );

    // The proxy is told who it is and what groups they are in.
    let r = verify(&mut b, "");
    assert_eq!(r.status, 200);
    assert_eq!(r.header("remote-user"), Some("alice"));
    assert_eq!(r.header("remote-groups"), Some("admins,staff"));
    assert_eq!(r.header("remote-name"), Some("Alice Liddell"));
    assert_eq!(r.header("remote-email"), Some("alice@example.com"));
    assert_eq!(verify(&mut b, "?group=admins").status, 200);
    assert_eq!(verify(&mut b, "?group=ops,staff").status, 200);
    assert_eq!(verify(&mut b, "?group=ops").status, 403);
    // The pending cookie is worth nothing now.
    let mut old = w.browser();
    old.cookie = Some(pending);
    assert_eq!(verify(&mut old, "").status, 401);

    let s: serde_json::Value = serde_json::from_str(&b.get("/api/session").body).unwrap();
    assert_eq!(s["user"], "alice");
    assert_eq!(s["signed_in"], true);
    assert_eq!(s["amr"], serde_json::json!(["pwd", "otp"]));
    assert_eq!(s["groups"], serde_json::json!(["admins", "staff"]));

    // Second sign-in: the TOTP form now, not enrolment. The code already
    // used is refused; the next step's is accepted.
    let mut again = w.browser();
    assert_eq!(again.login("ALICE", "alice-pw").status, 303);
    let page = again.get("/2fa");
    assert!(page.body.contains(r#"action="/2fa/totp""#), "{}", page.body);
    assert!(!page.body.contains("totp-secret"));
    let r = again.post("/2fa/totp", &format!("code={used}"));
    assert_eq!(r.status, 401);
    assert!(r.body.contains("already used"), "{}", r.body);
    let r = again.post("/2fa/totp", &format!("code={}", totp(&secret, 1)));
    assert_eq!(r.status, 303, "{}\n{}", r.body, w.log());
    assert_eq!(verify(&mut again, "").header("remote-user"), Some("alice"));

    // The store holds no cookie in the clear: sessions are keyed by its hash.
    let keys = w.kv_keys();
    assert!(keys.contains("totp/alice"), "{keys}");
    assert!(keys.contains("session/"), "{keys}");
    assert!(!keys.contains(&signed_in), "{keys}");

    // Signing out ends the session for the proxy too.
    let r = b.post("/logout", "");
    assert_eq!(r.status, 303);
    assert!(r.header("set-cookie").unwrap().contains("Max-Age=0"));
    let mut gone = w.browser();
    gone.cookie = Some(signed_in);
    assert_eq!(verify(&mut gone, "").status, 401);
    assert_eq!(
        verify(&mut again, "").status,
        200,
        "another session is untouched"
    );
}

#[test]
fn the_webhook_approves_denies_or_keeps_the_user_waiting() {
    let Some(w) = World::start(
        "webhook",
        serde_json::json!({"second_factor": "webhook", "totp_enroll": "off"}),
    ) else {
        return;
    };

    // Pending, then approved when the waiting page asks again.
    w.hook.answer(|v| match v["event"].as_str() {
        Some("challenge") => Decision::Pending,
        _ => Decision::Approve,
    });
    let mut b = w.browser();
    assert_eq!(b.login("alice", "alice-pw").status, 303);
    let page = b.get("/2fa");
    assert!(
        page.body.contains(r#"action="/2fa/webhook""#),
        "{}",
        page.body
    );
    assert!(!page.body.contains("totp"), "TOTP is not a factor here");
    let r = b.post("/2fa/webhook", "");
    assert_eq!(r.status, 200);
    assert!(r.body.contains(r#"http-equiv="refresh""#), "{}", r.body);
    assert_eq!(verify(&mut b, "").status, 401);
    let r = b.get("/2fa/webhook");
    assert_eq!(r.status, 303, "{}\n{}", r.body, w.log());
    assert_eq!(verify(&mut b, "").header("remote-user"), Some("alice"));
    let s: serde_json::Value = serde_json::from_str(&b.get("/api/session").body).unwrap();
    assert_eq!(s["amr"], serde_json::json!(["pwd", "mca"]));

    // What the webhook was told: one challenge id across both events,
    // the user and their groups, and never the password.
    let seen = w.hook.seen();
    assert_eq!(seen.len(), 2);
    assert_eq!(seen[0]["event"], "challenge");
    assert_eq!(seen[1]["event"], "poll");
    assert_eq!(seen[0]["challenge"], seen[1]["challenge"]);
    assert_eq!(seen[0]["user"], "alice");
    assert_eq!(seen[0]["purpose"], "login");
    assert_eq!(seen[0]["groups"], serde_json::json!(["admins", "staff"]));
    assert_eq!(seen[0]["remote_addr"], "127.0.0.1");
    assert!(!seen[0].to_string().contains("alice-pw"));

    // A code the webhook sent some other way (SMS, mail) is passed to it.
    w.hook
        .answer(|v| match (v["event"].as_str(), v["code"].as_str()) {
            (Some("code"), Some("4242")) => Decision::Approve,
            (Some("code"), _) => Decision::Deny,
            _ => Decision::Pending,
        });
    let mut c = w.browser();
    c.login("bob", "bob-pw");
    assert_eq!(c.post("/2fa/webhook", "").status, 200);
    assert_eq!(c.get("/2fa/webhook").status, 200, "still pending");
    let r = c.post("/2fa/webhook", "code=4242");
    assert_eq!(r.status, 303, "{}\n{}", r.body, w.log());
    assert_eq!(verify(&mut c, "").header("remote-user"), Some("bob"));
    assert_eq!(verify(&mut c, "").header("remote-groups"), Some("staff"));

    // Denied: the attempt is over, and the cookie names nothing.
    w.hook.answer(|_| Decision::Deny);
    let mut d = w.browser();
    d.login("bob", "bob-pw");
    let r = d.post("/2fa/webhook", "");
    assert_eq!(r.status, 401);
    assert!(r.body.contains("denied"), "{}", r.body);
    assert_eq!(d.get("/2fa").status, 401);

    // A webhook that fails approves nothing.
    for answer in [Decision::Status(500), Decision::Status(302)] {
        w.hook.answer(move |_| answer);
        let mut e = w.browser();
        e.login("bob", "bob-pw");
        let r = e.post("/2fa/webhook", "");
        assert_eq!(r.status, 502, "{answer:?}: {}", r.body);
        assert_eq!(verify(&mut e, "").status, 401);
    }
}

#[test]
fn failures_ban_a_user_even_with_the_right_password() {
    let Some(w) = World::start(
        "ban",
        serde_json::json!({"max_retries": 3, "find_time": 60, "ban_time": 60}),
    ) else {
        return;
    };
    // Passwords: three wrong ones, and the right one is refused too.
    let mut b = w.browser();
    for _ in 0..3 {
        assert_eq!(b.login("bob", "nope").status, 401);
    }
    let r = b.login("bob", "bob-pw");
    assert_eq!(r.status, 429, "{}", r.body);
    assert!(b.cookie.is_none());
    // Spelled differently, still the same user.
    assert_eq!(b.login("BOB", "bob-pw").status, 429);

    // Second-factor guesses count against the user as well: alice enrols,
    // then three wrong codes on a later sign-in end it.
    let (_, secret) = w.enrolled_alice();
    let mut a = w.browser();
    assert_eq!(a.login("alice", "alice-pw").status, 303);
    for _ in 0..2 {
        assert_eq!(a.post("/2fa/totp", "code=000000").status, 401);
    }
    assert_eq!(a.post("/2fa/totp", "code=111111").status, 401);
    // The right code no longer helps, and nor does a fresh password.
    let r = a.post("/2fa/totp", &format!("code={}", totp(&secret, 1)));
    assert_eq!(r.status, 429, "{}", r.body);
    assert_eq!(w.browser().login("alice", "alice-pw").status, 429);
    assert!(w.log().contains("alice is banned"), "{}", w.log());
}

#[test]
fn a_session_lives_as_long_as_its_kv_record() {
    let Some(mut w) = World::start(
        "ttl",
        serde_json::json!({"second_factor": "none", "session_ttl": 2}),
    ) else {
        return;
    };
    // second_factor none: the password alone signs in.
    let mut b = w.browser();
    let r = b.login("bob", "bob-pw");
    assert_eq!(r.status, 303);
    assert_eq!(r.header("location"), Some("/"));
    assert_eq!(verify(&mut b, "").header("remote-user"), Some("bob"));

    // The record lives in kv, so a restarted program still knows it.
    w.restart(serde_json::json!({"second_factor": "none", "session_ttl": 2}));
    b.port = w.port;
    assert_eq!(verify(&mut b, "").status, 200, "{}", w.log());

    // And it expires with the record's TTL.
    std::thread::sleep(Duration::from_millis(3200));
    assert_eq!(verify(&mut b, "").status, 401);
    assert!(!w.kv_keys().contains("session/"), "{}", w.kv_keys());
}

#[test]
fn a_token_from_the_session_verifies_under_the_hosts_key() {
    let Some(w) = World::start(
        "jwt",
        serde_json::json!({"jwt_ttl": 120, "jwt_audience": "drt-topics"}),
    ) else {
        return;
    };
    let mut pending = w.browser();
    pending.login("alice", "alice-pw");
    assert_eq!(pending.post("/api/token", "").status, 401);

    let (mut b, _) = w.enrolled_alice();
    let r = b.post("/api/token", "");
    assert_eq!(r.status, 200, "{}", r.body);
    let v: serde_json::Value = serde_json::from_str(&r.body).unwrap();
    assert_eq!(v["expires_in"], 120);
    let token = v["token"].as_str().unwrap().to_string();

    // Checked by another program holding only the same master: the
    // signature, the issuer and the claims the session carried.
    let program = w.dir.join("check.dlua");
    std::fs::write(
        &program,
        format!(
            "host.call('crypto/derive', {{label = 'session:1', alg = 'EdDSA'}})\n\
             local r = host.call('crypto/jwt_verify', {{token = '{token}'}})\n\
             print(r.valid, r.claims.iss, r.claims.sub, table.concat(r.claims.groups, ','),\n\
                   table.concat(r.claims.amr, '+'), r.claims.aud, r.claims.exp - r.claims.iat)\n"
        ),
    )
    .unwrap();
    let config = w.dir.join("check.json");
    std::fs::write(
        &config,
        serde_json::json!({
            "program": {"path": program},
            "caps": [
                {"capability": "host:crypto/derive", "scope": ["session:*"]},
                {"capability": "host:crypto/jwt_verify"}
            ],
            "connectors": {"crypto": {"scope": {"key": MASTER}}}
        })
        .to_string(),
    )
    .unwrap();
    let out = drt()
        .arg("run")
        .arg("--config")
        .arg(&config)
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert_eq!(
        text.trim(),
        "true\tExample\talice\tadmins,staff\tpwd+otp\tdrt-topics\t120",
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    // The header names the key, so a verifier picks it from a JWKS.
    let header = token.split('.').next().unwrap();
    assert_eq!(
        header,
        "eyJhbGciOiJFZERTQSIsInR5cCI6IkpXVCIsImtpZCI6InNlc3Npb246MSJ9"
    );
}

#[test]
fn a_sign_in_only_sends_the_browser_inside_the_cookies_domain() {
    let Some(w) = World::start("rd", serde_json::json!({"second_factor": "none"})) else {
        return;
    };
    for (rd, to) in [
        (
            "https%3A%2F%2Fapp.example.test%2Fx",
            "https://app.example.test/x",
        ),
        ("https%3A%2F%2Fexample.test%2F", "https://example.test/"),
        ("%2Fhome", "/home"),
        ("https%3A%2F%2Fevil.test%2F", "/"),
        ("https%3A%2F%2Fexample.test.evil.test%2F", "/"),
        ("https%3A%2F%2Fapp.example.test%40evil.test%2F", "/"),
        ("%2F%2Fevil.test%2F", "/"),
        ("javascript%3Aalert(1)", "/"),
    ] {
        let mut b = w.browser();
        let r = b.post("/login", &format!("username=bob&password=bob-pw&rd={rd}"));
        assert_eq!(r.status, 303, "{rd}");
        assert_eq!(r.header("location"), Some(to), "{rd}");
    }
    // A plain http address is fine here only because the cookie is not
    // Secure in these tests. (The username is form-encoded with spaces
    // around it, which are trimmed.)
    let mut b = w.browser();
    let r = b.post(
        "/login",
        "username=+bob%20&password=bob-pw&rd=http%3A%2F%2Fapp.example.test%2F",
    );
    assert_eq!(r.header("location"), Some("http://app.example.test/"));
}

#[test]
fn enrolment_follows_the_policy() {
    // off: a user with no TOTP on file is not offered one.
    let Some(mut w) = World::start(
        "enrol",
        serde_json::json!({"second_factor": "totp", "totp_enroll": "off"}),
    ) else {
        return;
    };
    let mut b = w.browser();
    b.login("bob", "bob-pw");
    let page = b.get("/2fa");
    assert!(
        page.body.contains("No second factor is set up for you"),
        "{}",
        page.body
    );
    assert_eq!(b.post("/2fa/enroll", "code=123456").status, 409);
    assert!(!w.kv_keys().contains("totp/bob"));

    // webhook: the webhook approves first, then enrolment opens, and the
    // first code signs in.
    w.restart(serde_json::json!({"second_factor": "totp,webhook", "totp_enroll": "webhook"}));
    w.hook.answer(|_| Decision::Approve);
    let mut b = w.browser();
    b.login("bob", "bob-pw");
    let page = b.get("/2fa");
    assert!(!page.body.contains("totp-secret"), "not before the webhook");
    let r = b.post("/2fa/webhook", "");
    assert_eq!(r.status, 303, "{}\n{}", r.body, w.log());
    assert_eq!(r.header("location"), Some("/2fa"));
    assert_eq!(
        verify(&mut b, "").status,
        401,
        "approval opened enrolment, not a session"
    );
    assert_eq!(w.hook.seen().last().unwrap()["purpose"], "enroll");
    let page = b.get("/2fa");
    let secret = enrol_secret(&page.body);
    let r = b.post("/2fa/enroll", &format!("code={}", totp(&secret, 0)));
    assert_eq!(r.status, 303, "{}", r.body);
    assert_eq!(verify(&mut b, "").header("remote-user"), Some("bob"));
    let s: serde_json::Value = serde_json::from_str(&b.get("/api/session").body).unwrap();
    assert_eq!(s["amr"], serde_json::json!(["pwd", "mca", "otp"]));

    // Enrolled now, so a later sign-in asks for the code, and the
    // webhook still works as the other way in.
    let mut c = w.browser();
    c.login("bob", "bob-pw");
    let page = c.get("/2fa");
    assert!(page.body.contains(r#"action="/2fa/totp""#));
    assert!(page.body.contains(r#"action="/2fa/webhook""#));
    assert_eq!(c.post("/2fa/webhook", "").status, 303);
    assert_eq!(verify(&mut c, "").header("remote-user"), Some("bob"));
}

#[test]
fn a_misconfigured_deployment_is_refused_by_name() {
    let Some(w) = World::start("refuse", serde_json::json!({})) else {
        return;
    };
    for (args, says) in [
        (
            serde_json::json!({"second_factor": "sms"}),
            "second_factor is totp, webhook",
        ),
        (
            serde_json::json!({"totp_enroll": "always"}),
            "totp_enroll is off, first_login or webhook",
        ),
        (
            serde_json::json!({"session_ttl": "long"}),
            "session_ttl is a number",
        ),
        (
            serde_json::json!({"colour": "blue"}),
            "colour is not an argument",
        ),
        (
            serde_json::json!({"second_factor": "webhook", "webhook_url": ""}),
            "webhook_url is not set",
        ),
    ] {
        let config = w.config(args);
        let out = drt()
            .arg("start")
            .arg("--config")
            .arg(config)
            .stdin(Stdio::null())
            .output()
            .unwrap();
        let text = String::from_utf8_lossy(&out.stdout).to_string()
            + &String::from_utf8_lossy(&out.stderr);
        assert!(text.contains(says), "{says}: {text}");
    }
}
