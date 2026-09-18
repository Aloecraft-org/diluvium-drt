//! The echo fixture: a real plugin, for the channel's tests to exec.
//!
//! # Surface
//!
//! Entry point: `main`. Obtains a byte stream, reads frames from it,
//! answers on it, and exits when the host closes it.
//!
//! Configurable values: none. Behaviour is chosen per call by `target`.
//!
//! Fan-out, two of them:
//!
//! - **How the stream is obtained**, which is the whole difference between
//!   the transports this fixture serves:
//!   * `fd 3` — the `process` transport put it there before exec. Unix
//!     only, because there is no fd 3 anywhere else.
//!   * dial-back — the `spawn` transport passed `--drt-plugin-dial
//!     127.0.0.1:PORT` and a secret in the environment, and the plugin
//!     connects and presents the secret. Every native target.
//!   `doc/Plugins.md` §4.1 claims "a plugin written for fd 3 becomes a
//!   `tcp` or `spawn` plugin by changing where it reads and writes and
//!   nothing else". `serve` below is that claim: one function, generic
//!   over `Read + Write`, and the two ways in are the only code that
//!   differs.
//! - **The `target` match** in `answer` —
//!   * `echo/say` — answer with the args, unchanged.
//!   * `echo/fail` — answer with a `plugin`-class error.
//!   * `echo/quit` — exit without answering, so the host sees a plugin that
//!     died mid-call.
//!   * `echo/junk` — write bytes that are not a frame, so the host sees a
//!     desynchronised stream.
//!   * anything else — answer with a `capability`-class error, which is what
//!     a plugin says about a target it does not serve. Never `denied`: that
//!     word is the dispatcher's.
//!
//! **This fixture blocks, and that is correct.** The non-blocking rule is
//! the *host's*, because the host has a drive loop full of other guests to
//! serve. A plugin has one job and may wait on its own socket.
//!
//! It encodes with this crate's own codec rather than by hand. That makes
//! it a test of the channel — fd passing, the dial-back, process lifetime,
//! backpressure — and not an independent check of the wire;
//! `doc/Plugins.md` §2 names a from-scratch C fixture for that, and it
//! lives on the diluvium side.

use std::io::{Read, Write};

use drt_plugin::frame::{
    self, ErrorClass, PluginError, Reply, ReplyBody, Request, PROTOCOL_VERSION,
};

fn main() {
    match std::env::args().position(|a| a == drt_plugin::spawn::DIAL_FLAG) {
        // The `spawn` transport: dial back, prove who we are, serve.
        Some(flag_at) => {
            let addr = std::env::args().nth(flag_at + 1).unwrap_or_else(|| {
                panic!(
                    "{} was given without an address",
                    drt_plugin::spawn::DIAL_FLAG
                )
            });
            let secret = std::env::var(drt_plugin::spawn::SECRET_ENV).unwrap_or_else(|_| {
                panic!(
                    "the host dialed this plugin but set no {}",
                    drt_plugin::spawn::SECRET_ENV
                )
            });
            let mut stream = std::net::TcpStream::connect(&addr)
                .unwrap_or_else(|e| panic!("the plugin could not dial {addr}: {e}"));
            // The secret first, raw, before any frame: the host reads
            // exactly this many bytes and will not look at a frame until
            // it matches.
            stream
                .write_all(secret.as_bytes())
                .unwrap_or_else(|e| panic!("the plugin could not present its secret: {e}"));
            stream
                .flush()
                .unwrap_or_else(|e| panic!("the plugin could not present its secret: {e}"));
            serve(&mut stream);
        }
        // The `process` transport: fd 3 is already the channel.
        None => serve(&mut from_fd_three()),
    }
}

/// The channel the `process` transport put on fd 3 before exec; see
/// `process::PLUGIN_FD`.
#[cfg(unix)]
fn from_fd_three() -> std::fs::File {
    use std::os::unix::io::FromRawFd;
    // SAFETY: the host `dup2`'d the plugin's end of a socketpair onto fd 3
    // before exec, and nothing else in this process owns it.
    unsafe { std::fs::File::from_raw_fd(3) }
}

/// There is no fd 3 off unix, so a host that started this fixture without
/// telling it where to dial has made a mistake worth naming.
#[cfg(not(unix))]
fn from_fd_three() -> std::fs::File {
    panic!(
        "this plugin was started with no {}, and there is no fd 3 on this platform \
         to fall back to",
        drt_plugin::spawn::DIAL_FLAG
    );
}

/// Serve frames over whatever stream the caller obtained.
///
/// The one function both transports share, and the point of the fixture:
/// nothing below this line knows how the bytes arrive.
fn serve<C: Read + Write>(channel: &mut C) {
    let mut pending = Vec::new();
    let mut chunk = [0u8; 4096];

    loop {
        while let Some((request, used)) = decode(&pending) {
            pending.drain(..used);
            if !answer(channel, request) {
                return;
            }
        }
        match channel.read(&mut chunk) {
            Ok(0) | Err(_) => return, // the host is gone.
            Ok(n) => pending.extend_from_slice(&chunk[..n]),
        }
    }
}

/// A frame off the front, or `None` for "not yet". A body this fixture
/// cannot read is the host's bug, and dying loudly is the right answer.
fn decode(buf: &[u8]) -> Option<(Request, usize)> {
    match frame::decode::<Request>(buf) {
        Ok(frame) => frame,
        Err(e) => panic!("the host sent a frame the fixture cannot read: {e}"),
    }
}

/// Answer one call. `false` means stop serving.
fn answer<C: Read + Write>(channel: &mut C, request: Request) -> bool {
    let body = match request.target.as_str() {
        "echo/say" => ReplyBody::Ok {
            value: request.args.unwrap_or(rmpv::Value::Nil),
        },
        "echo/fail" => ReplyBody::Err {
            error: PluginError {
                class: ErrorClass::Plugin,
                code: "asked_to_fail".into(),
                message: "the fixture was asked to fail".into(),
            },
        },
        "echo/quit" => return false,
        "echo/junk" => {
            // Not a frame: a length that promises far more than follows,
            // so the host's next decode is reading a body as a header.
            let _ = channel.write_all(&[0xff, 0xff, 0xff, 0xf0, 0x01, 0x02]);
            let _ = channel.flush();
            return true;
        }
        other => ReplyBody::Err {
            error: PluginError {
                class: ErrorClass::Capability,
                code: "no_such_target".into(),
                message: format!("this plugin does not serve '{other}'"),
            },
        },
    };

    let reply = Reply {
        version: PROTOCOL_VERSION,
        id: request.id,
        is_final: true,
        body,
    };
    let bytes = frame::encode(&reply).expect("a reply this fixture built must encode");
    channel.write_all(&bytes).is_ok() && channel.flush().is_ok()
}
