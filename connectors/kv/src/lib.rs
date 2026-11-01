//! The kv connector: `host:kv/*`, one topic of a signalling server's kv
//! store (`doc/DRT-Signalling.md` §10), from a program.
//!
//! ```text
//! kv/get    {key}                          -> {value, version} | nil
//! kv/put    {key, value, ttl?, version?}   -> {version} | {conflict, version?}
//! kv/delete {key, version?}                -> {deleted} | {conflict, version?}
//! kv/list   {prefix?, after?, limit?}      -> {revision, keys, more}
//! kv/watch  {since, prefix?, wait_ms?}     -> {revision, events, reset?}
//! ```
//!
//! ## surface block
//!
//! - Entry points: the five calls above, dispatched in [`Connector::call`];
//!   [`KvScopeType`] for the wiring.
//! - Configurable: [`TIMEOUT`], [`WATCH_DEFAULT_MS`], [`WATCH_MAX_MS`].
//! - Fan-out: [`CALLS`], the five calls and their HTTP requests.
//!
//! **The scope is the topic.** The operator names one topic URL,
//! `http(s)://host:port/v1/<topic>` (or `drt://`, `https` unless the host
//! is loopback, as `drt p2p` reads it), and an optional `token` sent as
//! `Authorization: Bearer`, which the program never sees. A program names
//! keys inside that topic and nothing else: it cannot reach another topic,
//! another server, or any other URL. So the private-address rule rest
//! applies to a guest-chosen URL does not apply here; the operator chose it.
//!
//! **`version` is compare-and-set.** `put {version = 3}` writes only over
//! version 3 (If-Match), `version = 0` only where the key is absent
//! (If-None-Match: *). A lost race is an answer, `{conflict = true,
//! version = <current>}`, not an error, so a retry loop reads as one.
//!
//! **`watch` waits in the host.** One held request (`?watch=poll`) that
//! answers when a change after `since` arrives, or empty after `wait_ms`.
//! A program waits longer than that in `host.call`: pass a `waitms` above
//! `wait_ms`, since the default is 10 s. `reset = true` means `since` is
//! older than the server keeps, and the program should list again.
//!
//! Values are UTF-8 text, at most 16 KiB; the server refuses past that.

use std::time::Duration;

use drt_caps::{Scope, ScopeType};
use drt_connector::{CallError, CallResult, Connector};
use drt_connector_rest::{Response, RestScope, Url};
use serde::Deserialize;

/// One request to the server, end to end.
pub const TIMEOUT: Duration = Duration::from_secs(10);
/// How long `kv/watch` holds when the program names no `wait_ms`.
pub const WATCH_DEFAULT_MS: u64 = 20_000;
/// The server holds a poll at most 25 s (its `KV_POLL_MS`).
pub const WATCH_MAX_MS: u64 = 25_000;

/// The calls, and the request each one is.
pub const CALLS: &[(&str, &str)] = &[
    ("kv/get", "GET /v1/<topic>/kv/<key>"),
    (
        "kv/put",
        "PUT /v1/<topic>/kv/<key>[?ttl=s], If-Match or If-None-Match from version",
    ),
    (
        "kv/delete",
        "DELETE /v1/<topic>/kv/<key>, If-Match from version",
    ),
    ("kv/list", "GET /v1/<topic>/kv?prefix=&after=&limit="),
    ("kv/watch", "GET /v1/<topic>/kv?watch=poll&since=&prefix="),
];

// depth: the scope

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum ScopeShape {
    Url(String),
    Full {
        url: String,
        #[serde(default)]
        token: Option<Token>,
        #[serde(default)]
        extra_roots: Vec<String>,
    },
}

/// The token as written, or `{"env": "NAME"}`, read at startup.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum Token {
    Text(String),
    Env { env: String },
}

/// The wiring, parsed: the topic's base URL and the rest scope that
/// grants exactly its origin, with the token injected.
struct KvScope {
    base: String,
    rest: RestScope,
}

/// `drt://host[:port]/…` as `drt p2p` reads it: plain http to loopback,
/// https anywhere else.
fn http_url(url: &str) -> String {
    let Some(rest) = url.strip_prefix("drt://") else {
        return url.to_string();
    };
    let authority = rest.split('/').next().unwrap_or("");
    let host = match authority.rsplit_once(':') {
        Some((h, p)) if p.bytes().all(|b| b.is_ascii_digit()) && !authority.ends_with(']') => h,
        _ => authority,
    };
    let host = host.trim_start_matches('[').trim_end_matches(']');
    let loopback = host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback());
    format!("{}://{rest}", if loopback { "http" } else { "https" })
}

impl KvScope {
    fn parse(scope: Option<&Scope>) -> Result<KvScope, String> {
        let Some(Scope(value)) = scope else {
            return Err("the kv scope names the topic: \"http://host:port/v1/<topic>\"".into());
        };
        let shape: ScopeShape = rmpv::ext::from_value(value.clone()).map_err(|e| {
            format!("the kv scope is a topic URL, or {{url, token?, extra_roots?}}: {e}")
        })?;
        let (url, token, extra_roots) = match shape {
            ScopeShape::Url(u) => (u, None, Vec::new()),
            ScopeShape::Full {
                url,
                token,
                extra_roots,
            } => (url, token, extra_roots),
        };
        let base = http_url(url.trim_end_matches('/'));
        let parsed = Url::parse(&base).map_err(|e| format!("the kv url '{url}': {e}"))?;
        let topic = parsed.path.strip_prefix("/v1/").unwrap_or("");
        if topic.is_empty() || topic.contains(['/', '?', '#']) {
            return Err(format!(
                "the kv url '{url}' does not end at a topic: http://host:port/v1/<topic>"
            ));
        }
        let mut headers = Vec::new();
        if let Some(token) = token {
            let token = match token {
                Token::Text(t) => t,
                Token::Env { env } => std::env::var(&env)
                    .map_err(|_| format!("the kv token names env var '{env}', which is not set"))?,
            };
            headers.push((
                rmpv::Value::from("authorization"),
                rmpv::Value::from(format!("Bearer {token}")),
            ));
        }
        let entry = rmpv::Value::Map(vec![
            (
                rmpv::Value::from("origin"),
                rmpv::Value::from(parsed.origin()),
            ),
            (rmpv::Value::from("headers"), rmpv::Value::Map(headers)),
        ]);
        let rest = Scope(rmpv::Value::Map(vec![
            (rmpv::Value::from("allow"), rmpv::Value::Array(vec![entry])),
            // The operator named this origin; a guest names only keys.
            (rmpv::Value::from("allow_private"), rmpv::Value::from(true)),
            (
                rmpv::Value::from("extra_roots"),
                rmpv::Value::Array(extra_roots.into_iter().map(rmpv::Value::from).collect()),
            ),
        ]));
        Ok(KvScope {
            base,
            rest: RestScope::parse(Some(&rest))?,
        })
    }
}

pub struct KvScopeType;

impl ScopeType for KvScopeType {
    fn describe(&self) -> &str {
        "a topic URL at a signalling server, http(s)://host:port/v1/<topic>, or {url, token?, extra_roots?}"
    }
    fn validate(&self, scope: Option<&Scope>) -> Result<(), String> {
        KvScope::parse(scope).map(|_| ())
    }
}

// depth: the calls

#[derive(Debug, Deserialize)]
struct KeyArgs {
    key: String,
    #[serde(default)]
    version: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct PutArgs {
    key: String,
    value: String,
    #[serde(default)]
    ttl: Option<u64>,
    #[serde(default)]
    version: Option<u64>,
}

#[derive(Debug, Default, Deserialize)]
struct ListArgs {
    #[serde(default)]
    prefix: Option<String>,
    #[serde(default)]
    after: Option<String>,
    #[serde(default)]
    limit: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct WatchArgs {
    since: u64,
    #[serde(default)]
    prefix: Option<String>,
    #[serde(default)]
    wait_ms: Option<u64>,
}

/// A key or prefix as it rides in a URL. The server's key alphabet is
/// `A-Z a-z 0-9 . _ ~ - /`; anything else is escaped, so the server
/// refuses it by name rather than the URL breaking.
fn escape(text: &str) -> String {
    let mut out = String::new();
    for b in text.bytes() {
        if b.is_ascii_alphanumeric() || b"._~-/".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn args_of<T: serde::de::DeserializeOwned>(
    call: &str,
    args: Option<rmpv::Value>,
) -> Result<T, CallError> {
    let args = args.unwrap_or(rmpv::Value::Map(Vec::new()));
    rmpv::ext::from_value(args).map_err(|e| CallError::new(format!("{call} args: {e}")))
}

fn json(call: &str, resp: &Response) -> Result<rmpv::Value, CallError> {
    let v: serde_json::Value = serde_json::from_slice(&resp.body)
        .map_err(|e| CallError::new(format!("{call}: the server's reply is not JSON: {e}")))?;
    rmpv::ext::to_value(v).map_err(|e| CallError::new(format!("{call}: {e}")))
}

/// A status this call does not expect, in the server's own words.
fn refused(call: &str, resp: &Response) -> CallError {
    let said = serde_json::from_slice::<serde_json::Value>(&resp.body)
        .ok()
        .and_then(|v| v["error"].as_str().map(str::to_string))
        .unwrap_or_default();
    CallError::new(match said.is_empty() {
        true => format!("{call}: the server answered {}", resp.status),
        false => format!("{call}: the server answered {}: {said}", resp.status),
    })
}

fn etag_version(resp: &Response) -> Option<u64> {
    resp.header("etag")?
        .trim_start_matches("W/")
        .trim_matches('"')
        .parse()
        .ok()
}

fn map(pairs: Vec<(&str, rmpv::Value)>) -> rmpv::Value {
    rmpv::Value::Map(
        pairs
            .into_iter()
            .map(|(k, v)| (rmpv::Value::from(k), v))
            .collect(),
    )
}

/// `{conflict = true, version = <current>}`, the answer to a lost race.
fn conflict(resp: &Response) -> rmpv::Value {
    let mut pairs = vec![("conflict", rmpv::Value::from(true))];
    if let Some(v) = etag_version(resp) {
        pairs.push(("version", rmpv::Value::from(v)));
    }
    map(pairs)
}

pub struct KvConnector;

impl KvConnector {
    pub fn new() -> Self {
        KvConnector
    }
}

impl Default for KvConnector {
    fn default() -> Self {
        Self::new()
    }
}

impl KvConnector {
    async fn send(
        &self,
        sc: &KvScope,
        method: &str,
        tail: &str,
        headers: Vec<(&str, String)>,
        body: Option<&[u8]>,
        timeout: Duration,
    ) -> Result<Response, CallError> {
        let url = Url::parse(&format!("{}/kv{tail}", sc.base)).map_err(CallError::new)?;
        let headers = (!headers.is_empty()).then(|| {
            headers
                .into_iter()
                .map(|(k, v)| (k.to_string(), v))
                .collect()
        });
        drt_connector_rest::request(&url, method, &headers, body, &sc.rest, timeout)
            .await
            .map_err(|e| CallError::new(format!("{}: {e}", sc.base)))
    }
}

#[async_trait::async_trait]
impl Connector for KvConnector {
    fn scope_type(&self) -> Box<dyn ScopeType> {
        Box::new(KvScopeType)
    }

    async fn call(
        &self,
        call: &str,
        args: Option<rmpv::Value>,
        scope: Option<&Scope>,
    ) -> CallResult {
        if !CALLS.iter().any(|(name, _)| *name == call) {
            return Err(CallError::new(format!("'{call}' is not a kv call")));
        }
        let sc = KvScope::parse(scope).map_err(CallError::new)?;
        match call {
            "kv/get" => {
                let a: KeyArgs = args_of(call, args)?;
                let resp = self
                    .send(
                        &sc,
                        "GET",
                        &format!("/{}", escape(&a.key)),
                        vec![],
                        None,
                        TIMEOUT,
                    )
                    .await?;
                match resp.status {
                    200 => Ok(map(vec![
                        (
                            "value",
                            rmpv::Value::from(String::from_utf8_lossy(&resp.body).into_owned()),
                        ),
                        (
                            "version",
                            etag_version(&resp).map_or(rmpv::Value::Nil, rmpv::Value::from),
                        ),
                    ])),
                    404 => Ok(rmpv::Value::Nil),
                    _ => Err(refused(call, &resp)),
                }
            }
            "kv/put" => {
                let a: PutArgs = args_of(call, args)?;
                let mut tail = format!("/{}", escape(&a.key));
                if let Some(ttl) = a.ttl {
                    tail.push_str(&format!("?ttl={ttl}"));
                }
                let headers = match a.version {
                    Some(0) => vec![("if-none-match", "*".to_string())],
                    Some(v) => vec![("if-match", format!("\"{v}\""))],
                    None => vec![],
                };
                let resp = self
                    .send(
                        &sc,
                        "PUT",
                        &tail,
                        headers,
                        Some(a.value.as_bytes()),
                        TIMEOUT,
                    )
                    .await?;
                match resp.status {
                    200 | 201 => Ok(map(vec![(
                        "version",
                        etag_version(&resp).map_or(rmpv::Value::Nil, rmpv::Value::from),
                    )])),
                    412 => Ok(conflict(&resp)),
                    _ => Err(refused(call, &resp)),
                }
            }
            "kv/delete" => {
                let a: KeyArgs = args_of(call, args)?;
                let headers = match a.version {
                    Some(v) => vec![("if-match", format!("\"{v}\""))],
                    None => vec![],
                };
                let resp = self
                    .send(
                        &sc,
                        "DELETE",
                        &format!("/{}", escape(&a.key)),
                        headers,
                        None,
                        TIMEOUT,
                    )
                    .await?;
                match resp.status {
                    204 => Ok(map(vec![("deleted", rmpv::Value::from(true))])),
                    404 => Ok(map(vec![("deleted", rmpv::Value::from(false))])),
                    412 => Ok(conflict(&resp)),
                    _ => Err(refused(call, &resp)),
                }
            }
            "kv/list" => {
                let a: ListArgs = args_of(call, args)?;
                let mut query = Vec::new();
                if let Some(p) = &a.prefix {
                    query.push(format!("prefix={}", escape(p)));
                }
                if let Some(p) = &a.after {
                    query.push(format!("after={}", escape(p)));
                }
                if let Some(n) = a.limit {
                    query.push(format!("limit={n}"));
                }
                let tail = match query.is_empty() {
                    true => String::new(),
                    false => format!("?{}", query.join("&")),
                };
                let resp = self.send(&sc, "GET", &tail, vec![], None, TIMEOUT).await?;
                match resp.status {
                    200 => json(call, &resp),
                    _ => Err(refused(call, &resp)),
                }
            }
            "kv/watch" => {
                let a: WatchArgs = args_of(call, args)?;
                let wait = a.wait_ms.unwrap_or(WATCH_DEFAULT_MS);
                if wait > WATCH_MAX_MS {
                    return Err(CallError::new(format!(
                        "kv/watch: wait_ms is at most {WATCH_MAX_MS}, the server's hold"
                    )));
                }
                let mut tail = format!("?watch=poll&since={}&wait={wait}", a.since);
                if let Some(p) = &a.prefix {
                    tail.push_str(&format!("&prefix={}", escape(p)));
                }
                // The server holds for `wait` and answers empty after it.
                let resp = self
                    .send(
                        &sc,
                        "GET",
                        &tail,
                        vec![],
                        None,
                        TIMEOUT + Duration::from_millis(wait),
                    )
                    .await?;
                match resp.status {
                    200 => json(call, &resp),
                    _ => Err(refused(call, &resp)),
                }
            }
            _ => unreachable!("the call was checked against CALLS above"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scope(v: &str) -> Scope {
        Scope(rmpv::Value::from(v))
    }

    #[test]
    fn a_scope_names_one_topic() {
        let ok = KvScope::parse(Some(&scope("http://127.0.0.1:8090/v1/home/"))).unwrap();
        assert_eq!(ok.base, "http://127.0.0.1:8090/v1/home");
        assert!(KvScope::parse(Some(&scope("http://127.0.0.1:8090/v1/"))).is_err());
        assert!(KvScope::parse(Some(&scope("http://127.0.0.1:8090/v1/a/b"))).is_err());
        assert!(KvScope::parse(Some(&scope("ftp://h/v1/a"))).is_err());
        assert!(KvScope::parse(None).is_err());
    }

    #[test]
    fn drt_urls_read_as_p2p_reads_them() {
        assert_eq!(
            http_url("drt://127.0.0.1:1/v1/t"),
            "http://127.0.0.1:1/v1/t"
        );
        assert_eq!(http_url("drt://localhost/v1/t"), "http://localhost/v1/t");
        assert_eq!(
            http_url("drt://match.example/v1/t"),
            "https://match.example/v1/t"
        );
        assert_eq!(http_url("https://x/v1/t"), "https://x/v1/t");
    }

    #[test]
    fn keys_outside_the_alphabet_are_escaped_not_dropped() {
        assert_eq!(escape("bpi/info-2.x_~"), "bpi/info-2.x_~");
        assert_eq!(escape("a b?&"), "a%20b%3F%26");
    }

    #[test]
    fn an_unknown_call_is_refused_by_name() {
        let err = pollster::block_on(KvConnector::new().call(
            "kv/frob",
            None,
            Some(&scope("http://127.0.0.1:1/v1/t")),
        ))
        .unwrap_err();
        assert!(err.to_string().contains("not a kv call"), "{err}");
    }
}
