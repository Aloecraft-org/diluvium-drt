//! A small LDAP server for tests: one directory under `dc=example,dc=com`,
//! simple bind, and search with equality, presence, and/or/not filters.
//!
//! Shared by this crate's tests and `crates/drt/tests/ldap.rs` (included by
//! `#[path]`), so the connector and `drt ldap` are tested against the same
//! directory.
//!
//! It answers a bind with a DN and an empty password with success, as RFC
//! 4513 §5.1.2's unauthenticated bind and most real servers do. That is the
//! behaviour the connector must not be fooled by, so the fake has it.
//!
//! ## surface block
//!
//! - Entry points: [`start`], which serves on a loopback port from its own
//!   thread and answers a [`Fake`] holding the port and the log.
//! - Configurable values: [`BASE`], [`SERVICE_DN`], [`SERVICE_PW`], and the
//!   people in [`directory`].
//! - Fan-out: the match on `LdapOp` in `serve`.
#![allow(dead_code)]

use std::sync::{Arc, Mutex};

use futures_util::{SinkExt, StreamExt};
use ldap3_proto::proto::{
    LdapBindCred, LdapBindResponse, LdapFilter, LdapMsg, LdapOp, LdapPartialAttribute, LdapResult,
    LdapResultCode, LdapSearchResultEntry, LdapSearchScope,
};
use ldap3_proto::LdapCodec;
use tokio_util::codec::Framed;

pub const BASE: &str = "dc=example,dc=com";
pub const SERVICE_DN: &str = "cn=svc,ou=services,dc=example,dc=com";
pub const SERVICE_PW: &str = "service-secret";

pub struct Person {
    pub dn: &'static str,
    pub password: &'static str,
    pub attrs: &'static [(&'static str, &'static [&'static str])],
}

/// Alice and Bob, two people sharing `uid=twin` so a filter can be
/// ambiguous, and two groups: `admins` (Alice) and `staff` (Alice, Bob).
pub fn directory() -> Vec<Person> {
    vec![
        Person {
            dn: SERVICE_DN,
            password: SERVICE_PW,
            attrs: &[("objectClass", &["person"]), ("cn", &["svc"])],
        },
        Person {
            dn: "uid=alice,ou=people,dc=example,dc=com",
            password: "alice-pw",
            attrs: &[
                ("objectClass", &["person", "inetOrgPerson"]),
                ("uid", &["alice"]),
                ("cn", &["Alice Liddell"]),
                ("mail", &["alice@example.com"]),
                ("memberOf", &["cn=admins,ou=groups,dc=example,dc=com"]),
            ],
        },
        Person {
            dn: "uid=bob,ou=people,dc=example,dc=com",
            password: "bob-pw",
            attrs: &[
                ("objectClass", &["person", "inetOrgPerson"]),
                ("uid", &["bob"]),
                ("cn", &["Bob"]),
                ("mail", &["bob@example.com"]),
            ],
        },
        Person {
            dn: "cn=twin1,ou=people,dc=example,dc=com",
            password: "twin-pw",
            attrs: &[("objectClass", &["person"]), ("uid", &["twin"])],
        },
        Person {
            dn: "cn=twin2,ou=people,dc=example,dc=com",
            password: "twin-pw",
            attrs: &[("objectClass", &["person"]), ("uid", &["twin"])],
        },
        Person {
            dn: "cn=admins,ou=groups,dc=example,dc=com",
            password: "",
            attrs: &[
                ("objectClass", &["groupOfNames"]),
                ("cn", &["admins"]),
                ("member", &["uid=alice,ou=people,dc=example,dc=com"]),
            ],
        },
        Person {
            dn: "cn=staff,ou=groups,dc=example,dc=com",
            password: "",
            attrs: &[
                ("objectClass", &["groupOfNames"]),
                ("cn", &["staff"]),
                (
                    "member",
                    &[
                        "uid=alice,ou=people,dc=example,dc=com",
                        "uid=bob,ou=people,dc=example,dc=com",
                    ],
                ),
            ],
        },
    ]
}

pub struct Fake {
    pub port: u16,
    /// One line per operation: `bind <dn> ok|refused|anonymous`, and
    /// `search <base> <filter> as <the DN bound at the time>`.
    pub log: Arc<Mutex<Vec<String>>>,
}

impl Fake {
    pub fn url(&self) -> String {
        format!("ldap://127.0.0.1:{}", self.port)
    }

    pub fn log(&self) -> Vec<String> {
        self.log.lock().unwrap().clone()
    }
}

pub fn start() -> Fake {
    let log = Arc::new(Mutex::new(Vec::new()));
    let std_listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = std_listener.local_addr().unwrap().port();
    std_listener.set_nonblocking(true).unwrap();
    let thread_log = log.clone();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async move {
            let listener = tokio::net::TcpListener::from_std(std_listener).unwrap();
            loop {
                let Ok((socket, _)) = listener.accept().await else {
                    return;
                };
                tokio::spawn(serve(socket, thread_log.clone()));
            }
        });
    });
    Fake { port, log }
}

fn done(code: LdapResultCode, message: &str) -> LdapResult {
    LdapResult {
        code,
        matcheddn: String::new(),
        message: message.into(),
        referral: Vec::new(),
    }
}

async fn serve(socket: tokio::net::TcpStream, log: Arc<Mutex<Vec<String>>>) {
    let mut framed = Framed::new(socket, LdapCodec::default());
    let people = directory();
    let mut bound: Option<String> = None;
    while let Some(Ok(msg)) = framed.next().await {
        let id = msg.msgid;
        match msg.op {
            LdapOp::BindRequest(req) => {
                let LdapBindCred::Simple(pw) = req.cred else {
                    return;
                };
                let (code, line) = if pw.is_empty() {
                    // RFC 4513 §5.1.2: unauthenticated, and answered as success.
                    bound = None;
                    (LdapResultCode::Success, "anonymous")
                } else if people.iter().any(|p| {
                    !p.password.is_empty() && p.dn.eq_ignore_ascii_case(&req.dn) && p.password == pw
                }) {
                    bound = Some(req.dn.to_ascii_lowercase());
                    (LdapResultCode::Success, "ok")
                } else {
                    bound = None;
                    (LdapResultCode::InvalidCredentials, "refused")
                };
                log.lock().unwrap().push(format!("bind {} {line}", req.dn));
                let res = LdapBindResponse {
                    res: done(code, ""),
                    saslcreds: None,
                };
                let _ = framed
                    .send(LdapMsg::new(id, LdapOp::BindResponse(res)))
                    .await;
            }
            LdapOp::SearchRequest(req) => {
                log.lock().unwrap().push(format!(
                    "search {} {:?} as {}",
                    req.base,
                    req.filter,
                    bound.as_deref().unwrap_or("anonymous")
                ));
                if bound.is_none() {
                    let res = done(LdapResultCode::InsufficentAccessRights, "bind first");
                    let _ = framed
                        .send(LdapMsg::new(id, LdapOp::SearchResultDone(res)))
                        .await;
                    continue;
                }
                let base = req.base.to_ascii_lowercase();
                let in_scope = |dn: &str| {
                    let dn = dn.to_ascii_lowercase();
                    match req.scope {
                        LdapSearchScope::Base => dn == base,
                        LdapSearchScope::OneLevel => {
                            dn.split_once(',').is_some_and(|(_, parent)| parent == base)
                        }
                        _ => dn == base || dn.ends_with(&format!(",{base}")),
                    }
                };
                let matched: Vec<&Person> = people
                    .iter()
                    .filter(|p| in_scope(p.dn) && matches(&req.filter, p))
                    .collect();
                let limit = if req.sizelimit > 0 {
                    req.sizelimit as usize
                } else {
                    usize::MAX
                };
                for p in matched.iter().take(limit) {
                    let all = req.attrs.is_empty() || req.attrs.iter().any(|a| a == "*");
                    let none = req.attrs.iter().all(|a| a == "1.1") && !req.attrs.is_empty();
                    let attributes = p
                        .attrs
                        .iter()
                        .filter(|(name, _)| {
                            !none && (all || req.attrs.iter().any(|a| a.eq_ignore_ascii_case(name)))
                        })
                        .map(|(name, vals)| LdapPartialAttribute {
                            atype: name.to_string(),
                            vals: vals.iter().map(|v| v.as_bytes().to_vec()).collect(),
                        })
                        .collect();
                    let entry = LdapSearchResultEntry {
                        dn: p.dn.to_string(),
                        attributes,
                    };
                    let _ = framed
                        .send(LdapMsg::new(id, LdapOp::SearchResultEntry(entry)))
                        .await;
                }
                let code = if matched.len() > limit {
                    LdapResultCode::SizeLimitExceeded
                } else {
                    LdapResultCode::Success
                };
                let _ = framed
                    .send(LdapMsg::new(id, LdapOp::SearchResultDone(done(code, ""))))
                    .await;
            }
            LdapOp::UnbindRequest => return,
            _ => return,
        }
    }
}

fn values<'a>(p: &'a Person, attr: &str) -> Vec<&'a str> {
    p.attrs
        .iter()
        .filter(|(name, _)| name.eq_ignore_ascii_case(attr))
        .flat_map(|(_, vals)| vals.iter().copied())
        .collect()
}

fn matches(filter: &LdapFilter, p: &Person) -> bool {
    match filter {
        LdapFilter::And(fs) => fs.iter().all(|f| matches(f, p)),
        LdapFilter::Or(fs) => fs.iter().any(|f| matches(f, p)),
        LdapFilter::Not(f) => !matches(f, p),
        LdapFilter::Present(attr) => {
            attr.eq_ignore_ascii_case("objectClass") || !values(p, attr).is_empty()
        }
        LdapFilter::Equality(attr, value) => values(p, attr)
            .iter()
            .any(|v| v.eq_ignore_ascii_case(value)),
        _ => false,
    }
}
