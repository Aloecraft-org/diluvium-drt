//! The park role (`doc/P2P.md` §2.2): the answerer of
//! `doc/DRT-Signalling.md`, natively. It polls the server by cursor, holds
//! the call notification stream when the server offers one, and answers
//! each call with its record; the host behind it serves the `--forward`.
//! A `wss://` park is today's `drt tunnel --park` under this verb (§4.3):
//! one WebSocket leg held at the relay, carrying one target's bytes.
//!
//! ## surface block
//!
//! - Entry points: [`run`], the role carried out.
//! - Configurable: [`POLL`], [`BACKOFF_MAX`], [`ACCEPT_HEADER`].
//! - Fan-out: the match on [`How`] in `run`; what the WebSocket park can
//!   carry, in [`ws_sink`].

use std::time::Duration;

use drt_config::RootConfig;
use drt_rtc::host::Window;
use drt_rtc::Command;
use tokio_rustls::rustls::pki_types::CertificateDer;

use super::http;
use super::peer::{ForwardSpec, How};
use super::ParkRole;

/// How often a parked side polls when it holds no notification stream, and
/// between notifications when it does; under the 30 s that keep it present
/// (`doc/DRT-Signalling.md` §4.2).
pub const POLL: Duration = Duration::from_secs(5);
/// The longest wait between retries after the server could not be reached.
pub const BACKOFF_MAX: Duration = Duration::from_secs(60);
/// The admission range travels on the poll (`doc/P2P.md` §11).
pub const ACCEPT_HEADER: &str = "DRT-Accept";

/// The role: serve the forward, and answer calls at the signalling server
/// until stopped.
pub async fn run(
    role: &ParkRole,
    config: &RootConfig,
    roots: &[CertificateDer<'static>],
) -> Result<(), String> {
    match &role.signalling.how {
        How::Ws(url) => park_ws(url, role, config, roots).await,
        How::Signal { name: None, .. } => Err(format!(
            "--park {} names no answerer: drt://host[:port]/v1/<name>",
            role.signalling.shown()
        )),
        How::Record(_) => Err("--park takes a signalling server, not a record".into()),
        How::Signal { base, .. } => park_signal(base, role, config, roots).await,
    }
}

// depth: the profile's answerer

async fn park_signal(
    base: &str,
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
    let calls_url = role.signalling.url("/calls");
    let events_url = role.signalling.url("/events");
    eprintln!(
        "drt p2p: parked at {}, serving {}",
        crate::tunnel::shown(base),
        role.forward.describe()
    );

    // The call notification stream, when the server has one: each event
    // wakes the poll below. A stream that will not open is polling alone.
    let (wake_tx, mut wake) = tokio::sync::mpsc::unbounded_channel::<()>();
    {
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
                                if event.lines().any(|l| l.trim() == "event: call") {
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
        });
    }

    let mut cursor: Option<String> = None;
    let mut backoff = Duration::from_secs(1);
    let mut said = false;
    loop {
        let url = match &cursor {
            Some(c) if calls_url.contains('?') => format!("{calls_url}&since={c}"),
            Some(c) => format!("{calls_url}?since={c}"),
            None => calls_url.clone(),
        };
        match http::request("GET", &url, &headers, None, roots).await {
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
                    serving.sender.send(Command::Open {
                        peer: id.to_string(),
                        rtc: record.to_string(),
                    });
                    let answer = role.signalling.url(&format!("/calls/{id}/answer"));
                    let mine = serving.record.borrow().clone();
                    match http::request("POST", &answer, &headers, Some(&mine), roots).await {
                        Ok(r) if r.status == 204 || r.status == 200 => {
                            eprintln!("drt p2p: answered {id}")
                        }
                        Ok(r) => eprintln!(
                            "drt p2p: {id}: {}",
                            r.refusal(&answer)
                                .unwrap_or_else(|| format!("answered {}", r.status))
                        ),
                        Err(e) => eprintln!("drt p2p: {id}: {e}"),
                    }
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
                    match service.open("", 0, Window::default()).await {
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
