//! `host:rest/get` and `host:rest/post` — outbound HTTP from a guest.
//!
//! The guest-facing contract is not invented here. It is diluvium's, from
//! `plugins/rest/rest.plugin.json`, and this connector answers the same two
//! calls with the same argument and result shapes so that a program written
//! against `diluvium-host` runs unchanged on DRT. Where the two hosts could
//! drift, upstream wins — the point of DRT is to run the same deployments.
//!
//! ## Same contract, different mechanism
//!
//! The C host implements this as an out-of-process **plugin**: it execs a
//! binary, hands it a socketpair, and speaks framed msgpack, precisely so
//! that `diluvium-host` links no TLS and opens no socket. That is the right
//! answer for a host whose whole point is to static-link onto a lean box.
//!
//! DRT already has a connector layer with typed scopes, so the subprocess
//! buys nothing here and costs a process, a manifest and a checksum. This
//! is an ordinary connector — and it can do something the plugin cannot:
//! **take a scope.**
//!
//! ## The scope is the point
//!
//! `plugins/README.md` wires the rest plugin with `caps = { "host:rest/*" }`
//! and no scope, because the plugin protocol has nowhere to put one. An
//! unscoped outbound-HTTP capability is an SSRF primitive: a guest that
//! holds it can reach cloud metadata at `169.254.169.254`, your database on
//! `10.x`, or the relay's own control plane. The C host's answer is "then
//! do not grant it", which is a real answer and a blunt one.
//!
//! Here the scope is an **origin allowlist**, and it is checked twice:
//!
//! 1. Against the URL, before anything is resolved.
//! 2. Against the **resolved address**, before anything is connected —
//!    because an allowed name that resolves into private space is the DNS
//!    rebinding shape, and a check that only reads the URL never sees it.
//!
//! [`RestScopeType::validate`] parses the allowlist at startup, so a
//! malformed one is a refusal by name rather than a surprise on the first
//! call.
//!
//! ## Redirects are not followed, deliberately
//!
//! Neither does the C plugin — there is no `Location` handling in
//! `rest_plugin.c`, and the manifest's note that a clipped `Location` is a
//! wrong answer implies the guest is expected to see it. Matching that is
//! both compatibility and safety: a followed redirect walks straight out of
//! the origin that was just authorised, and re-checking each hop is a
//! policy the guest is better placed to decide than we are. A 30x comes
//! back as a 30x, with its `location` header intact.
//!
//! ## The framing ends the body, not the connection
//!
//! [`BodyEnd`] is read off the response head and is what the read loop
//! stops on. This is not a refinement: reading to EOF instead cost issue
//! #10 an afternoon and blocked a deployment entirely. Every response
//! Amazon Bedrock sends arrives complete and is then followed by a TCP
//! close with no TLS `close_notify`, which rustls reports as
//! `UnexpectedEof` — so a connector that reads one byte past the body it
//! already has fails a call the server answered `200`.
//!
//! A close without `close_notify` is therefore recorded and judged, never
//! raised on sight: it is an error only for a body the framing says is
//! unfinished, and then the message names what was expected and what
//! arrived, because "the connection is broken" is what sent that
//! afternoon to the network and the credentials rather than to this loop.

use std::net::IpAddr;
use std::time::Duration;

use drt_caps::{Scope, ScopeType};
use drt_connector::{CallError, CallResult, Connector};
use serde::Deserialize;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Bounds, matched to `plugins/rest/rest_plugin.c` so that a program that
/// works on one host works on the other. Changing one of these is a
/// compatibility change, not a tuning knob.
mod limits {
    /// `REST_MAX_BODY`.
    pub const MAX_BODY: usize = 16 * 1024 * 1024;
    /// `REST_DEFAULT_MS`.
    pub const DEFAULT_MS: u64 = 15_000;
    /// The manifest's `timeout_ms` maximum.
    pub const MAX_MS: u64 = 120_000;
    /// `REST_RESP_HDRS` / `_NAME_MAX` / `_VAL_MAX`. Past a bound the header
    /// is dropped **whole, never truncated** — a clipped `Location` or
    /// `Set-Cookie` is a wrong answer wearing the shape of a right one.
    pub const RESP_HDRS: usize = 32;
    pub const RESP_NAME_MAX: usize = 64;
    pub const RESP_VAL_MAX: usize = 4096;
    /// `REST_MAX_HEADERS`, request side.
    pub const REQ_HEADERS: usize = 64;
}

/// Headers as they cross the boundary: name → value, names lowercased.
pub type Headers = std::collections::BTreeMap<String, String>;

/// A parsed response head: the status, the headers that survived the
/// bounds, how many bytes of head were consumed, and how the body after it
/// is framed.
pub struct Head {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub consumed: usize,
    pub body_end: BodyEnd,
}

/// How a response body ends (RFC 9112 §6.3). This is the whole of what
/// separates a complete body from a truncated one, which is why the read
/// loop is written around it rather than around EOF: to a TLS layer, a
/// peer that closes politely and a peer that cuts the connection look the
/// same, and only the framing knows the difference.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BodyEnd {
    /// `content-length: N`, and also the statuses that carry no body
    /// whatever their headers say (1xx, 204, 304), which are `Length(0)`.
    Length(usize),
    /// `transfer-encoding: chunked`: the zero-size chunk ends it.
    Chunked,
    /// Neither header, so the close *is* the framing (RFC 9112 §6.3 item
    /// 8). A truncation cannot be told from an ending here, by
    /// construction — distinguishing them is what `content-length` is for,
    /// and this response sent none.
    Close,
}

/// A URL, split. Mirrors the C plugin's `struct url`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Url {
    pub tls: bool,
    pub host: String,
    pub port: u16,
    pub path: String,
}

impl Url {
    /// `scheme://host[:port][/path]`. Refuses anything that is not http or
    /// https by name, in the plugin's own words.
    pub fn parse(raw: &str) -> Result<Url, String> {
        let (tls, rest) = if let Some(r) = raw.strip_prefix("https://") {
            (true, r)
        } else if let Some(r) = raw.strip_prefix("http://") {
            (false, r)
        } else {
            return Err("the url must begin http:// or https://".into());
        };
        let (authority, path) = match rest.find('/') {
            Some(i) => (&rest[..i], &rest[i..]),
            None => (rest, "/"),
        };
        if authority.is_empty() {
            return Err("the url names no host".into());
        }
        // Userinfo is refused rather than ignored: `https://evil@internal/`
        // reads to a human as a host that it is not, and silently dropping
        // it would authorise the wrong origin.
        if authority.contains('@') {
            return Err("the url carries userinfo, which this connector refuses".into());
        }
        let (host, port) = match authority.rsplit_once(':') {
            // A colon inside brackets is an IPv6 literal, not a port.
            Some((h, p)) if !authority.ends_with(']') && !h.is_empty() => {
                let port: u16 = p
                    .parse()
                    .map_err(|_| format!("the url's port is not a number: '{p}'"))?;
                (h.to_string(), port)
            }
            _ => (authority.to_string(), if tls { 443 } else { 80 }),
        };
        Ok(Url {
            tls,
            host: host.trim_matches(|c| c == '[' || c == ']').to_string(),
            port,
            path: path.to_string(),
        })
    }

    /// `scheme://host:port`, the form the allowlist matches on.
    pub fn origin(&self) -> String {
        format!(
            "{}://{}:{}",
            if self.tls { "https" } else { "http" },
            self.host,
            self.port
        )
    }
}

/// The wiring this connector was granted: which origins a guest holding
/// `host:rest/*` may reach, and on what header terms.
///
/// Accepts a bare string, a list, or `{allow = [...], allow_private = bool}`.
/// Each allow entry is either an origin string or a table:
///
/// ```lua
/// allow = {
///   "https://api.example.com",
///   { origin = "https://billing.example.com",
///     headers       = { ["x-api-key"] = "sk_live_..." },
///     allow_headers = { "accept", "content-type" } },
/// }
/// ```
///
/// - **`headers`** are set by the connector on every request to that origin
///   and the guest can neither set nor read them. That is the interesting
///   one: it lets a deployment call an authenticated API without the
///   program ever holding the credential, which is the capability model
///   doing its actual job rather than just gating a call name.
/// - **`allow_headers`**, when present, is the *only* set the guest may
///   set on that origin. Absent means "anything not reserved".
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RestScope {
    allow: Vec<AllowEntry>,
    /// Off by default. On, private/loopback/link-local/CGNAT destinations
    /// are permitted — for a deployment whose whole job is talking to a
    /// service on its own network, stated deliberately rather than reached
    /// by accident.
    allow_private: bool,
}

/// One granted origin and its header terms.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AllowEntry {
    pub origin: Url,
    /// Injected by the connector, invisible to the guest.
    pub headers: std::collections::BTreeMap<String, String>,
    /// When `Some`, the exhaustive set of header names the guest may set.
    pub allow_headers: Option<std::collections::BTreeSet<String>>,
}

impl AllowEntry {
    /// Whether the guest may set this header name on this origin.
    fn guest_may_set(&self, lower: &str) -> bool {
        // An operator-supplied header is not a default the guest can
        // override; overriding an injected `authorization` would be the
        // whole point of injecting it, undone.
        if self.headers.contains_key(lower) {
            return false;
        }
        match &self.allow_headers {
            Some(set) => set.contains(lower),
            None => true,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum EntryShape {
    Origin(String),
    Table {
        origin: String,
        #[serde(default)]
        headers: std::collections::BTreeMap<String, String>,
        #[serde(default)]
        allow_headers: Option<Vec<String>>,
    },
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum ScopeShape {
    One(String),
    Many(Vec<EntryShape>),
    Full {
        #[serde(default)]
        allow: Vec<EntryShape>,
        #[serde(default)]
        allow_private: bool,
    },
}

impl RestScope {
    pub fn parse(scope: Option<&Scope>) -> Result<RestScope, String> {
        let Some(scope) = scope else {
            // No scope is not "everything". An unscoped outbound-HTTP
            // capability is the SSRF hole this connector exists to close,
            // so the empty case refuses every call rather than allowing
            // every call.
            return Ok(RestScope::default());
        };
        let shape: ScopeShape = rmpv::ext::from_value(scope.0.clone()).map_err(|e| {
            format!(
                "the rest scope is not an origin, a list of them, or {{allow, allow_private}}: {e}"
            )
        })?;
        let (raw, allow_private) = match shape {
            ScopeShape::One(s) => (vec![EntryShape::Origin(s)], false),
            ScopeShape::Many(v) => (v, false),
            ScopeShape::Full {
                allow,
                allow_private,
            } => (allow, allow_private),
        };
        let mut allow = Vec::new();
        for entry in raw {
            let (raw_origin, headers, allow_headers) = match entry {
                EntryShape::Origin(o) => (o, Default::default(), None),
                EntryShape::Table {
                    origin,
                    headers,
                    allow_headers,
                } => (origin, headers, allow_headers),
            };
            // Parsed through the same parser the request uses, so an
            // allowlist entry cannot mean something different from the URL
            // it is meant to authorise.
            let origin =
                Url::parse(&raw_origin).map_err(|e| format!("allow entry '{raw_origin}': {e}"))?;
            // Names are normalised once, here, so no comparison below has
            // to remember that HTTP header names are case-insensitive.
            let mut lowered = std::collections::BTreeMap::new();
            for (k, v) in headers {
                let lk = k.to_ascii_lowercase();
                if lk.is_empty() || lk.contains(['\r', '\n', ':']) || v.contains(['\r', '\n']) {
                    return Err(format!(
                        "allow entry '{raw_origin}': header '{k}' contains a forbidden character"
                    ));
                }
                if is_reserved(&lk) {
                    return Err(format!(
                        "allow entry '{raw_origin}': header '{k}' is set by the connector"
                    ));
                }
                lowered.insert(lk, v);
            }
            allow.push(AllowEntry {
                origin,
                headers: lowered,
                allow_headers: allow_headers
                    .map(|v| v.into_iter().map(|h| h.to_ascii_lowercase()).collect()),
            });
        }
        Ok(RestScope {
            allow,
            allow_private,
        })
    }

    /// The granted entry covering this URL, if any. Returning the entry
    /// rather than a bool is what lets the header terms travel with the
    /// permission instead of being looked up again.
    pub fn matching(&self, url: &Url) -> Option<&AllowEntry> {
        self.allow.iter().find(|a| {
            a.origin.tls == url.tls
                && a.origin.port == url.port
                && match a.origin.host.strip_prefix("*.") {
                    // A wildcard matches a strict subdomain only.
                    Some(suffix) => url
                        .host
                        .strip_suffix(suffix)
                        .is_some_and(|p| p.ends_with('.') && p.len() > 1),
                    None => a.origin.host.eq_ignore_ascii_case(&url.host),
                }
        })
    }

    /// Whether this URL is inside the granted origins.
    pub fn permits(&self, url: &Url) -> bool {
        self.matching(url).is_some()
    }

    /// Whether a resolved address may be connected to.
    ///
    /// Separate from [`permits`] on purpose: this is the check an
    /// allowlist alone cannot make. An authorised name that resolves to
    /// `169.254.169.254` or `10.0.0.5` is the rebinding shape, and it
    /// arrives looking exactly like a legitimate request.
    pub fn permits_address(&self, ip: IpAddr) -> bool {
        if self.allow_private {
            return true;
        }
        !is_private(ip)
    }
}

/// Addresses a guest may not reach without `allow_private`. Deliberately
/// broad: the cost of refusing one reachable host is an error message, and
/// the cost of allowing one unreachable-by-design host is the cloud
/// credentials on the metadata endpoint.
fn is_private(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            v4.is_private()
                || v4.is_loopback()
                || v4.is_link_local()
                || v4.is_broadcast()
                || v4.is_documentation()
                || v4.is_unspecified()
                || o[0] == 0
                // CGNAT 100.64.0.0/10
                || (o[0] == 100 && (64..=127).contains(&o[1]))
                // benchmarking 198.18.0.0/15
                || (o[0] == 198 && (o[1] == 18 || o[1] == 19))
                || o[0] >= 240
        }
        IpAddr::V6(v6) => {
            v6.is_loopback()
                || v6.is_unspecified()
                || (v6.segments()[0] & 0xffc0) == 0xfe80
                || (v6.octets()[0] & 0xfe) == 0xfc
                // v4-mapped: judge the address it actually carries
                || v6.to_ipv4_mapped().is_some_and(|m| is_private(IpAddr::V4(m)))
        }
    }
}

/// Headers the connector owns. A guest that could set `content-length` or
/// `transfer-encoding` could smuggle a second request past the origin, and
/// one that could set `host` could aim it somewhere else entirely.
fn is_reserved(lower: &str) -> bool {
    matches!(
        lower,
        "host" | "connection" | "content-length" | "transfer-encoding"
    )
}

pub struct RestScopeType;

impl ScopeType for RestScopeType {
    fn describe(&self) -> &str {
        "allowed origins: \"https://api.example.com\", a list of them, or {allow: [...], allow_private?: bool}"
    }

    fn validate(&self, scope: Option<&Scope>) -> Result<(), String> {
        let parsed = RestScope::parse(scope)?;
        // An empty allowlist parses but can answer nothing. Refusing at
        // startup means the operator learns it now rather than from a
        // guest's first denied call in production.
        if parsed.allow.is_empty() {
            return Err(
                "the rest scope grants no origins, so every call would be refused; name at least one, e.g. \"https://api.example.com\"".into(),
            );
        }
        Ok(())
    }
}

#[derive(Debug, Deserialize)]
struct GetArgs {
    url: String,
    #[serde(default)]
    headers: Option<std::collections::BTreeMap<String, String>>,
    #[serde(default)]
    timeout_ms: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct PostArgs {
    url: String,
    #[serde(default)]
    headers: Option<std::collections::BTreeMap<String, String>>,
    #[serde(default)]
    body: Option<rmpv::Value>,
    #[serde(default)]
    timeout_ms: Option<u64>,
}

/// What came back. The msgpack wire carries a real `bin` type, so the body
/// is bytes here rather than the base64-in-string the manifest's JSON
/// Schema has to use — same value, one less encoding for the guest to
/// undo, and what the C host's msgpack reply also carries.
#[derive(Debug)]
struct Response {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

pub struct RestConnector;

impl RestConnector {
    pub fn new() -> Self {
        RestConnector
    }
}

impl Default for RestConnector {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl Connector for RestConnector {
    fn scope_type(&self) -> Box<dyn ScopeType> {
        Box::new(RestScopeType)
    }

    async fn call(
        &self,
        call: &str,
        args: Option<rmpv::Value>,
        scope: Option<&Scope>,
    ) -> CallResult {
        // The verb is checked before the arguments: `rest/put` with no args
        // is an unknown call, not a missing-argument error, and answering
        // the second question first sends the reader looking for a typo in
        // the wrong place.
        if !matches!(call, "rest/get" | "rest/post") {
            return Err(CallError::new(format!("'{call}' is not a rest call")));
        }
        let sc = RestScope::parse(scope).map_err(CallError::new)?;
        let args = args.ok_or_else(|| CallError::new(format!("{call} takes args {{url, ...}}")))?;

        let (method, url_raw, headers, body, timeout_ms) = match call {
            "rest/get" => {
                let a: GetArgs = rmpv::ext::from_value(args)
                    .map_err(|e| CallError::new(format!("rest/get args: {e}")))?;
                ("GET", a.url, a.headers, None, a.timeout_ms)
            }
            "rest/post" => {
                let a: PostArgs = rmpv::ext::from_value(args)
                    .map_err(|e| CallError::new(format!("rest/post args: {e}")))?;
                let body = a.body.as_ref().and_then(bytes_of).unwrap_or_default();
                ("POST", a.url, a.headers, Some(body), a.timeout_ms)
            }
            _ => unreachable!("the verb was checked above"),
        };

        let url = Url::parse(&url_raw).map_err(CallError::new)?;
        let Some(entry) = sc.matching(&url) else {
            // Named refusal: which origin, and that the scope is the reason
            // — the denial strings are the best-written text in this
            // product and this one has to earn its place among them.
            return Err(CallError::new(format!(
                "'{}' is outside this instance's granted origins",
                url.origin()
            )));
        };
        let timeout =
            Duration::from_millis(timeout_ms.unwrap_or(limits::DEFAULT_MS).min(limits::MAX_MS));

        let work = async {
            tokio::time::timeout(
                timeout,
                fetch(&url, method, &headers, body.as_deref(), &sc, entry),
            )
            .await
        };
        // `drt start` drives connectors on a tokio runtime; `drt run` drives
        // them with `pollster::block_on` (run.rs:67), which has no reactor —
        // and every socket call below needs one. Awaiting directly under
        // `run` panicked with "there is no reactor running", a Rust
        // backtrace and exit 101, which is the worst failure this codebase
        // has: not a refusal a program can read, not even a message an
        // operator can act on.
        //
        // So the connector carries its own runtime for the case where the
        // caller has none. It is leaked rather than dropped, for FM-1's
        // reason — tokio 1.53.1 use-after-frees on runtime teardown — and
        // built once for the life of the process.
        let outcome = match tokio::runtime::Handle::try_current() {
            Ok(_) => work.await,
            Err(_) => own_runtime().block_on(work),
        };
        let resp = outcome
            .map_err(|_| CallError::new("timeout"))?
            .map_err(CallError::new)?;

        let mut map = vec![
            (
                rmpv::Value::from("status"),
                rmpv::Value::from(resp.status as u64),
            ),
            (rmpv::Value::from("body"), rmpv::Value::Binary(resp.body)),
        ];
        let ct = resp
            .headers
            .iter()
            .find(|(n, _)| n == "content-type")
            .map(|(_, v)| rmpv::Value::from(v.as_str()))
            .unwrap_or(rmpv::Value::Nil);
        map.push((rmpv::Value::from("content_type"), ct));
        map.push((
            rmpv::Value::from("headers"),
            rmpv::Value::Map(
                resp.headers
                    .into_iter()
                    .map(|(n, v)| (rmpv::Value::from(n), rmpv::Value::from(v)))
                    .collect(),
            ),
        ));
        Ok(rmpv::Value::Map(map))
    }
}

fn bytes_of(value: &rmpv::Value) -> Option<Vec<u8>> {
    match value {
        rmpv::Value::String(s) => Some(s.as_bytes().to_vec()),
        rmpv::Value::Binary(b) => Some(b.clone()),
        _ => None,
    }
}

/// The runtime this connector falls back to when the caller has none.
///
/// One per process, created on first use, and never dropped: FM-1 is a
/// use-after-free in tokio 1.53.1's runtime teardown, and `drt relay`,
/// `drt stun` and `drt tunnel` all leak theirs for the same reason. A
/// `OnceLock` holding it forever is that mitigation spelled as ownership
/// rather than as `mem::forget`.
fn own_runtime() -> &'static tokio::runtime::Runtime {
    static RT: std::sync::OnceLock<tokio::runtime::Runtime> = std::sync::OnceLock::new();
    RT.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .expect("a tokio runtime for the rest connector")
    })
}

/// Build the request line and headers. Separated from the socket so it can
/// be asserted on without one.
pub fn request_bytes(
    url: &Url,
    method: &str,
    headers: &Option<Headers>,
    body: Option<&[u8]>,
    entry: &AllowEntry,
) -> Result<Vec<u8>, String> {
    let mut out = format!("{} {} HTTP/1.1\r\n", method, url.path);
    // Host carries the port only when it is not the scheme's default,
    // which is what an origin server expects and what a vhost matches on.
    let default_port = if url.tls { 443 } else { 80 };
    if url.port == default_port {
        out.push_str(&format!("host: {}\r\n", url.host));
    } else {
        out.push_str(&format!("host: {}:{}\r\n", url.host, url.port));
    }
    // Close after one response: no keep-alive, no pipelining, no connection
    // reuse across guests. One call, one connection, and nothing of one
    // instance's traffic can be observed on another's socket.
    out.push_str("connection: close\r\n");

    if let Some(h) = headers {
        if h.len() > limits::REQ_HEADERS {
            return Err(format!(
                "at most {} request headers, {} given",
                limits::REQ_HEADERS,
                h.len()
            ));
        }
        for (name, value) in h {
            let lower = name.to_ascii_lowercase();
            // Header injection: a name or value carrying CR/LF would end
            // the header block and let a guest write its own request line.
            if name.is_empty() || name.contains(['\r', '\n', ':']) || value.contains(['\r', '\n']) {
                return Err(format!("header '{name}' contains a forbidden character"));
            }
            if is_reserved(&lower) {
                return Err(format!("header '{name}' is set by the connector"));
            }
            // The scope's header terms. An operator-injected header cannot
            // be overridden, and an `allow_headers` list is exhaustive —
            // both refused by name, because "your header was silently
            // dropped" is the debugging session nobody wins.
            if !entry.guest_may_set(&lower) {
                return Err(format!(
                    "header '{name}' is not one this instance may set on {}",
                    entry.origin.origin()
                ));
            }
            out.push_str(&format!("{lower}: {value}\r\n"));
        }
    }
    // Injected last so they cannot be shadowed by anything above, and
    // never echoed back to the guest.
    for (name, value) in &entry.headers {
        out.push_str(&format!("{name}: {value}\r\n"));
    }
    // A default identity, only when nobody named one. Not politeness: a
    // request with NO user-agent is the signature Cloudflare's rule 1010
    // blocks, and `api.discofetch.net` is behind Cloudflare — so a guest
    // calling it got an error page rather than an API, from a connector
    // that had sent no identity at all.
    //
    // Overridable, deliberately: a guest that names its own is naming
    // itself, and an operator can pin one through the scope's `headers`
    // like any other. This only fills the silence.
    let named_agent = headers
        .as_ref()
        .is_some_and(|h| h.keys().any(|k| k.eq_ignore_ascii_case("user-agent")))
        || entry
            .headers
            .keys()
            .any(|k| k.eq_ignore_ascii_case("user-agent"));
    if !named_agent {
        out.push_str(&format!(
            "user-agent: drt/{}\r\n",
            env!("CARGO_PKG_VERSION")
        ));
    }
    if let Some(b) = body {
        out.push_str(&format!("content-length: {}\r\n", b.len()));
    }
    out.push_str("\r\n");
    let mut bytes = out.into_bytes();
    if let Some(b) = body {
        bytes.extend_from_slice(b);
    }
    Ok(bytes)
}

/// Parse a response head, applying the bounds. Returns the status, the kept
/// headers, how many bytes of head were consumed, and the body framing.
pub fn parse_head(buf: &[u8]) -> Result<Option<Head>, String> {
    let mut hbuf = [httparse::EMPTY_HEADER; 128];
    let mut resp = httparse::Response::new(&mut hbuf);
    match resp.parse(buf).map_err(|e| format!("status: {e}"))? {
        httparse::Status::Partial => Ok(None),
        httparse::Status::Complete(n) => {
            let status = resp.code.ok_or("status: the response carries no code")?;
            let mut kept = Vec::new();
            for h in resp.headers.iter() {
                if kept.len() >= limits::RESP_HDRS {
                    break;
                }
                let name = h.name.to_ascii_lowercase();
                let Ok(value) = std::str::from_utf8(h.value) else {
                    continue;
                };
                // Dropped whole, never truncated.
                if name.len() > limits::RESP_NAME_MAX || value.len() > limits::RESP_VAL_MAX {
                    continue;
                }
                kept.push((name, value.to_string()));
            }
            Ok(Some(Head {
                status,
                headers: kept,
                consumed: n,
                body_end: framing(status, resp.headers)?,
            }))
        }
    }
}

// depth: which of RFC 9112 §6.3's rules ends this body
//
// Read off every header the response sent rather than off the bounded
// list `parse_head` keeps: a `content-length` past `RESP_HDRS` is still
// the framing, and losing it would send the read loop back to waiting for
// an EOF — which is the failure this whole path exists to avoid.
fn framing(status: u16, headers: &[httparse::Header]) -> Result<BodyEnd, String> {
    let mut length: Option<usize> = None;
    let mut chunked = false;
    for h in headers {
        if h.name.eq_ignore_ascii_case("transfer-encoding") {
            chunked |= String::from_utf8_lossy(h.value)
                .to_ascii_lowercase()
                .contains("chunked");
        } else if h.name.eq_ignore_ascii_case("content-length") {
            let n = std::str::from_utf8(h.value)
                .ok()
                .and_then(|v| v.trim().parse::<usize>().ok())
                .ok_or("status: the response's content-length is not a number")?;
            // Two that disagree is the request-smuggling shape, and
            // picking one of them is how a reader gets played off against
            // the next hop. There is no next hop here, and it is still not
            // ours to guess.
            if length.is_some_and(|prev| prev != n) {
                return Err("status: the response carries two different content-lengths".into());
            }
            length = Some(n);
        }
    }
    // No body at all for these, whatever the headers say.
    if (100..200).contains(&status) || status == 204 || status == 304 {
        return Ok(BodyEnd::Length(0));
    }
    // §6.1: when a message carries both, `transfer-encoding` wins and
    // `content-length` is discarded.
    if chunked {
        return Ok(BodyEnd::Chunked);
    }
    Ok(length.map_or(BodyEnd::Close, BodyEnd::Length))
}

/// The head that answers the request: the first that is not interim.
///
/// RFC 9110 §15.2 — a client must parse a 1xx and carry on to the response
/// that follows it. Unprompted ones are ordinary (103 Early Hints, from
/// more than one CDN), and since the loop below now stops the moment a
/// head says the body is complete, taking one as the answer would hand the
/// guest an empty body and a status the server never meant as its reply.
///
/// `consumed` counts from the start of `buf`, interim heads included, so
/// the body still begins where it says.
fn final_head(buf: &[u8]) -> Result<Option<Head>, String> {
    let mut at = 0;
    loop {
        let Some(h) = parse_head(&buf[at..])? else {
            return Ok(None);
        };
        if (100..200).contains(&h.status) {
            at += h.consumed;
            continue;
        }
        return Ok(Some(Head {
            consumed: at + h.consumed,
            ..h
        }));
    }
}

async fn fetch(
    url: &Url,
    method: &str,
    headers: &Option<Headers>,
    body: Option<&[u8]>,
    scope: &RestScope,
    entry: &AllowEntry,
) -> Result<Response, String> {
    let request = request_bytes(url, method, headers, body, entry)?;

    // Resolve first and check every candidate, then connect to one we
    // checked — not to a name we would resolve again. Re-resolving after
    // the check is the rebinding window.
    let addrs: Vec<std::net::SocketAddr> = tokio::net::lookup_host((url.host.as_str(), url.port))
        .await
        .map_err(|_| "dns".to_string())?
        .collect();
    if addrs.is_empty() {
        return Err("dns".into());
    }
    let Some(addr) = addrs
        .iter()
        .copied()
        .find(|a| scope.permits_address(a.ip()))
    else {
        return Err(format!(
            "'{}' resolves only into private address space, which this instance was not granted (allow_private)",
            url.host
        ));
    };

    let stream = tokio::net::TcpStream::connect(addr)
        .await
        .map_err(|e| format!("connect: {e}"))?;

    if url.tls {
        let mut roots = tokio_rustls::rustls::RootCertStore::empty();
        roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        // `builder()` resolves the crypto provider from rustls's own enabled
        // features and panics if more than one is on. Checked, not assumed:
        // `cargo tree -e features -i rustls` shows exactly `ring`, `std`,
        // `tls12` on this workspace. `aws-lc-rs` IS in the graph -- russh
        // pulls it -- but not as a rustls provider feature, so there is no
        // ambiguity to resolve and no panic to hit.
        //
        // It would become one the moment something enables rustls's
        // `aws-lc-rs` feature, and the failure would be a runtime panic in a
        // shipped connector rather than a build error. If that ever happens,
        // the fix is `builder_with_provider(ring::default_provider())`.
        let config = tokio_rustls::rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();
        let name = tokio_rustls::rustls::pki_types::ServerName::try_from(url.host.clone())
            .map_err(|_| "tls".to_string())?;
        let connector = tokio_rustls::TlsConnector::from(std::sync::Arc::new(config));
        let tls = connector
            .connect(name, stream)
            .await
            .map_err(|_| "tls".to_string())?;
        exchange(tls, &request).await
    } else {
        exchange(stream, &request).await
    }
}

async fn exchange<S: AsyncReadExt + AsyncWriteExt + Unpin>(
    mut sock: S,
    request: &[u8],
) -> Result<Response, String> {
    sock.write_all(request)
        .await
        .map_err(|e| format!("send: {e}"))?;
    sock.flush().await.map_err(|e| format!("send: {e}"))?;

    let mut buf = Vec::new();
    let mut chunk = [0u8; 16 * 1024];
    let mut head: Option<Head> = None;
    // A chunked response is framing, not content. Fed as bytes land rather
    // than decoded at the end, because the loop below asks after every read
    // whether the body has ended and only the decoder knows.
    let mut chunks = Dechunker::default();
    // The peer dropped the connection without the TLS `close_notify` that
    // frames a clean end. Recorded rather than raised: it is an error only
    // for a body the framing says is unfinished, and a great many servers
    // -- every one behind Amazon Bedrock among them -- end a complete
    // response exactly this way.
    let mut unclean = false;
    loop {
        // Stop where the framing says the body ends, never at EOF. Reading
        // on past a body already complete is issue #10: every byte had
        // arrived, and the read that failed was the one after the last one
        // that mattered.
        if let Some(h) = &head {
            let done = match h.body_end {
                BodyEnd::Length(n) => buf.len() - h.consumed.min(buf.len()) >= n,
                BodyEnd::Chunked => chunks.done,
                BodyEnd::Close => false,
            };
            if done {
                break;
            }
        }
        let n = match sock.read(&mut chunk).await {
            Ok(n) => n,
            // What rustls answers for a peer that closed the connection
            // without a `close_notify`. Whether it matters is the framing's
            // question, asked after the loop.
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                unclean = true;
                0
            }
            Err(e) => return Err(format!("read: {e}")),
        };
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
        if head.is_none() {
            head = final_head(&buf)?;
        }
        if let Some(h) = &head {
            // A length we would refuse is refused now, rather than after
            // reading the megabytes it names.
            if let BodyEnd::Length(n) = h.body_end {
                if n > limits::MAX_BODY {
                    return Err("too_large".into());
                }
            }
            if h.body_end == BodyEnd::Chunked {
                chunks.feed(&buf[h.consumed.min(buf.len())..])?;
            }
        }
        // Bound the whole transfer, head included, so a response that never
        // ends cannot grow this buffer without limit.
        if buf.len() > limits::MAX_BODY + 64 * 1024 {
            return Err("too_large".into());
        }
    }
    let Some(head) = head.or(final_head(&buf)?) else {
        return Err("status: the response head never completed".into());
    };
    let raw = &buf[head.consumed.min(buf.len())..];
    let body = match head.body_end {
        BodyEnd::Length(n) => {
            if raw.len() < n {
                return Err(short_body(raw.len(), Some(n), unclean));
            }
            // Anything past the length is not this response's.
            raw[..n].to_vec()
        }
        // The head may have completed on the very last read, leaving frames
        // unfed; `feed` resumes where it stopped, so this is a no-op when it
        // did not. Without the terminating chunk the body is short, which is
        // the same silent-corruption shape as handing the guest
        // `5\r\nhello\r\n6\r\n world\r\n0\r\n\r\n` and answering `ok`.
        BodyEnd::Chunked => {
            chunks.feed(raw)?;
            if !chunks.done {
                return Err(short_body(raw.len(), None, unclean));
            }
            chunks.out
        }
        // The close is the framing, so the close ends the body -- with or
        // without `close_notify`, there being nothing here that could tell
        // the two apart.
        BodyEnd::Close => raw.to_vec(),
    };
    if body.len() > limits::MAX_BODY {
        return Err("too_large".into());
    }
    Ok(Response {
        status: head.status,
        headers: head.headers,
        body,
    })
}

/// What a body that ended early is reported as.
///
/// It names what was expected and what arrived because the rustls error it
/// replaces named neither: "the connection is broken" reads as a network
/// or credential problem, and issue #10 spent an afternoon there before
/// reaching this loop. `want` is the `content-length`, or `None` for a
/// chunked body that never reached its final chunk.
fn short_body(got: usize, want: Option<usize>, unclean: bool) -> String {
    let close = if unclean {
        "the peer closed without close_notify"
    } else {
        "the peer closed"
    };
    match want {
        Some(n) => format!("read: read {got} of {n} bytes, then {close}"),
        None => {
            format!("read: the chunked body reached no final chunk in {got} bytes, then {close}")
        }
    }
}

// depth: chunked transfer decoding (RFC 9112 §7.1), resumable
//
// Deliberately strict. A frame this cannot read is an error rather than a
// best-effort salvage, because the failure being fixed here is exactly a
// body that looked fine and was not -- answering half a decode with `ok`
// would be the same bug wearing a different hat. Trailers are read and
// discarded: they are headers, the head is already parsed, and admitting a
// second header block after the bounds were applied would let a response
// past `RESP_HDRS`.
//
// Resumable because `exchange` asks after every read whether the body has
// ended, and re-decoding the whole buffer to answer would be quadratic in
// the size of a 16 MB response. A frame that has not arrived whole is not
// an error -- it is a frame to wait for.
#[derive(Default)]
struct Dechunker {
    /// The decoded body so far.
    out: Vec<u8>,
    /// How far into the raw body whole frames have been read. The next
    /// `feed` starts here.
    at: usize,
    /// The zero-size chunk arrived, so `out` is the whole body.
    done: bool,
}

impl Dechunker {
    /// Read every whole frame `raw` holds beyond what earlier calls read.
    /// `raw` is the same body, extended: each call must be given the bytes
    /// the last one saw plus whatever has arrived since.
    fn feed(&mut self, raw: &[u8]) -> Result<(), String> {
        while !self.done {
            let rest = &raw[self.at.min(raw.len())..];
            // The size line, once it has arrived whole.
            let Some(eol) = rest.windows(2).position(|w| w == b"\r\n") else {
                return Ok(());
            };
            // `size` may carry chunk extensions after a `;`, which are ignored.
            let head = &rest[..eol];
            let size_text = match head.iter().position(|&b| b == b';') {
                Some(at) => &head[..at],
                None => head,
            };
            let size_text = std::str::from_utf8(size_text)
                .map_err(|_| "chunked: a chunk size was not text".to_string())?
                .trim();
            let size = usize::from_str_radix(size_text, 16)
                .map_err(|_| format!("chunked: '{size_text}' is not a chunk size"))?;
            let data = eol + 2;
            if size == 0 {
                self.at += data;
                self.done = true;
                return Ok(());
            }
            if self.out.len() + size > limits::MAX_BODY {
                return Err("too_large".into());
            }
            // The chunk, and the CRLF its size promises after it.
            if rest.len() < data + size + 2 {
                return Ok(());
            }
            if &rest[data + size..data + size + 2] != b"\r\n" {
                return Err("chunked: a chunk did not end where its size said".into());
            }
            self.out.extend_from_slice(&rest[data..data + size]);
            self.at += data + size + 2;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scope_of(v: rmpv::Value) -> Scope {
        Scope(v)
    }

    /// The decoder over a body that has already arrived whole. Production
    /// feeds `Dechunker` as bytes land; these tests are about the frames,
    /// so they hand it all of them at once. Stopping short of the final
    /// chunk is a truncation here, which is what `exchange` also calls it.
    fn dechunk(raw: &[u8]) -> Result<Vec<u8>, String> {
        let mut d = Dechunker::default();
        d.feed(raw)?;
        if !d.done {
            return Err("chunked: the body reached no final chunk".into());
        }
        Ok(d.out)
    }

    /// A bare entry, for the request-building tests.
    fn open_entry(origin: &str) -> AllowEntry {
        AllowEntry {
            origin: Url::parse(origin).unwrap(),
            headers: Default::default(),
            allow_headers: None,
        }
    }

    #[test]
    fn a_url_splits_the_way_the_c_plugin_splits_it() {
        let u = Url::parse("https://api.example.com/v1/things?x=1").unwrap();
        assert!(u.tls);
        assert_eq!(u.host, "api.example.com");
        assert_eq!(u.port, 443);
        assert_eq!(u.path, "/v1/things?x=1");
        assert_eq!(Url::parse("http://h").unwrap().port, 80);
        assert_eq!(Url::parse("http://h:8080/").unwrap().port, 8080);
        // A bare host with no path still asks for "/".
        assert_eq!(Url::parse("https://h").unwrap().path, "/");
    }

    #[test]
    fn a_non_http_scheme_is_refused_by_name() {
        for bad in ["file:///etc/passwd", "gopher://x/", "//x/", "x"] {
            let e = Url::parse(bad).unwrap_err();
            assert!(e.contains("http://"), "{bad}: {e}");
        }
    }

    #[test]
    fn userinfo_is_refused_rather_than_ignored() {
        // `https://api.example.com@169.254.169.254/` has authority
        // 169.254.169.254 and reads to a human as api.example.com. Dropping
        // the userinfo silently would authorise against the wrong host.
        let e = Url::parse("https://api.example.com@169.254.169.254/").unwrap_err();
        assert!(e.contains("userinfo"), "{e}");
    }

    #[test]
    fn an_absent_scope_permits_nothing() {
        let sc = RestScope::parse(None).unwrap();
        assert!(!sc.permits(&Url::parse("https://api.example.com/").unwrap()));
    }

    #[test]
    fn the_allowlist_matches_scheme_host_and_port_together() {
        let sc = RestScope::parse(Some(&scope_of(rmpv::Value::from(
            "https://api.example.com",
        ))))
        .unwrap();
        assert!(sc.permits(&Url::parse("https://api.example.com/v1").unwrap()));
        // Same host, plaintext: a different origin, and downgrading is how
        // an allowlisted API becomes an interceptable one.
        assert!(!sc.permits(&Url::parse("http://api.example.com/v1").unwrap()));
        // Same host, different port.
        assert!(!sc.permits(&Url::parse("https://api.example.com:8443/").unwrap()));
        // A neighbour that merely shares a suffix.
        assert!(!sc.permits(&Url::parse("https://evil-api.example.com/").unwrap()));
        assert!(!sc.permits(&Url::parse("https://api.example.com.evil.test/").unwrap()));
    }

    #[test]
    fn a_wildcard_matches_subdomains_but_never_the_bare_parent() {
        let sc =
            RestScope::parse(Some(&scope_of(rmpv::Value::from("https://*.example.com")))).unwrap();
        assert!(sc.permits(&Url::parse("https://api.example.com/").unwrap()));
        assert!(sc.permits(&Url::parse("https://a.b.example.com/").unwrap()));
        assert!(
            !sc.permits(&Url::parse("https://example.com/").unwrap()),
            "a wildcard that widens by one is an allowlist that means nothing"
        );
        assert!(!sc.permits(&Url::parse("https://notexample.com/").unwrap()));
    }

    #[test]
    fn private_space_is_refused_even_when_the_name_is_allowed() {
        // The rebinding shape: the allowlist says yes, the address says no.
        let sc =
            RestScope::parse(Some(&scope_of(rmpv::Value::from("http://metadata.test")))).unwrap();
        assert!(sc.permits(&Url::parse("http://metadata.test/").unwrap()));
        for bad in [
            "169.254.169.254", // cloud metadata
            "127.0.0.1",
            "10.0.0.5",
            "192.168.1.1",
            "172.16.0.1",
            "100.64.0.1", // CGNAT
            "0.0.0.0",
        ] {
            assert!(
                !sc.permits_address(bad.parse().unwrap()),
                "{bad} must be refused"
            );
        }
        // A genuinely routable address passes. Note 203.0.113.x does NOT:
        // it is TEST-NET-3, which is documentation space and not routable,
        // so refusing it is correct even though the netcheck spec uses it
        // as a stand-in for a public address.
        assert!(sc.permits_address("93.184.216.34".parse().unwrap()));
        assert!(
            !sc.permits_address("203.0.113.7".parse().unwrap()),
            "documentation space is not routable"
        );
    }

    #[test]
    fn a_v4_mapped_v6_address_is_judged_by_the_v4_it_carries() {
        let sc = RestScope::default();
        assert!(!sc.permits_address("::ffff:169.254.169.254".parse().unwrap()));
        assert!(!sc.permits_address("::1".parse().unwrap()));
        assert!(!sc.permits_address("fe80::1".parse().unwrap()));
        assert!(!sc.permits_address("fd00::1".parse().unwrap()));
    }

    #[test]
    fn allow_private_is_opt_in_and_says_so() {
        let v = rmpv::Value::Map(vec![
            (
                rmpv::Value::from("allow"),
                rmpv::Value::Array(vec![rmpv::Value::from("http://db.internal")]),
            ),
            (rmpv::Value::from("allow_private"), rmpv::Value::from(true)),
        ]);
        let sc = RestScope::parse(Some(&scope_of(v))).unwrap();
        assert!(sc.permits_address("10.0.0.5".parse().unwrap()));
    }

    #[test]
    fn an_empty_allowlist_is_a_startup_refusal_not_a_runtime_surprise() {
        let e = RestScopeType
            .validate(Some(&scope_of(rmpv::Value::Array(vec![]))))
            .unwrap_err();
        assert!(e.contains("grants no origins"), "{e}");
        assert!(RestScopeType
            .validate(Some(&scope_of(rmpv::Value::from("https://a.test"))))
            .is_ok());
    }

    #[test]
    fn a_header_carrying_crlf_cannot_write_its_own_request_line() {
        let url = Url::parse("http://h/").unwrap();
        let mut h = std::collections::BTreeMap::new();
        h.insert("x-evil".to_string(), "a\r\nGET /admin HTTP/1.1".to_string());
        let e = request_bytes(&url, "GET", &Some(h), None, &open_entry("http://h")).unwrap_err();
        assert!(e.contains("forbidden character"), "{e}");
    }

    #[test]
    fn the_framing_headers_are_the_connectors_to_set() {
        let url = Url::parse("http://h/").unwrap();
        for name in ["content-length", "Transfer-Encoding", "Host", "connection"] {
            let mut h = std::collections::BTreeMap::new();
            h.insert(name.to_string(), "1".to_string());
            let e = request_bytes(&url, "POST", &Some(h), Some(b"x"), &open_entry("http://h"))
                .unwrap_err();
            assert!(e.contains("set by the connector"), "{name}: {e}");
        }
    }

    #[test]
    fn the_request_carries_host_close_and_a_length_for_a_body() {
        let url = Url::parse("http://h:8080/p").unwrap();
        let out = String::from_utf8(
            request_bytes(
                &url,
                "POST",
                &None,
                Some(b"hello"),
                &open_entry("http://h:8080"),
            )
            .unwrap(),
        )
        .unwrap();
        assert!(out.starts_with("POST /p HTTP/1.1\r\n"), "{out}");
        assert!(out.contains("host: h:8080\r\n"), "{out}");
        assert!(out.contains("connection: close\r\n"), "{out}");
        assert!(out.contains("content-length: 5\r\n"), "{out}");
        assert!(out.ends_with("\r\n\r\nhello"), "{out}");
        // The default port is omitted, which is what a vhost matches on.
        let d = String::from_utf8(
            request_bytes(
                &Url::parse("https://h/").unwrap(),
                "GET",
                &None,
                None,
                &open_entry("https://h"),
            )
            .unwrap(),
        )
        .unwrap();
        assert!(d.contains("host: h\r\n"), "{d}");
    }

    #[test]
    fn an_oversized_response_header_is_dropped_whole_never_truncated() {
        let long_value = "v".repeat(limits::RESP_VAL_MAX + 1);
        let raw = format!(
            "HTTP/1.1 302 Found\r\nlocation: /next\r\nx-big: {long_value}\r\ncontent-type: text/plain\r\n\r\nbody"
        );
        let head = parse_head(raw.as_bytes()).unwrap().unwrap();
        assert_eq!(head.status, 302);
        assert!(head
            .headers
            .iter()
            .any(|(n, v)| n == "location" && v == "/next"));
        assert!(
            !head.headers.iter().any(|(n, _)| n == "x-big"),
            "an oversized header is dropped, not clipped"
        );
        assert!(head.headers.iter().any(|(n, _)| n == "content-type"));
        assert_eq!(&raw.as_bytes()[head.consumed..], b"body");
    }

    #[test]
    fn a_partial_head_is_not_an_error() {
        assert!(parse_head(b"HTTP/1.1 200 OK\r\nx: y\r\n")
            .unwrap()
            .is_none());
    }

    fn scope_with_terms() -> RestScope {
        let entry = rmpv::Value::Map(vec![
            (
                rmpv::Value::from("origin"),
                rmpv::Value::from("https://billing.example.com"),
            ),
            (
                rmpv::Value::from("headers"),
                rmpv::Value::Map(vec![(
                    rmpv::Value::from("X-Api-Key"),
                    rmpv::Value::from("sk_live_secret"),
                )]),
            ),
            (
                rmpv::Value::from("allow_headers"),
                rmpv::Value::Array(vec![
                    rmpv::Value::from("accept"),
                    rmpv::Value::from("Content-Type"),
                ]),
            ),
        ]);
        RestScope::parse(Some(&scope_of(rmpv::Value::Array(vec![entry])))).unwrap()
    }

    #[test]
    fn an_operator_header_is_injected_and_the_guest_never_supplies_it() {
        let sc = scope_with_terms();
        let url = Url::parse("https://billing.example.com/v1/charges").unwrap();
        let entry = sc.matching(&url).expect("granted");
        let out =
            String::from_utf8(request_bytes(&url, "GET", &None, None, entry).unwrap()).unwrap();
        assert!(
            out.contains("x-api-key: sk_live_secret\r\n"),
            "the credential is the connector's to add: {out}"
        );
    }

    #[test]
    fn a_guest_cannot_override_an_injected_header() {
        // Overriding an injected credential would undo the entire reason
        // for injecting it, so this refuses rather than losing quietly.
        let sc = scope_with_terms();
        let url = Url::parse("https://billing.example.com/v1").unwrap();
        let entry = sc.matching(&url).unwrap();
        let mut h = std::collections::BTreeMap::new();
        h.insert("X-API-KEY".to_string(), "mine".to_string());
        let e = request_bytes(&url, "GET", &Some(h), None, entry).unwrap_err();
        assert!(e.contains("not one this instance may set"), "{e}");
    }

    #[test]
    fn an_injected_header_is_protected_even_with_no_allow_headers_list() {
        // Without this case the override test above passes for the wrong
        // reason: `allow_headers` catches x-api-key before the injected-
        // header guard ever runs, so deleting that guard breaks nothing.
        // Found by deleting it and watching the suite stay green.
        let entry = rmpv::Value::Map(vec![
            (
                rmpv::Value::from("origin"),
                rmpv::Value::from("https://a.test"),
            ),
            (
                rmpv::Value::from("headers"),
                rmpv::Value::Map(vec![(
                    rmpv::Value::from("authorization"),
                    rmpv::Value::from("Bearer operator-token"),
                )]),
            ),
            // No allow_headers: the guest may set anything unreserved.
        ]);
        let sc = RestScope::parse(Some(&scope_of(rmpv::Value::Array(vec![entry])))).unwrap();
        let url = Url::parse("https://a.test/").unwrap();
        let e = sc.matching(&url).unwrap();

        // Anything else is fine here.
        let mut fine = std::collections::BTreeMap::new();
        fine.insert("x-trace".to_string(), "1".to_string());
        assert!(request_bytes(&url, "GET", &Some(fine), None, e).is_ok());

        // The injected one is not, in any casing.
        for name in ["authorization", "Authorization", "AUTHORIZATION"] {
            let mut h = std::collections::BTreeMap::new();
            h.insert(name.to_string(), "Bearer stolen".to_string());
            let err = request_bytes(&url, "GET", &Some(h), None, e).unwrap_err();
            assert!(
                err.contains("not one this instance may set"),
                "{name}: {err}"
            );
        }

        // And the operator's value is the one on the wire.
        let out = String::from_utf8(request_bytes(&url, "GET", &None, None, e).unwrap()).unwrap();
        assert!(
            out.contains("authorization: Bearer operator-token\r\n"),
            "{out}"
        );
    }

    #[test]
    fn allow_headers_is_exhaustive_and_case_insensitive() {
        let sc = scope_with_terms();
        let url = Url::parse("https://billing.example.com/v1").unwrap();
        let entry = sc.matching(&url).unwrap();

        // Listed, in any casing: allowed.
        let mut ok = std::collections::BTreeMap::new();
        ok.insert("Accept".to_string(), "application/json".to_string());
        let out =
            String::from_utf8(request_bytes(&url, "GET", &Some(ok), None, entry).unwrap()).unwrap();
        assert!(out.contains("accept: application/json\r\n"), "{out}");

        // Not listed: refused by name, naming the origin.
        let mut no = std::collections::BTreeMap::new();
        no.insert("x-trace".to_string(), "1".to_string());
        let e = request_bytes(&url, "GET", &Some(no), None, entry).unwrap_err();
        assert!(e.contains("billing.example.com"), "{e}");
    }

    #[test]
    fn without_allow_headers_a_guest_may_set_anything_unreserved() {
        let sc = RestScope::parse(Some(&scope_of(rmpv::Value::from(
            "https://api.example.com",
        ))))
        .unwrap();
        let url = Url::parse("https://api.example.com/").unwrap();
        let entry = sc.matching(&url).unwrap();
        let mut h = std::collections::BTreeMap::new();
        h.insert("x-anything".to_string(), "yes".to_string());
        let out =
            String::from_utf8(request_bytes(&url, "GET", &Some(h), None, entry).unwrap()).unwrap();
        assert!(out.contains("x-anything: yes\r\n"), "{out}");
    }

    #[test]
    fn an_injected_header_may_not_be_a_reserved_one() {
        // `{headers = {host = "elsewhere"}}` would aim the request at
        // another vhost from inside the allowlist. Refused at parse.
        let entry = rmpv::Value::Map(vec![
            (
                rmpv::Value::from("origin"),
                rmpv::Value::from("https://a.test"),
            ),
            (
                rmpv::Value::from("headers"),
                rmpv::Value::Map(vec![(
                    rmpv::Value::from("Host"),
                    rmpv::Value::from("elsewhere.test"),
                )]),
            ),
        ]);
        let e = RestScope::parse(Some(&scope_of(rmpv::Value::Array(vec![entry])))).unwrap_err();
        assert!(e.contains("set by the connector"), "{e}");
    }

    #[test]
    fn response_headers_come_back_to_the_guest() {
        let raw =
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\nx-request-id: abc\r\n\r\n{}";
        let head = parse_head(raw.as_bytes()).unwrap().unwrap();
        assert_eq!(head.status, 200);
        assert!(head
            .headers
            .iter()
            .any(|(n, v)| n == "x-request-id" && v == "abc"));
        assert!(head
            .headers
            .iter()
            .any(|(n, v)| n == "content-type" && v == "application/json"));
    }

    #[tokio::test]
    async fn an_ungranted_origin_is_refused_before_any_socket_is_opened() {
        let c = RestConnector::new();
        let sc = Scope(rmpv::Value::from("https://api.example.com"));
        let args = rmpv::Value::Map(vec![(
            rmpv::Value::from("url"),
            // Would be a connect to loopback if the scope were not checked.
            rmpv::Value::from("http://127.0.0.1:1/"),
        )]);
        let e = c.call("rest/get", Some(args), Some(&sc)).await.unwrap_err();
        assert!(
            e.0.contains("outside this instance's granted origins"),
            "{}",
            e.0
        );
    }

    #[tokio::test]
    async fn an_unknown_call_in_the_family_is_an_error_not_a_panic() {
        let c = RestConnector::new();
        let e = c.call("rest/put", None, None).await.unwrap_err();
        assert!(e.0.contains("is not a rest call"), "{}", e.0);
    }

    /// Chunked responses were handed to the guest as framing.
    ///
    /// `exchange` read to EOF and took everything after the head verbatim,
    /// so a chunked body arrived as `5\r\nhello\r\n6\r\n world\r\n0\r\n\r\n`
    /// — answered `ok`. The same silent-corruption shape as the SQL
    /// transaction bug: every layer above believes the reply.
    ///
    /// Reproduced against a real socket before this was written; these pin
    /// the decoder itself, including the frames it must refuse rather than
    /// salvage, because a half-decode answered `ok` is the bug again.
    #[test]
    fn chunked_bodies_are_decoded_and_bad_frames_refused() {
        assert_eq!(
            dechunk(b"5\r\nhello\r\n6\r\n world\r\n0\r\n\r\n").unwrap(),
            b"hello world"
        );

        // A single chunk, and the empty body, which is a bare terminator.
        assert_eq!(dechunk(b"3\r\nabc\r\n0\r\n\r\n").unwrap(), b"abc");
        assert_eq!(dechunk(b"0\r\n\r\n").unwrap(), b"");

        // Chunk extensions after `;` are ignored, per RFC 9112.
        assert_eq!(
            dechunk(b"3;name=value\r\nabc\r\n0\r\n\r\n").unwrap(),
            b"abc"
        );

        // Hex, and upper case hex, because a size is base 16 and servers
        // differ on case. 16 bytes.
        assert_eq!(
            dechunk(b"10\r\n0123456789abcdef\r\n0\r\n\r\n")
                .unwrap()
                .len(),
            16
        );
        assert_eq!(dechunk(b"A\r\n0123456789\r\n0\r\n\r\n").unwrap().len(), 10);

        // Trailers after the terminator are read and discarded: the head is
        // already parsed and bounded, and a second header block arriving
        // here would slip past RESP_HDRS.
        assert_eq!(
            dechunk(b"3\r\nabc\r\n0\r\nx-trailer: v\r\n\r\n").unwrap(),
            b"abc"
        );

        // And the refusals. Each of these used to be handed to the guest as
        // a body with `ok` beside it.
        for bad in [
            &b"3\r\nab\r\n0\r\n\r\n"[..],   // shorter than its size
            &b"3\r\nabcX0\r\n\r\n"[..],     // no CRLF where the size said
            &b"zz\r\nabc\r\n0\r\n\r\n"[..], // not a hex size
            &b"3\r\nabc\r\n"[..],           // truncated: no terminator
            &b"3"[..],                      // a header that never ended
        ] {
            assert!(
                dechunk(bad).is_err(),
                "must refuse rather than salvage: {:?}",
                String::from_utf8_lossy(bad)
            );
        }
    }

    /// A socket that answers `bytes` and then fails the way rustls does for
    /// a peer that closed the TCP connection without a TLS `close_notify`.
    ///
    /// The whole of issue #10, and no TCP test can produce it: the error is
    /// raised by the TLS layer, over a connection that carried every byte
    /// it promised. Amazon Bedrock ends every response this way, so every
    /// `rest/post` from a guest failed against a server answering 200.
    struct UncleanClose {
        bytes: Vec<u8>,
        at: usize,
    }

    impl UncleanClose {
        fn new(bytes: &[u8]) -> Self {
            UncleanClose {
                bytes: bytes.to_vec(),
                at: 0,
            }
        }
    }

    impl tokio::io::AsyncRead for UncleanClose {
        fn poll_read(
            mut self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
            buf: &mut tokio::io::ReadBuf<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            if self.at >= self.bytes.len() {
                return std::task::Poll::Ready(Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "peer closed connection without sending TLS close_notify",
                )));
            }
            let n = buf.remaining().min(self.bytes.len() - self.at);
            let at = self.at;
            buf.put_slice(&self.bytes[at..at + n]);
            self.at += n;
            std::task::Poll::Ready(Ok(()))
        }
    }

    impl tokio::io::AsyncWrite for UncleanClose {
        fn poll_write(
            self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
            buf: &[u8],
        ) -> std::task::Poll<std::io::Result<usize>> {
            std::task::Poll::Ready(Ok(buf.len()))
        }
        fn poll_flush(
            self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Ready(Ok(()))
        }
        fn poll_shutdown(
            self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Ready(Ok(()))
        }
    }

    const REQ: &[u8] = b"GET / HTTP/1.1\r\nhost: x\r\nconnection: close\r\n\r\n";

    /// Issue #10: a complete response, failed for how the peer hung up.
    ///
    /// The bytes have all arrived and `content-length` says so. The read
    /// that used to fail was the one *after* the last one that mattered --
    /// so the fix is that there is no such read, not that its error is
    /// swallowed. Both framings, because a server picks either.
    #[tokio::test]
    async fn a_complete_body_survives_a_close_without_close_notify() {
        let sock = UncleanClose::new(b"HTTP/1.1 200 OK\r\ncontent-length: 11\r\n\r\nhello world");
        let r = exchange(sock, REQ).await.unwrap();
        assert_eq!(r.status, 200);
        assert_eq!(r.body, b"hello world");

        let sock = UncleanClose::new(
            b"HTTP/1.1 200 OK\r\ntransfer-encoding: chunked\r\n\r\n5\r\nhello\r\n6\r\n world\r\n0\r\n\r\n",
        );
        assert_eq!(exchange(sock, REQ).await.unwrap().body, b"hello world");
    }

    /// With no length and no chunking the close *is* the framing, so the
    /// close ends the body -- politely framed or not. There is nothing here
    /// that could tell a truncation from an ending, which is the reason
    /// `content-length` exists and the reason the test below can be strict.
    #[tokio::test]
    async fn a_close_framed_body_takes_the_close_as_its_end() {
        let sock = UncleanClose::new(b"HTTP/1.1 200 OK\r\ncontent-type: text/plain\r\n\r\nhello");
        assert_eq!(exchange(sock, REQ).await.unwrap().body, b"hello");
    }

    /// A body genuinely cut short still fails, and says what it counted.
    ///
    /// The rustls message this replaces named neither the expectation nor
    /// the arrival, so it read as a broken network and sent the reporter to
    /// the credentials for an afternoon. These assertions are on the words.
    #[tokio::test]
    async fn a_truncated_body_fails_naming_what_was_expected() {
        let sock = UncleanClose::new(b"HTTP/1.1 200 OK\r\ncontent-length: 11\r\n\r\nhello");
        let e = exchange(sock, REQ).await.unwrap_err();
        assert!(e.contains("read 5 of 11 bytes"), "{e}");
        assert!(e.contains("without close_notify"), "{e}");

        let sock = UncleanClose::new(
            b"HTTP/1.1 200 OK\r\ntransfer-encoding: chunked\r\n\r\n5\r\nhello\r\n",
        );
        let e = exchange(sock, REQ).await.unwrap_err();
        assert!(e.contains("no final chunk"), "{e}");
    }

    /// The framing ends the read, so a connection left open after a
    /// complete body does not hold the call until its timeout.
    ///
    /// The server here answers and then says nothing and closes nothing,
    /// which is what a peer ignoring `connection: close` does. Before this,
    /// every such call took `timeout_ms` and then failed.
    #[tokio::test]
    async fn a_framed_body_returns_without_waiting_for_a_close() {
        for (head, wire, want) in [
            (
                "content-length: 11",
                &b"hello world"[..],
                &b"hello world"[..],
            ),
            (
                "transfer-encoding: chunked",
                &b"5\r\nhello\r\n6\r\n world\r\n0\r\n\r\n"[..],
                &b"hello world"[..],
            ),
            // 204 carries no body whatever its headers say, so there is
            // nothing to wait for either.
            ("content-type: text/plain", &b""[..], &b""[..]),
        ] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let port = listener.local_addr().unwrap().port();
            let status = if wire.is_empty() {
                "204 No Content"
            } else {
                "200 OK"
            };
            let response = format!("HTTP/1.1 {status}\r\n{head}\r\n\r\n").into_bytes();
            let wire = wire.to_vec();
            let held = tokio::spawn(async move {
                let (mut sock, _) = listener.accept().await.unwrap();
                let mut scratch = [0u8; 1024];
                let _ = sock.read(&mut scratch).await;
                sock.write_all(&response).await.unwrap();
                sock.write_all(&wire).await.unwrap();
                // Never closed: the framing is what has to end the read.
                std::future::pending::<()>().await;
            });
            let sock = tokio::net::TcpStream::connect(("127.0.0.1", port))
                .await
                .unwrap();
            let r = tokio::time::timeout(Duration::from_secs(5), exchange(sock, REQ))
                .await
                .expect("the framing must end the read, not the timeout")
                .unwrap();
            assert_eq!(r.body, want);
            held.abort();
        }
    }

    /// An interim 1xx is not the answer, and stopping on one would now be
    /// easy: it frames a body of zero bytes, which is complete on arrival.
    /// 103 Early Hints is sent unprompted by more than one CDN.
    #[tokio::test]
    async fn an_interim_response_is_skipped_rather_than_answered() {
        let sock = UncleanClose::new(
            b"HTTP/1.1 103 Early Hints\r\nlink: </s.css>; rel=preload\r\n\r\n\
              HTTP/1.1 200 OK\r\ncontent-length: 11\r\n\r\nhello world",
        );
        let r = exchange(sock, REQ).await.unwrap();
        assert_eq!(r.status, 200);
        assert_eq!(r.body, b"hello world");
        assert!(r.headers.iter().any(|(n, _)| n == "content-length"));
    }

    /// The decoder is fed as bytes land, so a frame split across reads must
    /// be resumed rather than re-read or refused.
    #[test]
    fn chunked_frames_split_across_reads_are_resumed() {
        let whole = b"5\r\nhello\r\n6\r\n world\r\n0\r\n\r\n";
        // The body ends at the zero-size chunk's own line; the trailer
        // block after it is read and discarded, so it is not waited for.
        let ends_at = whole.len() - 2;
        for cut in 1..whole.len() {
            let mut d = Dechunker::default();
            d.feed(&whole[..cut]).unwrap();
            assert_eq!(d.done, cut >= ends_at, "cut at {cut}");
            d.feed(whole).unwrap();
            assert!(d.done, "cut at {cut}");
            assert_eq!(d.out, b"hello world", "cut at {cut}");
        }
    }

    /// A response that is not chunked is untouched, so the decoder cannot
    /// corrupt the ordinary path.
    #[tokio::test]
    async fn a_content_length_body_is_not_dechunked() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut scratch = [0u8; 1024];
            let _ = sock.read(&mut scratch).await;
            sock.write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 11\r\n\r\nhello world")
                .await
                .unwrap();
        });
        let sock = tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .unwrap();
        let r = exchange(
            sock,
            b"GET / HTTP/1.1\r\nhost: x\r\nconnection: close\r\n\r\n",
        )
        .await
        .unwrap();
        assert_eq!(r.body, b"hello world");
    }

    /// A request with no identity is a request Cloudflare blocks.
    ///
    /// `api.discofetch.net` is behind Cloudflare, whose rule 1010 refuses
    /// signatures like an unidentified client's — and this connector sent
    /// no `user-agent` at all, so a guest calling that API got an error page
    /// rather than an API from a connector that had said nothing about
    /// itself.
    #[test]
    fn a_default_user_agent_is_sent_and_can_be_overridden() {
        let entry = open_entry("https://api.example");
        let out = String::from_utf8(
            request_bytes(
                &Url::parse("https://api.example/x").unwrap(),
                "GET",
                &None,
                None,
                &entry,
            )
            .unwrap(),
        )
        .unwrap();
        assert!(out.contains("user-agent: drt/"), "{out}");

        // A guest naming its own is naming itself; this only fills silence.
        let mut named = Headers::new();
        named.insert("user-agent".into(), "mine/1".into());
        let out = String::from_utf8(
            request_bytes(
                &Url::parse("https://api.example/x").unwrap(),
                "GET",
                &Some(named),
                None,
                &entry,
            )
            .unwrap(),
        )
        .unwrap();
        assert!(out.contains("user-agent: mine/1"), "{out}");
        assert_eq!(out.matches("user-agent").count(), 1, "not both: {out}");
    }

    /// The v0.4.0 bug, kept red-able.
    ///
    /// Every other test in this file is a `#[tokio::test]`, so every one of
    /// them runs with a reactor — which is why none of them caught a
    /// connector that could not run without one. `drt run` drives connectors
    /// under `pollster::block_on` (run.rs:67), and an allowed URL died with
    /// "there is no reactor running" and exit 101 while every *refusal*
    /// worked, because a refusal is decided before a socket is touched.
    ///
    /// So this one is a plain `#[test]`, and what it asserts is the shape of
    /// the failure rather than its content: dialing a closed port must
    /// return `Err`. Reaching the assertion means it did not panic.
    #[test]
    fn a_call_with_no_reactor_refuses_rather_than_panicking() {
        // A listener that never accepts, rather than a closed port. The
        // distinction is the test: a refused connection returns immediately
        // and `tokio::time::timeout` never has to arm its timer, so it never
        // touches the reactor and the bug does not appear. The request has to
        // actually *wait* on something. The kernel completes the handshake
        // from the backlog, the response never comes, and the timer arms.
        //
        // Leaked deliberately: dropping the listener would close the port and
        // turn this back into the case that proves nothing.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::mem::forget(listener);
        let origin = format!("http://127.0.0.1:{port}");
        let sc = scope_of(rmpv::Value::Map(vec![
            (
                rmpv::Value::from("allow"),
                rmpv::Value::Array(vec![rmpv::Value::from(origin.as_str())]),
            ),
            // Loopback is private space, so the allowlist would refuse this
            // before reaching a socket -- which is exactly the path that
            // never showed the bug.
            (rmpv::Value::from("allow_private"), rmpv::Value::from(true)),
        ]));
        let args = rmpv::Value::Map(vec![
            (
                rmpv::Value::from("url"),
                rmpv::Value::from(format!("{origin}/anything").as_str()),
            ),
            // Short, because the timeout firing is the pass condition.
            (rmpv::Value::from("timeout_ms"), rmpv::Value::from(300u64)),
        ]);

        let out = pollster::block_on(RestConnector::new().call("rest/get", Some(args), Some(&sc)));
        out.expect_err("nothing is listening; this cannot succeed");
    }
}
