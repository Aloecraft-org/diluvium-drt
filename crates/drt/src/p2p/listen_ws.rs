//! `drt p2p --listen-ws` (`doc/P2P.md` §2.5): plain WebSockets in on a TCP
//! port, each spliced to the one `--forward`.
//!
//! ```text
//! page --ws--> [--listen-ws] --tcp--> host:port      (websockify's shape)
//!                            --p2p--> drt://peer     (ws in, p2p out)
//! ```
//!
//! The target is this side's. A page never names it, so a bare `--forward`
//! (the caller names the destination) is refused here, and so is every
//! forward that is not one target. `--host` keeps its meaning: who may
//! connect. TLS is not built; serve on localhost, or behind a TLS
//! terminator as with `--match`. The accept loop and the splice are
//! `crate::bridge`'s, which `drt start`'s `bridge` block runs too.
//!
//! ## surface block
//!
//! - Entry points: [`check_forward`], what this role refuses; [`run`], the
//!   role carried out; [`block_target`], a `bridge` block's `drt://` target
//!   for `drt start`.
//! - Configurable: nothing; the bridge's [`crate::bridge::SUBPROTOCOL`] is
//!   the one subprotocol answered.
//! - Fan-out: [`target`], one arm per forward this role takes (one TCP
//!   target, or another DRT peer).

use std::net::SocketAddr;

use drt_config::RootConfig;
use tokio_rustls::rustls::pki_types::CertificateDer;

use super::peer::{ForwardSpec, HostSpec, Peer};
use super::ServeSettings;

/// `--listen-ws`, resolved.
#[derive(Debug)]
pub struct ListenWsRole {
    pub port: u16,
    pub host: HostSpec,
    /// [`ForwardSpec::One`] or [`ForwardSpec::Peer`]; [`check_forward`]
    /// has refused the rest.
    pub forward: ForwardSpec,
    /// For a `drt://` forward: how it is called.
    pub settings: ServeSettings,
}

/// The forwards this role takes: one target this side fixes. Each other
/// row of §5's table is refused by name.
pub fn check_forward(forward: &ForwardSpec) -> Result<(), String> {
    let why = match forward {
        ForwardSpec::One(_) | ForwardSpec::Peer(_) => return Ok(()),
        ForwardSpec::Relay => {
            "a bare --forward lets the caller name the destination, and a page must never \
             pick the target"
        }
        ForwardSpec::Repl => "it needs a --forward: host:port, or drt://… for a peer",
        ForwardSpec::Stdio => "--forward - serves one session on stdio, not WebSockets",
        ForwardSpec::Ports { .. } | ForwardSpec::Many(_) => {
            "a WebSocket names no port, so --listen-ws forwards to one target: host:port or \
             drt://…"
        }
    };
    Err(format!("--listen-ws: {why}"))
}

/// The forward as the bridge's target. A `scheme://host:port` entry is
/// dialled at `host:port`; its scheme only names a service to a p2p caller.
fn target(
    forward: &ForwardSpec,
    settings: &ServeSettings,
    roots: &[CertificateDer<'static>],
) -> Result<crate::bridge::Target, String> {
    match forward {
        ForwardSpec::One(entry) => {
            let host = if entry.host.contains(':') {
                format!("[{}]", entry.host)
            } else {
                entry.host.clone()
            };
            Ok(crate::bridge::Target::Tcp(format!("{host}:{}", entry.port)))
        }
        ForwardSpec::Peer(peer) => Ok(crate::bridge::Target::Open {
            to: peer.shown(),
            open: super::serve::peer_opener(peer, settings, roots),
        }),
        other => {
            check_forward(other)?;
            unreachable!("check_forward takes one target or a peer, and refuses the rest")
        }
    }
}

/// Bind, say where, and serve until the listener fails.
pub async fn run(role: &ListenWsRole, roots: &[CertificateDer<'static>]) -> Result<(), String> {
    let target = target(&role.forward, &role.settings, roots)?;
    let bind = SocketAddr::new(role.host.bind_ip(), role.port);
    let listener = tokio::net::TcpListener::bind(bind)
        .await
        .map_err(|e| format!("--listen-ws: cannot bind {bind}: {e}"))?;
    let bound = listener.local_addr().map_err(|e| e.to_string())?;
    eprintln!(
        "drt p2p: ws on {bound} forwarding to {}",
        role.forward.describe()
    );
    let accept = role.host.accept();
    let admit: Option<crate::bridge::Admit> = if accept.is_empty() {
        None
    } else {
        eprintln!(
            "drt p2p: --host {}: WebSockets from that range only",
            accept[0]
        );
        Some(std::sync::Arc::new(move |ip| {
            accept.iter().any(|range| range.contains(ip))
        }))
    };
    crate::bridge::serve(listener, target, admit, None).await
}

/// A `bridge` block's `drt://` `to`, for `drt start`: called with the
/// file's `default.stun` and `default.extra_roots`, and no headers.
pub fn block_target(to: &str, config: &RootConfig) -> Result<crate::bridge::Target, String> {
    let peer = Peer::parse(to)?;
    let roots =
        crate::roots::load_roots_named("default.extra_roots", &config.defaults.extra_roots)?;
    let settings = ServeSettings {
        stun: config.defaults.stun.clone(),
        ..ServeSettings::default()
    };
    target(&ForwardSpec::Peer(peer), &settings, &roots)
}
