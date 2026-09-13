//! The unix transport against a real subprocess.
//!
//! # Surface
//!
//! Entry points: the tests. Each spawns `plugin-echo` over a socketpair
//! and drives a [`Session`] the way the drive loop will.
//!
//! Configurable values:
//! - `DEADLINE` — how long a test waits before calling a plugin hung.
//! - `TICK` — the pause between polls, standing in for the drive loop's
//!   own cadence.
//!
//! Fan-out: none. One transport, one fixture.
//!
//! These are the cases the in-memory `Loopback` cannot reach: that fd 3
//! really arrives, that a non-blocking socket really answers zero rather
//! than waiting, and that a plugin exiting is seen as the channel closing.

#![cfg(unix)]

use std::path::Path;
use std::time::{Duration, Instant};

use drt_plugin::channel::ChannelError;
use drt_plugin::frame::{ErrorClass, ReplyBody};
use drt_plugin::process::ProcessChannel;
use drt_plugin::session::{Session, SessionError};

const DEADLINE: Duration = Duration::from_secs(10);
const TICK: Duration = Duration::from_millis(2);

fn fixture() -> Session<ProcessChannel> {
    let exec = Path::new(env!("CARGO_BIN_EXE_plugin-echo"));
    let channel = ProcessChannel::spawn(exec, &[]).expect("the fixture starts");
    Session::new(channel, 0)
}

/// Poll until `id` is answered, the way the drive loop would.
fn wait_for(
    s: &mut Session<ProcessChannel>,
    id: u64,
) -> Result<drt_plugin::frame::Reply, SessionError> {
    let until = Instant::now() + DEADLINE;
    loop {
        s.poll()?;
        if let Some(reply) = s.take(id)? {
            return Ok(reply);
        }
        assert!(
            Instant::now() < until,
            "the fixture never answered call {id}"
        );
        std::thread::sleep(TICK);
    }
}

#[test]
fn a_call_reaches_a_real_subprocess_and_comes_back() {
    let mut s = fixture();
    let id = s
        .begin("echo/say", Some(rmpv::Value::from("over fd 3")))
        .unwrap();
    let reply = wait_for(&mut s, id).unwrap();
    assert_eq!(
        reply.body,
        ReplyBody::Ok {
            value: rmpv::Value::from("over fd 3")
        }
    );
}

/// The property the `Loopback` proves in memory, proven again where the
/// replies really do share one socket.
#[test]
fn three_calls_over_one_socket_each_get_their_own_answer() {
    let mut s = fixture();
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
    let mut s = fixture();
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

#[test]
fn an_error_reply_arrives_as_an_error_and_leaves_the_session_usable() {
    let mut s = fixture();
    let id = s.begin("echo/fail", None).unwrap();
    match wait_for(&mut s, id).unwrap().body {
        ReplyBody::Err { error } => {
            assert_eq!(error.class, ErrorClass::Plugin);
            assert_eq!(error.code, "asked_to_fail");
        }
        other => panic!("expected an error, got {other:?}"),
    }

    let again = s.begin("echo/say", Some(rmpv::Value::from(1))).unwrap();
    assert!(wait_for(&mut s, again).is_ok(), "the channel survived it");
}

/// A plugin refusing a target says `capability`, never `denied` — that
/// word belongs to the dispatcher, which never reached the plugin.
#[test]
fn an_unserved_target_is_the_plugins_refusal_not_a_denial() {
    let mut s = fixture();
    let id = s.begin("echo/nothing_here", None).unwrap();
    match wait_for(&mut s, id).unwrap().body {
        ReplyBody::Err { error } => assert_eq!(error.class, ErrorClass::Capability),
        other => panic!("expected a refusal, got {other:?}"),
    }
}

#[test]
fn a_plugin_that_exits_mid_call_closes_the_channel() {
    let mut s = fixture();
    let id = s.begin("echo/quit", None).unwrap();

    let until = Instant::now() + DEADLINE;
    let ended = loop {
        if let Err(e) = s.poll() {
            break e;
        }
        assert!(Instant::now() < until, "the exit was never noticed");
        std::thread::sleep(TICK);
    };
    assert!(
        matches!(ended, SessionError::Channel(ChannelError::Closed)),
        "a plugin that exits reads as the channel closing, got {ended}"
    );
    assert!(s.take(id).is_err(), "and the outstanding call is told");
}

#[test]
fn a_plugin_that_misframes_ends_the_session_by_name() {
    let mut s = fixture();
    let id = s.begin("echo/junk", None).unwrap();

    let until = Instant::now() + DEADLINE;
    let ended = loop {
        if let Err(e) = s.poll() {
            break e;
        }
        assert!(Instant::now() < until, "the junk was never read");
        std::thread::sleep(TICK);
    };
    assert!(
        matches!(ended, SessionError::Frame(_)),
        "a desynced stream is a frame failure, got {ended}"
    );
    assert!(s.take(id).is_err(), "and the call that caused it is told");
}

#[test]
fn a_relative_exec_is_refused_rather_than_searched_for() {
    let err = ProcessChannel::spawn(Path::new("plugin-echo"), &[]).unwrap_err();
    let said = err.to_string();
    assert!(
        said.contains("absolute") && said.contains("PATH"),
        "the refusal says why: {said}"
    );
}

#[test]
fn a_plugin_that_is_not_there_fails_at_spawn_not_at_the_first_call() {
    let err = ProcessChannel::spawn(Path::new("/nonexistent/plugin"), &[]).unwrap_err();
    assert!(
        err.to_string().contains("would not start"),
        "named at spawn: {err}"
    );
}
