//! The connector against a real OpenLDAP `slapd`, over each transport it
//! speaks: plaintext to loopback, StartTLS and ldaps, the last two with a
//! CA minted here and trusted through `extra_roots`.
//!
//! `slapd` runs unprivileged from a temporary directory, with its own
//! config, database and certificates; nothing on the machine is touched.
//! Without `slapd` (Debian/Ubuntu: `apt install slapd`) every test here
//! says `skipped` and passes, unless `DRT_TEST_REQUIRE_LDAP=1` is set, as
//! CI sets it after installing one: there a skip is a broken runner.
//!
//! ## surface block
//!
//! - Entry points: the tests; [`Slapd::start`], the server.
//! - Configurable values: [`SCHEMA_DIRS`], [`MODULE_DIRS`], where a distro
//!   keeps slapd's schema and modules; the directory in [`LDIF`].
//! - Fan-out: [`Slapd::url`] by transport.

#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use drt_caps::Scope;
use drt_connector::Connector;
use drt_connector_ldap::LdapConnector;

const SCHEMA_DIRS: &[&str] = &[
    "/etc/ldap/schema",
    "/etc/openldap/schema",
    "/usr/local/etc/openldap/schema",
];
const MODULE_DIRS: &[&str] = &["/usr/lib/ldap", "/usr/lib64/openldap", "/usr/lib/openldap"];
const SLAPD: &[&str] = &[
    "/usr/sbin/slapd",
    "/usr/local/libexec/slapd",
    "/usr/libexec/slapd",
];

const BASE: &str = "dc=example,dc=com";
const SERVICE_DN: &str = "cn=svc,ou=services,dc=example,dc=com";
const SERVICE_PW: &str = "service-secret";

/// Passwords are stored in the clear, which slapd accepts for a simple
/// bind; a real directory hashes them, and the bind is the same either way.
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
cn: Alice Liddell
sn: Liddell
mail: alice@example.com
userPassword: alice-pw

dn: uid=bob,ou=people,dc=example,dc=com
objectClass: inetOrgPerson
uid: bob
cn: Bob
sn: Bob
mail: bob@example.com
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

macro_rules! skipped {
    ($($arg:tt)*) => {{
        if std::env::var_os("DRT_TEST_REQUIRE_LDAP").is_some() {
            panic!("DRT_TEST_REQUIRE_LDAP is set, and this test would have skipped: {}", format!($($arg)*));
        }
        eprintln!("skipped: {}", format!($($arg)*));
    }};
}

/// Held across every fork in this file; see [`Slapd::start`].
static SPAWN: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn unconfined_copy(slapd: &Path) -> PathBuf {
    static COPY: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    COPY.get_or_init(|| {
        let dir = std::env::temp_dir().join(format!("drt-ldap-bin-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let copy = dir.join("slapd");
        std::fs::copy(slapd, &copy).unwrap();
        copy
    })
    .clone()
}

struct Slapd {
    child: Child,
    dir: PathBuf,
    plain: u16,
    tls: u16,
}

impl Drop for Slapd {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn first_existing(candidates: &[&str]) -> Option<PathBuf> {
    candidates.iter().map(PathBuf::from).find(|p| p.exists())
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
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
        let dir = std::env::temp_dir().join(format!("drt-ldap-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("db")).unwrap();
        write_certs(&dir);
        let module = match first_existing(MODULE_DIRS) {
            Some(m) if m.join("back_mdb.so").exists() || m.join("back_mdb.la").exists() => {
                format!("modulepath {}\nmoduleload back_mdb\n", m.display())
            }
            // Built in, as some distros build it.
            _ => String::new(),
        };
        let d = dir.display();
        let s = schema.display();
        let conf = format!(
            "include {s}/core.schema\ninclude {s}/cosine.schema\ninclude {s}/inetorgperson.schema\n\
             pidfile {d}/slapd.pid\n{module}\
             TLSCACertificateFile {d}/ca.pem\nTLSCertificateFile {d}/server.pem\n\
             TLSCertificateKeyFile {d}/server.key\n\
             database mdb\nmaxsize 10485760\nsuffix \"{BASE}\"\nrootdn \"cn=admin,{BASE}\"\n\
             rootpw admin-pw\ndirectory {d}/db\nsizelimit 500\n\
             access to attrs=userPassword by self read by anonymous auth by * none\n\
             access to * by * read\n"
        );
        std::fs::write(dir.join("slapd.conf"), conf).unwrap();
        std::fs::write(dir.join("data.ldif"), LDIF).unwrap();
        // Every fork in this file happens under this lock; see below.
        let _spawning = SPAWN.lock().unwrap_or_else(|e| e.into_inner());
        let slapadd = slapd.with_file_name("slapadd");
        let added = Command::new(&slapadd)
            .arg("-f")
            .arg(dir.join("slapd.conf"))
            .arg("-l")
            .arg(dir.join("data.ldif"))
            .output()
            .unwrap_or_else(|e| panic!("{}: {e}", slapadd.display()));
        assert!(
            added.status.success(),
            "slapadd: {}",
            String::from_utf8_lossy(&added.stderr)
        );

        let (plain, tls) = (free_port(), free_port());
        // Run a copy. Ubuntu's AppArmor profile for slapd attaches to the
        // path /usr/sbin/slapd and lets it read only its own directories,
        // so the real binary cannot open this test's config, certificates
        // or database; `slapadd`, a hard link to the same file under
        // another path, is not confined, which is why it worked. A copy
        // at another path is the same program without the profile.
        //
        // One copy per process, made and run under one lock: a copy being
        // written while another test forks leaks the write descriptor into
        // that child, and exec of the copy then fails with ETXTBSY.
        let copy = unconfined_copy(&slapd);
        let log = std::fs::File::create(dir.join("slapd.log")).unwrap();
        let child = Command::new(&copy)
            .arg("-f")
            .arg(dir.join("slapd.conf"))
            .arg("-h")
            .arg(format!(
                "ldap://127.0.0.1:{plain}/ ldaps://127.0.0.1:{tls}/"
            ))
            // -d keeps it in the foreground, so the child is the server;
            // `-d 1` would say more, and 0 still reports a refused start.
            .args(["-d", "0"])
            .stdout(Stdio::null())
            .stderr(log)
            .spawn()
            .unwrap();
        let mut server = Slapd {
            child,
            dir,
            plain,
            tls,
        };
        let deadline = Instant::now() + Duration::from_secs(10);
        let listening = |port| std::net::TcpStream::connect(("127.0.0.1", port)).is_ok();
        while !(listening(plain) && listening(tls)) {
            let exited = server.child.try_wait().unwrap();
            if exited.is_some() || Instant::now() > deadline {
                let said =
                    std::fs::read_to_string(server.dir.join("slapd.log")).unwrap_or_default();
                panic!("slapd did not listen on {plain} and {tls} ({exited:?}):\n{said}");
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        Some(server)
    }

    fn ca(&self) -> PathBuf {
        self.dir.join("ca.pem")
    }
}

/// A CA, and a server certificate for 127.0.0.1 it signed.
fn write_certs(dir: &Path) {
    use rcgen::{BasicConstraints, CertificateParams, IsCa, KeyPair};
    let ca_key = KeyPair::generate().unwrap();
    let mut ca_params = CertificateParams::new(Vec::new()).unwrap();
    ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    ca_params
        .distinguished_name
        .push(rcgen::DnType::CommonName, "drt test CA");
    let ca = ca_params.self_signed(&ca_key).unwrap();
    let key = KeyPair::generate().unwrap();
    let leaf = CertificateParams::new(vec!["127.0.0.1".into(), "localhost".into()])
        .unwrap()
        .signed_by(&key, &ca, &ca_key)
        .unwrap();
    std::fs::write(dir.join("ca.pem"), ca.pem()).unwrap();
    std::fs::write(dir.join("server.pem"), leaf.pem()).unwrap();
    std::fs::write(dir.join("server.key"), key.serialize_pem()).unwrap();
}

fn scope(json: serde_json::Value) -> Scope {
    Scope(rmpv::ext::to_value(json).unwrap())
}

fn no_terminal(what: &str, _: drt_config::secret::Echo) -> Result<String, String> {
    Err(format!("{what}: no terminal"))
}

fn call(scope: &Scope, verb: &str, args: serde_json::Value) -> Result<serde_json::Value, String> {
    let connector = LdapConnector::wired(Some(scope), &no_terminal)?;
    pollster::block_on(connector.call(verb, Some(rmpv::ext::to_value(args).unwrap()), Some(scope)))
        .map(|v| serde_json::to_value(v).unwrap())
        .map_err(|e| e.detail)
}

fn with_service(mut json: serde_json::Value) -> Scope {
    json["base_dn"] = BASE.into();
    json["bind_dn"] = SERVICE_DN.into();
    json["bind_password"] = SERVICE_PW.into();
    scope(json)
}

/// Every verb, over plaintext to loopback: the case a local LLDAP is.
#[test]
fn plaintext_on_loopback_binds_searches_and_reads_groups() {
    let Some(server) = Slapd::start("plain") else {
        return;
    };
    let s = with_service(serde_json::json!({
        "url": format!("ldap://127.0.0.1:{}", server.plain),
        "starttls": false,
    }));

    let r = call(
        &s,
        "ldap/bind",
        serde_json::json!({"user": "alice", "password": "alice-pw", "attrs": ["mail"]}),
    )
    .unwrap();
    assert_eq!(r["ok"], true, "{r}");
    assert_eq!(r["dn"], "uid=alice,ou=people,dc=example,dc=com");
    assert_eq!(r["attrs"]["mail"], serde_json::json!(["alice@example.com"]));
    assert_eq!(r["groups"], serde_json::json!(["admins", "staff"]));

    for (user, password) in [
        ("alice", "wrong"),
        ("nobody", "x"),
        ("alice", ""),
        ("*", "alice-pw"),
    ] {
        let r = call(
            &s,
            "ldap/bind",
            serde_json::json!({"user": user, "password": password}),
        )
        .unwrap();
        assert_eq!(r, serde_json::json!({"ok": false}), "{user}/{password}");
    }

    let r = call(&s, "ldap/groups", serde_json::json!({"user": "bob"})).unwrap();
    assert_eq!(r["groups"], serde_json::json!(["staff"]), "{r}");
    assert_eq!(
        r["group_dns"],
        serde_json::json!(["cn=staff,ou=groups,dc=example,dc=com"])
    );

    let r = call(
        &s,
        "ldap/search",
        serde_json::json!({"filter": "(objectClass=inetOrgPerson)", "attrs": ["uid"]}),
    )
    .unwrap();
    let dns: Vec<&str> = r["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["dn"].as_str().unwrap())
        .collect();
    assert_eq!(dns.len(), 3, "{r}");
    assert!(
        dns.contains(&"uid=bob,ou=people,dc=example,dc=com"),
        "{dns:?}"
    );
    assert_eq!(r["truncated"], false);
    // No userPassword: the ACL says so, and the connector adds nothing.
    let r = call(
        &s,
        "ldap/search",
        serde_json::json!({"filter": "(uid=alice)"}),
    )
    .unwrap();
    assert!(
        r["entries"][0]["attrs"].get("userPassword").is_none(),
        "{r}"
    );
    assert_eq!(
        r["entries"][0]["attrs"]["cn"],
        serde_json::json!(["Alice Liddell"])
    );

    let r = call(
        &s,
        "ldap/search",
        serde_json::json!({"filter": "(objectClass=*)", "limit": 3}),
    )
    .unwrap();
    assert_eq!(r["entries"].as_array().unwrap().len(), 3);
    assert_eq!(r["truncated"], true, "{r}");

    let r = call(
        &s,
        "ldap/search",
        serde_json::json!({"filter": "(objectClass=*)", "base": "ou=nowhere,dc=example,dc=com"}),
    )
    .unwrap();
    assert_eq!(r["entries"], serde_json::json!([]));
}

/// StartTLS, verified against the CA the scope trusts, and refused when the
/// scope trusts only webpki's roots.
#[test]
fn starttls_verifies_the_server_against_extra_roots() {
    let Some(server) = Slapd::start("starttls") else {
        return;
    };
    let url = format!("ldap://127.0.0.1:{}", server.plain);
    let trusted = with_service(serde_json::json!({"url": url, "extra_roots": [server.ca()]}));
    let r = call(
        &trusted,
        "ldap/bind",
        serde_json::json!({"user": "bob", "password": "bob-pw"}),
    )
    .unwrap();
    assert_eq!(r["ok"], true, "{r}");

    let untrusted = with_service(serde_json::json!({"url": url}));
    let e = call(
        &untrusted,
        "ldap/bind",
        serde_json::json!({"user": "bob", "password": "bob-pw"}),
    )
    .unwrap_err();
    assert!(
        e.to_lowercase().contains("certificate") || e.contains("UnknownIssuer"),
        "{e}"
    );
}

#[test]
fn ldaps_verifies_the_server_against_extra_roots() {
    let Some(server) = Slapd::start("ldaps") else {
        return;
    };
    let url = format!("ldaps://127.0.0.1:{}", server.tls);
    let trusted = with_service(serde_json::json!({"url": url, "extra_roots": [server.ca()]}));
    let r = call(
        &trusted,
        "ldap/groups",
        serde_json::json!({"user": "alice"}),
    )
    .unwrap();
    assert_eq!(r["groups"], serde_json::json!(["admins", "staff"]), "{r}");

    let untrusted = with_service(serde_json::json!({"url": url}));
    assert!(call(
        &untrusted,
        "ldap/groups",
        serde_json::json!({"user": "alice"})
    )
    .is_err());
}

/// No service account: the lookup is anonymous, which this slapd allows,
/// and the groups are read as the user who just bound.
#[test]
fn without_a_service_account_the_user_reads_their_own_groups() {
    let Some(server) = Slapd::start("anon") else {
        return;
    };
    let s = scope(serde_json::json!({
        "url": format!("ldap://127.0.0.1:{}", server.plain),
        "starttls": false,
        "base_dn": BASE,
        "group_base": format!("ou=groups,{BASE}"),
    }));
    let r = call(
        &s,
        "ldap/bind",
        serde_json::json!({"user": "alice", "password": "alice-pw"}),
    )
    .unwrap();
    assert_eq!(r["ok"], true, "{r}");
    assert_eq!(r["groups"], serde_json::json!(["admins", "staff"]));
}

/// A wrong service password is the operator's mistake, named as theirs.
#[test]
fn a_wrong_service_password_names_the_config() {
    let Some(server) = Slapd::start("svcpw") else {
        return;
    };
    let s = scope(serde_json::json!({
        "url": format!("ldap://127.0.0.1:{}", server.plain),
        "starttls": false,
        "base_dn": BASE,
        "bind_dn": SERVICE_DN,
        "bind_password": "not-it",
    }));
    let e = call(
        &s,
        "ldap/search",
        serde_json::json!({"filter": "(uid=alice)"}),
    )
    .unwrap_err();
    assert!(e.contains("connectors.ldap"), "{e}");
}
