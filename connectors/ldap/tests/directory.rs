//! `ldap/bind` and `ldap/search` against a real LDAP conversation: the
//! `ldap3` client in the connector talking to the fake directory in
//! `fake/`, over a loopback socket.

mod fake;

use drt_caps::Scope;
use drt_connector::Connector;
use drt_connector_ldap::LdapConnector;

fn scope_for(fake: &fake::Fake, extra: &str) -> Scope {
    let json = format!(
        r#"{{"url":"{}","starttls":false,"base_dn":"{}","bind_dn":"{}",
            "bind_password":"{}"{extra}}}"#,
        fake.url(),
        fake::BASE,
        fake::SERVICE_DN,
        fake::SERVICE_PW
    );
    let v: serde_json::Value = serde_json::from_str(&json).unwrap();
    Scope(rmpv::ext::to_value(v).unwrap())
}

fn args(json: &str) -> Option<rmpv::Value> {
    let v: serde_json::Value = serde_json::from_str(json).unwrap();
    Some(rmpv::ext::to_value(v).unwrap())
}

fn no_terminal(what: &str, _: drt_config::secret::Echo) -> Result<String, String> {
    Err(format!("{what}: no terminal"))
}

/// Called the way `drt run` calls it: no reactor on this thread.
fn call(scope: &Scope, verb: &str, a: &str) -> Result<serde_json::Value, String> {
    let connector = LdapConnector::wired(Some(scope), &no_terminal)?;
    pollster::block_on(connector.call(verb, args(a), Some(scope)))
        .map(|v| serde_json::to_value(v).unwrap())
        .map_err(|e| e.detail)
}

#[test]
fn bind_answers_ok_with_the_dn_and_attrs_for_the_right_password() {
    let fake = fake::start();
    let scope = scope_for(&fake, "");
    let r = call(
        &scope,
        "ldap/bind",
        r#"{"user":"alice","password":"alice-pw","attrs":["mail","memberOf"]}"#,
    )
    .unwrap();
    assert_eq!(r["ok"], true, "{r}");
    assert_eq!(r["dn"], "uid=alice,ou=people,dc=example,dc=com");
    assert_eq!(r["attrs"]["mail"][0], "alice@example.com");
    assert_eq!(
        r["attrs"]["memberOf"][0],
        "cn=admins,ou=groups,dc=example,dc=com"
    );
    assert!(
        r["attrs"].get("cn").is_none(),
        "only what was asked for: {r}"
    );
    let log = fake.log();
    assert_eq!(log[0], format!("bind {} ok", fake::SERVICE_DN), "{log:?}");
    assert_eq!(
        log[2], "bind uid=alice,ou=people,dc=example,dc=com ok",
        "{log:?}"
    );
}

/// A wrong password, an unknown user and an ambiguous one answer the same
/// thing, so the answer says nothing about who exists.
#[test]
fn every_refusal_is_the_same_bare_ok_false() {
    let fake = fake::start();
    let scope = scope_for(&fake, "");
    for a in [
        r#"{"user":"alice","password":"wrong"}"#,
        r#"{"user":"nobody","password":"x"}"#,
        r#"{"user":"twin","password":"twin-pw"}"#,
    ] {
        let r = call(&scope, "ldap/bind", a).unwrap();
        assert_eq!(r, serde_json::json!({"ok": false}), "{a}");
    }
}

/// The fake answers a DN with an empty password as success, as real
/// servers do. The connector must not ask.
#[test]
fn an_empty_password_is_refused_without_reaching_the_server() {
    let fake = fake::start();
    let scope = scope_for(&fake, "");
    let r = call(&scope, "ldap/bind", r#"{"user":"alice","password":""}"#).unwrap();
    assert_eq!(r, serde_json::json!({"ok": false}));
    assert!(fake.log().is_empty(), "{:?}", fake.log());
}

/// `*` and `)(` in a username are literal characters, not filter syntax.
#[test]
fn a_username_cannot_widen_the_filter() {
    let fake = fake::start();
    let scope = scope_for(&fake, "");
    for user in ["*", "alice)(uid=*", "*)(|(uid=*"] {
        let a = serde_json::json!({"user": user, "password": "alice-pw"}).to_string();
        let r = call(&scope, "ldap/bind", &a).unwrap();
        assert_eq!(r["ok"], false, "{user}");
    }
    let searches: Vec<String> = fake
        .log()
        .into_iter()
        .filter(|l| l.starts_with("search"))
        .collect();
    assert!(
        searches.iter().all(|l| l.contains("Equality(\"uid\"")),
        "{searches:?}"
    );
}

#[test]
fn search_returns_entries_sorted_attrs_and_respects_limit() {
    let fake = fake::start();
    let scope = scope_for(&fake, "");
    let r = call(
        &scope,
        "ldap/search",
        r#"{"filter":"(objectClass=inetOrgPerson)","attrs":["uid","mail"]}"#,
    )
    .unwrap();
    assert_eq!(r["truncated"], false);
    let entries = r["entries"].as_array().unwrap();
    assert_eq!(entries.len(), 2, "{r}");
    assert_eq!(entries[0]["dn"], "uid=alice,ou=people,dc=example,dc=com");
    assert_eq!(entries[0]["attrs"]["uid"][0], "alice");

    let r = call(
        &scope,
        "ldap/search",
        r#"{"filter":"(objectClass=person)","limit":2}"#,
    )
    .unwrap();
    assert_eq!(r["entries"].as_array().unwrap().len(), 2);
    assert_eq!(r["truncated"], true, "{r}");

    let r = call(
        &scope,
        "ldap/search",
        r#"{"filter":"(uid=*)","base":"ou=people,dc=example,dc=com","scope":"one"}"#,
    )
    .unwrap();
    assert_eq!(r["entries"].as_array().unwrap().len(), 4, "{r}");
}

#[test]
fn search_refuses_a_base_outside_the_grant_and_a_bad_filter_before_connecting() {
    let fake = fake::start();
    let scope = scope_for(&fake, "");
    let e = call(
        &scope,
        "ldap/search",
        r#"{"filter":"(uid=*)","base":"dc=com"}"#,
    )
    .unwrap_err();
    assert!(e.contains("outside"), "{e}");
    let e = call(&scope, "ldap/search", r#"{"filter":"uid=alice("}"#).unwrap_err();
    assert!(e.contains("not an LDAP filter"), "{e}");
    let e = call(&scope, "ldap/search", r#"{"filter":"(uid=*)","limit":0}"#).unwrap_err();
    assert!(e.contains("limit"), "{e}");
    assert!(fake.log().is_empty(), "{:?}", fake.log());
}

/// The operator's mistake is named as the operator's.
#[test]
fn a_wrong_service_password_is_an_error_naming_the_config() {
    let fake = fake::start();
    let json = format!(
        r#"{{"url":"{}","starttls":false,"base_dn":"{}","bind_dn":"{}","bind_password":"nope"}}"#,
        fake.url(),
        fake::BASE,
        fake::SERVICE_DN
    );
    let v: serde_json::Value = serde_json::from_str(&json).unwrap();
    let scope = Scope(rmpv::ext::to_value(v).unwrap());
    let e = call(&scope, "ldap/search", r#"{"filter":"(uid=alice)"}"#).unwrap_err();
    assert!(
        e.contains("service bind") && e.contains("connectors.ldap"),
        "{e}"
    );
}

#[test]
fn an_unknown_verb_names_the_ones_there_are() {
    let fake = fake::start();
    let scope = scope_for(&fake, "");
    let e = call(&scope, "ldap/modify", r#"{}"#).unwrap_err();
    assert!(e.contains("ldap/bind") && e.contains("ldap/search"), "{e}");
}

/// A server that accepts and never answers is a timeout, not a hang.
#[test]
fn a_silent_server_times_out() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let json = format!(
        r#"{{"url":"ldap://127.0.0.1:{port}","starttls":false,"base_dn":"dc=x","timeout_ms":300}}"#
    );
    let v: serde_json::Value = serde_json::from_str(&json).unwrap();
    let scope = Scope(rmpv::ext::to_value(v).unwrap());
    let started = std::time::Instant::now();
    let e = call(&scope, "ldap/search", r#"{"filter":"(uid=a)"}"#).unwrap_err();
    assert!(e.contains("timed out"), "{e}");
    assert!(started.elapsed() < std::time::Duration::from_secs(3));
    drop(listener);
}

#[test]
fn bind_answers_the_users_groups_by_name_and_dn_read_as_the_service() {
    let fake = fake::start();
    let scope = scope_for(&fake, "");
    let r = call(
        &scope,
        "ldap/bind",
        r#"{"user":"alice","password":"alice-pw"}"#,
    )
    .unwrap();
    assert_eq!(r["groups"], serde_json::json!(["admins", "staff"]), "{r}");
    assert_eq!(
        r["group_dns"],
        serde_json::json!([
            "cn=admins,ou=groups,dc=example,dc=com",
            "cn=staff,ou=groups,dc=example,dc=com"
        ])
    );
    let r = call(&scope, "ldap/bind", r#"{"user":"bob","password":"bob-pw"}"#).unwrap();
    assert_eq!(r["groups"], serde_json::json!(["staff"]), "{r}");
    let group_search = fake
        .log()
        .into_iter()
        .rfind(|l| l.contains("\"member\""))
        .unwrap();
    assert!(
        group_search.ends_with(&format!("as {}", fake::SERVICE_DN.to_ascii_lowercase())),
        "{group_search}"
    );
}

/// No service account and a directory that refuses anonymous search: the
/// user lookup fails, by name, before any password is tried. The path
/// where the user's own bind reads the groups is in `openldap.rs`, against
/// a server that allows the anonymous lookup.
#[test]
fn an_anonymous_lookup_the_directory_refuses_is_an_error() {
    let fake = fake::start();
    let json = format!(
        r#"{{"url":"{}","starttls":false,"base_dn":"{}","group_base":"ou=groups,{}"}}"#,
        fake.url(),
        fake::BASE,
        fake::BASE
    );
    let v: serde_json::Value = serde_json::from_str(&json).unwrap();
    let scope = Scope(rmpv::ext::to_value(v).unwrap());
    // The fake refuses an anonymous search, as many directories do, so the
    // user lookup fails before any password is tried.
    let e = call(
        &scope,
        "ldap/bind",
        r#"{"user":"alice","password":"alice-pw"}"#,
    )
    .unwrap_err();
    assert!(e.contains("search"), "{e}");
    assert!(!fake
        .log()
        .iter()
        .any(|l| l.contains("alice") && l.starts_with("bind")));
}

#[test]
fn groups_looks_a_user_up_without_a_password() {
    let fake = fake::start();
    let scope = scope_for(&fake, r#","group_base":"ou=groups,dc=example,dc=com""#);
    let r = call(&scope, "ldap/groups", r#"{"user":"alice"}"#).unwrap();
    assert_eq!(r["found"], true);
    assert_eq!(r["dn"], "uid=alice,ou=people,dc=example,dc=com");
    assert_eq!(r["groups"], serde_json::json!(["admins", "staff"]));
    assert!(r.get("attrs").is_none());
    let r = call(&scope, "ldap/groups", r#"{"user":"nobody"}"#).unwrap();
    assert_eq!(r, serde_json::json!({"found": false}));
    let r = call(&scope, "ldap/groups", r#"{"user":"twin"}"#).unwrap();
    assert_eq!(r, serde_json::json!({"found": false}));
}
