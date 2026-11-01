//! The ldap connector: `host:ldap/bind`, `host:ldap/groups` and
//! `host:ldap/search` against one directory (SPEC.md §7).
//!
//! `ssmtp`'s sibling in the way that matters: the scope names a place (one
//! directory server and one subtree of it) and carries a credential the
//! guest never holds. A program asks "is this the right password for
//! alice?" or "what is alice's mail?" and never sees the service account's
//! password, never names another server, and never reads outside the base
//! DN the deployment granted.
//!
//! ```text
//! ldap/search {filter, base?, scope?, attrs?, limit?}
//!     -> {entries = [{dn, attrs = {name = {value, ...}}}], truncated}
//! ldap/bind   {user, password, attrs?}
//!     -> {ok = true, dn, attrs, groups, group_dns} | {ok = false}
//!        (`attrs` holds only what the call named; none named, none)
//! ldap/groups {user}
//!     -> {found = true, dn, groups, group_dns} | {found = false}
//! ```
//!
//! `ldap/bind` is the one a login form wants. It finds the user's entry with
//! the scope's `user_filter` (the username escaped, RFC 4515), as the
//! service account, then binds as that entry with the given password. A
//! wrong password, an unknown user and an ambiguous one all answer
//! `{ok = false}` and nothing else, so a caller cannot use the answer to
//! learn which users exist.
//!
//! Groups are first-class because a login is rarely only a yes: a session
//! wants to know what the user is a member of. After a successful bind the
//! connector searches `group_filter` (the user's DN escaped into `{dn}`)
//! under `group_base`, and answers each group's `cn` in `groups` and its DN
//! in `group_dns`, sorted. The group search runs as the service account
//! when the scope has one, and as the user who just bound when it does not.
//! `ldap/groups` is the same lookup without a password, for a session that
//! re-reads a user's groups, and is its own capability so a program can be
//! granted logins without being granted a directory of who is in what.
//!
//! The client is the `ldap3` crate, over rustls with webpki's roots plus the
//! scope's `extra_roots`, the same trust `rest` and `ssmtp` use.
//!
//! ## surface block
//!
//! - Entry points: [`LdapConnector`], built by [`LdapConnector::wired`],
//!   which resolves the bind password once; [`LdapConnector::call`] answers
//!   the three verbs in [`VERBS`].
//! - Configurable values: [`LdapScope`], the wiring (url, base_dn, bind_dn,
//!   bind_password, starttls, extra_roots, user_filter, group_filter,
//!   group_base, timeout_ms); and the bounds [`MAX_ENTRIES`],
//!   [`DEFAULT_LIMIT`], [`MAX_FILTER_BYTES`], [`MAX_ATTRS`],
//!   [`DEFAULT_TIMEOUT_MS`], [`DEFAULT_USER_FILTER`],
//!   [`DEFAULT_GROUP_FILTER`].
//! - Fan-out: [`VERBS`] and the match in `call` that dispatches them;
//!   [`Transport`], the three ways a connection is made (ldaps, StartTLS,
//!   plaintext to loopback only).
//!
//! ## The things that make this dangerous, and what is done about them
//!
//! 1. **Unauthenticated bind.** RFC 4513 §5.1.2: a simple bind with a DN
//!    and an *empty* password is an anonymous bind, and most servers answer
//!    it with success. A login check that forwarded an empty password would
//!    let anyone in as anyone. An empty password answers `{ok = false}`
//!    without contacting the server.
//! 2. **Filter injection.** The username goes into `user_filter` escaped
//!    with `ldap3::ldap_escape`, so `*` or `)(uid=*` is a literal name and
//!    not a wildcard. `ldap/search`'s filter is the program's own and is
//!    passed as written: the bound on it is the base DN, not the filter.
//! 3. **Credentials in the clear.** A bind sends a password, the service
//!    account's or the user's. `ldap://` with `starttls: false` is refused
//!    at startup unless the host is loopback, where nothing crosses a
//!    network (a local LLDAP on 3890 is the case it allows).
//! 4. **Reading outside the grant.** `base` must be the scope's `base_dn`
//!    or under it, compared case-insensitively with the spaces around `,`
//!    and `=` ignored. Referrals are not followed.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use serde::Deserialize;

use drt_caps::{Scope, ScopeType};
use drt_config::secret::Prompter;
use drt_config::Secret;
use drt_connector::{CallError, CallResult, Connector};
use ldap3::{LdapConnAsync, LdapConnSettings, SearchEntry, SearchOptions};

/// The calls this family answers. A call outside it is an error naming it.
pub const VERBS: &[&str] = &["ldap/bind", "ldap/groups", "ldap/search"];
/// The most entries one `ldap/search` returns. A program that wants more
/// pages with a narrower filter; the bound is what keeps one call's reply
/// a size a guest can hold.
pub const MAX_ENTRIES: usize = 1000;
/// Entries returned when the call names no `limit`.
pub const DEFAULT_LIMIT: usize = 100;
/// A filter longer than this is a mistake, not a long filter.
pub const MAX_FILTER_BYTES: usize = 4096;
/// Attributes one call may name.
pub const MAX_ATTRS: usize = 64;
/// One call, connection to unbind, including the TLS handshake.
pub const DEFAULT_TIMEOUT_MS: u64 = 10_000;
/// LLDAP's and OpenLDAP's `inetOrgPerson` spelling. Active Directory wants
/// `(sAMAccountName={user})`, which the scope sets.
pub const DEFAULT_USER_FILTER: &str = "(uid={user})";
/// `groupOfNames` (OpenLDAP, LLDAP) and `groupOfUniqueNames` both. Active
/// Directory's nested groups want
/// `(member:1.2.840.113556.1.4.1941:={dn})`, which the scope sets.
pub const DEFAULT_GROUP_FILTER: &str = "(|(member={dn})(uniqueMember={dn}))";
/// The result code a server answers a wrong password with (RFC 4511 §A.2).
const INVALID_CREDENTIALS: u32 = 49;
/// The result code for "more entries matched than the size limit".
const SIZE_LIMIT_EXCEEDED: u32 = 4;
/// The result code for a search base that names nothing.
const NO_SUCH_OBJECT: u32 = 32;

/// The place this connector is wired to: which directory, which subtree of
/// it, and as whom the connector searches.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LdapScope {
    /// `ldaps://host[:port]` or `ldap://host[:port]`. Nothing after the
    /// port: the base DN is its own key.
    pub url: String,
    /// The subtree every call stays inside.
    pub base_dn: String,
    /// The service account searches run as. Absent, they run anonymously,
    /// which many directories refuse.
    #[serde(default)]
    pub bind_dn: Option<String>,
    /// The service account's password: a string, or `{"env"}`, `{"file"}`
    /// or `{"prompt"}` (`drt_config::Secret`). Resolved once, when the
    /// connector is wired, and held by the host; the guest never sees it.
    #[serde(default)]
    pub bind_password: Option<Secret>,
    /// For `ldap://`: upgrade with StartTLS before anything else. Default
    /// true. `false` is plaintext and is refused unless the host is
    /// loopback. Meaningless for `ldaps://`, and refused there.
    #[serde(default)]
    pub starttls: Option<bool>,
    /// PEM files trusted beside webpki's roots: an internal CA, typically.
    /// Added, never substituted, as `rest`'s `extra_roots` are.
    #[serde(default)]
    pub extra_roots: Vec<PathBuf>,
    /// How `ldap/bind` finds a user's entry. Must contain `{user}`.
    #[serde(default)]
    pub user_filter: Option<String>,
    /// How a user's groups are found. Must contain `{dn}`, the user's DN.
    #[serde(default)]
    pub group_filter: Option<String>,
    /// Where groups are searched: `base_dn` or under it. Default: base_dn.
    #[serde(default)]
    pub group_base: Option<String>,
    #[serde(default)]
    pub timeout_ms: Option<u64>,
}

/// How a connection is made, decided once from the url and `starttls`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transport {
    /// `ldaps://`: TLS from the first byte.
    Ldaps,
    /// `ldap://` upgraded with StartTLS before any bind.
    StartTls,
    /// `ldap://` to a loopback host with `starttls: false`.
    PlainLoopback,
}

impl LdapScope {
    /// Parse and judge a scope, without resolving the password.
    pub fn parse(scope: Option<&Scope>) -> Result<LdapScope, String> {
        let Some(Scope(value)) = scope else {
            return Err("scope is required: {url, base_dn, ...}".into());
        };
        // Through JSON rather than `rmpv::ext`: a scope is written as JSON,
        // and `Secret`'s `{"prompt": "hidden"}` is an enum spelled as a
        // string, which `rmpv::ext` reads only as an index.
        let parsed: LdapScope = serde_json::to_value(value)
            .and_then(serde_json::from_value)
            .map_err(|e| format!("scope does not parse: {e}"))?;
        parsed.validate()?;
        Ok(parsed)
    }

    fn validate(&self) -> Result<(), String> {
        self.transport()?;
        if self.base_dn.trim().is_empty() {
            return Err("scope.base_dn is empty; name the subtree calls stay inside".into());
        }
        if self.bind_dn.is_some() != self.bind_password.is_some() {
            return Err(
                "scope names one of bind_dn/bind_password; a service bind needs both, and \
                 an anonymous one neither"
                    .into(),
            );
        }
        let filter = self.user_filter();
        if !filter.contains("{user}") {
            return Err(format!(
                "scope.user_filter {filter:?} has no {{user}}, so it would find the same \
                 entry whoever logs in"
            ));
        }
        if ldap3::parse_filter(filter.replace("{user}", "x")).is_err() {
            return Err(format!(
                "scope.user_filter {filter:?} is not an LDAP filter"
            ));
        }
        let filter = self.group_filter();
        if !filter.contains("{dn}") {
            return Err(format!(
                "scope.group_filter {filter:?} has no {{dn}}, so it would find the same \
                 groups for everyone"
            ));
        }
        if ldap3::parse_filter(filter.replace("{dn}", "x")).is_err() {
            return Err(format!(
                "scope.group_filter {filter:?} is not an LDAP filter"
            ));
        }
        if !self.contains(self.group_base()) {
            return Err(format!(
                "scope.group_base '{}' is outside scope.base_dn '{}'",
                self.group_base(),
                self.base_dn
            ));
        }
        if self.timeout_ms == Some(0) {
            return Err("scope.timeout_ms of 0 would refuse every call".into());
        }
        load_roots(&self.extra_roots)?;
        Ok(())
    }

    /// How this scope connects, or why it may not.
    pub fn transport(&self) -> Result<Transport, String> {
        let (scheme, rest) = self
            .url
            .split_once("://")
            .ok_or_else(|| format!("scope.url {:?} is not ldap:// or ldaps://", self.url))?;
        let authority = rest.trim_end_matches('/');
        if authority.is_empty() || authority.contains('/') || authority.contains('?') {
            return Err(format!(
                "scope.url {:?} must be scheme://host[:port] and nothing more; the base DN \
                 is scope.base_dn",
                self.url
            ));
        }
        match (scheme.to_ascii_lowercase().as_str(), self.starttls) {
            ("ldaps", Some(_)) => Err(
                "scope.starttls is for ldap:// only; ldaps:// is TLS from the first byte".into(),
            ),
            ("ldaps", None) => Ok(Transport::Ldaps),
            ("ldap", None | Some(true)) => Ok(Transport::StartTls),
            ("ldap", Some(false)) if is_loopback(authority) => Ok(Transport::PlainLoopback),
            ("ldap", Some(false)) => Err(format!(
                "scope.url {:?} with starttls false would send passwords in the clear over a \
                 network; use ldaps://, leave starttls on, or reach the server on loopback",
                self.url
            )),
            _ => Err(format!(
                "scope.url {:?} is not ldap:// or ldaps://",
                self.url
            )),
        }
    }

    pub fn user_filter(&self) -> &str {
        self.user_filter.as_deref().unwrap_or(DEFAULT_USER_FILTER)
    }

    pub fn group_filter(&self) -> &str {
        self.group_filter.as_deref().unwrap_or(DEFAULT_GROUP_FILTER)
    }

    pub fn group_base(&self) -> &str {
        self.group_base.as_deref().unwrap_or(&self.base_dn)
    }

    fn timeout(&self) -> Duration {
        Duration::from_millis(self.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS))
    }

    /// Whether `dn` is the base DN or under it.
    pub fn contains(&self, dn: &str) -> bool {
        let base = normalize_dn(&self.base_dn);
        let dn = normalize_dn(dn);
        dn == base || dn.ends_with(&format!(",{base}"))
    }
}

struct LdapScopeType;

impl ScopeType for LdapScopeType {
    fn describe(&self) -> &str {
        "{url, base_dn, bind_dn?, bind_password?, starttls?, extra_roots?, user_filter?, \
         group_filter?, group_base?, timeout_ms?}"
    }

    fn validate(&self, scope: Option<&Scope>) -> Result<(), String> {
        LdapScope::parse(scope).map(|_| ())
    }
}

/// The connector. Holds the service password, resolved when it was wired.
pub struct LdapConnector {
    bind_password: Option<String>,
}

impl LdapConnector {
    /// Judge `scope` and resolve its `bind_password` now, with `prompt` for
    /// a `{"prompt": ..}` value. Called once, at startup: a missing variable
    /// or file is a refusal by name there, and a prompt is asked once rather
    /// than on every call.
    pub fn wired(scope: Option<&Scope>, prompt: Prompter<'_>) -> Result<LdapConnector, String> {
        let parsed = LdapScope::parse(scope)?;
        let bind_password = match &parsed.bind_password {
            Some(secret) => Some(secret.resolve("connectors.ldap.scope.bind_password", prompt)?),
            None => None,
        };
        Ok(LdapConnector { bind_password })
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SearchArgs {
    filter: String,
    #[serde(default)]
    base: Option<String>,
    /// `sub` (the default), `one` or `base`.
    #[serde(default)]
    scope: Option<String>,
    #[serde(default)]
    attrs: Option<Vec<String>>,
    #[serde(default)]
    limit: Option<usize>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct BindArgs {
    user: String,
    password: String,
    #[serde(default)]
    attrs: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct GroupsArgs {
    user: String,
}

#[async_trait::async_trait]
impl Connector for LdapConnector {
    fn scope_type(&self) -> Box<dyn ScopeType> {
        Box::new(LdapScopeType)
    }

    async fn call(
        &self,
        call: &str,
        args: Option<rmpv::Value>,
        scope: Option<&Scope>,
    ) -> CallResult {
        let scope = LdapScope::parse(scope).map_err(CallError::new)?;
        let args = args.ok_or_else(|| CallError::new(format!("{call} needs arguments")))?;
        let timeout = scope.timeout();
        let work = async {
            match call {
                "ldap/search" => {
                    let args: SearchArgs = rmpv::ext::from_value(args)
                        .map_err(|e| format!("arguments do not parse: {e}"))?;
                    search(&scope, self.bind_password.as_deref(), args).await
                }
                "ldap/bind" => {
                    let args: BindArgs = rmpv::ext::from_value(args)
                        .map_err(|e| format!("arguments do not parse: {e}"))?;
                    bind(&scope, self.bind_password.as_deref(), args).await
                }
                "ldap/groups" => {
                    let args: GroupsArgs = rmpv::ext::from_value(args)
                        .map_err(|e| format!("arguments do not parse: {e}"))?;
                    groups(&scope, self.bind_password.as_deref(), args).await
                }
                other => Err(format!(
                    "'{other}' is not an ldap call; the calls are {}",
                    VERBS.join(", ")
                )),
            }
        };
        let work = async { tokio::time::timeout(timeout, work).await };
        // FM-3: `drt start` has a reactor, `drt run` does not.
        let outcome = match tokio::runtime::Handle::try_current() {
            Ok(_) => work.await,
            Err(_) => own_runtime().block_on(work),
        };
        outcome
            .map_err(|_| {
                CallError::timed_out(format!("{call} timed out after {}ms", timeout.as_millis()))
            })?
            .map_err(CallError::new)
    }
}

// depth: the two verbs

async fn search(
    scope: &LdapScope,
    password: Option<&str>,
    args: SearchArgs,
) -> Result<rmpv::Value, String> {
    if args.filter.len() > MAX_FILTER_BYTES {
        return Err(format!(
            "filter is {} bytes; {MAX_FILTER_BYTES} is the bound",
            args.filter.len()
        ));
    }
    // Refused here, by name, rather than as a protocol error from the
    // server after a connection and a bind.
    ldap3::parse_filter(&args.filter)
        .map_err(|_| format!("filter {:?} is not an LDAP filter", args.filter))?;
    let base = args.base.unwrap_or_else(|| scope.base_dn.clone());
    if !scope.contains(&base) {
        return Err(format!(
            "base '{base}' is outside this instance's granted subtree '{}'",
            scope.base_dn
        ));
    }
    let search_scope = match args.scope.as_deref() {
        None | Some("sub") => ldap3::Scope::Subtree,
        Some("one") => ldap3::Scope::OneLevel,
        Some("base") => ldap3::Scope::Base,
        Some(other) => return Err(format!("scope {other:?} is not sub, one or base")),
    };
    let attrs = attrs_of(args.attrs)?;
    let limit = args.limit.unwrap_or(DEFAULT_LIMIT);
    if limit == 0 || limit > MAX_ENTRIES {
        return Err(format!("limit {limit} is outside 1..={MAX_ENTRIES}"));
    }

    let mut ldap = connect(scope).await?;
    service_bind(&mut ldap, scope, password).await?;
    let (entries, truncated) =
        run_search(&mut ldap, &base, search_scope, &args.filter, &attrs, limit).await?;
    let _ = ldap.unbind().await;
    Ok(rmpv::Value::Map(vec![
        (
            "entries".into(),
            rmpv::Value::Array(entries.into_iter().map(entry_value).collect()),
        ),
        ("truncated".into(), rmpv::Value::Boolean(truncated)),
    ]))
}

async fn bind(
    scope: &LdapScope,
    password: Option<&str>,
    args: BindArgs,
) -> Result<rmpv::Value, String> {
    let refused = || {
        Ok(rmpv::Value::Map(vec![(
            "ok".into(),
            rmpv::Value::Boolean(false),
        )]))
    };
    // Danger 1: an empty password is an anonymous bind, which succeeds.
    if args.password.is_empty() || args.user.is_empty() {
        return refused();
    }
    // None named is none returned: a login check that answered the whole
    // entry would hand a program attributes it never asked for.
    let attrs = match args.attrs {
        Some(a) => attrs_of(Some(a))?,
        None => vec!["1.1".into()],
    };

    let mut ldap = connect(scope).await?;
    service_bind(&mut ldap, scope, password).await?;
    let Some(entry) = find_user(&mut ldap, scope, &args.user, &attrs).await? else {
        let _ = ldap.unbind().await;
        return refused();
    };
    let result = ldap
        .simple_bind(&entry.dn, &args.password)
        .await
        .map_err(|e| format!("bind: {e}"))?;
    match result.rc {
        0 => {}
        INVALID_CREDENTIALS => {
            let _ = ldap.unbind().await;
            return refused();
        }
        rc => return Err(format!("bind: the server answered {rc} ({})", result.text)),
    }
    // Groups as the service account when there is one, since a directory
    // need not let a user read groups; as the user who just bound otherwise.
    if scope.bind_dn.is_some() {
        service_bind(&mut ldap, scope, password).await?;
    }
    let (names, dns) = find_groups(&mut ldap, scope, &entry.dn).await?;
    let _ = ldap.unbind().await;
    Ok(rmpv::Value::Map(vec![
        ("ok".into(), rmpv::Value::Boolean(true)),
        ("dn".into(), entry.dn.as_str().into()),
        ("attrs".into(), attrs_value(&entry)),
        ("groups".into(), names),
        ("group_dns".into(), dns),
    ]))
}

async fn groups(
    scope: &LdapScope,
    password: Option<&str>,
    args: GroupsArgs,
) -> Result<rmpv::Value, String> {
    let absent = || {
        Ok(rmpv::Value::Map(vec![(
            "found".into(),
            rmpv::Value::Boolean(false),
        )]))
    };
    if args.user.is_empty() {
        return absent();
    }
    let mut ldap = connect(scope).await?;
    service_bind(&mut ldap, scope, password).await?;
    // `1.1` asks for no attributes (RFC 4511 §4.5.1.8): only the DN is wanted.
    let Some(entry) = find_user(&mut ldap, scope, &args.user, &["1.1".into()]).await? else {
        let _ = ldap.unbind().await;
        return absent();
    };
    let (names, dns) = find_groups(&mut ldap, scope, &entry.dn).await?;
    let _ = ldap.unbind().await;
    Ok(rmpv::Value::Map(vec![
        ("found".into(), rmpv::Value::Boolean(true)),
        ("dn".into(), entry.dn.as_str().into()),
        ("groups".into(), names),
        ("group_dns".into(), dns),
    ]))
}

/// The one entry `user_filter` finds for a username, or none when it finds
/// none or several. Two are asked for, so an ambiguous filter is seen as
/// ambiguous rather than as the first of several people.
async fn find_user(
    ldap: &mut ldap3::Ldap,
    scope: &LdapScope,
    user: &str,
    attrs: &[String],
) -> Result<Option<SearchEntry>, String> {
    // Danger 2: the username is a value, never filter syntax.
    let filter = scope
        .user_filter()
        .replace("{user}", &ldap3::ldap_escape(user));
    let (mut found, _) = run_search(
        ldap,
        &scope.base_dn,
        ldap3::Scope::Subtree,
        &filter,
        attrs,
        2,
    )
    .await?;
    Ok(if found.len() == 1 {
        Some(found.remove(0))
    } else {
        None
    })
}

/// The groups `group_filter` finds for a DN: their `cn`s and their DNs,
/// each sorted. A group with no `cn` is named by its DN's first value.
async fn find_groups(
    ldap: &mut ldap3::Ldap,
    scope: &LdapScope,
    dn: &str,
) -> Result<(rmpv::Value, rmpv::Value), String> {
    let filter = scope
        .group_filter()
        .replace("{dn}", &ldap3::ldap_escape(dn));
    let (found, _) = run_search(
        ldap,
        scope.group_base(),
        ldap3::Scope::Subtree,
        &filter,
        &["cn".into()],
        MAX_ENTRIES,
    )
    .await?;
    let mut names: Vec<String> = found
        .iter()
        .map(|g| {
            g.attrs
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case("cn"))
                .and_then(|(_, v)| v.first().cloned())
                .unwrap_or_else(|| first_rdn_value(&g.dn))
        })
        .collect();
    let mut dns: Vec<String> = found.into_iter().map(|g| g.dn).collect();
    names.sort();
    dns.sort();
    let list = |v: Vec<String>| rmpv::Value::Array(v.into_iter().map(Into::into).collect());
    Ok((list(names), list(dns)))
}

// depth: connections, TLS and the search loop

async fn connect(scope: &LdapScope) -> Result<ldap3::Ldap, String> {
    let transport = scope.transport()?;
    let mut settings = LdapConnSettings::new().set_conn_timeout(scope.timeout());
    if transport != Transport::PlainLoopback {
        settings = settings.set_config(tls_config(&scope.extra_roots)?);
    }
    if transport == Transport::StartTls {
        settings = settings.set_starttls(true);
    }
    let (conn, ldap) = LdapConnAsync::with_settings(settings, &scope.url)
        .await
        .map_err(|e| format!("connect to {}: {e}", scope.url))?;
    ldap3::drive!(conn);
    Ok(ldap)
}

async fn service_bind(
    ldap: &mut ldap3::Ldap,
    scope: &LdapScope,
    password: Option<&str>,
) -> Result<(), String> {
    let Some(dn) = &scope.bind_dn else {
        return Ok(());
    };
    let password = password.ok_or("the service password was not resolved when ldap was wired")?;
    let result = ldap
        .simple_bind(dn, password)
        .await
        .map_err(|e| format!("service bind: {e}"))?;
    match result.rc {
        0 => Ok(()),
        // Named as the operator's problem: the program did nothing wrong.
        INVALID_CREDENTIALS => Err(format!(
            "the service bind as '{dn}' was refused: invalid credentials in connectors.ldap"
        )),
        rc => Err(format!(
            "service bind: the server answered {rc} ({})",
            result.text
        )),
    }
}

async fn run_search(
    ldap: &mut ldap3::Ldap,
    base: &str,
    scope: ldap3::Scope,
    filter: &str,
    attrs: &[String],
    limit: usize,
) -> Result<(Vec<SearchEntry>, bool), String> {
    // One past the limit, so a full page and an overfull one differ.
    let sizelimit = i32::try_from(limit + 1).unwrap_or(i32::MAX);
    ldap.with_search_options(SearchOptions::new().sizelimit(sizelimit));
    let mut stream = ldap
        .streaming_search(base, scope, filter, attrs.to_vec())
        .await
        .map_err(|e| format!("search: {e}"))?;
    let mut entries = Vec::new();
    let mut truncated = false;
    while let Some(entry) = stream.next().await.map_err(|e| format!("search: {e}"))? {
        if entries.len() == limit {
            truncated = true;
            break;
        }
        entries.push(SearchEntry::construct(entry));
    }
    if !truncated {
        let done = stream.finish().await;
        match done.rc {
            0 => {}
            SIZE_LIMIT_EXCEEDED => truncated = true,
            // No such object: the base names nothing. An empty answer, the
            // way a filter that matches nothing is.
            NO_SUCH_OBJECT => {}
            rc => return Err(format!("search: the server answered {rc} ({})", done.text)),
        }
    }
    Ok((entries, truncated))
}

fn tls_config(extra_roots: &[PathBuf]) -> Result<Arc<rustls::ClientConfig>, String> {
    let mut roots = rustls::RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    for cert in load_roots(extra_roots)? {
        roots.add(cert).map_err(|e| format!("extra_roots: {e}"))?;
    }
    let config = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .map_err(|e| format!("tls: {e}"))?
    .with_root_certificates(roots)
    .with_no_client_auth();
    Ok(Arc::new(config))
}

/// The PEM files `extra_roots` names, each of which must hold at least one
/// certificate webpki can use: a wrong path or a key file handed over by
/// mistake is a refusal by name at startup. `rest`'s rule.
fn load_roots(
    paths: &[PathBuf],
) -> Result<Vec<rustls::pki_types::CertificateDer<'static>>, String> {
    use rustls::pki_types::{pem::PemObject, CertificateDer};
    let mut out = Vec::new();
    for path in paths {
        let name = path.display();
        let certs: Vec<CertificateDer<'static>> = CertificateDer::pem_file_iter(path)
            .map_err(|e| format!("extra_roots '{name}': {e}"))?
            .collect::<Result<_, _>>()
            .map_err(|e| format!("extra_roots '{name}': {e}"))?;
        if certs.is_empty() {
            return Err(format!("extra_roots '{name}': no certificate in it"));
        }
        let mut probe = rustls::RootCertStore::empty();
        let (_, ignored) = probe.add_parsable_certificates(certs.iter().cloned());
        if ignored > 0 {
            return Err(format!(
                "extra_roots '{name}': {ignored} certificate(s) are not usable as trust anchors"
            ));
        }
        out.extend(certs);
    }
    Ok(out)
}

// depth: small helpers

/// The attributes a call asked for. None asks for every user attribute,
/// which is LDAP's own default (`*`).
fn attrs_of(attrs: Option<Vec<String>>) -> Result<Vec<String>, String> {
    let attrs = attrs.unwrap_or_else(|| vec!["*".into()]);
    if attrs.len() > MAX_ATTRS {
        return Err(format!(
            "{} attrs named; {MAX_ATTRS} is the bound",
            attrs.len()
        ));
    }
    Ok(attrs)
}

fn entry_value(entry: SearchEntry) -> rmpv::Value {
    rmpv::Value::Map(vec![
        ("dn".into(), entry.dn.as_str().into()),
        ("attrs".into(), attrs_value(&entry)),
    ])
}

/// `{name = {value, ...}}`, names in order so a reply is the same bytes
/// for the same directory. A value that is not UTF-8 (a `jpegPhoto`, an
/// `objectGUID`) arrives as binary rather than as a mangled string.
fn attrs_value(entry: &SearchEntry) -> rmpv::Value {
    let mut out: BTreeMap<&str, Vec<rmpv::Value>> = BTreeMap::new();
    for (name, values) in &entry.attrs {
        out.entry(name)
            .or_default()
            .extend(values.iter().map(|v| v.as_str().into()));
    }
    for (name, values) in &entry.bin_attrs {
        out.entry(name)
            .or_default()
            .extend(values.iter().map(|v| rmpv::Value::Binary(v.clone())));
    }
    rmpv::Value::Map(
        out.into_iter()
            .map(|(k, v)| (k.into(), rmpv::Value::Array(v)))
            .collect(),
    )
}

/// Lowercased, with the spaces around `,` and `=` dropped: enough to
/// compare the DNs an operator and a program write, which differ in case
/// and spacing and not in escaping.
fn normalize_dn(dn: &str) -> String {
    let mut out = String::with_capacity(dn.len());
    for part in dn.split(',') {
        if !out.is_empty() {
            out.push(',');
        }
        let (k, v) = part.split_once('=').unwrap_or((part, ""));
        out.push_str(&k.trim().to_ascii_lowercase());
        out.push('=');
        out.push_str(&v.trim().to_ascii_lowercase());
    }
    out
}

/// `admins` from `cn=admins,ou=groups,dc=example,dc=com`.
fn first_rdn_value(dn: &str) -> String {
    let first = dn.split(',').next().unwrap_or(dn);
    first
        .split_once('=')
        .map_or(first, |(_, v)| v)
        .trim()
        .to_string()
}

/// Whether `host[:port]` names this machine.
fn is_loopback(authority: &str) -> bool {
    let host = match authority.strip_prefix('[') {
        Some(v6) => v6.split(']').next().unwrap_or(""),
        None => authority.rsplit_once(':').map_or(authority, |(h, _)| h),
    };
    host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

/// One process-wide runtime for callers that have none, leaked rather than
/// dropped (FM-1, FM-3 in doc/Failure-Modes.md), as `ssmtp`'s is.
fn own_runtime() -> &'static tokio::runtime::Runtime {
    static RT: std::sync::OnceLock<tokio::runtime::Runtime> = std::sync::OnceLock::new();
    RT.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .expect("the ldap connector could not build its own runtime")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scope(json: &str) -> Scope {
        let v: serde_json::Value = serde_json::from_str(json).unwrap();
        Scope(rmpv::ext::to_value(v).unwrap())
    }

    fn judge(json: &str) -> Result<LdapScope, String> {
        LdapScope::parse(Some(&scope(json)))
    }

    #[test]
    fn transport_follows_the_url_and_refuses_plaintext_off_loopback() {
        let t = |j: &str| judge(j).and_then(|s| s.transport());
        assert_eq!(
            t(r#"{"url":"ldaps://ldap.example.com","base_dn":"dc=x"}"#),
            Ok(Transport::Ldaps)
        );
        assert_eq!(
            t(r#"{"url":"ldap://ldap.example.com:389","base_dn":"dc=x"}"#),
            Ok(Transport::StartTls)
        );
        assert_eq!(
            t(r#"{"url":"ldap://127.0.0.1:3890","base_dn":"dc=x","starttls":false}"#),
            Ok(Transport::PlainLoopback)
        );
        assert_eq!(
            t(r#"{"url":"ldap://[::1]:3890","base_dn":"dc=x","starttls":false}"#),
            Ok(Transport::PlainLoopback)
        );
        assert_eq!(
            t(r#"{"url":"ldap://localhost","base_dn":"dc=x","starttls":false}"#),
            Ok(Transport::PlainLoopback)
        );
        let e = t(r#"{"url":"ldap://ldap.example.com","base_dn":"dc=x","starttls":false}"#)
            .unwrap_err();
        assert!(e.contains("in the clear"), "{e}");
        assert!(t(r#"{"url":"ldaps://h","base_dn":"dc=x","starttls":true}"#).is_err());
        assert!(t(r#"{"url":"http://h","base_dn":"dc=x"}"#).is_err());
        assert!(t(r#"{"url":"ldap://h/dc=x","base_dn":"dc=x"}"#).is_err());
    }

    #[test]
    fn a_scope_is_judged_whole_at_startup() {
        assert!(judge(r#"{"url":"ldaps://h","base_dn":""}"#).is_err());
        let e = judge(r#"{"url":"ldaps://h","base_dn":"dc=x","bind_dn":"cn=svc"}"#).unwrap_err();
        assert!(e.contains("bind_password"), "{e}");
        let e = judge(r#"{"url":"ldaps://h","base_dn":"dc=x","user_filter":"(uid=alice)"}"#)
            .unwrap_err();
        assert!(e.contains("{user}"), "{e}");
        assert!(
            judge(r#"{"url":"ldaps://h","base_dn":"dc=x","user_filter":"uid={user}("}"#).is_err()
        );
        assert!(
            judge(r#"{"url":"ldaps://h","base_dn":"dc=x","extra_roots":["/nope.pem"]}"#).is_err()
        );
        assert!(judge(r#"{"url":"ldaps://h","base_dn":"dc=x","bogus":1}"#).is_err());
        let ok = judge(
            r#"{"url":"ldaps://h","base_dn":"dc=x","bind_dn":"cn=svc,dc=x",
                "bind_password":{"env":"LDAP_PW"},"user_filter":"(sAMAccountName={user})"}"#,
        )
        .unwrap();
        assert_eq!(ok.user_filter(), "(sAMAccountName={user})");
        assert_eq!(ok.group_filter(), DEFAULT_GROUP_FILTER);
        assert_eq!(ok.group_base(), "dc=x");
        let e = judge(r#"{"url":"ldaps://h","base_dn":"dc=x","group_filter":"(member=x)"}"#)
            .unwrap_err();
        assert!(e.contains("{dn}"), "{e}");
        let e = judge(r#"{"url":"ldaps://h","base_dn":"dc=x","group_base":"dc=y"}"#).unwrap_err();
        assert!(e.contains("group_base"), "{e}");
    }

    #[test]
    fn the_password_is_resolved_once_when_wired_and_named_when_it_cannot_be() {
        let no_terminal = |what: &str, _| Err(format!("{what}: no terminal"));
        let s = scope(
            r#"{"url":"ldaps://h","base_dn":"dc=x","bind_dn":"cn=svc,dc=x",
                "bind_password":{"env":"DRT_LDAP_TEST_UNSET_VARIABLE"}}"#,
        );
        let e = LdapConnector::wired(Some(&s), &no_terminal).err().unwrap();
        assert!(
            e.contains("connectors.ldap.scope.bind_password")
                && e.contains("DRT_LDAP_TEST_UNSET_VARIABLE"),
            "{e}"
        );
        let s = scope(
            r#"{"url":"ldaps://h","base_dn":"dc=x","bind_dn":"cn=svc,dc=x",
                "bind_password":{"prompt":"hidden"}}"#,
        );
        let asked = |what: &str, _| Ok(format!("typed for {what}"));
        let c = LdapConnector::wired(Some(&s), &asked).unwrap();
        assert_eq!(
            c.bind_password.as_deref(),
            Some("typed for connectors.ldap.scope.bind_password")
        );
    }

    #[test]
    fn base_must_be_the_granted_subtree_or_under_it() {
        let s = judge(r#"{"url":"ldaps://h","base_dn":"dc=Example, dc=com"}"#).unwrap();
        assert!(s.contains("dc=example,dc=com"));
        assert!(s.contains("ou=people, DC=example,dc=com"));
        assert!(!s.contains("dc=com"));
        assert!(!s.contains("dc=notexample,dc=com"));
        assert!(!s.contains("ou=people,dc=example,dc=com,dc=evil"));
    }
}
