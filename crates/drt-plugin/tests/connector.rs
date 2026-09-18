//! A plugin served as a connector, against a real subprocess.
//!
//! # Surface
//!
//! Entry points: the tests. Each builds a manifest naming the echo
//! fixture, wraps it in a [`PluginConnector`], and calls it the way the
//! dispatcher will.
//!
//! Configurable values: none.
//!
//! Fan-out: none. One connector, one fixture.
//!
//! An integration test rather than a unit one for a mundane reason that
//! decides where every fixture-driving test in this crate lives:
//! `CARGO_BIN_EXE_plugin-echo` is set for integration tests and not for
//! unit tests, so a test that needs a real program to start has to be
//! here.

#![cfg(any(unix, windows))]

use std::time::{Duration, Instant};

use drt_caps::Scope;
use drt_connector::Connector;
use drt_plugin::connector::PluginConnector;
use drt_plugin::manifest::Manifest;

fn manifest(extra: &str) -> Manifest {
    let exec = env!("CARGO_BIN_EXE_plugin-echo");
    Manifest::parse(
        format!(r#"{{"family":"echo","transport":"spawn","scope":"root","exec":"{exec}"{extra}}}"#)
            .as_bytes(),
    )
    .expect("the fixture's manifest parses")
}

fn connector(extra: &str) -> PluginConnector {
    PluginConnector::new(manifest(extra)).expect("a root manifest is served")
}

fn call(c: &PluginConnector, target: &str, args: Option<rmpv::Value>) -> drt_connector::CallResult {
    let scope: Option<&Scope> = None;
    pollster::block_on(c.call(target, args, scope))
}

/// The whole segment in one test: a manifest, a connector, a call, and an
/// answer that came from another process.
#[test]
fn a_call_reaches_the_plugin_and_the_answer_comes_back() {
    let c = connector("");
    let answer = call(
        &c,
        "echo/say",
        Some(rmpv::Value::from("through the connector")),
    )
    .expect("an answer");
    assert_eq!(answer.as_str(), Some("through the connector"), "{answer}");
}

/// The plugin is started once, on first use, and the second call reaches
/// the same process rather than a new one.
#[test]
fn the_plugin_starts_once_and_serves_every_call() {
    let c = connector("");
    for n in 0..4 {
        let answer = call(&c, "echo/say", Some(rmpv::Value::from(n))).expect("an answer");
        assert_eq!(answer.as_i64(), Some(n), "{answer}");
    }
}

/// The plugin's own error is an error, with its code carried through
/// rather than flattened.
#[test]
fn an_error_the_plugin_chose_is_relayed() {
    let c = connector("");
    let err = call(&c, "echo/fail", None).unwrap_err();
    assert!(err.to_string().contains("asked_to_fail"), "{err}");
}

/// A target the plugin does not serve is an ordinary error, never
/// `denied`: that word is the dispatcher's and a plugin does not borrow it.
#[test]
fn a_target_the_plugin_does_not_serve_is_an_error_and_not_denied() {
    let c = connector("");
    let err = call(&c, "echo/nothing", None).unwrap_err();
    let text = err.to_string();
    assert!(text.contains("no_such_target"), "{text}");
    assert!(!text.contains("denied"), "a plugin said denied: {text}");
}

/// The manifest's deadline is the host's, and the call ends by itself.
/// The fixture exits without replying, so either answer is correct and
/// both are named; what must not happen is hanging.
#[test]
fn a_plugin_that_does_not_answer_ends_the_call_rather_than_hanging() {
    let c = connector(r#","call_timeout_ms":300"#);
    let started = Instant::now();
    let err = call(&c, "echo/quit", None).unwrap_err();
    let text = err.to_string();
    assert!(
        text.contains("did not answer") || text.contains("is gone"),
        "{text}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "the call hung for {:?}",
        started.elapsed()
    );
}

/// A `node` manifest is refused when the connector is built, by name,
/// rather than served as if it were `root` -- which would hand a
/// deployment the shared process the node model exists to prevent.
#[test]
fn a_node_scope_manifest_is_refused_rather_than_shared() {
    let exec = env!("CARGO_BIN_EXE_plugin-echo");
    let m = Manifest::parse(
        format!(r#"{{"family":"echo","transport":"spawn","scope":"node","exec":"{exec}"}}"#)
            .as_bytes(),
    )
    .expect("it parses");
    let err = PluginConnector::new(m).unwrap_err();
    assert!(err.contains("`node` scope"), "{err}");
    assert!(err.contains("only so far"), "{err}");
}
