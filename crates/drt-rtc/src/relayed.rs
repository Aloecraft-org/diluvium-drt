//! A relayed address (`doc/P2P.md` §2.6): a TURN allocation the program
//! made and hands to a [`crate::Host`] or a [`crate::caller::Caller`] as
//! one more local candidate. drt-rtc speaks no TURN itself; the allocation
//! is two channels of datagrams, each tagged with the peer's address as the
//! relay saw it, so whatever made it (a TURN client, a test) stays outside.
//!
//! ## surface block
//!
//! - Entry points: [`Relayed`], built by the program; [`RelayPolicy`], how
//!   a host uses it.
//! - Configurable: none.
//! - Fan-out: [`RelayPolicy`]'s two policies.

use std::net::SocketAddr;

use str0m::Candidate;
use tokio::sync::mpsc;

/// A datagram and the peer it goes to or came from.
pub type Datagram = (Vec<u8>, SocketAddr);

/// One allocation: where peers send to reach this side, and the datagrams
/// that cross it each way.
pub struct Relayed {
    /// The relayed transport address the allocation holds, which the
    /// record publishes and peers send to.
    pub address: SocketAddr,
    /// The local address the allocation was asked from: the candidate's
    /// related address, never published.
    pub local: SocketAddr,
    /// Datagrams this side sends through the relay, to the peer named.
    pub outbound: mpsc::UnboundedSender<Datagram>,
    /// Datagrams that came through the relay, from the peer named.
    pub inbound: mpsc::UnboundedReceiver<Datagram>,
}

/// How a host uses its relayed address beside its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RelayPolicy {
    /// One candidate more, below the direct ones: ICE picks it only when
    /// nothing direct works.
    Also,
    /// The only candidate: the host publishes and uses nothing else, so no
    /// peer learns its address and every session crosses the relay. The
    /// `relay` of WebRTC's ICE transport policy.
    Only,
}

impl Relayed {
    /// The local candidate str0m pairs, and the line a record publishes
    /// for it, with its related address blanked as a srflx line's is.
    pub(crate) fn candidate(&self) -> Result<(Candidate, String), String> {
        let c = Candidate::relayed(self.address, self.local, "udp")
            .map_err(|e| format!("rtc: {} cannot be a relayed candidate: {e}", self.address))?;
        let line = crate::host::srflx_line(&c, &self.address);
        Ok((c, line))
    }
}

/// The next datagram through the relay, or never when there is none: a
/// `select!` arm that is idle without one.
pub(crate) async fn next(inbound: &mut Option<mpsc::UnboundedReceiver<Datagram>>) -> Datagram {
    match inbound {
        Some(rx) => match rx.recv().await {
            Some(d) => d,
            None => std::future::pending().await,
        },
        None => std::future::pending().await,
    }
}
