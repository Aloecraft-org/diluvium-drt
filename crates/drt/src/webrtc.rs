//! The `webrtc` block inside `drt start`: the deployment's end of a
//! `drt_rtc::Host` (`doc/BrowserAccess.md` §8).
//!
//! Nearly the arrangement `turn` and `wireguard` have: the host is tokio
//! and the drive loop is not, so the host runs on a thread and a runtime of
//! its own -- `Host::start` brings both, with a stack sized for str0m (see
//! `drt_rtc::host`) -- and the loop moves messages between it and the root
//! program without blocking. Reports go out on `queue`; commands come in on
//! `reply_queue`.
//!
//! **Signaling is the program's.** The block reports its own presence record
//! and opens a session from a browser's; how the two records met -- a
//! Discofetch room, the M0 mock, anything -- is not this file's business, so
//! a change to that API is never a DRT release. WireGuard's rendezvous is
//! the precedent (`doc/WireGuard.md` §4).
//!
//! ## surface block
//!
//! - Entry points: [`WebrtcBridge::start`], [`WebrtcBridge::collect`],
//!   [`WebrtcBridge::report`].
//! - Configurable: [`HELD_MAX`], [`MAX_PEER`]; everything else is the block's
//!   (`drt_config::WebrtcConfig`).
//! - Fan-out: [`command_from`] (the two commands), [`report_value`] (the
//!   three reports).

use std::net::SocketAddr;
use std::time::Duration;

use drt_config::WebrtcConfig;
use drt_rtc::host::{Event, SessionState, StreamState};
use drt_rtc::{Command, Entry, Host, HostConfig, Identity, Scope};

/// Reports held for a queue the program has not declared yet. The record is
/// the one that matters and it is first; past this, the oldest are dropped
/// and the drop is said once on stderr.
const HELD_MAX: usize = 4096;
/// The longest `peer` a command may name. A session id is whatever the
/// signaling service chose, so it is bounded before it becomes a map key.
pub const MAX_PEER: usize = 64;

pub struct WebrtcBridge {
    host: Host,
    queue: String,
    reply_queue: String,
    held: std::collections::VecDeque<Vec<u8>>,
    dropped: bool,
}

impl WebrtcBridge {
    /// Read the identity, parse the scope, bind, and serve.
    ///
    /// Everything that can be refused is refused here, by name, before
    /// `drt start` reports itself up: a scope entry that is not
    /// `scheme://host[:port]`, a `default` outside the scope, an identity
    /// file that exists and does not parse, a port in use.
    pub fn start(config: &WebrtcConfig) -> Result<WebrtcBridge, String> {
        // Dropping the host stops it, and the bridge lives as long as the
        // deployment does.
        let host = Host::start(host_config(config)?)?;
        Ok(WebrtcBridge {
            host,
            queue: config.queue.clone(),
            reply_queue: config.reply_queue.clone(),
            held: Default::default(),
            dropped: false,
        })
    }

    /// The address the host candidate advertises.
    pub fn local_addr(&self) -> SocketAddr {
        self.host.local_addr()
    }

    /// Hand the host every command waiting on the reply queue.
    pub fn collect(&mut self, pop: &mut dyn FnMut(&str) -> Option<Vec<u8>>) {
        while let Some(raw) = pop(&self.reply_queue) {
            let parsed = rmpv::decode::read_value(&mut &raw[..])
                .map_err(|e| format!("a command that is not msgpack: {e}"))
                .and_then(|v| command_from(&v));
            match parsed {
                Ok(command) => self.host.send(command),
                // The deployment's own program wrote it: naming what was
                // wrong beats dropping it in silence.
                Err(reason) => eprintln!("drt start: webrtc: {reason}"),
            }
        }
    }

    /// Push every report the host has made, in order, holding the ones the
    /// queue will not take yet. Non-blocking.
    ///
    /// Held rather than dropped because the first report is the record, and
    /// a program that declares its queue after its first park would
    /// otherwise never learn what to publish.
    pub fn report(&mut self, push: &mut dyn FnMut(&str, &[u8]) -> bool) {
        while let Some(e) = self.host.try_event() {
            if self.held.len() >= HELD_MAX {
                self.held.pop_front();
                if !self.dropped {
                    self.dropped = true;
                    eprintln!(
                        "drt start: webrtc: queue '{}' is not taking reports; dropping the oldest",
                        self.queue
                    );
                }
            }
            self.held.push_back(encode(&report_value(&e)));
        }
        while let Some(msg) = self.held.front() {
            if !push(&self.queue, msg) {
                return;
            }
            self.held.pop_front();
        }
    }
}

/// The block, checked and turned into what the host takes.
fn host_config(c: &WebrtcConfig) -> Result<HostConfig, String> {
    let bind: SocketAddr = c
        .bind
        .parse()
        .map_err(|_| format!("webrtc.bind '{}' is not ip:port", c.bind))?;
    let entries = c
        .scope
        .iter()
        .map(|s| Entry::parse(s).map_err(|e| format!("webrtc.{e}")))
        .collect::<Result<Vec<_>, _>>()?;
    let scope = Scope::new(entries);
    let default = match &c.default {
        None => None,
        Some(d) => {
            let e = Entry::parse(d).map_err(|e| format!("webrtc.default: {e}"))?;
            if scope.allows(&e.host, e.port).is_none() {
                return Err(format!(
                    "webrtc.default '{d}' is not in webrtc.scope; a browser sent there would be refused"
                ));
            }
            Some(e)
        }
    };
    if c.stun_refresh_s == 0 {
        return Err("webrtc.stun_refresh_s must be at least 1".into());
    }
    if c.max_sessions == 0 || c.max_streams_per_session == 0 {
        return Err("webrtc: max_sessions and max_streams_per_session must be at least 1".into());
    }
    Ok(HostConfig {
        bind,
        identity: Identity::load_or_create(&c.identity_file)?,
        stun: c.stun.clone(),
        publish_host_candidates: c.publish_host_candidates,
        service: c.service.clone(),
        default,
        scope,
        max_sessions: c.max_sessions,
        max_streams: c.max_streams_per_session,
        idle_timeout: Duration::from_secs(c.idle_stream_timeout_s),
        connect_timeout: Duration::from_secs(c.connect_timeout_s),
        stun_refresh: Duration::from_secs(c.stun_refresh_s),
    })
}

fn text<'a>(v: &'a rmpv::Value, key: &str) -> Option<&'a str> {
    v.as_map()?
        .iter()
        .find(|(k, _)| k.as_str() == Some(key))
        .and_then(|(_, v)| v.as_str())
}

/// `{command = "open", peer, rtc}` or `{command = "close", peer}`.
pub fn command_from(v: &rmpv::Value) -> Result<Command, String> {
    let peer = || {
        let peer = text(v, "peer").ok_or_else(|| "a command needs `peer`, a string".to_string())?;
        // An opaque id a signaling service chose: bounded here so nothing
        // downstream has to be (the Discofetch contract's own bound).
        if peer.is_empty() || peer.len() > MAX_PEER {
            return Err(format!("`peer` must be 1..={MAX_PEER} bytes"));
        }
        Ok(peer.to_string())
    };
    match text(v, "command") {
        Some("open") => Ok(Command::Open {
            peer: peer()?,
            rtc: text(v, "rtc")
                .ok_or("`open` needs `rtc`, the browser's presence record")?
                .to_string(),
        }),
        Some("close") => Ok(Command::Close { peer: peer()? }),
        Some(other) => Err(format!("unknown command '{other}' (open, close)")),
        None => Err("a command needs `command`, a string".into()),
    }
}

/// A report as the program sees it, named by `event` like every other
/// block's so one `if m.event == …` chain reads them all.
pub fn report_value(e: &Event) -> rmpv::Value {
    use rmpv::Value;
    let opt = |s: Option<&str>| s.map(Value::from).unwrap_or(Value::Nil);
    match e {
        Event::Record { rtc } => Value::Map(vec![
            ("event".into(), "webrtc_record".into()),
            ("rtc".into(), rtc.as_str().into()),
        ]),
        Event::Session {
            peer,
            state,
            reason,
        } => Value::Map(vec![
            ("event".into(), "webrtc_session".into()),
            ("peer".into(), peer.as_str().into()),
            (
                "state".into(),
                match state {
                    SessionState::Connected => "connected",
                    SessionState::Closed => "closed",
                }
                .into(),
            ),
            ("reason".into(), opt(reason.as_deref())),
        ]),
        Event::Stream {
            peer,
            stream,
            host,
            port,
            state,
            reason,
            bytes_up,
            bytes_down,
        } => Value::Map(vec![
            ("event".into(), "webrtc_stream".into()),
            ("peer".into(), peer.as_str().into()),
            ("stream".into(), Value::from(*stream)),
            ("host".into(), host.as_str().into()),
            ("port".into(), Value::from(*port)),
            (
                "state".into(),
                match state {
                    StreamState::Open => "open",
                    StreamState::Closed => "closed",
                }
                .into(),
            ),
            ("reason".into(), opt(*reason)),
            ("bytes_up".into(), Value::from(*bytes_up)),
            ("bytes_down".into(), Value::from(*bytes_down)),
        ]),
    }
}

fn encode(value: &rmpv::Value) -> Vec<u8> {
    let mut msg = Vec::new();
    rmpv::encode::write_value(&mut msg, value).expect("a webrtc report encodes");
    msg
}

#[cfg(test)]
mod tests {
    use super::*;
    use rmpv::Value;

    fn map(pairs: &[(&str, &str)]) -> Value {
        Value::Map(
            pairs
                .iter()
                .map(|(k, v)| ((*k).into(), (*v).into()))
                .collect(),
        )
    }

    #[test]
    fn commands_are_read_and_bad_ones_named() {
        assert_eq!(
            command_from(&map(&[("command", "open"), ("peer", "b1"), ("rtc", "{}")])),
            Ok(Command::Open {
                peer: "b1".into(),
                rtc: "{}".into()
            })
        );
        assert_eq!(
            command_from(&map(&[("command", "close"), ("peer", "b1")])),
            Ok(Command::Close { peer: "b1".into() })
        );
        assert!(command_from(&map(&[("command", "open"), ("peer", "b1")]))
            .unwrap_err()
            .contains("`rtc`"));
        assert!(command_from(&map(&[("command", "wake")]))
            .unwrap_err()
            .contains("unknown command"));
    }

    fn block(scope: &[&str], default: Option<&str>) -> WebrtcConfig {
        serde_json::from_value(serde_json::json!({
            "identity_file": "/nonexistent/for/this/test",
            "scope": scope,
            "default": default,
        }))
        .unwrap()
    }

    #[test]
    fn a_bad_scope_or_default_is_refused_at_start_by_name() {
        let e = host_config(&block(&["http://h/path"], None)).unwrap_err();
        assert!(e.starts_with("webrtc.scope entry"), "{e}");
        let e = host_config(&block(&["http://a:80"], Some("http://b:80"))).unwrap_err();
        assert!(e.contains("is not in webrtc.scope"), "{e}");
    }
}
