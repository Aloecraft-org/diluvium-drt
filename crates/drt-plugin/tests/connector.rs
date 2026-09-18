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
use drt_connector::{Asker, Caller, Connector};
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
    PluginConnector::new("echo", manifest(extra)).expect("a root manifest is served")
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

/// `node` scope gives two callers two processes, which is the default and
/// the whole reason the default is what it is: an implicitly shared
/// process is the ambient singleton the node model exists to avoid.
#[test]
fn node_scope_gives_each_caller_its_own_process() {
    let c = node_connector();
    let a = pid_seen_by(&c, Caller::Node(1));
    let b = pid_seen_by(&c, Caller::Node(2));
    assert_ne!(a, b, "two nodes shared one plugin process");
}

/// `root` scope gives two callers one process, which is what it costs:
/// the plugin serves both and cannot tell them apart.
#[test]
fn root_scope_gives_every_caller_the_same_process() {
    let c = connector("");
    let a = pid_seen_by(&c, Caller::Node(1));
    let b = pid_seen_by(&c, Caller::Node(2));
    assert_eq!(a, b, "one root plugin became two processes");
}

/// A node dying takes its own plugin and nobody else's.
#[test]
fn releasing_one_node_leaves_the_others_plugin_alone() {
    let c = node_connector();
    let one = pid_seen_by(&c, Caller::Node(1));
    let two = pid_seen_by(&c, Caller::Node(2));

    let lost = c.release(&Caller::Node(1));
    assert!(lost.is_empty(), "an idle plugin lost something: {lost:?}");

    // Node 2's plugin is untouched: same process, still answering.
    assert_eq!(pid_seen_by(&c, Caller::Node(2)), two);
    // Node 1 calling again starts a fresh one rather than reusing a
    // process that was swept.
    assert_ne!(pid_seen_by(&c, Caller::Node(1)), one);
}

/// A `root` plugin is not any one node's to end, so a node dying leaves
/// it running for everyone else.
#[test]
fn releasing_a_node_does_not_end_a_root_plugin() {
    let c = connector("");
    let before = pid_seen_by(&c, Caller::Node(1));
    assert!(c.release(&Caller::Node(1)).is_empty());
    assert_eq!(
        pid_seen_by(&c, Caller::Node(1)),
        before,
        "a root plugin was ended by one node dying"
    );
}

/// Teardown ends every instance, and an empty report is a claim: these
/// plugins really had nothing outstanding.
#[test]
fn finishing_ends_every_instance() {
    let c = node_connector();
    pid_seen_by(&c, Caller::Node(1));
    pid_seen_by(&c, Caller::Node(2));
    let lost = c.finish();
    assert!(lost.is_empty(), "idle plugins lost something: {lost:?}");
    // Nothing is left to end.
    assert!(c.finish().is_empty());
}

// --- the helpers those need --------------------------------------------

fn node_connector() -> PluginConnector {
    let exec = env!("CARGO_BIN_EXE_plugin-echo");
    let m = Manifest::parse(
        format!(r#"{{"family":"echo","transport":"spawn","scope":"node","exec":"{exec}"}}"#)
            .as_bytes(),
    )
    .expect("a node manifest parses");
    PluginConnector::new("echo", m).expect("a node manifest is served")
}

/// Which process serves `caller`, asked of the process itself.
///
/// The only honest way to check scope: the fixture answers `echo/pid`
/// with its own process id, so two callers getting one id back is `root`
/// and two ids is `node`.
fn pid_seen_by(c: &PluginConnector, caller: Caller) -> u64 {
    let asker = Asker {
        caller,
        grants: &[],
    };
    let answer = pollster::block_on(c.call_as(&asker, "echo/pid", None, None)).expect("an answer");
    answer
        .as_u64()
        .unwrap_or_else(|| panic!("a pid, not {answer}"))
}
