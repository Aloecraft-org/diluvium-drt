//! The park deadline outlives residency (`doc/Plan-0.7.0.md` §6,
//! acceptance 22): a hibernated instance's timeout still fires, waking it
//! rather than killing it, and the guest sees the timeout on its own wait;
//! a deadline under the configured floor is refused when it is armed.
//!
//! # Surface
//!
//! Entry points: the tests. Each builds a deployment through
//! `DeployDriver::new`, the shape `drt start` runs, and drives it a step at
//! a time, sleeping `driver.idle()` between steps as the drive loop does.
//!
//! Configurable values:
//! - `WAIT_MS` — the timeout the parked root asks for.
//! - `FLOOR_MS` — a floor under it, so the park is allowed to arm; the
//!   floor is configurable for exactly this test's reason.
//! - `GUARD` — how long a test waits for what must happen before calling
//!   it a hang. A hung test explains nothing.
//!
//! Fan-out: none.

use std::time::{Duration, Instant};

use drt::start::DeployDriver;
use drt_config::RootConfig;
use drt_connector::{Dispatcher, Registry};
use drt_swarm::InstanceId;

const WAIT_MS: u64 = 80;
const FLOOR_MS: u64 = 10;
const GUARD: Duration = Duration::from_secs(5);

// depth: building and driving a deployment, and reading its outbox

/// A config whose program is inline source, with the rest of the root
/// config spliced in after it.
fn config_with_source(source: &str, rest: &str) -> RootConfig {
    let program = serde_json::to_string(source).unwrap();
    serde_json::from_str(&format!(r#"{{"program": {{"source": {program}}}{rest}}}"#)).unwrap()
}

fn deployment(config: &RootConfig) -> DeployDriver {
    DeployDriver::new(config, Dispatcher::new(Registry::new())).unwrap()
}

/// One message off the root's `outbox`, if the program has pushed one.
fn pop_outbox(driver: &mut DeployDriver, root: InstanceId) -> Option<rmpv::Value> {
    let sw = driver.deployment_mut();
    let inst = sw.instance_mut(root)?;
    let q = inst.queue("outbox")?;
    let raw = inst.pop(q).ok()??;
    Some(rmpv::decode::read_value(&mut raw.as_slice()).unwrap())
}

/// Step and idle, as the drive loop does, until the root has pushed to its
/// outbox or the guard runs out. `each` runs after every step with the
/// time elapsed since the first, for what a test wants to hold throughout.
fn drive_until_outbox(
    driver: &mut DeployDriver,
    root: InstanceId,
    begun: Instant,
    mut each: impl FnMut(&mut DeployDriver, Duration),
) -> rmpv::Value {
    loop {
        assert!(
            begun.elapsed() < GUARD,
            "nothing reached the outbox within {GUARD:?}"
        );
        driver.step();
        each(driver, begun.elapsed());
        if let Some(msg) = pop_outbox(driver, root) {
            return msg;
        }
        std::thread::sleep(driver.idle());
    }
}

/// A root that parks with a timeout, then reports that it came back, then
/// parks forever so the deployment stays observable. Nothing ever pushes
/// on the queue it waits on: the only way past the wait is the timeout.
fn parks_then_reports(timeout_ms: u64) -> String {
    format!(
        "local out = queue.lookup('outbox')\n\
         local nothing = queue.declare('nothing-pushes-here', {{capacity = 1}})\n\
         queue.wait({{nothing}}, {timeout_ms})\n\
         queue.push(out, 'timed out')\n\
         local hold = queue.declare('hold', {{capacity = 1}})\n\
         queue.wait({{hold}})\n"
    )
}

/// Acceptance 22, the first half: the root parks with a timeout above the
/// floor, is hibernated, and when the deadline passes is **woken** — not
/// killed — so its next drive hands the guest the timeout on its own wait,
/// which the guest reports.
#[test]
fn a_hibernated_instances_deadline_wakes_it_and_the_guest_sees_its_timeout() {
    let cfg = config_with_source(
        &parks_then_reports(WAIT_MS),
        &format!(r#", "residency": {{"max_resident": 0, "park_floor_ms": {FLOOR_MS}}}"#),
    );
    let mut driver = deployment(&cfg);
    let root = driver.root();
    // Before the first step, so every deadline the program arms is at or
    // after it.
    let begun = Instant::now();

    // The first step runs the root to its park; a hibernate then succeeds
    // only because it is parked. The residency policy exempts the root, so
    // this is the direct call — the policy's own eviction is the same
    // `hibernate` (start.rs, `enforce_residency`).
    driver.step();
    driver
        .deployment_mut()
        .hibernate(root)
        .expect("parked on its wait, so it hibernates");
    assert!(!driver.deployment_mut().resident(root));

    let wait = Duration::from_millis(WAIT_MS);
    let msg = drive_until_outbox(&mut driver, root, begun, |driver, elapsed| {
        let sw = driver.deployment_mut();
        assert_eq!(sw.alive(), 1, "the root was killed rather than woken");
        // Nothing wakes it before its own deadline: no message is coming,
        // and the deadline is the only other thing that can.
        if elapsed < wait {
            assert!(
                !sw.resident(root),
                "woken {elapsed:?} in, before its deadline"
            );
        }
    });
    assert_eq!(msg.as_str(), Some("timed out"));
    assert!(
        begun.elapsed() >= wait,
        "the guest came back before its own timeout"
    );
    let sw = driver.deployment_mut();
    assert!(sw.resident(root), "the deadline woke it; it is resident");
    assert_eq!(sw.alive(), 1);
}

/// Without a residency policy nothing hibernates, so there is no floor: a
/// short wait works as it always has, and its timeout still fires on the
/// host clock.
#[test]
fn without_a_residency_policy_there_is_no_floor() {
    let cfg = config_with_source(&parks_then_reports(20), "");
    let mut driver = deployment(&cfg);
    let root = driver.root();
    let begun = Instant::now();
    let msg = drive_until_outbox(&mut driver, root, begun, |driver, _| {
        assert_eq!(
            driver.deployment_mut().alive(),
            1,
            "a 20ms park was refused"
        );
    });
    assert_eq!(msg.as_str(), Some("timed out"));
    assert!(begun.elapsed() >= Duration::from_millis(20));
}

/// Acceptance 22, the second half: a deadline under the floor is refused
/// when it is armed. The root reads the refusal off `system/events` — the
/// same reason the fault line on stderr carries — and it names both the
/// timeout and the floor.
#[test]
fn a_deadline_under_the_floor_is_refused_when_it_is_armed() {
    const SUPERVISOR: &str = "\
        local sys = queue.declare('system/lifecycle', {capacity = 4})\n\
        local ev  = queue.declare('system/events', {capacity = 16})\n\
        local out = queue.lookup('outbox')\n\
        assert(queue.push(sys, {op = 'spawn',\n\
          code = \"local q = queue.declare('q', {capacity = 1}) queue.wait({q}, 20)\",\n\
          caps = {'queue:q'}}))\n\
        while true do\n\
          local _, e = queue.wait({ev})\n\
          if e.event == 'faulted' then queue.push(out, e.detail) end\n\
        end\n";
    let cfg = config_with_source(
        SUPERVISOR,
        r#", "caps": [{"capability": "lifecycle"}, {"capability": "queue:*"}],
            "residency": {"max_resident": 4, "park_floor_ms": 30000}"#,
    );
    let mut driver = deployment(&cfg);
    let root = driver.root();
    let begun = Instant::now();
    let detail = drive_until_outbox(&mut driver, root, begun, |_, _| {});
    let detail = detail.as_str().unwrap().to_string();
    assert!(detail.contains("20ms"), "names the timeout: {detail}");
    assert!(detail.contains("30000ms"), "names the floor: {detail}");
    assert!(detail.contains("park_floor_ms"), "{detail}");
    // Refused at arm time — long before the 20ms it asked for would have
    // let it be a wake, and with the supervisor's own untimed wait
    // untouched by the floor.
    assert_eq!(driver.deployment_mut().alive(), 1);
    assert!(driver.deployment_mut().resident(root));

    // The same refusal reaches the root, which has no supervisor but the
    // process: the deployment drains on the step that armed the park.
    let cfg = config_with_source(
        &parks_then_reports(20),
        r#", "residency": {"max_resident": 4}"#,
    );
    let mut driver = deployment(&cfg);
    assert_eq!(
        driver.step(),
        0,
        "a 20ms park under the default 30s floor was armed rather than refused"
    );
}

/// The floor is the default when the block names none, and the block still
/// refuses a key it does not know.
#[test]
fn the_floor_defaults_to_thirty_seconds_and_the_block_stays_closed() {
    let cfg: RootConfig = serde_json::from_str(
        r#"{"program": {"source": "return 1"}, "residency": {"max_resident": 1}}"#,
    )
    .unwrap();
    assert_eq!(cfg.residency.unwrap().park_floor_ms, 30_000);
    let err = serde_json::from_str::<RootConfig>(
        r#"{"program": {"source": "return 1"},
            "residency": {"max_resident": 1, "park_floor": 5}}"#,
    )
    .unwrap_err()
    .to_string();
    assert!(err.contains("park_floor"), "{err}");
}
