//! A byte stream in a page, for protocols that want one (doc/SshInBrowser.md).
//!
//! The page owns the `WebSocket`; this owns two channel ends. That split is
//! the whole design, and it is not a style preference: a `WebSocket` is a
//! `JsValue` and therefore not `Send`, while `russh::server::run_stream`
//! requires `R: AsyncRead + AsyncWrite + Send`. A stream holding the socket
//! could not be handed to it. A stream holding only channel ends can, and
//! the socket stays where it already lives -- the same shape `XtermTerminal`
//! uses for the keyboard, for the same reason.
//!
//! Nothing here imports a WebSocket API, so a page may hand over a real
//! `WebSocket`, a `RTCDataChannel`, a relayed pair, or a test double. What
//! is required is only that something pumps [`Socket`].
//!
//! ## surface block
//!
//! - [`WsStream`]: the Rust half. `AsyncRead + AsyncWrite + Send`.
//! - [`Socket`]: the page's half, exported as `DrtSocket`.
//! - [`channel`]: makes the pair.
//! - The page's loop, which is the whole integration:
//!
//! ```js
//! const { stream, socket } = DrtSocket.pair();     // stream goes to Rust
//! ws.binaryType = 'arraybuffer';
//! ws.onmessage = (e) => socket.deliver(new Uint8Array(e.data));
//! ws.onclose = () => socket.close();
//! (async () => {                                   // Rust -> the wire
//!   for (;;) {
//!     const out = await socket.nextOutgoing();
//!     if (out === undefined) break;
//!     ws.send(out);
//!   }
//! })();
//! ```

use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::sync::mpsc;

/// How many byte-chunks may queue in either direction before a sender
/// waits. Unbounded would let a page that stops reading grow without limit;
/// this is a backpressure point, not a tuning knob anyone has needed to
/// move.
const DEPTH: usize = 256;

/// The stream russh (or anything else taking a byte stream) is handed.
///
/// `Send`, because it holds channel ends and nothing else. The socket is
/// the page's.
pub struct WsStream {
    incoming: mpsc::Receiver<Vec<u8>>,
    outgoing: mpsc::Sender<Vec<u8>>,
    /// What a read could not fit in the caller's buffer last time.
    rest: Vec<u8>,
}

/// The page's end: what a socket's events reach, and what it sends from.
pub struct Socket {
    /// Taken on `close`, because dropping the last sender is what a read
    /// sees as end of input. Held in an `Option` for exactly that.
    to_rust: Option<mpsc::Sender<Vec<u8>>>,
    from_rust: Option<mpsc::Receiver<Vec<u8>>>,
}

/// One direction's queue, named so a host can hold it.
pub type Outgoing = mpsc::Receiver<Vec<u8>>;

/// A stream and the socket end that drives it.
pub fn channel() -> (WsStream, Socket) {
    let (to_rust, incoming) = mpsc::channel(DEPTH);
    let (outgoing, from_rust) = mpsc::channel(DEPTH);
    (
        WsStream {
            incoming,
            outgoing,
            rest: Vec::new(),
        },
        Socket {
            to_rust: Some(to_rust),
            from_rust: Some(from_rust),
        },
    )
}

impl Socket {
    /// Bytes that arrived on the wire.
    ///
    /// Dropped when the stream is gone, which is the honest answer for a
    /// session that ended: the page's loop sees the close and stops.
    pub fn deliver(&self, bytes: Vec<u8>) -> bool {
        self.to_rust
            .as_ref()
            .is_some_and(|tx| tx.try_send(bytes).is_ok())
    }

    /// The next chunk the stream wants written, or `None` once it is over.
    pub async fn next_outgoing(&mut self) -> Option<Vec<u8>> {
        self.from_rust.as_mut()?.recv().await
    }

    /// The outgoing queue itself, for a host that must own it -- the
    /// wasm-bindgen surface, whose promise outlives the call that made it.
    /// Taken once; `close` still ends the loop.
    pub fn take_outgoing(&mut self) -> Option<Outgoing> {
        self.from_rust.take()
    }

    /// The wire closed. Reads see end of input.
    pub fn close(&mut self) {
        self.from_rust = None;
        // Dropping the sender is what `poll_read` reads as EOF.
        self.to_rust = None;
    }
}

// ---------------------------------------------------------------------------
// depth: the two traits
// ---------------------------------------------------------------------------

impl AsyncRead for WsStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let me = self.get_mut();
        if me.rest.is_empty() {
            match me.incoming.poll_recv(cx) {
                Poll::Ready(Some(chunk)) => me.rest = chunk,
                // Every sender gone: the page closed, which is end of input
                // rather than an error. A protocol above reads it as the
                // peer hanging up.
                Poll::Ready(None) => return Poll::Ready(Ok(())),
                Poll::Pending => return Poll::Pending,
            }
        }
        let n = me.rest.len().min(buf.remaining());
        buf.put_slice(&me.rest[..n]);
        me.rest.drain(..n);
        Poll::Ready(Ok(()))
    }
}

impl AsyncWrite for WsStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let me = self.get_mut();
        match me.outgoing.try_send(buf.to_vec()) {
            Ok(()) => Poll::Ready(Ok(buf.len())),
            // The page is behind. Waking on the next poll is the
            // backpressure `DEPTH` exists to apply.
            Err(mpsc::error::TrySendError::Full(_)) => {
                cx.waker().wake_by_ref();
                Poll::Pending
            }
            Err(mpsc::error::TrySendError::Closed(_)) => Poll::Ready(Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "the page's socket is gone",
            ))),
        }
    }

    /// Nothing is buffered here: a write is queued for the page, and when
    /// it reaches the wire is the socket's business.
    fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        // Dropping the sender ends the page's `nextOutgoing` loop, which is
        // how it learns to close the socket.
        let me = self.get_mut();
        let (dead, _) = mpsc::channel(1);
        me.outgoing = dead;
        Poll::Ready(Ok(()))
    }
}

#[cfg(test)]
mod tests {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use super::*;

    /// Bytes cross both ways, and a read that does not fit is not lost.
    #[tokio::test]
    async fn the_stream_carries_bytes_both_ways() {
        let (mut stream, mut socket) = channel();

        assert!(socket.deliver(b"from the wire".to_vec()));
        // A buffer smaller than the chunk: the rest waits rather than
        // vanishing, which is the one thing `rest` exists for.
        let mut small = [0u8; 4];
        stream.read_exact(&mut small).await.unwrap();
        assert_eq!(&small, b"from");
        let mut more = [0u8; 9];
        stream.read_exact(&mut more).await.unwrap();
        assert_eq!(&more, b" the wire");

        stream.write_all(b"to the wire").await.unwrap();
        assert_eq!(socket.next_outgoing().await.unwrap(), b"to the wire");
    }

    /// The page hanging up is end of input, not an error: a protocol above
    /// reads it as the peer going away.
    #[tokio::test]
    async fn a_closed_socket_reads_as_end_of_input() {
        let (mut stream, mut socket) = channel();
        socket.deliver(b"last".to_vec());
        socket.close();

        let mut all = Vec::new();
        stream.read_to_end(&mut all).await.unwrap();
        assert_eq!(all, b"last");
    }

    /// And a write with nobody left to take it is a broken pipe rather than
    /// a silent success.
    #[tokio::test]
    async fn a_write_after_the_page_is_gone_fails_by_name() {
        let (mut stream, socket) = channel();
        drop(socket);
        let err = stream.write_all(b"nobody home").await.unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::BrokenPipe);
    }

    /// The bound that the whole design exists to satisfy.
    #[test]
    fn the_stream_is_send() {
        fn requires_send<T: Send>() {}
        requires_send::<WsStream>();
    }
}
