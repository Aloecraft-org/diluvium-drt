//! The ssmtp connector: `host:ssmtp/send` (SPEC.md §7, Capabilities.md §4).
//!
//! `rest`'s sibling, and built as one. `rest`'s scope is an origin
//! allowlist; this one is a **recipient** allowlist, and it carries the same
//! idea that makes `rest` worth having: the operator's secrets live in the
//! scope, the connector supplies them, and the guest can neither read nor
//! set them. For SMTP those are the credential and the envelope sender — so
//! an app sends mail without ever holding the relay password, and **cannot
//! forge the From line**. That is the whole argument for this being a
//! connector rather than something a program reaches through `rest`.
//!
//! ```text
//! ssmtp/send {to, subject, body, in_reply_to?, references?}
//!     -> {accepted, recipients}
//! ```
//!
//! A reply threads by naming what it answers. `in_reply_to` is the parent's
//! `Message-ID`, as its header spelled it; `references` is the thread,
//! oldest first — the parent's `References` and then its `Message-ID` —
//! and defaults to `in_reply_to`, which is what a reply to a thread's first
//! message carries. Both are the guest's to set because neither routes: a
//! relay reads nothing from them and a client only threads on them, so the
//! check is on shape rather than on trust, in [`msg_ids_of`]. No other
//! header is the guest's to name; [`SsmtpScope::from`] says why.
//!
//! The reference is discofetch's `deploy/mail/df-mail-puller`, which exists
//! precisely because "a guest has no SMTP" (`api/supervisor.lua:5182`): the
//! API composes mail into an outbox and a daemon on another network sends
//! it. Every choice below matches what that puller does, so a deployment can
//! move from the daemon to this without re-learning its relay.
//!
//! ## surface block
//!
//! - [`SsmtpConnector`] — the only entry point; `send` is the only verb.
//! - [`SsmtpScope`] — the wiring: relay, credential, sender, allowlist.
//! - Bounds: [`MAX_BODY_BYTES`], [`MAX_SUBJECT_BYTES`], [`MAX_RECIPIENTS`],
//!   [`MAX_REFERENCES`], [`MAX_MSGID_BYTES`], [`FOLD_AT_BYTES`],
//!   [`DEFAULT_TIMEOUT_MS`], [`DEFAULT_PORT`].
//! - What a call sends is a [`Message`]; its header block is [`headers`],
//!   the one place a header name is written.
//! - The SMTP conversation is one function, [`deliver`], read top to bottom.
//!
//! ## The three things that make this dangerous, and what is done about them
//!
//! 1. **Header injection.** A CR or LF inside `to` or `subject` ends the
//!    header and starts whatever the guest wrote next — a second `Bcc:`, a
//!    forged `From:`, or an early blank line making the rest of the headers
//!    into body. This is *the* SMTP injection vector, and it is refused by
//!    name in [`header_safe`] rather than escaped, because a header value
//!    that wanted a newline is a header value that wanted something else.
//! 2. **Dot-stuffing.** A body line that is exactly `.` ends the DATA phase.
//!    A guest could otherwise terminate the message early and have the rest
//!    of its body executed as SMTP commands. [`dot_stuff`] handles it, and
//!    normalises bare LF to CRLF while it is there, because SMTP lines are
//!    CRLF and a bare LF is what a Lua string will carry.
//! 3. **Credentials in the clear.** AUTH is sent only after STARTTLS has
//!    succeeded. A scope that names a user with `starttls: false` is refused
//!    **at startup**, not at first send: it is a deployment that would put a
//!    password on the wire, and finding that out at 3am is not the deal.

use std::time::Duration;

use base64::Engine;
use serde::Deserialize;

use drt_caps::{Scope, ScopeType};
use drt_connector::{CallError, CallResult, Connector};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

/// Matches `rest`'s MAX_BODY reasoning at a size an email actually is: a
/// relay will refuse far smaller, and the point is to bound the buffer
/// rather than to have an opinion about mail.
pub const MAX_BODY_BYTES: usize = 1024 * 1024;
/// RFC 5322 says 998 octets per line; a subject longer than this is a
/// mistake, not a long subject.
pub const MAX_SUBJECT_BYTES: usize = 998;
/// One `send` is one message. A guest wanting a mailing list can ask twice,
/// and a bound here is what stops one call becoming a blast.
pub const MAX_RECIPIENTS: usize = 16;
/// Ids one call may name in `in_reply_to` or in `references`. A thread
/// longer than this is a program that never trimmed, not a long thread;
/// mail clients trim `References` well short of it.
pub const MAX_REFERENCES: usize = 64;
/// One id must fit on its header's first line under RFC 5322's 998 octets,
/// after the longer of the two field names. An id past that is a mistake,
/// not a long id.
pub const MAX_MSGID_BYTES: usize = 998 - "In-Reply-To: ".len();
/// Where a list of ids folds onto a continuation line: RFC 5322 §2.1.1's
/// SHOULD, and what every client folds at.
pub const FOLD_AT_BYTES: usize = 78;
/// The puller's `timeout=30` (`df-mail-puller:154`).
pub const DEFAULT_TIMEOUT_MS: u64 = 30_000;
/// The submission port. `df-mail-puller` defaults to the same, and its
/// header says "465 is not supported here" — neither is it supported here,
/// for the same reason: implicit TLS is a different opening handshake and
/// nothing in the reference deployment uses it.
pub const DEFAULT_PORT: u16 = 587;

/// The place this connector is wired to: which relay, as whom, from what
/// address, to whom the guest may write.
#[derive(Debug, Clone, Deserialize)]
pub struct SsmtpScope {
    /// The relay. `df-mail-puller`'s `DF_SMTP_HOST`.
    pub host: String,
    #[serde(default)]
    pub port: Option<u16>,
    /// STARTTLS before anything else. Default **true**; `false` only for a
    /// plain relay inside a trusted network, which is the `DF_SMTP_PORT=25`
    /// case the puller documents.
    #[serde(default)]
    pub starttls: Option<bool>,
    /// The credential. Injected by the connector; the guest never holds it
    /// and cannot read it back — `send` answers with recipients, not scope.
    #[serde(default)]
    pub user: Option<String>,
    #[serde(default)]
    pub pass: Option<String>,
    /// The envelope sender and the `From:` header, both. **The guest cannot
    /// set this**, which is the point: an app that could choose its own From
    /// could send mail as anyone the relay will carry.
    pub from: String,
    /// Who this deployment may write to. `@example.com` allows a whole
    /// domain; anything else is an exact address. Empty is a startup
    /// refusal, not a connector that answers nothing.
    pub allow: Vec<String>,
    #[serde(default)]
    pub timeout_ms: Option<u64>,
}

impl SsmtpScope {
    fn parse(scope: Option<&Scope>) -> Result<Self, String> {
        let Some(Scope(value)) = scope else {
            return Err("scope is required".into());
        };
        let parsed: SsmtpScope = rmpv::ext::from_value(value.clone())
            .map_err(|e| format!("scope does not parse: {e}"))?;
        parsed.validate()?;
        Ok(parsed)
    }

    fn validate(&self) -> Result<(), String> {
        if self.host.trim().is_empty() {
            return Err("scope.host is empty".into());
        }
        if self.allow.is_empty() {
            return Err(
                "scope.allow is empty, which would refuse every recipient; name the addresses \
                 or domains this deployment may write to (\"@example.com\" or an exact address)"
                    .into(),
            );
        }
        for entry in &self.allow {
            if entry.trim().is_empty() || !entry.contains('@') {
                return Err(format!(
                    "scope.allow entry {entry:?} is not an address or an @domain"
                ));
            }
        }
        header_safe("from", &self.from)?;
        if self.from.trim().is_empty() {
            return Err("scope.from is empty; a message needs a sender".into());
        }
        // The one that would otherwise be found by tcpdump. AUTH is
        // base64, not encryption, so a credential with no TLS under it is a
        // credential on the wire.
        if self.user.is_some() && !self.starttls.unwrap_or(true) {
            return Err(
                "scope names a user with starttls disabled, which would send the credential \
                 in the clear; either enable starttls or drop user/pass for a relay that \
                 trusts this host's address"
                    .into(),
            );
        }
        if self.user.is_some() != self.pass.is_some() {
            return Err("scope names one of user/pass; AUTH needs both or neither".into());
        }
        if self.timeout_ms == Some(0) {
            return Err("scope.timeout_ms of 0 would refuse every call".into());
        }
        Ok(())
    }

    /// Whether this deployment may write to an address. `@domain` matches
    /// the domain, case-insensitively; anything else must match exactly.
    ///
    /// Deliberately not a pattern language. A glob in an allowlist is a
    /// thing people get subtly wrong, and the two shapes here are what a
    /// deployment actually needs.
    pub fn permits(&self, address: &str) -> bool {
        let addr = address.to_ascii_lowercase();
        self.allow.iter().any(|entry| {
            let entry = entry.to_ascii_lowercase();
            match entry.strip_prefix('@') {
                // `@example.com` allows `a@example.com`, and must NOT allow
                // `a@evil-example.com` or `a@example.com.evil.net`.
                Some(domain) => addr
                    .rsplit_once('@')
                    .is_some_and(|(_, host)| host == domain),
                None => addr == entry,
            }
        })
    }

    fn port(&self) -> u16 {
        self.port.unwrap_or(DEFAULT_PORT)
    }

    fn starttls(&self) -> bool {
        self.starttls.unwrap_or(true)
    }

    fn timeout(&self) -> Duration {
        Duration::from_millis(self.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS))
    }
}

struct SsmtpScopeType;

impl ScopeType for SsmtpScopeType {
    fn describe(&self) -> &str {
        "{host, port?, starttls?, user?, pass?, from, allow: [...], timeout_ms?}"
    }

    fn validate(&self, scope: Option<&Scope>) -> Result<(), String> {
        SsmtpScope::parse(scope).map(|_| ())
    }
}

/// A header value a guest supplied, refused rather than escaped if it
/// carries a line ending or a NUL.
///
/// Escaping would be the wrong call. There is no legitimate newline in a
/// `To:` or a `Subject:` that a guest could mean, so a value carrying one is
/// either a bug or an injection, and both want the same answer.
pub fn header_safe(field: &str, value: &str) -> Result<(), String> {
    if let Some(bad) = value
        .chars()
        .find(|c| *c == '\r' || *c == '\n' || *c == '\0')
    {
        return Err(format!(
            "{field} contains {bad:?}, which would end the header and start another; \
             a header value may not carry a line ending"
        ));
    }
    Ok(())
}

/// CRLF line endings, and a leading `.` on any line doubled.
///
/// A line that is exactly `.` is what ends DATA. Without this a guest could
/// close the message early and have the remainder of its body read as SMTP
/// commands — a second `MAIL FROM` to somewhere the allowlist never saw.
pub fn dot_stuff(body: &str) -> String {
    let mut out = String::with_capacity(body.len() + 16);
    for line in body.replace("\r\n", "\n").split('\n') {
        if line.starts_with('.') {
            out.push('.');
        }
        out.push_str(line);
        out.push_str("\r\n");
    }
    out
}

/// One process-wide runtime for callers that have none, leaked rather than
/// dropped. FM-1 is a use-after-free in tokio's runtime teardown; FM-3 is
/// the panic this exists to avoid — `drt run` drives connectors under
/// `pollster::block_on`, which carries no reactor, and every socket call
/// below needs one. See doc/Failure-Modes.md.
fn own_runtime() -> &'static tokio::runtime::Runtime {
    static RT: std::sync::OnceLock<tokio::runtime::Runtime> = std::sync::OnceLock::new();
    RT.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .expect("the ssmtp connector could not build its own runtime")
    })
}

#[derive(Debug, Deserialize)]
struct SendArgs {
    /// One address or several. A Lua table of strings arrives as an array.
    to: rmpv::Value,
    subject: String,
    body: String,
    /// The `Message-ID` of what this answers: one id or several, as the
    /// text of a copied header or as a list.
    #[serde(default)]
    in_reply_to: Option<rmpv::Value>,
    /// The thread, oldest first. Absent, it is `in_reply_to`.
    #[serde(default)]
    references: Option<rmpv::Value>,
}

#[derive(Default)]
pub struct SsmtpConnector;

impl SsmtpConnector {
    pub fn new() -> Self {
        Self
    }
}

/// The recipients a call named, after parsing and before the allowlist.
fn recipients_of(to: &rmpv::Value) -> Result<Vec<String>, String> {
    let raw: Vec<String> = match to {
        rmpv::Value::String(s) => {
            vec![s.as_str().ok_or("to is not valid text")?.trim().to_string()]
        }
        rmpv::Value::Array(items) => items
            .iter()
            .map(|v| {
                v.as_str()
                    .map(|s| s.trim().to_string())
                    .ok_or_else(|| "to contains a value that is not text".to_string())
            })
            .collect::<Result<_, _>>()?,
        _ => return Err("to is neither an address nor a list of them".into()),
    };
    if raw.is_empty() {
        return Err("to names no recipient".into());
    }
    if raw.len() > MAX_RECIPIENTS {
        return Err(format!(
            "to names {} recipients; {MAX_RECIPIENTS} is the bound for one message",
            raw.len()
        ));
    }
    for address in &raw {
        header_safe("to", address)?;
        // Not a validator — a relay decides what it accepts. This refuses
        // the shapes that are certainly not one address.
        if address.is_empty() || !address.contains('@') || address.contains(' ') {
            return Err(format!("{address:?} is not an address"));
        }
    }
    Ok(raw)
}

/// The message ids a call named in `in_reply_to` or `references`: text
/// holding one id or several separated by whitespace, exactly as a copied
/// header reads, or a list with one per entry. Each comes back as `<id>`.
///
/// Whitespace is a separator here rather than a refusal, unlike in
/// [`header_safe`], because a `References` line long enough to have been
/// folded by its sender arrives with a CRLF inside it, and that fold is the
/// one legitimate line ending a header value can carry. Nothing of the
/// guest's text reaches the wire as written: the ids are re-joined, so what
/// follows a fold is one more id inside the same header and never a header
/// of its own.
pub fn msg_ids_of(field: &str, value: &rmpv::Value) -> Result<Vec<String>, String> {
    let texts: Vec<&str> = match value {
        rmpv::Value::String(s) => vec![s
            .as_str()
            .ok_or_else(|| format!("{field} is not valid text"))?],
        rmpv::Value::Array(items) => items
            .iter()
            .map(|v| {
                v.as_str()
                    .ok_or_else(|| format!("{field} contains a value that is not text"))
            })
            .collect::<Result<_, _>>()?,
        _ => {
            return Err(format!(
                "{field} is neither a message id nor a list of them"
            ))
        }
    };
    let mut ids = Vec::new();
    for token in texts.iter().flat_map(|t| t.split_whitespace()) {
        ids.push(msg_id(field, token)?);
    }
    if ids.is_empty() {
        return Err(format!("{field} names no message id"));
    }
    if ids.len() > MAX_REFERENCES {
        return Err(format!(
            "{field} names {} message ids; {MAX_REFERENCES} is the bound for one message",
            ids.len()
        ));
    }
    Ok(ids)
}

/// One token as `<id>`, the brackets supplied when the guest's source had
/// stripped them — JMAP's `messageId` does; a copied header does not.
fn msg_id(field: &str, token: &str) -> Result<String, String> {
    let inner = match token.strip_prefix('<') {
        Some(rest) => rest.strip_suffix('>').ok_or_else(|| {
            format!("{field} entry {token:?} opens a message id and does not close it")
        })?,
        None => token,
    };
    // RFC 5322 §3.6.4: `<` id-left `@` id-right `>`, no whitespace, no
    // nested brackets, no controls. Not a validator — a client threads on
    // equality, not on syntax, and ids without an `@` exist in the wild —
    // so this refuses only the shapes that are certainly not one id.
    if inner.is_empty()
        || inner
            .chars()
            .any(|c| c.is_control() || matches!(c, '<' | '>' | '"' | '(' | ')' | ',' | ';' | '\\'))
    {
        return Err(format!(
            "{field} entry {token:?} is not a message id; one reads <id@host>"
        ));
    }
    if inner.len() + 2 > MAX_MSGID_BYTES {
        return Err(format!(
            "{field} entry is {} bytes; {MAX_MSGID_BYTES} is the bound for one message id",
            inner.len() + 2
        ));
    }
    Ok(format!("<{inner}>"))
}

/// `Field: <id> <id>…` with CRLF, folded between ids once a line would pass
/// [`FOLD_AT_BYTES`]. The first id stays on the field's own line whatever
/// its length, as every client writes it — [`MAX_MSGID_BYTES`] is sized so
/// that line still fits the hard limit.
pub fn fold_ids(field: &str, ids: &[String]) -> String {
    let mut out = format!("{field}:");
    let mut line = out.len();
    for (i, id) in ids.iter().enumerate() {
        if i > 0 && line + 1 + id.len() > FOLD_AT_BYTES {
            out.push_str("\r\n");
            line = 0;
        }
        out.push(' ');
        out.push_str(id);
        line += 1 + id.len();
    }
    out.push_str("\r\n");
    out
}

/// What one call asked to send, after every check: the guest's part of a
/// message. The scope's part — sender, relay, credential — stays in the
/// scope, and the two meet only in [`headers`].
struct Message {
    recipients: Vec<String>,
    subject: String,
    body: String,
    in_reply_to: Vec<String>,
    references: Vec<String>,
}

/// The header block, through the blank line that ends it. The scope's
/// sender first and the guest's fields under it, never above — which is
/// what puts a subject of `From: x` visibly below the real one.
fn headers(scope: &SsmtpScope, msg: &Message) -> String {
    let mut out = format!(
        "From: {}\r\nTo: {}\r\nSubject: {}\r\n",
        scope.from,
        msg.recipients.join(", "),
        msg.subject
    );
    if !msg.in_reply_to.is_empty() {
        out.push_str(&fold_ids("In-Reply-To", &msg.in_reply_to));
    }
    if !msg.references.is_empty() {
        out.push_str(&fold_ids("References", &msg.references));
    }
    out.push_str("MIME-Version: 1.0\r\nContent-Type: text/plain; charset=utf-8\r\n\r\n");
    out
}

#[async_trait::async_trait]
impl Connector for SsmtpConnector {
    fn scope_type(&self) -> Box<dyn ScopeType> {
        Box::new(SsmtpScopeType)
    }

    async fn call(
        &self,
        call: &str,
        args: Option<rmpv::Value>,
        scope: Option<&Scope>,
    ) -> CallResult {
        if call != "ssmtp/send" {
            return Err(CallError::new(format!("'{call}' is not an ssmtp call")));
        }
        let scope = SsmtpScope::parse(scope).map_err(CallError::new)?;
        let args: SendArgs = rmpv::ext::from_value(
            args.ok_or_else(|| CallError::new("ssmtp/send needs {to, subject, body}"))?,
        )
        .map_err(|e| CallError::new(format!("arguments do not parse: {e}")))?;

        let recipients = recipients_of(&args.to).map_err(CallError::new)?;
        // The refusal names the address, because "denied" without it sends
        // the reader to guess which of four recipients was the problem.
        for address in &recipients {
            if !scope.permits(address) {
                return Err(CallError::new(format!(
                    "'{address}' is outside this instance's granted recipients"
                )));
            }
        }
        header_safe("subject", &args.subject).map_err(CallError::new)?;
        if args.subject.len() > MAX_SUBJECT_BYTES {
            return Err(CallError::new(format!(
                "subject is {} bytes; {MAX_SUBJECT_BYTES} is the bound",
                args.subject.len()
            )));
        }
        if args.body.len() > MAX_BODY_BYTES {
            return Err(CallError::new("too_large"));
        }
        let in_reply_to = match &args.in_reply_to {
            Some(v) => msg_ids_of("in_reply_to", v).map_err(CallError::new)?,
            None => Vec::new(),
        };
        let references = match &args.references {
            Some(v) => msg_ids_of("references", v).map_err(CallError::new)?,
            None => in_reply_to.clone(),
        };
        let msg = Message {
            recipients,
            subject: args.subject,
            body: args.body,
            in_reply_to,
            references,
        };

        let work = async { tokio::time::timeout(scope.timeout(), deliver(&scope, &msg)).await };
        // FM-3: `drt start` has a reactor, `drt run` does not.
        let outcome = match tokio::runtime::Handle::try_current() {
            Ok(_) => work.await,
            Err(_) => own_runtime().block_on(work),
        };
        outcome
            .map_err(|_| {
                CallError::new(format!(
                    "ssmtp/send timed out after {}ms",
                    scope.timeout().as_millis()
                ))
            })?
            .map_err(CallError::new)?;

        Ok(rmpv::Value::Map(vec![
            ("accepted".into(), rmpv::Value::Boolean(true)),
            (
                "recipients".into(),
                rmpv::Value::Array(msg.recipients.iter().map(|r| r.as_str().into()).collect()),
            ),
        ]))
    }
}

// depth: the SMTP conversation
//
// Hand-rolled for the same reason `rest` hand-rolls HTTP/1.1: the exchange
// is a dozen lines, and a mail crate is a large dependency and a large
// surface for a connector whose whole job is to be bounded and auditable.
// Read it top to bottom; it is the sequence a relay expects.

/// One line of an SMTP reply, and whether more follow (`250-` continues,
/// `250 ` ends).
fn parse_reply_line(line: &str) -> Result<(u16, bool), String> {
    if line.len() < 4 {
        return Err(format!("the relay answered {line:?}, which is not a reply"));
    }
    let code: u16 = line[..3]
        .parse()
        .map_err(|_| format!("the relay answered {line:?}, which is not a reply"))?;
    Ok((code, line.as_bytes()[3] == b'-'))
}

macro_rules! smtp {
    ($io:expr, $expect:expr, $($arg:tt)*) => {{
        let line = format!($($arg)*);
        $io.write_all(line.as_bytes()).await.map_err(|e| format!("send: {e}"))?;
        $io.write_all(b"\r\n").await.map_err(|e| format!("send: {e}"))?;
        $io.flush().await.map_err(|e| format!("send: {e}"))?;
        read_reply(&mut $io, $expect).await?
    }};
}

async fn read_reply<S>(io: &mut S, expect: u16) -> Result<String, String>
where
    S: tokio::io::AsyncRead + Unpin,
{
    let mut reader = BufReader::new(io);
    let mut text = String::new();
    loop {
        let mut line = String::new();
        let n = reader
            .read_line(&mut line)
            .await
            .map_err(|e| format!("read: {e}"))?;
        if n == 0 {
            return Err("the relay closed the connection".into());
        }
        let trimmed = line.trim_end();
        let (code, more) = parse_reply_line(trimmed)?;
        text.push_str(trimmed);
        if !more {
            if code / 100 != expect / 100 {
                // The relay's own words, because a relay refusing a
                // recipient says something a deployment needs to read.
                return Err(format!("the relay answered: {text}"));
            }
            return Ok(text);
        }
        text.push('\n');
    }
}

async fn deliver(scope: &SsmtpScope, msg: &Message) -> Result<(), String> {
    let stream = tokio::net::TcpStream::connect((scope.host.as_str(), scope.port()))
        .await
        .map_err(|e| format!("connect: {e}"))?;

    if scope.starttls() {
        let mut io = stream;
        read_reply(&mut io, 220).await?;
        smtp!(io, 250, "EHLO {}", ehlo_name(&scope.from));
        smtp!(io, 220, "STARTTLS");

        let mut roots = tokio_rustls::rustls::RootCertStore::empty();
        roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        let config = tokio_rustls::rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();
        let name = tokio_rustls::rustls::pki_types::ServerName::try_from(scope.host.clone())
            .map_err(|_| "tls: the relay name is not one a certificate can be checked against")?;
        let mut tls = tokio_rustls::TlsConnector::from(std::sync::Arc::new(config))
            .connect(name, io)
            .await
            .map_err(|e| format!("tls: {e}"))?;
        // EHLO again: the extensions before STARTTLS are not the ones that
        // count, and a relay may advertise AUTH only once encrypted.
        smtp!(tls, 250, "EHLO {}", ehlo_name(&scope.from));
        session(&mut tls, scope, msg).await
    } else {
        let mut io = stream;
        read_reply(&mut io, 220).await?;
        smtp!(io, 250, "EHLO {}", ehlo_name(&scope.from));
        session(&mut io, scope, msg).await
    }
}

/// The name in EHLO. A relay logs it and some check it resolves; the
/// sender's domain is the honest answer and needs no new configuration.
fn ehlo_name(from: &str) -> String {
    from.rsplit_once('@')
        .map(|(_, d)| d.trim_end_matches('>').to_string())
        .filter(|d| !d.is_empty() && !d.contains(' '))
        .unwrap_or_else(|| "localhost".into())
}

async fn session<S>(mut io: &mut S, scope: &SsmtpScope, msg: &Message) -> Result<(), String>
where
    S: tokio::io::AsyncRead + AsyncWriteExt + Unpin,
{
    if let (Some(user), Some(pass)) = (&scope.user, &scope.pass) {
        // AUTH PLAIN is `\0user\0pass`, base64. Reached only under TLS —
        // `validate` refuses the scope that would get here otherwise.
        let secret = format!("\0{user}\0{pass}");
        let encoded = base64::engine::general_purpose::STANDARD.encode(secret.as_bytes());
        smtp!(io, 235, "AUTH PLAIN {encoded}");
    }
    // The envelope sender is the scope's, never the guest's.
    smtp!(io, 250, "MAIL FROM:<{}>", envelope(&scope.from));
    for address in &msg.recipients {
        smtp!(io, 250, "RCPT TO:<{address}>");
    }
    smtp!(io, 354, "DATA");

    io.write_all(headers(scope, msg).as_bytes())
        .await
        .map_err(|e| format!("send: {e}"))?;
    io.write_all(dot_stuff(&msg.body).as_bytes())
        .await
        .map_err(|e| format!("send: {e}"))?;
    smtp!(io, 250, ".");
    // Best effort: the message is accepted at the 250 above, and a relay
    // that dislikes our QUIT has not unsent it.
    let _ = io.write_all(b"QUIT\r\n").await;
    Ok(())
}

/// The bare address out of a `Name <addr>` sender, for the envelope.
fn envelope(from: &str) -> String {
    match (from.find('<'), from.find('>')) {
        (Some(a), Some(b)) if b > a + 1 => from[a + 1..b].to_string(),
        _ => from.trim().to_string(),
    }
}
