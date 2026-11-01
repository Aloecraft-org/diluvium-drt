//! `drt ldap`: the ldap connector's two calls from a terminal, against the
//! directory the config's `connectors.ldap` names.
//!
//! The same connector and the same scope a program gets, called the same
//! way: what `drt ldap search` prints is what `host.call("ldap/search")`
//! answers, and a scope this refuses is one `drt run` refuses too.
//!
//! ```text
//! drt --config drt.json ldap search '(uid=alice)' mail cn
//! drt --config drt.json ldap bind alice          # asks for the password
//! drt --config drt.json ldap groups alice
//! ```
//!
//! ## surface block
//!
//! - Entry points: [`Args`] and [`run`], the verb; [`connector`], the one
//!   wiring of a scope per process, which `cli::wire_connectors` uses too.
//! - Configurable values: none here; the scope is `connectors.ldap`.
//! - Fan-out: [`Action`], `search`, `bind` and `groups`, one per connector
//!   call.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, OnceLock};

use drt_config::secret::Echo;
use drt_config::RootConfig;
use drt_connector::Connector;
use drt_connector_ldap::LdapConnector;

#[derive(clap::Args, Debug)]
pub struct Args {
    #[command(subcommand)]
    pub action: Action,
    /// Print the connector's answer as JSON, exactly as a program gets it.
    #[arg(long, global = true)]
    pub json: bool,
}

#[derive(clap::Subcommand, Debug)]
pub enum Action {
    /// `ldap/search`: print the entries a filter matches, as LDIF-like
    /// `name: value` lines, one blank line between entries.
    Search {
        /// An RFC 4515 filter, `(uid=alice)`.
        filter: String,
        /// Attributes to return. None returns every user attribute.
        attrs: Vec<String>,
        /// Search from here, inside the scope's base_dn. Default: base_dn.
        #[arg(long)]
        base: Option<String>,
        /// `sub` (default), `one` or `base`.
        #[arg(long)]
        scope: Option<String>,
        /// At most this many entries (the connector's bound applies).
        #[arg(long)]
        limit: Option<usize>,
    },
    /// `ldap/bind`: check a user's password. Exit 0 and print the DN when
    /// it is right, exit 1 when it is not. The password is asked for on the
    /// terminal, blind, unless --password-stdin.
    Bind {
        /// The username `user_filter` looks up, not a DN.
        user: String,
        /// Read the password as one line of stdin, for a script.
        #[arg(long)]
        password_stdin: bool,
        /// Attributes of the user's entry to print with the DN.
        attrs: Vec<String>,
    },
    /// `ldap/groups`: print a user's DN and the groups `group_filter` finds
    /// for them, one `group:` line each. Exit 1 when no one user matches.
    Groups {
        /// The username `user_filter` looks up, not a DN.
        user: String,
    },
}

/// The connector for one ldap scope, wired once per process.
///
/// Once, because wiring resolves `bind_password`, and a `{"prompt": ..}`
/// one is asked on the terminal: `sshd` and the matchmaker wire a registry
/// per session, and a prompt per session would be a prompt nobody sees.
/// Keyed by the scope as written, so a second, different scope is its own.
pub fn connector(scope: Option<&drt_caps::Scope>) -> Result<Arc<LdapConnector>, String> {
    static WIRED: OnceLock<Mutex<BTreeMap<String, Arc<LdapConnector>>>> = OnceLock::new();
    let key = scope
        .map(|s| serde_json::to_string(&s.0).unwrap_or_default())
        .unwrap_or_default();
    let mut wired = WIRED.get_or_init(Default::default).lock().unwrap();
    if let Some(c) = wired.get(&key) {
        return Ok(c.clone());
    }
    let c = Arc::new(LdapConnector::wired(scope, &crate::prompt::terminal)?);
    wired.insert(key, c.clone());
    Ok(c)
}

/// Run the verb. Exit 0 on an answer, 1 on a refused bind, 2 on an error.
pub fn run(args: &Args, config: &RootConfig) -> Result<u8, String> {
    let wiring = config.connectors.get("ldap").ok_or(
        "the config wires no ldap connector; give --config a file with connectors.ldap.scope \
         ({url, base_dn, bind_dn, bind_password, ...})",
    )?;
    let scope = wiring.scope.as_ref();
    let connector = connector(scope)?;
    let (call, call_args) = match &args.action {
        Action::Search {
            filter,
            attrs,
            base,
            scope,
            limit,
        } => {
            let mut m = vec![("filter", rmpv::Value::from(filter.as_str()))];
            if !attrs.is_empty() {
                m.push(("attrs", strings(attrs)));
            }
            if let Some(base) = base {
                m.push(("base", base.as_str().into()));
            }
            if let Some(scope) = scope {
                m.push(("scope", scope.as_str().into()));
            }
            if let Some(limit) = limit {
                m.push(("limit", (*limit as u64).into()));
            }
            ("ldap/search", m)
        }
        Action::Bind {
            user,
            password_stdin,
            attrs,
        } => {
            let password = if *password_stdin {
                let mut line = String::new();
                std::io::stdin()
                    .read_line(&mut line)
                    .map_err(|e| format!("cannot read the password from stdin: {e}"))?;
                line.trim_end_matches(['\r', '\n']).to_string()
            } else {
                crate::prompt::terminal(&format!("password for {user}"), Echo::Hidden)?
            };
            let mut m = vec![
                ("user", rmpv::Value::from(user.as_str())),
                ("password", password.into()),
            ];
            if !attrs.is_empty() {
                m.push(("attrs", strings(attrs)));
            }
            ("ldap/bind", m)
        }
        Action::Groups { user } => ("ldap/groups", vec![("user", user.as_str().into())]),
    };
    let call_args = rmpv::Value::Map(m_into(call_args));
    let runtime = tokio::runtime::Runtime::new().map_err(|e| format!("a tokio runtime: {e}"))?;
    let answer = runtime.block_on(connector.call(call, Some(call_args), scope));
    // FM-1 (doc/Failure-Modes.md): leaked, not dropped, as every verb here does.
    std::mem::forget(runtime);
    let answer = answer.map_err(|e| e.detail)?;
    if args.json {
        println!(
            "{}",
            serde_json::to_string(&answer).map_err(|e| e.to_string())?
        );
    }
    // `ok` for a bind, `found` for a groups lookup: false is exit 1.
    let ok = get(&answer, "ok")
        .or_else(|| get(&answer, "found"))
        .and_then(|v| v.as_bool());
    if !args.json {
        print!("{}", human(call, &answer));
    }
    Ok(if ok == Some(false) { 1 } else { 0 })
}

// depth: printing

fn strings(values: &[String]) -> rmpv::Value {
    rmpv::Value::Array(values.iter().map(|s| s.as_str().into()).collect())
}

fn m_into(m: Vec<(&str, rmpv::Value)>) -> Vec<(rmpv::Value, rmpv::Value)> {
    m.into_iter().map(|(k, v)| (k.into(), v)).collect()
}

fn get<'a>(map: &'a rmpv::Value, key: &str) -> Option<&'a rmpv::Value> {
    map.as_map()?
        .iter()
        .find(|(k, _)| k.as_str() == Some(key))
        .map(|(_, v)| v)
}

/// `dn: ...` and `name: value` lines, the shape `ldapsearch` prints, so a
/// reader who knows one reads the other. A binary value prints as its
/// length, never as bytes on a terminal.
fn human(call: &str, answer: &rmpv::Value) -> String {
    let mut out = String::new();
    let entry = |out: &mut String, dn: &rmpv::Value, attrs: Option<&rmpv::Value>| {
        out.push_str(&format!("dn: {}\n", dn.as_str().unwrap_or("")));
        for (name, values) in attrs.and_then(|a| a.as_map()).into_iter().flatten() {
            for value in values.as_array().into_iter().flatten() {
                let shown = match value {
                    rmpv::Value::Binary(b) => format!("<{} bytes>", b.len()),
                    v => v.as_str().unwrap_or("").to_string(),
                };
                out.push_str(&format!("{}: {shown}\n", name.as_str().unwrap_or("")));
            }
        }
    };
    if call == "ldap/bind" || call == "ldap/groups" {
        match get(answer, "dn") {
            Some(dn) => entry(&mut out, dn, get(answer, "attrs")),
            None if call == "ldap/bind" => out.push_str("refused\n"),
            None => out.push_str("not found\n"),
        }
        for group in get(answer, "groups")
            .and_then(|g| g.as_array())
            .into_iter()
            .flatten()
        {
            out.push_str(&format!("group: {}\n", group.as_str().unwrap_or("")));
        }
        return out;
    }
    let entries = get(answer, "entries").and_then(|e| e.as_array());
    for (i, e) in entries.into_iter().flatten().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        if let Some(dn) = get(e, "dn") {
            entry(&mut out, dn, get(e, "attrs"));
        }
    }
    if get(answer, "truncated").and_then(|t| t.as_bool()) == Some(true) {
        out.push_str("# truncated: more entries matched than --limit\n");
    }
    out
}
