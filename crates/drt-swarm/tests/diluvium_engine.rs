//! The Engine seam against the real thing: a program driven through
//! `dyn Engine` / `dyn Instance` only, so these tests prove the seam carries
//! everything a host needs — including the durable-agent loop the snapshot
//! store exists for (park → snapshot → store → restore in a *fresh* engine →
//! continue).

#![cfg(feature = "engine-diluvium")]

use drt_config::Budget;
use drt_swarm::engine::{
    diluvium_engine::DiluviumEngine, Engine, EngineError, Instance, LoadSpec, ProgramBytes, Step,
};
use drt_swarm::snapshot::{DirectoryStore, SnapshotStore};

fn load(engine: &DiluviumEngine, source: &str, name: &str) -> Box<dyn Instance> {
    engine
        .load(LoadSpec {
            program: ProgramBytes::Source(source),
            name,
            budget: Budget::default(),
            numeric: Default::default(),
            unsafe_stdlib: false,
        })
        .unwrap()
}

#[test]
fn version_first() {
    let engine = DiluviumEngine::new().unwrap();
    assert_eq!(
        engine.abi_version(),
        2,
        "dv ABI v2 — a bump is a decision, not a surprise"
    );
}

/// What ABI 2 added, present and callable.
///
/// The number moving is the decision `version_first` guards; this is what
/// the number is *about*. `dv.h`'s own note on the bump names these three
/// plus the numeric setters, and a build that reports 2 while any of them
/// is missing is reporting a version it does not speak.
#[test]
fn abi_two_carries_what_it_says_it_does() {
    // Features and the build number, read off the library rather than
    // stated about it — the pair that made `buildinfo`'s hard-coded
    // tables unnecessary.
    let features = drt_swarm::engine::core_features().expect("this build has an engine");
    assert!(
        features.contains(&"regex"),
        "the feature list is the library's: {features:?}"
    );
    assert!(
        !features.iter().any(|f| f.is_empty()),
        "a blank feature name means the string was split wrong: {features:?}"
    );
    assert!(drt_swarm::engine::core_build().expect("this build has an engine") > 0);

    // And the per-instance numeric surface, which B3 wires to the config.
    let engine = DiluviumEngine::new().unwrap();
    let inst = load(&engine, "return 1", "abi2");
    assert!(
        !inst.numeric_touched_fast(),
        "no fast backend exists to set it"
    );
}

#[test]
fn a_program_runs_to_done() {
    let engine = DiluviumEngine::new().unwrap();
    let mut inst = load(&engine, "return 1", "trivial");
    assert!(matches!(inst.run().unwrap(), Step::Done));
}

#[test]
fn a_guest_error_is_the_instances_fate_not_the_engines() {
    let engine = DiluviumEngine::new().unwrap();
    let mut inst = load(&engine, "error('boom', 0)", "bad");
    match inst.run() {
        Err(EngineError::Program(msg)) => assert!(msg.contains("boom")),
        other => panic!("expected a program error, got {other:?}"),
    }
}

#[test]
fn messages_round_trip_through_a_park() {
    let engine = DiluviumEngine::new().unwrap();
    let mut inst = load(
        &engine,
        r#"
        local inbox = queue.lookup("inbox")
        local outbox = queue.lookup("outbox")
        while true do
            local _, n = queue.wait({inbox})
            if n == 0 then return end
            queue.push(outbox, n * 3)
        end
        "#,
        "tripler",
    );
    let inbox = inst.queue("inbox").unwrap();
    let outbox = inst.queue("outbox").unwrap();

    let mut step = inst.run().unwrap();
    for n in [1u32, 2, 3] {
        match &step {
            Step::Parked(w) => assert_eq!(w.queues(), [inbox]),
            Step::Done => panic!("finished early"),
        }
        let msg = rmp_serde::to_vec(&n).unwrap();
        assert!(inst.push(inbox, &msg).unwrap().is_accepted());
        step = inst.resume(inbox).unwrap();
        let raw = inst.pop(outbox).unwrap().expect("an answer came back");
        let answer: u32 = rmp_serde::from_slice(&raw).unwrap();
        assert_eq!(answer, n * 3);
    }
    assert!(inst
        .push(inbox, &rmp_serde::to_vec(&0u32).unwrap())
        .unwrap()
        .is_accepted());
    assert!(matches!(inst.resume(inbox).unwrap(), Step::Done));
    assert!(inst.usage().instructions > 0 || inst.usage().bytes_now > 0 || !inst.exceeded());
}

/// The durable-agent loop, end to end: a parked program is snapshotted
/// through the seam, persisted by the directory store, restored under a
/// *fresh* engine (a stand-in for another process next week), and continues
/// with its state — the counter proves the heap crossed, not just the code.
#[test]
fn a_snapshot_survives_the_process_and_continues() {
    let dir = tempfile::tempdir().unwrap();
    let source = r#"
        local inbox = queue.lookup("inbox")
        local outbox = queue.lookup("outbox")
        local total = 0
        while true do
            local _, n = queue.wait({inbox})
            if n == 0 then queue.push(outbox, total) return end
            total = total + n
        end
    "#;

    {
        let engine = DiluviumEngine::new().unwrap();
        let mut inst = load(&engine, source, "accumulator");
        let inbox = inst.queue("inbox").unwrap();
        assert!(matches!(inst.run().unwrap(), Step::Parked(_)));
        inst.push(inbox, &rmp_serde::to_vec(&40u32).unwrap())
            .unwrap();
        assert!(matches!(inst.resume(inbox).unwrap(), Step::Parked(_)));

        let bytes = inst.snapshot(Some("node-a")).unwrap();
        let store = DirectoryStore::open(dir.path()).unwrap();
        store.put("accumulator", &bytes).unwrap();
    }

    // "Another process": a fresh engine, a fresh store over the directory.
    let engine = DiluviumEngine::new().unwrap();
    let store = DirectoryStore::open(dir.path()).unwrap();
    let bytes = store
        .get("accumulator")
        .unwrap()
        .expect("the snapshot survived");

    // The stamp is checked: restoring under the wrong identity is refused.
    let wrong = engine.restore(drt_swarm::engine::RestoreSpec {
        snapshot: &bytes,
        host_stamp: Some("node-b"),
        budget: Budget::default(),
        numeric: Default::default(),
        unsafe_stdlib: false,
    });
    assert!(matches!(wrong, Err(EngineError::SnapshotMismatch(_))));

    let mut inst = engine
        .restore(drt_swarm::engine::RestoreSpec {
            snapshot: &bytes,
            host_stamp: Some("node-a"),
            budget: Budget::default(),
            numeric: Default::default(),
            unsafe_stdlib: false,
        })
        .unwrap();

    // A restored instance is continuing, not starting: current_wait, then
    // resume — never run.
    let wait = inst.current_wait().expect("parked exactly as it was");
    let inbox = inst.queue("inbox").unwrap();
    let outbox = inst.queue("outbox").unwrap();
    assert!(wait.queues().contains(&inbox));

    inst.push(inbox, &rmp_serde::to_vec(&2u32).unwrap())
        .unwrap();
    assert!(matches!(inst.resume(inbox).unwrap(), Step::Parked(_)));
    inst.push(inbox, &rmp_serde::to_vec(&0u32).unwrap())
        .unwrap();
    assert!(matches!(inst.resume(inbox).unwrap(), Step::Done));

    let raw = inst.pop(outbox).unwrap().expect("the total came out");
    let total: u32 = rmp_serde::from_slice(&raw).unwrap();
    assert_eq!(
        total, 42,
        "40 accumulated before the snapshot, 2 after the restore"
    );
}

/// The budget crosses the seam: a runaway program is stopped, and the
/// engine reports it as the instance's fate with `exceeded()` set.
#[test]
fn a_budget_bounds_a_runaway_program() {
    let engine = DiluviumEngine::new().unwrap();
    let mut inst = engine
        .load(LoadSpec {
            program: ProgramBytes::Source("while true do end"),
            name: "runaway",
            budget: Budget {
                instructions: Some(10_000),
                memory_kb: None,
            },
            numeric: Default::default(),
            unsafe_stdlib: false,
        })
        .unwrap();
    match inst.run() {
        Err(EngineError::Program(_)) => assert!(inst.exceeded()),
        other => panic!("expected the budget to stop it, got {other:?}"),
    }
}

/// The numeric bounds a `LoadSpec` states reach the instance, and the audit
/// flag is false because nothing can have set it.
///
/// This is the test that fails when session A's A2 pin lands and
/// `DiluviumInstance::apply_numeric` is still a no-op: the bounds have to
/// stop being merely *carried* and start being *applied*, and the flag has
/// to stop being a literal `false` and start being a reading. Until then
/// both halves are true and this passes for the right reasons.
#[test]
fn numeric_bounds_reach_the_instance_and_no_fast_kernel_has_run() {
    use drt_config::{Numeric, Tier};

    let engine = DiluviumEngine::new().unwrap();
    let bounds = Numeric {
        max_elements: Some(4096),
        max_tier: Some(Tier::Reproducible),
    };
    let inst = engine
        .load(LoadSpec {
            program: ProgramBytes::Source("local x = 1"),
            name: "bounded",
            budget: Budget::default(),
            numeric: bounds,
            unsafe_stdlib: false,
        })
        .unwrap();

    assert_eq!(
        inst.numeric_bounds(),
        bounds,
        "the bounds the spec stated did not reach the instance"
    );
    // No fast-tier backend exists in this workspace or in the pinned core,
    // so no fast kernel can have run. `false` here is a fact, not a stub.
    assert!(!inst.numeric_touched_fast());

    // An instance loaded with no bounds states none, rather than inheriting
    // some default this layer invented.
    let plain = engine
        .load(LoadSpec {
            program: ProgramBytes::Source("local x = 1"),
            name: "plain",
            budget: Budget::default(),
            numeric: Numeric::default(),
            unsafe_stdlib: false,
        })
        .unwrap();
    assert!(plain.numeric_bounds().is_unbounded());
}
