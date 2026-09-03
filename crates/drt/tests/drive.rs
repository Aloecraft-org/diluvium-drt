//! The driver (doc/Wasm.md D6, M3): `tick()` says what the host does next,
//! and a connector that answers later answers on a later tick.
//!
//! These are the two properties the browser tier stands on and the native
//! loops must keep: nothing in the drive path blocks, and the cadence the
//! host is told to keep is exact — neither a spin nor an overshoot.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use drt::drive::{Next, Outcome, Solo, POLL_TICK};
use drt::start::{DeployDriver, IDLE_TICK};
use drt_caps::{CapSet, Grant, Scope};
use drt_config::RootConfig;
use drt_connector::{CallError, CallResult, Connector, Dispatcher, Registry};
use drt_swarm::engine::{diluvium_engine::DiluviumEngine, LoadSpec, ProgramBytes};

/// Ready on its second poll, never its first.
struct YieldOnce(bool);

impl Future for YieldOnce {
    type Output = ();
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        if self.0 {
            Poll::Ready(())
        } else {
            self.0 = true;
            cx.waker().wake_by_ref();
            Poll::Pending
        }
    }
}

/// A connector whose answer is not ready when asked: a `fetch` in a page,
/// in the smallest form that exhibits it.
struct Later;

#[async_trait::async_trait]
impl Connector for Later {
    async fn call(&self, call: &str, _: Option<rmpv::Value>, _: Option<&Scope>) -> CallResult {
        YieldOnce(false).await;
        if call == "later" {
            Ok(rmpv::Value::from("eventually"))
        } else {
            Err(CallError::new("the later connector answers 'later'"))
        }
    }
}

/// One hostcall, then the reply's status and value on an exported queue.
const CALLER: &str = r#"
local calls = queue.declare("host/calls", { capacity = 4, exported = true, on_full = "reject" })
local replies = queue.declare("host/replies", { capacity = 4 })
local verdict = queue.declare("verdict", { capacity = 4, exported = true })
queue.push(calls, { tok = 3, call = "later" })
local _, reply = queue.wait({replies})
queue.push(verdict, reply.status .. "|" .. tostring(reply.value))
"#;

fn load(program: &str) -> Solo {
    let mut registry = Registry::new();
    registry.wire("later", Arc::new(Later), None).unwrap();
    let engine = DiluviumEngine::new().unwrap();
    Solo::load(
        &engine,
        LoadSpec {
            program: ProgramBytes::Source(program),
            name: "test",
            budget: Default::default(),
            unsafe_stdlib: false,
        },
        CapSet::root(vec![Grant::grant("host:later")]),
        Arc::new(Dispatcher::new(registry)),
    )
    .unwrap()
}

fn verdict(solo: &mut Solo) -> String {
    let q = solo.queue("verdict").unwrap();
    let raw = solo.pop(q).unwrap().expect("a verdict");
    rmpv::decode::read_value(&mut &raw[..])
        .unwrap()
        .as_str()
        .unwrap()
        .to_string()
}

#[test]
fn a_connector_that_answers_later_answers_on_a_later_tick() {
    let mut solo = load(CALLER);
    // First tick: the request is drained, the answer is in flight, and the
    // host is told to poll rather than to give up or to block.
    match solo.tick(None) {
        Next::Sleep(d) => assert!(d <= POLL_TICK, "{d:?}"),
        other => panic!("expected a poll sleep, got {other:?}"),
    }
    assert_eq!(solo.in_flight(), 1);
    // Later: the answer lands, the guest resumes on its reply queue, and
    // the run completes — within a couple of ticks, not a couple of
    // hundred.
    let mut ticks = 0;
    loop {
        match solo.tick(None) {
            Next::Done(Outcome::Exited) => break,
            Next::Sleep(d) => {
                ticks += 1;
                assert!(ticks < 10, "the answer never landed");
                std::thread::sleep(d);
            }
            other => panic!("{other:?}"),
        }
    }
    assert_eq!(solo.in_flight(), 0);
    assert_eq!(verdict(&mut solo), "ok|eventually");
}

#[test]
fn a_park_with_a_timeout_sleeps_what_it_asked_and_no_more() {
    let mut solo = load("local q = queue.declare('never', {capacity = 1}) queue.wait({q}, 30)");
    let begun = Instant::now();
    // The first tick asks for the whole timeout, minus the little that
    // already elapsed: neither a fixed idle tick (a spin) nor more.
    match solo.tick(None) {
        Next::Sleep(d) => assert!(
            d > Duration::from_millis(20) && d <= Duration::from_millis(30),
            "{d:?}"
        ),
        other => panic!("{other:?}"),
    }
    loop {
        match solo.tick(None) {
            Next::Sleep(d) => std::thread::sleep(d),
            Next::Done(Outcome::Exited) => break,
            other => panic!("{other:?}"),
        }
    }
    assert!(begun.elapsed() >= Duration::from_millis(30));
}

#[test]
fn a_park_nothing_will_wake_is_named_not_slept_through() {
    let mut solo = load("local q = queue.declare('never', {capacity = 1}) queue.wait({q})");
    assert_eq!(solo.tick(None), Next::Stuck { for_space: false });
    let mut full = load(
        "local q = queue.declare('full', {capacity = 1, on_full = 'block'}) \
         queue.push(q, 1) queue.push(q, 2)",
    );
    assert_eq!(full.tick(None), Next::Stuck { for_space: true });
}

#[test]
fn a_fault_is_reported_once_and_repeated_on_every_tick() {
    let mut solo = load("error('boom')");
    let first = solo.tick(None);
    match &first {
        Next::Failed(why) => assert!(why.contains("boom"), "{why}"),
        other => panic!("{other:?}"),
    }
    assert_eq!(
        solo.tick(None),
        first,
        "sticky: a host that ticks again hears the same thing"
    );
}

/// The deployment's cadence, pinned: a sleep is never longer than the idle
/// tick and never past the next deadline, and a deployment that drains
/// says so. This is the regression test for the relay-spins-a-core class
/// of bug (CHANGELOG.yaml, v0.4.0): a sleep of zero here would be the spin.
#[test]
fn a_deployment_idles_one_tick_at_a_time_and_wakes_for_its_deadline() {
    let program = serde_json::to_string(
        "local q = queue.declare('nothing-pushes-here', {capacity = 1})\n\
         queue.wait({q}, 25)\n",
    )
    .unwrap();
    let cfg: RootConfig =
        serde_json::from_str(&format!(r#"{{"program": {{"source": {program}}}}}"#)).unwrap();
    let mut driver = DeployDriver::new(&cfg, Dispatcher::new(Registry::new())).unwrap();
    let begun = Instant::now();
    let mut sleeps = 0;
    loop {
        match driver.tick() {
            Next::Sleep(d) => {
                assert!(d <= IDLE_TICK, "{d:?} is longer than the idle tick");
                assert!(
                    !d.is_zero() || begun.elapsed() >= Duration::from_millis(25),
                    "a zero sleep is a spin"
                );
                sleeps += 1;
                std::thread::sleep(d);
            }
            Next::Done(Outcome::Exited) => break,
            other => panic!("{other:?}"),
        }
    }
    assert!(begun.elapsed() >= Duration::from_millis(25));
    assert!(
        sleeps >= 5,
        "the deadline was reached in {sleeps} sleeps: the loop is not idling a tick at a time"
    );
}
