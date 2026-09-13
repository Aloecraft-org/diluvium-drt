//! The echo fixture: a real plugin, for the channel's tests to exec.
//!
//! # Surface
//!
//! Entry point: `main`. Reads frames from fd 3, answers on fd 3, exits
//! when the host closes the channel.
//!
//! Configurable values: none. Behaviour is chosen per call by `target`.
//!
//! Fan-out: the `target` match below is the whole of it —
//! - `echo/say` — answer with the args, unchanged.
//! - `echo/fail` — answer with a `plugin`-class error.
//! - `echo/quit` — exit without answering, so the host sees a plugin that
//!   died mid-call.
//! - `echo/junk` — write bytes that are not a frame, so the host sees a
//!   desynchronised stream.
//! - anything else — answer with a `capability`-class error, which is what
//!   a plugin says about a target it does not serve. Never `denied`: that
//!   word is the dispatcher's.
//!
//! **This fixture blocks, and that is correct.** The non-blocking rule is
//! the *host's*, because the host has a drive loop full of other guests to
//! serve. A plugin has one job and may wait on its own socket.
//!
//! It encodes with this crate's own codec rather than by hand. That makes
//! it a test of the channel — fd passing, process lifetime, backpressure —
//! and not an independent check of the wire; `doc/Plugins.md` §2 names a
//! from-scratch C fixture for that, and it lives on the diluvium side.

use std::io::{Read, Write};
use std::os::unix::io::FromRawFd;

use drt_plugin::frame::{
    self, ErrorClass, PluginError, Reply, ReplyBody, Request, PROTOCOL_VERSION,
};

fn main() {
    // fd 3 is put there by the host before exec; see `process::PLUGIN_FD`.
    let mut channel = unsafe { std::fs::File::from_raw_fd(3) };
    let mut pending = Vec::new();
    let mut chunk = [0u8; 4096];

    loop {
        while let Some((request, used)) = decode(&pending) {
            pending.drain(..used);
            if !answer(&mut channel, request) {
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
fn answer(channel: &mut std::fs::File, request: Request) -> bool {
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
