//! The park role (`doc/P2P.md` §2.2): the answerer of
//! `doc/DRT-Signalling.md`, natively. It polls the server by cursor, holds
//! the call notification stream when the server offers one, and answers
//! each call with its record; the host behind it serves the `--forward`.
//! Several names may be parked at once, each its own answerer to its
//! server and every one served by the same host. A `wss://` park is today's
//! `drt tunnel --park` under this verb (§4.3): one WebSocket leg held at
//! the relay, carrying one target's bytes, and it parks alone.
//!
//! Pairing (`doc/DRT-Signalling.md` §6.2): a `pair` entry in the poll
//! tells this side whom to call; [`PairRule`] is the consent `--pair`
//! gives, and [`follow_pair`] makes the call, serving on it, and reports
//! the outcome.
//!
//! ## surface block
//!
//! - Entry points: [`run`], the role carried out; [`follow_pair`], one
//!   `pair` entry carried out.
//! - Configurable: [`POLL`], [`BACKOFF_MAX`], [`ACCEPT_HEADER`],
//!   [`PAIR_CONNECT`], [`PAIR_MARGIN`].
//! - Fan-out: the match on [`How`] in `run`; one presence per parked
//!   name, in `park_signal`; what the WebSocket park can
//!   carry, in [`ws_sink`]; the two forms of [`PairRule`]; the four
//!   [`Outcome`]s.

use std::time::Duration;

use drt_config::RootConfig;
use drt_rtc::host::{Event, SessionState, Window};
use drt_rtc::Command;
use tokio_rustls::rustls::pki_types::CertificateDer;

use super::http;
use super::peer::{ForwardSpec, How, Peer};
use super::serve::Serving;
use super::ParkRole;

/// How often a parked side polls when it holds no notification stream, and
/// between notifications when it does; under the 30 s that keep it present
/// (`doc/DRT-Signalling.md` §4.2).
pub const POLL: Duration = Duration::from_secs(5);
/// The longest wait between retries after the server could not be reached.
pub const BACKOFF_MAX: Duration = Duration::from_secs(60);
/// The admission range travels on the poll (`doc/P2P.md` §11).
pub const ACCEPT_HEADER: &str = "DRT-Accept";
/// The ceiling on following a `pair` entry, from the entry to a session,
/// the caller's request included, for an entry that names no hold; an
/// entry's `expires_in` less [`PAIR_MARGIN`] bounds it below that. Past
/// it the call is reported unreachable, before the server stops taking
/// the report (`doc/DRT-Signalling.md` §6.2).
pub const PAIR_CONNECT: Duration = Duration::from_secs(20);
/// Kept back from an entry's `expires_in`, for the report to travel.
pub const PAIR_MARGIN: Duration = Duration::from_secs(3);

/// Whom the server may tell this side to call (`--pair`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PairRule {
    /// `*`: any name at the server this side is parked at.
    AnyHere,
    /// `drt://<server>/v1/<glob>`: names matching `glob` at `server`, a
    /// base as `Peer::parse` resolves it.
    At { server: String, glob: String },
}

impl PairRule {
    pub fn parse(s: &str) -> Result<PairRule, String> {
        let s = s.trim();
        if s == "*" {
            return Ok(PairRule::AnyHere);
        }
        let peer = Peer::parse(s)?;
        match peer.how {
            How::Signal {
                base,
                name: Some(glob),
                ..
            } => Ok(PairRule::At {
                server: server_of(&base).to_string(),
                glob,
            }),
            _ => Err(format!(
                "'{s}': * for any name at the server this side is parked at, or \
                 drt://<server>/v1/<glob> for a name pattern at a named server"
            )),
        }
    }

    /// Whether a `pair` entry naming `name` at `server` (the server this
    /// side is parked at when the entry names none) may be followed.
    pub fn allows(&self, here: &str, server: &str, name: &str) -> bool {
        match self {
            PairRule::AnyHere => same_server(here, server),
            PairRule::At { server: at, glob } => {
                same_server(at, server) && glob_matches(glob, name)
            }
        }
    }
}

/// The server half of a signalling base: `https://s.example/v1/mypc` is
/// at `https://s.example`.
fn server_of(base: &str) -> &str {
    base.rsplit_once("/v1/").map(|(s, _)| s).unwrap_or(base)
}

fn same_server(a: &str, b: &str) -> bool {
    a.trim_end_matches('/')
        .eq_ignore_ascii_case(b.trim_end_matches('/'))
}

/// `*` matches any run of characters; nothing else is special.
fn glob_matches(glob: &str, name: &str) -> bool {
    let mut parts = glob.split('*');
    let first = parts.next().unwrap_or("");
    let Some(mut rest) = name.strip_prefix(first) else {
        return false;
    };
    if !glob.contains('*') {
        return rest.is_empty();
    }
    let parts: Vec<&str> = parts.collect();
    for (i, part) in parts.iter().enumerate() {
        if i + 1 == parts.len() {
            return rest.ends_with(part);
        }
        match rest.find(part) {
            Some(at) => rest = &rest[at + part.len()..],
            None => return false,
        }
    }
    true
}

/// What became of a `pair` entry, as reported to the server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Connected,
    /// The called name answered with a refusal.
    Refused,
    /// The call failed before an answer, or the session never connected.
    Unreachable,
    /// The consent rule said no.
    Declined,
}

impl Outcome {
    fn name(self) -> &'static str {
        match self {
            Outcome::Connected => "connected",
            Outcome::Refused => "refused",
            Outcome::Unreachable => "unreachable",
            Outcome::Declined => "declined",
        }
    }
}

/// The role: serve the forward, and answer calls at the signalling server
/// until stopped. A `wss://` leg parks alone; names at signalling servers
/// may be several, every one answered by the same host.
pub async fn run(
    role: &ParkRole,
    config: &RootConfig,
    roots: &[CertificateDer<'static>],
) -> Result<(), String> {
    let several = role.names.len() > 1;
    let mut seen = std::collections::HashSet::new();
    for name in &role.names {
        match &name.how {
            How::Ws(url) if !several => return park_ws(url, role, config, roots).await,
            How::Ws(_) => {
                return Err(format!(
                    "--park {} is a wss:// leg, which parks alone; several --park are names at \
                     signalling servers (drt://host[:port]/v1/<name>)",
                    name.shown()
                ))
            }
            How::Signal { name: None, .. } => {
                return Err(format!(
                    "--park {} names no answerer: drt://host[:port]/v1/<name>",
                    name.shown()
                ))
            }
            How::Record(_) => return Err("--park takes a signalling server, not a record".into()),
            How::Signal { .. } => {
                if !seen.insert(name.canonical()) {
                    return Err(format!("--park names {} twice", name.shown()));
                }
            }
        }
    }
    park_signal(role, config, roots).await
}

// depth: the profile's answerer

async fn park_signal(
    role: &ParkRole,
    config: &RootConfig,
    roots: &[CertificateDer<'static>],
) -> Result<(), String> {
    let bind = "0.0.0.0:0".parse().expect("a literal");
    let serving = super::serve::start(
        &role.forward,
        bind,
        false,
        role.stun.clone(),
        role.accept.clone(),
        &role.settings,
        config,
        roots,
    )
    .await?;
    let mut headers = role.headers.clone();
    if !role.accept.is_empty() {
        let ranges: Vec<String> = role.accept.iter().map(|c| c.to_string()).collect();
        headers.push((ACCEPT_HEADER.to_string(), ranges.join(", ")));
    }
    let serving = std::sync::Arc::new(serving);
    // Each name is its own answerer to its server: its own poll, stream
    // and cursor. A name refused for good (401, 403) stops being answered;
    // the park ends when no name is left.
    let several = role.names.len() > 1;
    let mut presences = tokio::task::JoinSet::new();
    for name in &role.names {
        let How::Signal { base, .. } = &name.how else {
            unreachable!("run admits only names at signalling servers here");
        };
        eprintln!(
            "drt p2p: parked at {}, serving {}",
            crate::tunnel::shown(base),
            role.forward.describe()
        );
        let pairing = Pairing {
            signalling: name.clone(),
            headers: headers.clone(),
            rule: role.pair.clone(),
        };
        // A call's session is keyed by its id, which is unique only at
        // one name; with several, the name keeps two apart.
        let key = several.then(|| name.shown());
        let (base, serving, roots) = (base.clone(), serving.clone(), roots.to_vec());
        presences.spawn(async move {
            let ended = present(&base, key, pairing, serving, &roots).await;
            (base, ended)
        });
    }
    let mut last = Ok(());
    while let Some(done) = presences.join_next().await {
        let (base, ended) = done.map_err(|e| e.to_string())?;
        if let Err(why) = &ended {
            if several {
                eprintln!(
                    "drt p2p: no longer parked at {}: {why}",
                    crate::tunnel::shown(&base)
                );
            }
        }
        last = ended;
    }
    last
}

/// One name, answered until its server refuses it for good: the call
/// notification stream when the server has one, and the poll it wakes.
async fn present(
    base: &str,
    key: Option<String>,
    pairing: Pairing,
    serving: std::sync::Arc<Serving>,
    roots: &[CertificateDer<'static>],
) -> Result<(), String> {
    let headers = pairing.headers.clone();
    let calls_url = pairing.signalling.url("/calls");
    let events_url = pairing.signalling.url("/events");
    let pairing = std::sync::Arc::new(pairing);

    // The call notification stream, when the server has one: each event
    // wakes the poll below. A stream that will not open is polling alone.
    let (wake_tx, mut wake) = tokio::sync::mpsc::unbounded_channel::<()>();
    let stream = {
        let (url, headers, roots) = (events_url.clone(), headers.clone(), roots.to_vec());
        tokio::spawn(async move {
            let mut backoff = Duration::from_secs(1);
            loop {
                match http::stream("GET", &url, &headers, None, &roots).await {
                    Ok((head, mut body)) if head.status == 200 => {
                        backoff = Duration::from_secs(1);
                        let mut text = String::new();
                        while let Ok(Some(piece)) = body.next().await {
                            text.push_str(&String::from_utf8_lossy(&piece));
                            while let Some(at) = text.find("\n\n") {
                                let event: String = text.drain(..at + 2).collect();
                                if event
                                    .lines()
                                    .any(|l| matches!(l.trim(), "event: call" | "event: pair"))
                                {
                                    let _ = wake_tx.send(());
                                }
                            }
                        }
                    }
                    // No stream here, or none for this token: polling is
                    // the profile's floor and it is running already.
                    Ok((head, _)) if head.status == 404 || head.status == 405 => return,
                    Ok(_) | Err(_) => {}
                }
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(BACKOFF_MAX);
            }
        })
    };
    let ended = poll(base, key, &calls_url, &pairing, &serving, &mut wake, roots).await;
    stream.abort();
    ended
}

async fn poll(
    base: &str,
    key: Option<String>,
    calls_url: &str,
    pairing: &std::sync::Arc<Pairing>,
    serving: &std::sync::Arc<Serving>,
    wake: &mut tokio::sync::mpsc::UnboundedReceiver<()>,
    roots: &[CertificateDer<'static>],
) -> Result<(), String> {
    let headers = &pairing.headers;
    let mut cursor: Option<String> = None;
    let mut backoff = Duration::from_secs(1);
    let mut said = false;
    loop {
        let url = match &cursor {
            Some(c) if calls_url.contains('?') => format!("{calls_url}&since={c}"),
            Some(c) => format!("{calls_url}?since={c}"),
            None => calls_url.to_string(),
        };
        match http::request("GET", &url, headers, None, roots).await {
            Ok(reply) if reply.status == 200 => {
                backoff = Duration::from_secs(1);
                if !said {
                    eprintln!("drt p2p: present at {}", crate::tunnel::shown(base));
                    said = true;
                }
                let page: serde_json::Value =
                    serde_json::from_str(&reply.text()).unwrap_or_default();
                if let Some(c) = page["cursor"].as_str() {
                    cursor = Some(c.to_string());
                }
                for call in page["calls"].as_array().into_iter().flatten() {
                    let (Some(id), Some(record)) = (call["id"].as_str(), call["record"].as_str())
                    else {
                        continue;
                    };
                    let peer = match &key {
                        Some(name) => format!("{name} {id}"),
                        None => id.to_string(),
                    };
                    serving.sender.send(Command::Open {
                        peer: peer.clone(),
                        rtc: record.to_string(),
                    });
                    let answer = pairing.signalling.url(&format!("/calls/{id}/answer"));
                    let mine = serving.record.borrow().clone();
                    match http::request("POST", &answer, headers, Some(&mine), roots).await {
                        Ok(r) if r.status == 204 || r.status == 200 => {
                            eprintln!("drt p2p: answered {peer}")
                        }
                        Ok(r) => eprintln!(
                            "drt p2p: {peer}: {}",
                            r.refusal(&answer)
                                .unwrap_or_else(|| format!("answered {}", r.status))
                        ),
                        Err(e) => eprintln!("drt p2p: {peer}: {e}"),
                    }
                }
                for entry in page["pair"].as_array().into_iter().flatten() {
                    let (serving, pairing, roots) =
                        (serving.clone(), pairing.clone(), roots.to_vec());
                    let entry = entry.clone();
                    tokio::spawn(async move {
                        follow_pair(&entry, &pairing, &serving, &roots).await;
                    });
                }
            }
            Ok(reply) => {
                let why = reply
                    .refusal(&url)
                    .unwrap_or_else(|| format!("answered {}", reply.status));
                if reply.status == 401 || reply.status == 403 {
                    return Err(why);
                }
                eprintln!("drt p2p: {why}; retrying in {backoff:?}");
                said = false;
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(BACKOFF_MAX);
                continue;
            }
            Err(e) => {
                eprintln!("drt p2p: {e}; retrying in {backoff:?}");
                said = false;
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(BACKOFF_MAX);
                continue;
            }
        }
        tokio::select! {
            _ = tokio::time::sleep(POLL) => {}
            _ = wake.recv() => {}
        }
    }
}

// depth: pairing (doc/DRT-Signalling.md §6.2)

/// What following a `pair` entry needs of the park: where it is parked,
/// the headers that identify it there, and its consent.
#[derive(Debug, Clone)]
pub struct Pairing {
    pub signalling: Peer,
    pub headers: Vec<(String, String)>,
    pub rule: Option<PairRule>,
}

/// One `pair` entry: decline it, or call the name it gives with the token
/// it gives, at the server it gives (the one this side is parked at when
/// it gives none), serving on the session that results; then report.
pub async fn follow_pair(
    entry: &serde_json::Value,
    park: &Pairing,
    serving: &Serving,
    roots: &[CertificateDer<'static>],
) {
    let (Some(id), Some(name)) = (entry["id"].as_str(), entry["name"].as_str()) else {
        return;
    };
    let here = server_of(&park.signalling.canonical()).to_string();
    let server = entry["server"].as_str().unwrap_or(&here).to_string();
    let result_url = park.signalling.url(&format!("/pair/{id}/result"));
    // The whole follow, the caller's request included, inside the entry's
    // hold: the server holds that request until the name answers (§3),
    // which may be the whole hold, and a result after it is 404.
    let budget = entry["expires_in"]
        .as_u64()
        .map(|s| Duration::from_secs(s).saturating_sub(PAIR_MARGIN))
        .map_or(PAIR_CONNECT, |held| held.min(PAIR_CONNECT))
        .max(Duration::from_secs(1));
    let deadline = tokio::time::Instant::now() + budget;
    let report = |outcome: Outcome, why: String| {
        let (headers, roots, result_url) =
            (park.headers.clone(), roots.to_vec(), result_url.clone());
        async move {
            eprintln!("drt p2p: pair {id}: {}{}", outcome.name(), why_shown(&why));
            let body = serde_json::json!({"outcome": outcome.name(), "why": why}).to_string();
            match http::request("POST", &result_url, &headers, Some(&body), &roots).await {
                Ok(r) if r.status == 204 || r.status == 200 => {}
                Ok(r) => eprintln!(
                    "drt p2p: pair {id}: the result was not taken: {}",
                    r.refusal(&result_url)
                        .unwrap_or_else(|| format!("answered {}", r.status))
                ),
                Err(e) => eprintln!("drt p2p: pair {id}: the result was not taken: {e}"),
            }
        }
    };
    let allowed = park
        .rule
        .as_ref()
        .is_some_and(|rule| rule.allows(&here, &server, name));
    if !allowed {
        return report(
            Outcome::Declined,
            match &park.rule {
                None => "no --pair".to_string(),
                Some(_) => format!(
                    "{name} at {} is outside --pair",
                    crate::tunnel::shown(&server)
                ),
            },
        )
        .await;
    }
    eprintln!(
        "drt p2p: pair {id}: calling {name} at {}",
        crate::tunnel::shown(&server)
    );
    // The caller's request (§3), with the caller token the entry gives.
    let calls_url = format!("{}/v1/{name}/calls", server.trim_end_matches('/'));
    let mut headers = Vec::new();
    if let Some(token) = entry["token"].as_str() {
        headers.push(("Authorization".to_string(), format!("Bearer {token}")));
    }
    let mine = serving.record.borrow().clone();
    let request = http::request("POST", &calls_url, &headers, Some(&mine), roots);
    let answer = match tokio::time::timeout_at(deadline, request).await {
        Err(_) => {
            return report(
                Outcome::Unreachable,
                format!("no answer within {}s", budget.as_secs()),
            )
            .await
        }
        Ok(Ok(r)) if r.status == 200 => r.text(),
        Ok(Ok(r)) => {
            let why = r
                .refusal(&calls_url)
                .unwrap_or_else(|| format!("answered {}", r.status));
            let outcome = if r.status == 410 {
                Outcome::Refused
            } else {
                Outcome::Unreachable
            };
            return report(outcome, why).await;
        }
        Ok(Err(e)) => return report(Outcome::Unreachable, e).await,
    };
    let peer = format!("pair:{name}");
    let mut events = serving.events.subscribe();
    serving.sender.send(Command::Call {
        peer: peer.clone(),
        rtc: answer,
    });
    let fate = tokio::time::timeout_at(deadline, async {
        loop {
            match events.recv().await {
                Ok(Event::Session {
                    peer: p,
                    state,
                    reason,
                }) if p == peer => return Some((state, reason)),
                Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                Err(_) => return None,
            }
        }
    })
    .await;
    match fate {
        Ok(Some((SessionState::Connected, _))) => report(Outcome::Connected, String::new()).await,
        Ok(Some((SessionState::Closed, reason))) => {
            report(Outcome::Unreachable, reason.unwrap_or_default()).await
        }
        Ok(None) => report(Outcome::Unreachable, "the host stopped".into()).await,
        Err(_) => {
            serving.sender.send(Command::Close { peer });
            report(
                Outcome::Unreachable,
                format!("no session within {}s", budget.as_secs()),
            )
            .await
        }
    }
}

fn why_shown(why: &str) -> String {
    if why.is_empty() {
        String::new()
    } else {
        format!(": {why}")
    }
}

// depth: the WebSocket park

/// What a `wss://` park can carry: one target's bytes. A TCP target is
/// dialed lazily on the first claimed bytes, as `drt tunnel --park` did;
/// the REPL and stdio are opened then too.
fn ws_sink(spec: &ForwardSpec) -> Result<(), String> {
    match spec {
        ForwardSpec::One(_) | ForwardSpec::Repl | ForwardSpec::Stdio => Ok(()),
        other => Err(format!(
            "--park wss:// carries one target's bytes on one leg; {} needs a DRT session \
             (park at a drt:// signalling server instead)",
            other.describe()
        )),
    }
}

async fn park_ws(
    url: &str,
    role: &ParkRole,
    config: &RootConfig,
    roots: &[CertificateDer<'static>],
) -> Result<(), String> {
    ws_sink(&role.forward)?;
    if let ForwardSpec::One(entry) = &role.forward {
        let target = format!("{}:{}", entry.host, entry.port);
        return crate::tunnel::park(url, &target, roots, &role.headers).await;
    }
    // The REPL or stdio on a leg: the same loop, with the in-process service
    // opened when the leg is claimed.
    let (forward, _, done) = super::serve::sinks(&role.forward, &role.settings, config, roots)?;
    let drt_rtc::Forward::One(drt_rtc::Sink::Local(service)) = forward else {
        unreachable!("ws_sink admits only one-target forwards");
    };
    let mut backoff = Duration::from_secs(1);
    let mut announce = true;
    let mut done = done;
    loop {
        if let Some(d) = done.as_mut() {
            if d.try_recv().is_ok() {
                return Ok(());
            }
        }
        match crate::tunnel::park_leg(url, roots, &role.headers, announce).await {
            Ok(Some((ws, first))) => {
                backoff = Duration::from_secs(1);
                announce = false;
                let service = service.clone();
                tokio::spawn(async move {
                    match service
                        .open("", 0, Window::default(), drt_rtc::Report::none())
                        .await
                    {
                        Ok(mut io) => {
                            use tokio::io::AsyncWriteExt;
                            if io.write_all(&first).await.is_ok() {
                                let _ = crate::tunnel::pump(io, ws).await;
                            }
                        }
                        Err(code) => eprintln!(
                            "drt p2p: the leg was claimed and the service refused it: {}",
                            drt_rtc::wisp::reason::name(code)
                        ),
                    }
                });
            }
            Ok(None) => {
                backoff = Duration::from_secs(1);
                announce = false;
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
            Err(e) => {
                eprintln!("drt p2p: {e}; retrying in {backoff:?}");
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(BACKOFF_MAX);
                announce = true;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_pair_rule_is_any_name_here_or_a_pattern_at_a_named_server() {
        let here = "http://127.0.0.1:9000";
        let any = PairRule::parse("*").unwrap();
        assert!(any.allows(here, here, "room-7"));
        assert!(any.allows(here, "http://127.0.0.1:9000/", "x"));
        assert!(!any.allows(here, "https://elsewhere.example", "room-7"));

        let at = PairRule::parse("drt://127.0.0.1:9000/v1/room-*").unwrap();
        assert_eq!(
            at,
            PairRule::At {
                server: "http://127.0.0.1:9000".into(),
                glob: "room-*".into()
            }
        );
        assert!(at.allows(here, here, "room-7"));
        assert!(!at.allows(here, here, "lobby"));
        assert!(!at.allows(here, "http://127.0.0.1:9001", "room-7"));

        // A name without a server is not a rule: the risk is the server.
        assert!(PairRule::parse("room-*").is_err());
        assert!(PairRule::parse("drt://127.0.0.1:9000").is_err());
    }

    #[test]
    fn the_glob_is_star_and_nothing_else() {
        assert!(glob_matches("*", "anything"));
        assert!(glob_matches("room-*", "room-7"));
        assert!(!glob_matches("room-*", "lobby-room-7"));
        assert!(glob_matches("*-7", "room-7"));
        assert!(glob_matches("r*m*7", "room-7"));
        assert!(glob_matches("exact", "exact"));
        assert!(!glob_matches("exact", "exactly"));
        assert!(!glob_matches("a.b", "aXb"));
    }
}
