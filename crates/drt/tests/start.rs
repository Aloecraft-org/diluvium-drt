//! `drt start` end to end: real guests, the deployment host, the clock.

use std::time::Duration;

use drt::start;
use drt_config::RootConfig;
use drt_connector::{Dispatcher, Registry};

fn config(json: &str) -> RootConfig {
    serde_json::from_str(json).unwrap()
}

/// A config whose program is inline source — JSON strings cannot carry the
/// newlines a readable program wants, so the program is spliced in encoded.
fn config_with_source(source: &str, rest: &str) -> RootConfig {
    let program = serde_json::to_string(source).unwrap();
    serde_json::from_str(&format!(r#"{{"program": {{"source": {program}}}{rest}}}"#)).unwrap()
}

/// Run `start` on its own thread with a guard: a deployment that should
/// drain but does not is a hang, and a hung test explains nothing.
fn start_guarded(config: RootConfig, within: Duration) -> Result<(), String> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(start::start(&config, Dispatcher::new(Registry::new())));
    });
    rx.recv_timeout(within)
        .expect("the deployment did not drain in time — a park was never answered")
}

/// A deployment is config + a program: the root spawns a child, hears its
/// exit through `system/events`, and returns. The swarm drains and `start`
/// comes home — the batch-shaped contract.
#[test]
fn a_deployment_that_drains_returns() {
    let cfg = config_with_source(
        "local sys = queue.declare('system/lifecycle', {capacity = 4})\n\
         local ev  = queue.declare('system/events', {capacity = 16})\n\
         assert(queue.push(sys, {op = 'spawn', code = 'local x = 1', caps = {}}))\n\
         queue.wait({ev})\n",
        r#", "caps": [{"capability": "lifecycle"}, {"capability": "queue:*"}]"#,
    );
    start_guarded(cfg, Duration::from_secs(10)).unwrap();
}

/// The one thing the deployment host owes that the clockless bench host
/// does not: a park with a timeout is resumed when the timeout elapses.
/// Without the clock this test does not fail — it hangs, which is exactly
/// what `queue.wait({q}, 25)` would do to a real deployment.
#[test]
fn a_park_timeout_fires_on_the_host_clock() {
    let cfg = config_with_source(
        "local q = queue.declare('nothing-pushes-here', {capacity = 1})\n\
         queue.wait({q}, 25)\n",
        "",
    );
    let begun = std::time::Instant::now();
    start_guarded(cfg, Duration::from_secs(10)).unwrap();
    assert!(
        begun.elapsed() >= Duration::from_millis(25),
        "the wait came back before its own timeout"
    );
}

/// A config naming listeners is refused, not accepted-and-ignored: an
/// operator who wrote a listener block believes a port is being served.
#[test]
fn a_listener_config_is_refused_until_listen_exists() {
    let cfg = config(
        r#"{
            "program": {"source": "local x = 1"},
            "listeners": [{"scheme": "ssh", "address": "0.0.0.0:2222"}]
        }"#,
    );
    let err = start::start(&cfg, Dispatcher::new(Registry::new())).unwrap_err();
    assert!(err.contains("listener"), "{err}");
    assert!(err.contains("listen milestone"), "{err}");
}

#[test]
fn a_config_with_no_program_says_what_is_missing() {
    let err = start::start(&config("{}"), Dispatcher::new(Registry::new())).unwrap_err();
    assert!(err.contains("names no program"), "{err}");
}
