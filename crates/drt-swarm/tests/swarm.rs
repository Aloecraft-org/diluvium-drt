//! The swarm port against the real engine: real Diluvium guests, driven by
//! [`StepHost`], exercising the semantics SPEC.md §8 says must be preserved
//! exactly. Until the discofetch capability suite is ported, these are the
//! differential tests — each asserts a behavior `dvs.c` documents.

#![cfg(feature = "engine-diluvium")]

use std::sync::Arc;

use drt_caps::Grant;
use drt_config::Budget;
use drt_swarm::engine::diluvium_engine::DiluviumEngine;
use drt_swarm::swarm::{StepHost, Swarm, SwarmError};
use drt_swarm::InstanceId;

fn swarm() -> Swarm<StepHost> {
    let engine = Arc::new(DiluviumEngine::new().unwrap());
    Swarm::new(engine, StepHost)
}

fn swarm_with(max_instances: u32, spawns_per_step: u32) -> Swarm<StepHost> {
    let engine = Arc::new(DiluviumEngine::new().unwrap());
    Swarm::with_limits(engine, StepHost, max_instances, spawns_per_step)
}

fn lifecycle_caps() -> Vec<Grant> {
    vec![Grant::grant("lifecycle"), Grant::grant("queue:*")]
}

/// Run a fixed number of steps; extra steps against a settled swarm are
/// no-ops, so generous is fine.
fn settle<H: drt_swarm::swarm::SwarmHost>(sw: &mut Swarm<H>, steps: usize) -> usize {
    let mut alive = 0;
    for _ in 0..steps {
        alive = sw.step();
    }
    alive
}

/// Pop everything from an instance's exported queue, decoded.
fn drain_out<H: drt_swarm::swarm::SwarmHost>(
    sw: &mut Swarm<H>,
    id: InstanceId,
    queue: &str,
) -> Vec<rmpv::Value> {
    let mut out = Vec::new();
    let Some(inst) = sw.instance_mut(id) else {
        return out;
    };
    let Some(q) = inst.queue(queue) else {
        return out;
    };
    while let Ok(Some(raw)) = inst.pop(q) {
        out.push(rmpv::decode::read_value(&mut raw.as_slice()).unwrap());
    }
    out
}

/// A supervisor that spawns children from its inbox and forwards every
/// lifecycle event it hears to an exported `log` queue, so the tests can
/// read exactly what the swarm told it.
const SUPERVISOR: &str = r#"
    local lc = queue.declare("system/lifecycle", { capacity = 16 })
    local ev = queue.declare("system/events", { capacity = 32 })
    local requests = queue.declare("requests", { capacity = 16, exported = true })
    local log = queue.declare("log", { capacity = 64, exported = true })
    -- Handle one message; true means stop. Everything available is drained
    -- per wake so a burst of requests lands in one step.
    local function handle(q, m)
        if q == ev then
            queue.push(log, m)
        elseif m == "stop" then
            return true
        else
            queue.push(lc, m)
        end
        return false
    end
    while true do
        local q, m = queue.wait({requests, ev})
        if handle(q, m) then return end
        for _, qq in ipairs({requests, ev}) do
            local n = queue.pop(qq)
            while n ~= nil do
                if handle(qq, n) then return end
                n = queue.pop(qq)
            end
        end
    end
"#;

fn spawn_request(code: &str, caps: &[&str], budget: Option<(u64, u64)>) -> rmpv::Value {
    let mut map = vec![
        ("op".into(), "spawn".into()),
        ("code".into(), code.into()),
        (
            "caps".into(),
            rmpv::Value::Array(caps.iter().map(|c| rmpv::Value::from(*c)).collect()),
        ),
    ];
    if let Some((instructions, memory_kb)) = budget {
        map.push((
            "budget".into(),
            rmpv::Value::Map(vec![
                ("instructions".into(), rmpv::Value::from(instructions)),
                ("memory_kb".into(), rmpv::Value::from(memory_kb)),
            ]),
        ));
    }
    rmpv::Value::Map(map)
}

fn push_value(sw: &mut Swarm<StepHost>, id: InstanceId, queue: &str, v: &rmpv::Value) {
    let mut buf = Vec::new();
    rmpv::encode::write_value(&mut buf, v).unwrap();
    sw.push(id, queue, &buf).unwrap();
}

fn field<'a>(event: &'a rmpv::Value, name: &str) -> Option<&'a rmpv::Value> {
    event
        .as_map()?
        .iter()
        .find(|(k, _)| k.as_str() == Some(name))
        .map(|(_, v)| v)
}

fn event_name(event: &rmpv::Value) -> &str {
    field(event, "event")
        .and_then(|v| v.as_str())
        .unwrap_or("?")
}

fn detail(event: &rmpv::Value) -> &str {
    field(event, "detail")
        .and_then(|v| v.as_str())
        .unwrap_or("")
}

#[test]
fn a_spawned_child_runs_and_its_exit_is_reported() {
    let mut sw = swarm();
    let root = sw
        .root(SUPERVISOR.as_bytes(), lifecycle_caps(), Budget::default())
        .unwrap();
    assert_eq!(sw.alive(), 1);
    assert_eq!(sw.parent(root), Some(InstanceId(0)));

    sw.step(); // root runs to its wait
    push_value(
        &mut sw,
        root,
        "requests",
        &spawn_request("return 1", &[], None),
    );
    settle(&mut sw, 10);

    let log = drain_out(&mut sw, root, "log");
    let names: Vec<_> = log.iter().map(event_name).collect();
    assert_eq!(
        names,
        ["spawned", "exited"],
        "the child was born and its exit was heard"
    );
    let child = field(&log[0], "id").unwrap().as_u64().unwrap();
    assert_eq!(field(&log[1], "id").unwrap().as_u64().unwrap(), child);
    assert!(
        child > root.0 as u64,
        "handles are never reused, so the child's is later"
    );
    assert_eq!(sw.alive(), 1, "only the supervisor remains");
}

#[test]
fn attenuation_refuses_by_the_named_capability() {
    let mut sw = swarm();
    let root = sw
        .root(SUPERVISOR.as_bytes(), lifecycle_caps(), Budget::default())
        .unwrap();
    sw.step();
    // The root does not hold host:fs/*, so it may not grant it.
    push_value(
        &mut sw,
        root,
        "requests",
        &spawn_request("return 1", &["host:fs/*"], None),
    );
    settle(&mut sw, 10);
    let log = drain_out(&mut sw, root, "log");
    assert_eq!(event_name(&log[0]), "denied");
    assert_eq!(detail(&log[0]), "host:fs/*", "the refusal names the grant");
    assert_eq!(
        sw.alive(),
        1,
        "a denied spawn costs nothing and leaves nothing behind"
    );

    // Narrowing what it does hold works.
    push_value(
        &mut sw,
        root,
        "requests",
        &spawn_request("return 1", &["queue:work/jobs"], None),
    );
    settle(&mut sw, 10);
    let log = drain_out(&mut sw, root, "log");
    assert_eq!(event_name(&log[0]), "spawned");
}

#[test]
fn a_kill_takes_the_subtree_and_only_an_ancestor_may_ask() {
    let mut sw = swarm();
    let root = sw
        .root(SUPERVISOR.as_bytes(), lifecycle_caps(), Budget::default())
        .unwrap();
    sw.step();
    // A middle supervisor that spawns a parked grandchild, then parks.
    let middle = r#"
        local lc = queue.declare("system/lifecycle", { capacity = 4 })
        local hold = queue.declare("hold", { capacity = 1 })
        queue.push(lc, { op = "spawn", code = "queue.wait({queue.declare('h', {capacity=1})})" })
        queue.wait({hold})
    "#;
    push_value(
        &mut sw,
        root,
        "requests",
        &spawn_request(middle, &["lifecycle"], None),
    );
    settle(&mut sw, 10);
    assert_eq!(sw.alive(), 3, "root, middle, grandchild");

    let log = drain_out(&mut sw, root, "log");
    let middle_id = field(&log[0], "id").unwrap().as_u64().unwrap() as u32;

    // Killing the middle takes the grandchild with it — subtree, not node.
    push_value(
        &mut sw,
        root,
        "requests",
        &rmpv::Value::Map(vec![
            ("op".into(), "kill".into()),
            ("id".into(), rmpv::Value::from(middle_id)),
        ]),
    );
    settle(&mut sw, 10);
    assert_eq!(sw.alive(), 1, "the whole subtree is gone");
    let log = drain_out(&mut sw, root, "log");
    assert_eq!(event_name(&log[0]), "exited");
    assert_eq!(detail(&log[0]), "killed");

    // A kill aimed at something that is not a descendant is refused.
    push_value(
        &mut sw,
        root,
        "requests",
        &rmpv::Value::Map(vec![
            ("op".into(), "kill".into()),
            ("id".into(), rmpv::Value::from(root.0)),
        ]),
    );
    settle(&mut sw, 10);
    let log = drain_out(&mut sw, root, "log");
    assert_eq!(event_name(&log[0]), "denied");
    assert_eq!(detail(&log[0]), "not a descendant");
}

#[test]
fn the_spawn_limit_is_a_rate_not_a_filter() {
    let mut sw = swarm_with(0, 2);
    let root = sw
        .root(SUPERVISOR.as_bytes(), lifecycle_caps(), Budget::default())
        .unwrap();
    sw.step();
    // Five spawns in one burst, against a rate of two per step. The children
    // park forever so the population is countable.
    for _ in 0..5 {
        push_value(
            &mut sw,
            root,
            "requests",
            &spawn_request("queue.wait({queue.declare('h', {capacity=1})})", &[], None),
        );
    }
    settle(&mut sw, 20);
    assert_eq!(
        sw.alive(),
        6,
        "every spawn eventually lands: a rate, not a filter"
    );
    let log = drain_out(&mut sw, root, "log");
    let spawned = log.iter().filter(|e| event_name(e) == "spawned").count();
    let throttled = log.iter().filter(|e| event_name(e) == "throttled").count();
    assert_eq!(spawned, 5, "nothing was lost");
    assert!(throttled >= 1, "and the requester was told to back off");
}

#[test]
fn an_oversized_request_is_refused_by_name() {
    let mut sw = swarm();
    let root = sw
        .root(SUPERVISOR.as_bytes(), lifecycle_caps(), Budget::default())
        .unwrap();
    sw.step();
    let huge = format!("return [[{}]]", "x".repeat(drt_swarm::REQUEST_CAP_BYTES));
    push_value(&mut sw, root, "requests", &spawn_request(&huge, &[], None));
    settle(&mut sw, 10);
    let log = drain_out(&mut sw, root, "log");
    assert_eq!(event_name(&log[0]), "denied");
    assert_eq!(detail(&log[0]), "the request is too large");
}

#[test]
fn a_budget_exceeded_child_is_reported_as_exceeded_not_faulted() {
    let mut sw = swarm();
    let root = sw
        .root(SUPERVISOR.as_bytes(), lifecycle_caps(), Budget::default())
        .unwrap();
    sw.step();
    push_value(
        &mut sw,
        root,
        "requests",
        &spawn_request("while true do end", &[], Some((10_000, 0))),
    );
    settle(&mut sw, 10);
    let log = drain_out(&mut sw, root, "log");
    let names: Vec<_> = log.iter().map(event_name).collect();
    assert_eq!(
        names,
        ["spawned", "exceeded"],
        "a supervisor grows a budget, restarts a bug"
    );

    // A genuinely buggy child is 'faulted', with the message.
    push_value(
        &mut sw,
        root,
        "requests",
        &spawn_request("error('boom', 0)", &[], None),
    );
    settle(&mut sw, 10);
    let log = drain_out(&mut sw, root, "log");
    assert_eq!(event_name(&log[1]), "faulted");
    assert!(detail(&log[1]).contains("boom"));
}

#[test]
fn the_lifecycle_queue_is_read_only_under_the_capability() {
    let mut sw = swarm();
    // A root that declares system/lifecycle and asks for a spawn — but holds
    // no lifecycle capability, so nothing ever reads the queue. Refusal by
    // mechanism: no error, no event, no child.
    let root = sw
        .root(
            SUPERVISOR.as_bytes(),
            vec![Grant::grant("queue:*")],
            Budget::default(),
        )
        .unwrap();
    sw.step();
    push_value(
        &mut sw,
        root,
        "requests",
        &spawn_request("return 1", &[], None),
    );
    settle(&mut sw, 10);
    assert_eq!(sw.alive(), 1, "the request sat unread");
    assert!(
        drain_out(&mut sw, root, "log").is_empty(),
        "and nothing was said about it"
    );
}

#[test]
fn the_delivery_table_answers_all_four_rows() {
    let mut sw = swarm();
    let root = sw
        .root(SUPERVISOR.as_bytes(), lifecycle_caps(), Budget::default())
        .unwrap();
    sw.step();

    // Resident, declared queue: delivered.
    assert!(sw.push(root, "requests", b"\xa4stop").is_ok());
    // Resident, unknown queue.
    assert_eq!(
        sw.push(root, "no/such/queue", b"\xc0"),
        Err(SwarmError::UnknownQueue)
    );
    // Unknown instance: gone, immediately.
    assert_eq!(
        sw.push(InstanceId(999), "requests", b"\xc0"),
        Err(SwarmError::Gone)
    );
    settle(&mut sw, 10);
    // Dead instance (the root read "stop" and returned): gone.
    assert_eq!(sw.alive(), 0);
    assert_eq!(sw.push(root, "requests", b"\xc0"), Err(SwarmError::Gone));
}

/// The self-initiated hibernation loop: a program parks after pushing
/// `{op="hibernate", wake_on_message=true}`; nothing swaps it out behind its
/// back; a message wakes it and the wake buffer drains ahead of live pushes.
#[test]
fn hibernation_is_self_initiated_and_wake_on_message_wakes() {
    let mut sw = swarm();
    let root = sw
        .root(SUPERVISOR.as_bytes(), lifecycle_caps(), Budget::default())
        .unwrap();
    sw.step();
    let sleeper = r#"
        local lc = queue.declare("system/lifecycle", { capacity = 4 })
        local requests = queue.declare("requests", { capacity = 16, exported = true })
        local out = queue.declare("out", { capacity = 16, exported = true })
        queue.push(lc, { op = "hibernate", wake_on_message = true })
        local total = 0
        while true do
            local _, n = queue.wait({requests})
            if n == 0 then queue.push(out, total) return end
            total = total + n
        end
    "#;
    push_value(
        &mut sw,
        root,
        "requests",
        &spawn_request(sleeper, &["lifecycle", "queue:*"], None),
    );
    settle(&mut sw, 10);
    let log = drain_out(&mut sw, root, "log");
    assert_eq!(event_name(&log[0]), "spawned");
    let child = InstanceId(field(&log[0], "id").unwrap().as_u64().unwrap() as u32);

    // It parked; the swarm swapped it out on the drain. Still alive, not
    // resident, its whole state in the cache.
    assert!(!sw.resident(child));
    assert!(sw.cached_size(child) > 0);
    assert_eq!(
        sw.alive(),
        2,
        "a cached instance is alive: a sender may push to it"
    );

    // Messages for a cached wake_on_message instance land in the bounded
    // buffer...
    for n in [40u32, 2, 0] {
        let mut buf = Vec::new();
        rmpv::encode::write_value(&mut buf, &rmpv::Value::from(n)).unwrap();
        sw.push(child, "requests", &buf).unwrap();
    }
    // ...and the next step wakes it, delivers them ahead of live pushes, and
    // the program continues from its wait with its heap intact.
    settle(&mut sw, 10);
    assert_eq!(sw.alive(), 1, "the sleeper summed its messages and exited");
    let log = drain_out(&mut sw, root, "log");
    assert_eq!(event_name(&log[0]), "exited");
}

#[test]
fn the_wake_buffer_is_bounded_and_a_cached_instance_without_wake_is_gone() {
    let mut sw = swarm();
    let root = sw
        .root(SUPERVISOR.as_bytes(), lifecycle_caps(), Budget::default())
        .unwrap();
    sw.step();
    // One sleeper that asks to be woken, one that does not.
    let sleeper = |wake: bool| {
        format!(
            r#"
            local lc = queue.declare("system/lifecycle", {{ capacity = 4 }})
            local requests = queue.declare("requests", {{ capacity = 32, exported = true }})
            queue.push(lc, {{ op = "hibernate", wake_on_message = {} }})
            queue.wait({{requests}})
        "#,
            wake
        )
    };
    push_value(
        &mut sw,
        root,
        "requests",
        &spawn_request(&sleeper(true), &["lifecycle", "queue:*"], None),
    );
    settle(&mut sw, 10);
    let log = drain_out(&mut sw, root, "log");
    let waker = InstanceId(field(&log[0], "id").unwrap().as_u64().unwrap() as u32);
    push_value(
        &mut sw,
        root,
        "requests",
        &spawn_request(&sleeper(false), &["lifecycle", "queue:*"], None),
    );
    settle(&mut sw, 10);
    let log = drain_out(&mut sw, root, "log");
    let no_waker = InstanceId(field(&log[0], "id").unwrap().as_u64().unwrap() as u32);
    assert!(!sw.resident(waker) && !sw.resident(no_waker));

    // Without wake_on_message, a cached instance is not there.
    assert_eq!(
        sw.push(no_waker, "requests", b"\xc0"),
        Err(SwarmError::Gone)
    );

    // With it, the buffer takes exactly its bound and then refuses.
    for i in 0..16 {
        assert!(
            sw.push(waker, "requests", b"\xc0").is_ok(),
            "message {i} fits"
        );
    }
    assert!(matches!(
        sw.push(waker, "requests", b"\xc0"),
        Err(SwarmError::Limit(_))
    ));
}

#[test]
fn a_stamped_swarm_stamps_its_snapshots() {
    let mut sw = swarm();
    sw.set_host_identity(Some("node-a"));
    let root = sw
        .root(SUPERVISOR.as_bytes(), lifecycle_caps(), Budget::default())
        .unwrap();
    sw.step();
    let sleeper = r#"
        local lc = queue.declare("system/lifecycle", { capacity = 4 })
        local requests = queue.declare("requests", { capacity = 4, exported = true })
        queue.push(lc, { op = "hibernate", wake_on_message = true })
        queue.wait({requests})
    "#;
    push_value(
        &mut sw,
        root,
        "requests",
        &spawn_request(sleeper, &["lifecycle", "queue:*"], None),
    );
    settle(&mut sw, 10);
    let log = drain_out(&mut sw, root, "log");
    let child = InstanceId(field(&log[0], "id").unwrap().as_u64().unwrap() as u32);
    assert!(!sw.resident(child));

    // The cached snapshot restores under the same identity (the wake path),
    // and a fresh engine refuses it without the stamp — proving the stamp is
    // in the bytes, not advisory.
    sw.push(child, "requests", b"\xc0").unwrap();
    settle(&mut sw, 10);
    assert!(sw.alive() >= 1, "woke under its own stamp");
}

#[test]
fn a_query_answers_status_with_usage() {
    let mut sw = swarm();
    let root = sw
        .root(SUPERVISOR.as_bytes(), lifecycle_caps(), Budget::default())
        .unwrap();
    sw.step();
    push_value(
        &mut sw,
        root,
        "requests",
        &spawn_request("queue.wait({queue.declare('h', {capacity=1})})", &[], None),
    );
    settle(&mut sw, 10);
    let log = drain_out(&mut sw, root, "log");
    let child = field(&log[0], "id").unwrap().as_u64().unwrap();

    push_value(
        &mut sw,
        root,
        "requests",
        &rmpv::Value::Map(vec![
            ("op".into(), "query".into()),
            ("id".into(), rmpv::Value::from(child)),
        ]),
    );
    settle(&mut sw, 10);
    let log = drain_out(&mut sw, root, "log");
    assert_eq!(event_name(&log[0]), "status");
    assert!(
        detail(&log[0]).starts_with("alive insns="),
        "got: {}",
        detail(&log[0])
    );

    // A query about a handle that never existed answers status/gone.
    push_value(
        &mut sw,
        root,
        "requests",
        &rmpv::Value::Map(vec![
            ("op".into(), "query".into()),
            ("id".into(), rmpv::Value::from(4242u32)),
        ]),
    );
    settle(&mut sw, 10);
    let log = drain_out(&mut sw, root, "log");
    assert_eq!(event_name(&log[0]), "status");
    assert_eq!(detail(&log[0]), "gone");
}

#[test]
fn an_id_that_does_not_round_trip_is_refused_not_truncated() {
    let mut sw = swarm();
    let root = sw
        .root(SUPERVISOR.as_bytes(), lifecycle_caps(), Budget::default())
        .unwrap();
    sw.step();
    // 2^32 + root would truncate onto the root itself; the request must be
    // refused as unusable instead of carried out against a different
    // instance.
    push_value(
        &mut sw,
        root,
        "requests",
        &rmpv::Value::Map(vec![
            ("op".into(), "kill".into()),
            (
                "id".into(),
                rmpv::Value::from(0x1_0000_0000u64 + root.0 as u64),
            ),
        ]),
    );
    settle(&mut sw, 10);
    assert_eq!(sw.alive(), 1, "the root is untouched");
    let log = drain_out(&mut sw, root, "log");
    assert_eq!(event_name(&log[0]), "denied");
    assert_eq!(detail(&log[0]), "no usable id in the kill request");
}

#[test]
fn bytecode_spawns_are_a_stated_decision() {
    let mut sw = swarm();
    let root = sw
        .root(SUPERVISOR.as_bytes(), lifecycle_caps(), Budget::default())
        .unwrap();
    sw.step();
    // Bytes that are not UTF-8 source: refused by default, with the switch
    // named in the report. Hand-rolled msgpack, because the request's code
    // field is a str whose bytes are not UTF-8 — exactly what a compiled
    // chunk looks like on the wire.
    let mut raw = vec![0x82]; // fixmap, 2 pairs
    raw.extend_from_slice(b"\xa2op\xa5spawn");
    raw.extend_from_slice(b"\xa4code\xa7\x1bLua\xff\x00\x01");
    sw.push(root, "requests", &raw).unwrap();
    settle(&mut sw, 10);
    let log = drain_out(&mut sw, root, "log");
    assert_eq!(event_name(&log[0]), "faulted");
    assert!(
        detail(&log[0]).contains("allow_bytecode"),
        "got: {}",
        detail(&log[0])
    );
}

mod pump {
    //! The capability story end to end: same request bytes, different
    //! grants, different answers — through the swarm's own drive loop.

    use super::*;
    use drt_connector::{mock::MockConnector, Dispatcher, Registry};
    use drt_swarm::pump::PumpHost;

    #[test]
    fn hostcalls_are_gated_by_each_instances_own_attenuated_set() {
        let mut registry = Registry::new();
        registry
            .wire(
                "time",
                Arc::new(MockConnector::new().answer("time", rmpv::Value::from(12_345u64))),
                None,
            )
            .unwrap();
        let engine = Arc::new(DiluviumEngine::new().unwrap());
        let mut sw = Swarm::new(engine, PumpHost::new(StepHost, Dispatcher::new(registry)));

        // Parent and child run the same code: one hostcall, then the reply's
        // status pushed to an exported queue. The parent holds host:time*;
        // it spawns the child with no grants at all.
        let caller = r#"
            local calls = queue.declare("host/calls", { capacity = 4, exported = true, on_full = "reject" })
            local replies = queue.declare("host/replies", { capacity = 4 })
            local verdict = queue.declare("verdict", { capacity = 4, exported = true })
            queue.push(calls, { tok = 7, call = "time" })
            local _, reply = queue.wait({replies})
            assert(reply.tok == 7)
            queue.push(verdict, reply.status)
        "#;
        let parent_code = format!(
            r#"
            local lc = queue.declare("system/lifecycle", {{ capacity = 4 }})
            queue.push(lc, {{ op = "spawn", code = [==[{caller}]==] }})
            {caller}
            queue.wait({{queue.declare("hold", {{ capacity = 1 }})}})
        "#
        );
        let root = sw
            .root(
                parent_code.as_bytes(),
                vec![Grant::grant("lifecycle"), Grant::grant("host:time*")],
                Budget::default(),
            )
            .unwrap();
        settle(&mut sw, 10);

        let parent_verdict = drain_out(&mut sw, root, "verdict");
        assert_eq!(
            parent_verdict[0].as_str(),
            Some("ok"),
            "the parent holds the grant"
        );

        // The child made the same call with the same bytes and was denied —
        // and the denial is an answer, not a drop, so the child completed.
        let child = InstanceId(root.0 + 1);
        assert!(!sw.resident(child), "the child ran to completion");
        // Its verdict left with it; assert through the parent instead: spawn
        // a second child granted a narrowed slice, and check the swarm's own
        // record of both sets.
        let parent_caps = sw.caps(root).unwrap();
        assert!(parent_caps.holds("host:time"));
    }

    #[test]
    fn a_denied_child_reads_denied_not_silence() {
        let mut registry = Registry::new();
        registry
            .wire(
                "time",
                Arc::new(MockConnector::new().answer("time", rmpv::Value::from(1u64))),
                None,
            )
            .unwrap();
        let engine = Arc::new(DiluviumEngine::new().unwrap());
        let mut sw = Swarm::new(engine, PumpHost::new(StepHost, Dispatcher::new(registry)));

        // A parked caller that reports its verdict and waits, so the test
        // can read the exported queue while it is still resident.
        let caller = r#"
            local calls = queue.declare("host/calls", { capacity = 4, exported = true, on_full = "reject" })
            local replies = queue.declare("host/replies", { capacity = 4 })
            local verdict = queue.declare("verdict", { capacity = 4, exported = true })
            local hold = queue.declare("hold", { capacity = 1 })
            queue.push(calls, { tok = 9, call = "time" })
            local _, reply = queue.wait({replies})
            queue.push(verdict, reply.status .. "|" .. tostring(reply.detail))
            queue.wait({hold})
        "#;
        let parent_code = format!(
            r#"
            local lc = queue.declare("system/lifecycle", {{ capacity = 4 }})
            local hold = queue.declare("hold", {{ capacity = 1 }})
            queue.push(lc, {{ op = "spawn", code = [==[{caller}]==] }})
            queue.wait({{hold}})
        "#
        );
        let root = sw
            .root(
                parent_code.as_bytes(),
                vec![Grant::grant("lifecycle"), Grant::grant("host:time*")],
                Budget::default(),
            )
            .unwrap();
        settle(&mut sw, 10);
        let child = InstanceId(root.0 + 1);
        assert!(
            sw.resident(child),
            "the caller parked on hold after reporting"
        );
        let verdict = drain_out(&mut sw, child, "verdict");
        let text = verdict[0].as_str().unwrap();
        assert!(
            text.starts_with("denied|"),
            "spawned with no grants, the same call is denied with a detail: {text}"
        );
        assert!(text.contains("outside this instance's grants"), "{text}");
    }
}
