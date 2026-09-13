//! The byte stream under the frames, and the one test double for it.
//!
//! # Surface
//!
//! Entry points:
//! - [`Channel`] — what a transport must provide: two non-blocking halves.
//! - [`ChannelError`] — gone, or broken, and nothing else.
//! - [`Loopback`] — an in-memory `Channel` for tests, with a hand-driven
//!   peer. Not a mock of the protocol: it moves bytes and nothing more.
//!
//! Configurable values: none. A transport's own knobs (an address, an
//! exec path) belong to that transport's constructor.
//!
//! Fan-out: the implementations are [`Loopback`] here and `ProcessChannel`
//! in `process` (unix). `spawn`, `tcp`, a Worker and a WebSocket are the
//! rows in `doc/Plugins.md` §4.1 that this trait exists to keep uniform.
//!
//! # Why both halves are non-blocking
//!
//! A plugin call is answered by a future the drive loop polls with a no-op
//! waker (`drt_swarm::pump`), so nothing here may ever wait: a read that
//! blocks holds every guest in the deployment, which is exactly the
//! property `connectors/exec` documents about itself and this channel is
//! built to avoid. `read_some` and `write_some` both return how many bytes
//! moved, and zero means "not now", never "finished".
//!
//! Writes are buffered rather than blocking for the same reason. A plugin
//! that is busy stops reading, its socket fills, and a host that insisted
//! on writing the whole request would stall on a plugin doing exactly what
//! it is supposed to be doing.

/// What can go wrong with the stream itself, as opposed to the frames on
/// it. Both are terminal for the channel.
#[derive(Debug, thiserror::Error)]
pub enum ChannelError {
    #[error("the plugin closed the channel")]
    Closed,
    #[error("the plugin channel failed: {0}")]
    Broken(String),
}

/// A byte stream to a plugin, in whichever way this target obtains one.
///
/// Neither method may block. Both report how many bytes moved, and zero
/// is the ordinary "nothing right now" answer on a polled socket.
pub trait Channel {
    /// Take what it can of `bytes`; the caller keeps the remainder and
    /// offers it again. `Ok(0)` means the peer is not reading yet.
    fn write_some(&mut self, bytes: &[u8]) -> Result<usize, ChannelError>;

    /// Append whatever has arrived to `out`. `Ok(0)` means nothing has.
    fn read_some(&mut self, out: &mut Vec<u8>) -> Result<usize, ChannelError>;
}

// depth: the test double.

/// An in-memory channel with the peer's side exposed, so the frame state
/// machine can be tested with no subprocess, no socket and no fixture
/// binary — on every target, including the ones that have none of those.
///
/// `to_peer` is what the host wrote; `from_peer` is what the peer has
/// queued for the host. A test drives the far side by reading the first
/// and pushing to the second, which is all a real plugin does.
#[derive(Default)]
pub struct Loopback {
    pub to_peer: Vec<u8>,
    pub from_peer: Vec<u8>,
    /// Bytes `write_some` will accept per call, to exercise partial
    /// writes. `None` accepts everything offered.
    pub write_limit: Option<usize>,
    /// How much the peer's side will hold before `write_some` answers
    /// zero, the way a real socket's buffer does when the plugin has
    /// stopped reading. `None` is a peer that never fills.
    pub capacity: Option<usize>,
    /// Set to fail every subsequent call, the way a dead plugin does.
    pub broken: Option<ChannelError>,
}

impl Loopback {
    pub fn new() -> Self {
        Self::default()
    }

    /// The peer's read: take everything the host has written.
    pub fn peer_take(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.to_peer)
    }

    /// The peer's write: queue bytes for the host to read.
    pub fn peer_put(&mut self, bytes: &[u8]) {
        self.from_peer.extend_from_slice(bytes);
    }
}

impl Channel for Loopback {
    fn write_some(&mut self, bytes: &[u8]) -> Result<usize, ChannelError> {
        if let Some(e) = &self.broken {
            return Err(match e {
                ChannelError::Closed => ChannelError::Closed,
                ChannelError::Broken(m) => ChannelError::Broken(m.clone()),
            });
        }
        let room = match self.capacity {
            Some(cap) => cap.saturating_sub(self.to_peer.len()),
            None => bytes.len(),
        };
        let n = self
            .write_limit
            .unwrap_or(bytes.len())
            .min(bytes.len())
            .min(room);
        self.to_peer.extend_from_slice(&bytes[..n]);
        Ok(n)
    }

    fn read_some(&mut self, out: &mut Vec<u8>) -> Result<usize, ChannelError> {
        if let Some(e) = &self.broken {
            return Err(match e {
                ChannelError::Closed => ChannelError::Closed,
                ChannelError::Broken(m) => ChannelError::Broken(m.clone()),
            });
        }
        let n = self.from_peer.len();
        out.append(&mut self.from_peer);
        Ok(n)
    }
}
