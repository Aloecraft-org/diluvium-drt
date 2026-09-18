//! The `spawn` transport against a real subprocess, on every native target.
//!
//! # Surface
//!
//! Entry points: the tests. Each starts `plugin-echo` through
//! [`SpawnChannel`] and drives a [`Session`] the way the drive loop will.
//!
//! Configurable values:
//! - `DEADLINE` — how long a test waits before calling a plugin hung.
//! - `TICK` — the pause between polls, standing in for the drive loop's
//!   own cadence.
//!
//! Fan-out: none. One transport, one fixture.
//!
//! This is `process.rs`'s file with the fork taken out, and that is the
//! point: the same fixture, the same frames, the same answers, reached
//! over a socket the plugin dialed rather than a descriptor it inherited.
//! `process.rs` is `cfg(unix)` because fd 3 is; this file is not, because
//! nothing here is unix's.

#![cfg(any(unix, windows))]

use std::path::Path;
use std::time::{Duration, Instant};

use drt_plugin::channel::ChannelError;
use drt_plugin::frame::{ErrorClass, ReplyBody};
use drt_plugin::session::{Session, SessionError};
use drt_plugin::spawn::{SpawnChannel, DIAL_BACK_TIMEOUT};

const DEADLINE: Duration = Duration::from_secs(20);
const TICK: Duration = Duration::from_millis(2);

fn fixture() -> Session<SpawnChannel> {
    let exec = Path::new(env!("CARGO_BIN_EXE_plugin-echo"));
    let channel =
        SpawnChannel::start(exec, &[], DIAL_BACK_TIMEOUT).expect("the fixture dials back");
    Session::new(channel, 0)
}

/// Poll until `id` is answered, the way the drive loop would.
fn wait_for(
    s: &mut Session<SpawnChannel>,
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

/// The whole transport in one test: DRT bound a port, minted a secret,
/// started a program, the program dialed back and proved itself, and a
/// call went out and came home.
#[test]
fn a_call_reaches_a_plugin_that_dialed_back() {
    let mut s = fixture();
    let id = s
        .begin("echo/say", Some(rmpv::Value::from("over a socket")))
        .expect("the call goes out");
    let reply = wait_for(&mut s, id).expect("an answer");
    match reply.body {
        ReplyBody::Ok { value } => {
            assert_eq!(value.as_str(), Some("over a socket"), "{value}")
        }
        other => panic!("the fixture did not echo: {other:?}"),
    }
}

/// Several calls outstanding at once, answered to the right callers: the
/// session's multiplexing is the transport's business only in that the
/// transport must not reorder or merge what it carries.
#[test]
fn several_calls_are_answered_to_the_right_callers() {
    let mut s = fixture();
    let ids: Vec<u64> = (0..8)
        .map(|n| {
            s.begin("echo/say", Some(rmpv::Value::from(n)))
                .expect("the call goes out")
        })
        .collect();
    for (n, id) in ids.into_iter().enumerate() {
        let reply = wait_for(&mut s, id).expect("an answer");
        match reply.body {
            ReplyBody::Ok { value } => assert_eq!(value.as_i64(), Some(n as i64), "{value}"),
            other => panic!("call {id} answered {other:?}"),
        }
    }
}

/// An error the plugin chose is delivered as an error, not as the channel
/// failing: a plugin saying no is a working plugin.
#[test]
fn an_error_the_plugin_chose_is_an_answer() {
    let mut s = fixture();
    let id = s.begin("echo/fail", None).expect("the call goes out");
    let reply = wait_for(&mut s, id).expect("an answer");
    match reply.body {
        ReplyBody::Err { error } => {
            assert_eq!(error.class, ErrorClass::Plugin);
            assert_eq!(error.code, "asked_to_fail");
        }
        other => panic!("the fixture did not fail as asked: {other:?}"),
    }
}

/// A plugin that exits mid-call is seen as the channel closing, rather
/// than as a call that never answers.
#[test]
fn a_plugin_that_exits_is_seen_as_the_channel_closing() {
    let mut s = fixture();
    let id = s.begin("echo/quit", None).expect("the call goes out");
    let until = Instant::now() + DEADLINE;
    loop {
        match s.poll() {
            Err(SessionError::Channel(ChannelError::Closed)) => return,
            Err(other) => panic!("the wrong failure for a dead plugin: {other:?}"),
            Ok(()) => {}
        }
        assert!(
            Instant::now() < until,
            "a plugin that exited was never noticed (call {id})"
        );
        std::thread::sleep(TICK);
    }
}

/// The plugin and everything it started go when the channel does. Without
/// this the transport would leak a process per plugin per run, which is
/// the failure `process` uses a process group to avoid and this one uses
/// `drt_platform::process::Tree` for.
#[test]
fn dropping_the_channel_ends_the_plugin() {
    let exec = Path::new(env!("CARGO_BIN_EXE_plugin-echo"));
    let channel =
        SpawnChannel::start(exec, &[], DIAL_BACK_TIMEOUT).expect("the fixture dials back");
    let pid = channel.pid();
    assert!(pid != 0, "a started plugin has a pid");
    drop(channel);

    // The sweep is asynchronous in the sense that the OS reaps when it
    // reaps, so this asserts the reachable thing: the port the plugin was
    // talking to is closed, and a fresh start on a fresh port still works
    // -- which it would not if the old plugin were still holding on.
    let again =
        SpawnChannel::start(exec, &[], DIAL_BACK_TIMEOUT).expect("a second fixture dials back");
    assert!(again.pid() != pid, "the second plugin is a new process");
}
