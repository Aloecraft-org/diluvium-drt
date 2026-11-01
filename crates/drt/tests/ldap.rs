//! The ldap connector through the real binary, against the fake directory
//! the connector's own tests use: a dlua program under `drt run`, and
//! `drt ldap` on the command line, from the same config file.

#![cfg(feature = "connector-ldap")]

#[path = "../../../connectors/ldap/tests/fake/mod.rs"]
mod fake;

use std::io::Write;
use std::path::Path;
use std::process::{Command, Output, Stdio};

fn drt() -> Command {
    Command::new(env!("CARGO_BIN_EXE_drt"))
}

/// A config wiring ldap to the fake, with the service password read from
/// an environment variable, the shape a real deployment uses.
fn config(dir: &Path, fake: &fake::Fake, caps: &str) -> std::path::PathBuf {
    let path = dir.join("drt.json");
    let json = format!(
        r#"{{
          "caps": [{caps}],
          "connectors": {{
            "ldap": {{"scope": {{
              "url": "{}", "starttls": false, "base_dn": "{}",
              "bind_dn": "{}", "bind_password": {{"env": "DRT_TEST_LDAP_PW"}},
              "group_base": "ou=groups,{}"
            }}}}
          }}
        }}"#,
        fake.url(),
        fake::BASE,
        fake::SERVICE_DN,
        fake::BASE
    );
    std::fs::write(&path, json).unwrap();
    path
}

fn text(out: &Output) -> String {
    format!(
        "status {:?}\nstdout:\n{}\nstderr:\n{}",
        out.status.code(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

const PROGRAM: &str = r#"
local function show(label, call, args)
    local v, status, detail = host.try(call, args)
    if v and v.ok ~= nil then
        print(label, status, tostring(v.ok), v.dn or "-", table.concat(v.groups or {}, ","))
    elseif v and v.entries then
        local dns = {}
        for _, e in ipairs(v.entries) do dns[#dns + 1] = e.attrs.mail[1] end
        print(label, status, table.concat(dns, ","))
    else
        print(label, status, detail)
    end
end
show("right", "ldap/bind", { user = "alice", password = "alice-pw" })
show("wrong", "ldap/bind", { user = "alice", password = "nope" })
show("empty", "ldap/bind", { user = "alice", password = "" })
show("search", "ldap/search", { filter = "(objectClass=inetOrgPerson)", attrs = { "mail" } })
show("outside", "ldap/search", { filter = "(uid=*)", base = "dc=com" })
"#;

#[test]
fn a_program_logs_a_user_in_and_reads_their_groups() {
    let fake = fake::start();
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("prog.dlua"), PROGRAM).unwrap();
    let cfg = config(dir.path(), &fake, r#"{"capability": "host:ldap/*"}"#);
    let out = drt()
        .env("DRT_TEST_LDAP_PW", fake::SERVICE_PW)
        .arg("--config")
        .arg(&cfg)
        .arg("run")
        .arg(dir.path().join("prog.dlua"))
        .output()
        .unwrap();
    let all = text(&out);
    assert!(out.status.success(), "{all}");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(
        lines[0], "right\tok\ttrue\tuid=alice,ou=people,dc=example,dc=com\tadmins,staff",
        "{all}"
    );
    assert_eq!(lines[1], "wrong\tok\tfalse\t-\t", "{all}");
    assert_eq!(lines[2], "empty\tok\tfalse\t-\t", "{all}");
    assert_eq!(
        lines[3], "search\tok\talice@example.com,bob@example.com",
        "{all}"
    );
    assert!(
        lines[4].starts_with("outside\terror\t") && lines[4].contains("outside"),
        "{all}"
    );
}

/// `host:ldap/bind` alone: a program that may check logins may not search.
#[test]
fn a_grant_of_bind_alone_denies_search() {
    let fake = fake::start();
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("prog.dlua"),
        r#"local _, s = host.try("ldap/search", { filter = "(uid=*)" }) print(s)
           local _, b = host.try("ldap/groups", { user = "alice" }) print(b)
           local v, t = host.try("ldap/bind", { user = "bob", password = "bob-pw" }) print(t, v.ok)"#,
    )
    .unwrap();
    let cfg = config(dir.path(), &fake, r#"{"capability": "host:ldap/bind"}"#);
    let out = drt()
        .env("DRT_TEST_LDAP_PW", fake::SERVICE_PW)
        .arg("--config")
        .arg(&cfg)
        .arg("run")
        .arg(dir.path().join("prog.dlua"))
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "denied\ndenied\nok\ttrue\n",
        "{}",
        text(&out)
    );
}

/// An unset password variable stops the run before the program starts,
/// naming the config key and the variable.
#[test]
fn an_unset_service_password_is_refused_at_startup_by_name() {
    let fake = fake::start();
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("prog.dlua"), "print('ran')").unwrap();
    let cfg = config(dir.path(), &fake, r#"{"capability": "host:ldap/*"}"#);
    let out = drt()
        .env_remove("DRT_TEST_LDAP_PW")
        .arg("--config")
        .arg(&cfg)
        .arg("run")
        .arg(dir.path().join("prog.dlua"))
        .output()
        .unwrap();
    let all = text(&out);
    assert!(!out.status.success(), "{all}");
    assert!(
        !String::from_utf8_lossy(&out.stdout).contains("ran"),
        "{all}"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("connectors.ldap.scope.bind_password")
            && stderr.contains("DRT_TEST_LDAP_PW"),
        "{all}"
    );
    assert!(fake.log().is_empty());
}

#[test]
fn drt_ldap_searches_binds_and_lists_groups_from_the_same_config() {
    let fake = fake::start();
    let dir = tempfile::tempdir().unwrap();
    let cfg = config(dir.path(), &fake, "");
    let ldap = |args: &[&str], stdin: Option<&str>| {
        let mut child = drt()
            .env("DRT_TEST_LDAP_PW", fake::SERVICE_PW)
            .arg("--config")
            .arg(&cfg)
            .arg("ldap")
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        if let Some(line) = stdin {
            child
                .stdin
                .take()
                .unwrap()
                .write_all(line.as_bytes())
                .unwrap();
        }
        child.wait_with_output().unwrap()
    };

    let out = ldap(&["search", "(uid=alice)", "mail", "cn"], None);
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "dn: uid=alice,ou=people,dc=example,dc=com\ncn: Alice Liddell\nmail: alice@example.com\n",
        "{}",
        text(&out)
    );
    assert_eq!(out.status.code(), Some(0));

    let out = ldap(
        &["search", "(objectClass=person)", "--limit", "1", "uid"],
        None,
    );
    assert!(
        String::from_utf8_lossy(&out.stdout)
            .ends_with("# truncated: more entries matched than --limit\n"),
        "{}",
        text(&out)
    );

    let out = ldap(&["bind", "--password-stdin", "alice"], Some("alice-pw\n"));
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "dn: uid=alice,ou=people,dc=example,dc=com\ngroup: admins\ngroup: staff\n",
        "{}",
        text(&out)
    );
    assert_eq!(out.status.code(), Some(0));

    let out = ldap(&["bind", "--password-stdin", "alice"], Some("wrong\n"));
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "refused\n",
        "{}",
        text(&out)
    );
    assert_eq!(out.status.code(), Some(1));

    let out = ldap(&["--json", "groups", "bob"], None);
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["groups"], serde_json::json!(["staff"]), "{}", text(&out));

    let out = ldap(&["groups", "nobody"], None);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "not found\n");
    assert_eq!(out.status.code(), Some(1));

    // Without a terminal, `bind` without --password-stdin refuses at once
    // rather than waiting on a read nobody will answer.
    // Only checked where there is no terminal: with one, the child would ask.
    if !drt::prompt::has_terminal() {
        let out = drt()
            .env("DRT_TEST_LDAP_PW", fake::SERVICE_PW)
            .arg("--config")
            .arg(&cfg)
            .args(["ldap", "bind", "alice"])
            .stdin(Stdio::null())
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(2), "{}", text(&out));
        assert!(
            String::from_utf8_lossy(&out.stderr).contains("no terminal"),
            "{}",
            text(&out)
        );
    }

    let bare = dir.path().join("bare.json");
    std::fs::write(&bare, "{}").unwrap();
    let out = drt()
        .arg("--config")
        .arg(&bare)
        .args(["ldap", "groups", "x"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("connectors.ldap"),
        "{}",
        text(&out)
    );
}
