//! The tcp transport against a real socket.
//!
//! # Surface
//!
//! Entry points: the tests. Each drives a [`Session`] over a
//! [`TcpChannel`] the way the drive loop will, against a far side that is
//! a thread speaking this crate's own codec — the echo fixture minus the
//! process, which is what §8 says the transport is.
//!
//! Configurable values:
//! - `DEADLINE` — how long a test waits before calling the far side hung.
//! - `TICK` — the pause between polls, standing in for the drive loop's
//!   own cadence.
//!
//! Fan-out: none.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::thread;
use std::time::{Duration, Instant};

use drt_plugin::frame::{
    self, ErrorClass, PluginError, Reply, ReplyBody, Request, PROTOCOL_VERSION,
};
use drt_plugin::session::{Session, SessionError};
use drt_plugin::tcp::TcpChannel;

const DEADLINE: Duration = Duration::from_secs(10);
const TICK: Duration = Duration::from_millis(2);

/// The far side: frames in, answers out, blocking as a plugin may.
/// `echo/say` answers with the args; `echo/quit` hangs up; anything else
/// is a capability-class refusal, never `denied`.
fn serve(mut stream: TcpStream) {
    let mut pending = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        while let Some((request, used)) = frame::decode::<Request>(&pending).expect("a frame") {
            pending.drain(..used);
            let body = match request.target.as_str() {
                "echo/say" => ReplyBody::Ok {
                    value: request.args.unwrap_or(rmpv::Value::Nil),
                },
                "echo/quit" => return,
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
            let bytes = frame::encode(&reply).unwrap();
            if stream.write_all(&bytes).is_err() {
                return;
            }
        }
        match stream.read(&mut chunk) {
            Ok(0) | Err(_) => return,
            Ok(n) => pending.extend_from_slice(&chunk[..n]),
        }
    }
}

/// A plugin listening on loopback: serves the first connection on its
/// own thread. Returns where to dial.
fn plugin() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    thread::spawn(move || {
        if let Ok((stream, _)) = listener.accept() {
            serve(stream);
        }
    });
    addr
}

fn dialed() -> Session<TcpChannel> {
    Session::new(TcpChannel::dial(plugin()).unwrap(), 0)
}

/// Poll until `id` is answered, the way the drive loop would.
fn wait_for(s: &mut Session<TcpChannel>, id: u64) -> Result<Reply, SessionError> {
    let until = Instant::now() + DEADLINE;
    loop {
        s.poll()?;
        if let Some(reply) = s.take(id)? {
            return Ok(reply);
        }
        assert!(
            Instant::now() < until,
            "the far side never answered call {id}"
        );
        thread::sleep(TICK);
    }
}

#[test]
fn a_call_reaches_a_dialed_plugin_and_comes_back() {
    let mut s = dialed();
    let id = s
        .begin("echo/say", Some(rmpv::Value::from("over tcp")))
        .unwrap();
    let reply = wait_for(&mut s, id).unwrap();
    assert_eq!(
        reply.body,
        ReplyBody::Ok {
            value: rmpv::Value::from("over tcp")
        }
    );
}

#[test]
fn three_calls_over_one_stream_each_get_their_own_answer() {
    let mut s = dialed();
    let ids: Vec<u64> = (0..3)
        .map(|i| {
            s.begin("echo/say", Some(rmpv::Value::from(format!("call {i}"))))
                .unwrap()
        })
        .collect();
    for (i, id) in ids.iter().enumerate() {
        let reply = wait_for(&mut s, *id).unwrap();
        assert_eq!(
            reply.body,
            ReplyBody::Ok {
                value: rmpv::Value::from(format!("call {i}"))
            },
            "call {id} got its own answer"
        );
    }
}

/// A payload far larger than a socket buffer: the write blocks partway,
/// `write_some` answers zero, and the next poll carries on.
#[test]
fn a_payload_larger_than_the_socket_buffer_still_round_trips() {
    let mut s = dialed();
    let big = "x".repeat(1024 * 1024);
    let id = s
        .begin("echo/say", Some(rmpv::Value::from(big.clone())))
        .unwrap();
    let reply = wait_for(&mut s, id).unwrap();
    assert_eq!(
        reply.body,
        ReplyBody::Ok {
            value: rmpv::Value::from(big)
        }
    );
}

/// The far end hanging up is the channel closing, seen on the next poll
/// that reads — not a hang, not a stale "not yet".
#[test]
fn the_far_end_closing_is_the_channel_closing() {
    let mut s = dialed();
    let id = s.begin("echo/quit", None).unwrap();
    let until = Instant::now() + DEADLINE;
    let err = loop {
        if let Err(e) = s.poll() {
            break e;
        }
        assert!(s.take(id).unwrap().is_none(), "quit never answers");
        assert!(
            Instant::now() < until,
            "the closed stream was never noticed"
        );
        thread::sleep(TICK);
    };
    assert!(err.to_string().contains("closed"), "{err}");
}

/// The `spawn` row's other half: the host listens and the plugin dials
/// back. A stream obtained by accepting is the same channel.
#[test]
fn a_stream_obtained_by_accepting_is_the_same_channel() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    thread::spawn(move || serve(TcpStream::connect(addr).unwrap()));
    let (stream, _) = listener.accept().unwrap();
    let mut s = Session::new(TcpChannel::from_stream(stream).unwrap(), 0);
    let id = s
        .begin("echo/say", Some(rmpv::Value::from("dialed back")))
        .unwrap();
    let reply = wait_for(&mut s, id).unwrap();
    assert_eq!(
        reply.body,
        ReplyBody::Ok {
            value: rmpv::Value::from("dialed back")
        }
    );
}

/// Nobody answering is refused with the address in the sentence.
#[test]
fn nobody_answering_is_named_by_address() {
    let addr = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap();
    // The listener is dropped: the port is closed.
    let err = TcpChannel::dial(addr).unwrap_err();
    assert!(err.to_string().contains(&addr.to_string()), "{err}");
}
