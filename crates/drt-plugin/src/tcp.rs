//! The `tcp` transport: a stream obtained by dialing, or handed in.
//!
//! # Surface
//!
//! Entry points:
//! - [`TcpChannel::dial`] — connect to an address and hold the stream.
//! - [`TcpChannel::from_stream`] — adopt a stream obtained elsewhere: the
//!   `spawn` row's dial-back, or a connection a node accepted.
//! - [`TcpChannel::peer`] — the far end, for a caller that names it.
//!
//! Configurable values: none. The address is the caller's.
//!
//! Fan-out: none. One transport; `process` is the other, and they share
//! only [`Channel`].
//!
//! `ProcessChannel` minus the fork (`doc/Plan-0.7.0.md` §8): the same two
//! non-blocking halves with the same answers — zero for "not now",
//! `Closed` for a far end that has gone — over a stream this side did not
//! start a process for. What it lacks is exactly what the fork gave: DRT
//! does not own the far end's lifetime, so dropping the channel closes
//! the stream and sweeps nothing. A peer link needs this and so does a
//! plugin over a socket. The frames on it are the same frames either way,
//! and one frame vocabulary for both is what §8 forbids: two protocols
//! over one `Channel`, never one protocol.

use std::io::{ErrorKind, Read, Write};
use std::net::{SocketAddr, TcpStream};

use crate::channel::{Channel, ChannelError};

/// A stream to a plugin, or to whatever answered at the address.
#[derive(Debug)]
pub struct TcpChannel {
    stream: TcpStream,
    peer: SocketAddr,
}

impl TcpChannel {
    /// Connect to `addr` and hold the stream. Nobody answering is named
    /// with the address, since that is the one thing the operator can fix.
    pub fn dial(addr: SocketAddr) -> Result<Self, ChannelError> {
        let stream = TcpStream::connect(addr)
            .map_err(|e| ChannelError::Broken(format!("no plugin answers at {addr}: {e}")))?;
        Self::from_stream(stream)
    }

    /// Adopt a stream this side obtained some other way.
    pub fn from_stream(stream: TcpStream) -> Result<Self, ChannelError> {
        stream.set_nonblocking(true).map_err(|e| {
            ChannelError::Broken(format!("the plugin channel would not go non-blocking: {e}"))
        })?;
        // Frames are small and latency is the point: Nagle would hold a
        // request back waiting for a second one to share a segment with.
        stream.set_nodelay(true).map_err(|e| {
            ChannelError::Broken(format!("the plugin channel would not set nodelay: {e}"))
        })?;
        let peer = stream
            .peer_addr()
            .map_err(|e| ChannelError::Broken(format!("the plugin channel has no peer: {e}")))?;
        Ok(Self { stream, peer })
    }

    /// The far end, for a caller that wants to name it in a message.
    pub fn peer(&self) -> SocketAddr {
        self.peer
    }
}

impl Channel for TcpChannel {
    fn write_some(&mut self, bytes: &[u8]) -> Result<usize, ChannelError> {
        if bytes.is_empty() {
            return Ok(0);
        }
        loop {
            match self.stream.write(bytes) {
                Ok(n) => return Ok(n),
                Err(e) => match e.kind() {
                    ErrorKind::Interrupted => continue,
                    ErrorKind::WouldBlock => return Ok(0),
                    ErrorKind::BrokenPipe
                    | ErrorKind::ConnectionReset
                    | ErrorKind::ConnectionAborted => return Err(ChannelError::Closed),
                    _ => return Err(ChannelError::Broken(e.to_string())),
                },
            }
        }
    }

    fn read_some(&mut self, out: &mut Vec<u8>) -> Result<usize, ChannelError> {
        let mut buf = [0u8; 8192];
        loop {
            match self.stream.read(&mut buf) {
                // EOF: the far end closed its side.
                Ok(0) => return Err(ChannelError::Closed),
                Ok(n) => {
                    out.extend_from_slice(&buf[..n]);
                    return Ok(n);
                }
                Err(e) => match e.kind() {
                    ErrorKind::Interrupted => continue,
                    ErrorKind::WouldBlock => return Ok(0),
                    ErrorKind::ConnectionReset | ErrorKind::ConnectionAborted => {
                        return Err(ChannelError::Closed)
                    }
                    _ => return Err(ChannelError::Broken(e.to_string())),
                },
            }
        }
    }
}
