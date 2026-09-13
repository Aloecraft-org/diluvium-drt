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

/// Every push in these tests is the harness standing in for the runtime.
fn from() -> drt_config::peer::Sender {
    drt_config::peer::Sender::runtime(drt_config::project::NodePath::root())
}

fn swarm() -> Swarm<StepHost> {
    let engine = Arc::new(DiluviumEngine::new().unwrap());
    Swarm::new(engine, StepHost::new())
}

fn swarm_with(max_instances: u32, spawns_per_step: u32) -> Swarm<StepHost> {
    let engine = Arc::new(DiluviumEngine::new().unwrap());
    Swarm::with_limits(engine, StepHost::new(), max_instances, spawns_per_step)
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

/// A child that parks forever, so its slot is still there to be read. A
/// child that returns is released the moment it does, and `Swarm::budget`
/// answers `None` for a slot that no longer exists.
const PARKS: &str = r#"
    local q = queue.declare("idle", { capacity = 1 })
    queue.wait({q})
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
    sw.push(id, queue, &from(), &buf).unwrap();
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
    assert!(sw.push(root, "requests", &from(), b"\xa4stop").is_ok());
    // Resident, unknown queue.
    assert_eq!(
        sw.push(root, "no/such/queue", &from(), b"\xc0"),
        Err(SwarmError::UnknownQueue)
    );
    // Unknown instance: gone, immediately.
    assert_eq!(
        sw.push(InstanceId(999), "requests", &from(), b"\xc0"),
        Err(SwarmError::Gone)
    );
    settle(&mut sw, 10);
    // Dead instance (the root read "stop" and returned): gone.
    assert_eq!(sw.alive(), 0);
    assert_eq!(
        sw.push(root, "requests", &from(), b"\xc0"),
        Err(SwarmError::Gone)
    );
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
        sw.push(child, "requests", &from(), &buf).unwrap();
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
        sw.push(no_waker, "requests", &from(), b"\xc0"),
        Err(SwarmError::Gone)
    );

    // With it, the buffer takes exactly its bound and then refuses.
    for i in 0..16 {
        assert!(
            sw.push(waker, "requests", &from(), b"\xc0").is_ok(),
            "message {i} fits"
        );
    }
    assert!(matches!(
        sw.push(waker, "requests", &from(), b"\xc0"),
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
    sw.push(child, "requests", &from(), b"\xc0").unwrap();
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
    sw.push(root, "requests", &from(), &raw).unwrap();
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
        let mut sw = Swarm::new(
            engine,
            PumpHost::new(StepHost::new(), Dispatcher::new(registry)),
        );

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

    /// A connector answers a column, and the guest reads the bytes.
    ///
    /// The blob lane end to end (`doc/Plan-2026-09.md` §3.2): the connector
    /// wraps eight-byte doubles with `drt_hostcall::column`, the dispatcher
    /// moves them into the reply's side channel and leaves `{dtype, len,
    /// blob}` behind, and the pump's encode puts them where the descriptor
    /// was. The guest is an ordinary program with no `numeric` and no
    /// `array`, so what arrives is a Lua string -- which is exactly what
    /// §3.1 says `dv_array_adopt` does in a build without the feature, so
    /// this behaviour does not change when A0 lands.
    ///
    /// Asserted on the **bits**, per §3.3, not on a formatted double: the
    /// question is whether the exact bytes crossed, and `%g` on four
    /// platforms is four answers to a question nobody asked.
    #[test]
    fn a_connector_answers_a_column_and_the_guest_reads_its_bytes() {
        let bytes: Vec<u8> = [1.0f64, 2.0, 3.0]
            .iter()
            .flat_map(|d| d.to_le_bytes())
            .collect();
        let mut registry = Registry::new();
        registry
            .wire(
                "data",
                Arc::new(MockConnector::new().answer(
                    "data/read",
                    rmpv::Value::Map(vec![
                        ("rows".into(), rmpv::Value::from(3u64)),
                        (
                            "price".into(),
                            drt_hostcall::column(drt_hostcall::Dtype::F64, bytes.clone()),
                        ),
                    ]),
                )),
                None,
            )
            .unwrap();
        let engine = Arc::new(DiluviumEngine::new().unwrap());
        let mut sw = Swarm::new(
            engine,
            PumpHost::new(StepHost::new(), Dispatcher::new(registry)),
        );

        let caller = r#"
            local calls = queue.declare("host/calls", { capacity = 4, exported = true, on_full = "reject" })
            local replies = queue.declare("host/replies", { capacity = 4 })
            local verdict = queue.declare("verdict", { capacity = 4, exported = true })
            local hold = queue.declare("hold", { capacity = 1 })
            queue.push(calls, { tok = 5, call = "data/read" })
            local _, reply = queue.wait({replies})
            local column = reply.value.price
            queue.push(verdict, table.concat({
                reply.status,
                type(column),
                #column,
                ("%016x"):format(string.unpack("<I8", column)),
                ("%016x"):format(string.unpack("<I8", column, 17)),
                tostring(reply.value.rows),
            }, "|"))
            queue.wait({hold})
        "#;
        let root = sw
            .root(
                caller.as_bytes(),
                vec![Grant::grant("host:data/*")],
                Budget::default(),
            )
            .unwrap();
        settle(&mut sw, 10);

        let verdict = drain_out(&mut sw, root, "verdict");
        assert_eq!(
            verdict[0].as_str(),
            // 24 bytes, the first element's bits and the third's, and the
            // ordinary field beside the column still an ordinary field.
            Some("ok|string|24|3ff0000000000000|4008000000000000|3"),
            "the column did not cross intact"
        );
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
        let mut sw = Swarm::new(
            engine,
            PumpHost::new(StepHost::new(), Dispatcher::new(registry)),
        );

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

/// A program that fills its own exported queue and then parks *for space*,
/// not for a message.
///
/// `on_full = "block"` is the only way to get such a park (Messaging.md
/// §"Delivery"), and `dv.h` §8.3 makes the host say which kind of park it is
/// answering. Answering the wrong one resumes a program straight back into a
/// push that fails again — so a host that only ever asks "is the queue
/// non-empty" leaves this one parked forever, on a queue that is not merely
/// non-empty but completely full.
const FILLS_THEN_BLOCKS: &str = "\
local out = queue.declare('out', {capacity = 2, exported = true, on_full = 'block'})\n\
local idle = queue.declare('idle', {capacity = 1})\n\
for i = 1, 5 do\n\
  queue.push(out, i)\n\
end\n\
queue.wait({idle})\n\
";

#[test]
fn a_program_waiting_for_space_is_resumed_when_the_queue_drains() {
    let mut sw = swarm();
    let id = sw
        .root(FILLS_THEN_BLOCKS.as_bytes(), vec![], Budget::default())
        .unwrap();

    // Two land and the third blocks: the queue is at capacity and the
    // program is parked for space, not for a message.
    settle(&mut sw, 4);
    let mut got: Vec<i64> = Vec::new();
    let first = drain_out(&mut sw, id, "out");
    assert_eq!(first.len(), 2, "the queue should be at its capacity of 2");
    got.extend(first.iter().map(|v| v.as_i64().unwrap()));

    // Draining made room. A host that asks the message question sees a
    // now-*empty* queue and never resumes it; asking the space question
    // resumes it, and the rest land.
    for _ in 0..8 {
        settle(&mut sw, 4);
        got.extend(
            drain_out(&mut sw, id, "out")
                .iter()
                .map(|v| v.as_i64().unwrap()),
        );
        if got.len() == 5 {
            break;
        }
    }
    assert_eq!(
        got,
        vec![1, 2, 3, 4, 5],
        "the blocked pushes never landed — the space-park was answered as a \
         message-park, or not at all"
    );
}

/// Budgets attenuate, and the two ways they did not.
///
/// `GUARANTEES.md` says a child holds a subset of its parent's grants and
/// nothing more, and `08-spawn-and-hibernation` teaches that sentence. It
/// was true of capabilities and false of budgets: `do_spawn` took the
/// requested budget verbatim.
#[test]
fn a_child_may_not_state_a_budget_larger_than_its_parent_s() {
    let mut sw = swarm();
    let root = sw
        .root(
            SUPERVISOR.as_bytes(),
            lifecycle_caps(),
            Budget {
                instructions: Some(1_000_000),
                memory_kb: Some(4096),
            },
        )
        .unwrap();
    sw.step();

    // Asking for more instructions than the parent holds is refused by name,
    // as a reply rather than a fault -- the same shape as a capability the
    // parent does not hold.
    push_value(
        &mut sw,
        root,
        "requests",
        &spawn_request("return 1", &[], Some((9_000_000, 1024))),
    );
    settle(&mut sw, 10);
    let log = drain_out(&mut sw, root, "log");
    assert_eq!(event_name(&log[0]), "denied");
    assert!(
        detail(&log[0]).contains("budget"),
        "the refusal must name the budget, not just refuse: {}",
        detail(&log[0])
    );

    // Memory is the other bound and is checked the same way.
    push_value(
        &mut sw,
        root,
        "requests",
        &spawn_request("return 1", &[], Some((1000, 999_999))),
    );
    settle(&mut sw, 10);
    let log = drain_out(&mut sw, root, "log");
    assert_eq!(event_name(&log[0]), "denied");

    // Narrowing is the whole point and still works.
    push_value(
        &mut sw,
        root,
        "requests",
        &spawn_request("return 1", &[], Some((1000, 512))),
    );
    settle(&mut sw, 10);
    let log = drain_out(&mut sw, root, "log");
    assert_eq!(event_name(&log[0]), "spawned");
}

/// The cheaper escape: a child that states no budget at all.
///
/// It needed no intent — `budget = nil` is what a spawn request looks like
/// when nobody thought about it — and it produced an unlimited child under a
/// bounded parent. An unstated bound now resolves to the parent's ceiling,
/// which is what `fits_within` always claimed it meant.
#[test]
fn a_child_that_states_no_budget_inherits_its_parent_s_ceiling() {
    let mut sw = swarm();
    let parent_budget = Budget {
        instructions: Some(1_000_000),
        memory_kb: Some(4096),
    };
    let root = sw
        .root(SUPERVISOR.as_bytes(), lifecycle_caps(), parent_budget)
        .unwrap();
    sw.step();

    // A runaway child with no stated budget. Under the old behaviour this
    // spawned unlimited and ran until the step loop gave up on it; now the
    // parent's ceiling applies and it is reported `exceeded`.
    push_value(
        &mut sw,
        root,
        "requests",
        &spawn_request("while true do end", &[], None),
    );
    settle(&mut sw, 20);
    let log = drain_out(&mut sw, root, "log");
    let names: Vec<_> = log.iter().map(event_name).collect();
    assert_eq!(
        names,
        ["spawned", "exceeded"],
        "an unstated budget must inherit, not become unlimited"
    );

    // And the ceiling it inherited is the parent's, read back off the slot.
    // A *parked* child, because a finished one has already been released and
    // there is no slot left to read.
    push_value(
        &mut sw,
        root,
        "requests",
        &spawn_request(PARKS, &["queue:*"], None),
    );
    settle(&mut sw, 10);
    let log = drain_out(&mut sw, root, "log");
    assert_eq!(event_name(&log[0]), "spawned");
    let child = InstanceId(field(&log[0], "id").and_then(|v| v.as_u64()).unwrap() as u32);
    assert_eq!(sw.budget(child), Some(parent_budget));
}

/// A partially stated budget takes the parent's ceiling for the half it did
/// not name. Stating one bound must not silently unbound the other.
#[test]
fn a_half_stated_budget_inherits_the_other_half() {
    let mut sw = swarm();
    let root = sw
        .root(
            SUPERVISOR.as_bytes(),
            lifecycle_caps(),
            Budget {
                instructions: Some(1_000_000),
                memory_kb: Some(4096),
            },
        )
        .unwrap();
    sw.step();

    let mut request = spawn_request(PARKS, &["queue:*"], None);
    if let rmpv::Value::Map(ref mut map) = request {
        map.push((
            "budget".into(),
            rmpv::Value::Map(vec![("instructions".into(), rmpv::Value::from(500u64))]),
        ));
    }
    push_value(&mut sw, root, "requests", &request);
    settle(&mut sw, 10);
    let log = drain_out(&mut sw, root, "log");
    assert_eq!(event_name(&log[0]), "spawned");
    let child = InstanceId(field(&log[0], "id").and_then(|v| v.as_u64()).unwrap() as u32);
    assert_eq!(
        sw.budget(child),
        Some(Budget {
            instructions: Some(500),
            memory_kb: Some(4096),
        })
    );
}

// depth: node paths, derived at spawn

/// A spawn request that names the child. The presence of `name` is the whole
/// stateful-versus-ephemeral distinction, so a test for it is a test for both.
fn named_spawn(code: &str, caps: &[&str], name: &str) -> rmpv::Value {
    let mut map = spawn_request(code, caps, None).as_map().unwrap().to_vec();
    map.push(("name".into(), name.into()));
    rmpv::Value::Map(map)
}

/// Spawn one child through the supervisor and return its handle. The child
/// parks, so its slot is still there to be read.
fn spawn_child(
    sw: &mut Swarm<StepHost>,
    root: InstanceId,
    request: &rmpv::Value,
) -> Option<InstanceId> {
    sw.step();
    push_value(sw, root, "requests", request);
    settle(sw, 10);
    sw.ids().into_iter().find(|id| *id != root)
}

/// The root node is `root`, and a child's path is derived from its parent's.
/// This is the identity consent.md §6 makes the subject of a grant request, so
/// it has to come from the tree rather than from the asking program.
#[test]
fn a_child_s_node_path_is_derived_from_its_parent_s() {
    let mut sw = swarm();
    let root = sw
        .root(SUPERVISOR.as_bytes(), lifecycle_caps(), Budget::default())
        .unwrap();
    assert_eq!(sw.path(root).unwrap().as_str(), "root");

    let child = spawn_child(&mut sw, root, &named_spawn(PARKS, &["queue:*"], "intake"))
        .expect("a child was spawned");
    assert_eq!(
        sw.path(child).unwrap().as_str(),
        "root/intake",
        "the name the request asked for, under the parent's path"
    );
    assert_eq!(sw.path(child).unwrap().parent().unwrap().as_str(), "root");
}

/// An unnamed child gets its instance id, which is unique for the life of the
/// swarm because handles are never reused. The spawner not caring is the
/// ephemeral case, and it needs no declaration of its own.
#[test]
fn an_unnamed_child_gets_its_id_as_its_last_segment() {
    let mut sw = swarm();
    let root = sw
        .root(SUPERVISOR.as_bytes(), lifecycle_caps(), Budget::default())
        .unwrap();
    let child = spawn_child(&mut sw, root, &spawn_request(PARKS, &["queue:*"], None))
        .expect("a child was spawned");
    assert_eq!(
        sw.path(child).unwrap().as_str(),
        format!("root/{}", child.0)
    );
}

/// What "`drt` is a privileged name" means in practice: a node cannot claim
/// one, and the refusal arrives on the channel every other spawn refusal does.
#[test]
fn a_node_cannot_name_itself_after_the_runtime_or_a_root_directory() {
    for reserved in ["drt", "state", "live", "init", "log", "profile", "DRT"] {
        let mut sw = swarm();
        let root = sw
            .root(SUPERVISOR.as_bytes(), lifecycle_caps(), Budget::default())
            .unwrap();
        let denied = spawn_child(&mut sw, root, &named_spawn(PARKS, &["queue:*"], reserved));
        assert!(
            denied.is_none(),
            "'{reserved}' must spawn nothing, got {denied:?}"
        );

        let log = drain_out(&mut sw, root, "log");
        let names: Vec<_> = log.iter().map(event_name).collect();
        assert_eq!(names, ["denied"], "'{reserved}': {log:?}");
        let detail = field(&log[0], "detail")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        assert!(
            detail.contains(reserved) && detail.contains("reserved"),
            "'{reserved}': the refusal names it and says why -- {detail}"
        );
    }
}

/// A node's capability set records whose it is, so a hostcall answered by the
/// dispatcher knows which node asked without a second identity channel
/// threaded through every connector.
#[test]
fn a_node_s_capability_set_records_the_node_it_belongs_to() {
    let mut sw = swarm();
    let root = sw
        .root(SUPERVISOR.as_bytes(), lifecycle_caps(), Budget::default())
        .unwrap();
    assert_eq!(
        sw.caps(root).unwrap().holder().map(|p| p.0.clone()),
        Some("root".to_string())
    );

    let child = spawn_child(&mut sw, root, &named_spawn(PARKS, &["queue:*"], "intake"))
        .expect("a child was spawned");
    assert_eq!(
        sw.caps(child).unwrap().holder().map(|p| p.0.clone()),
        Some("root/intake".to_string()),
        "and a child's set says it is the child's"
    );
}

// ---------------------------------------------------- the peer groundwork --
//
// Nothing here reaches a peer. These hold the shape the slice that builds
// peer delivery needs, so that slice changes who fills a field in rather
// than what the field is.

/// Acceptance 1: a delivered message carries `{peer, node}`, and a program
/// can read it.
///
/// Read back out of the delivered bytes rather than asserted on the type,
/// because what matters is what a *node* sees. A test over `Sender` alone
/// would pass with nothing attached to the message at all.
#[test]
fn a_delivered_message_carries_who_it_came_from() {
    let mut sw = swarm();
    let root = sw
        .root(SUPERVISOR.as_bytes(), lifecycle_caps(), Budget::default())
        .unwrap();
    sw.step();

    let root_id = drt_config::id::Uuid7::mint(1_757_707_440_000, [0x66; 10]);
    let sender = drt_config::peer::Sender::local(
        root_id,
        drt_config::project::NodePath::root()
            .child("intake")
            .unwrap(),
    );

    let mut body = Vec::new();
    rmpv::encode::write_value(
        &mut body,
        &rmpv::Value::Map(vec![("op".into(), "ping".into())]),
    )
    .unwrap();

    let delivered = drt_swarm::swarm::attach_sender(&body, &sender);
    let value = rmpv::decode::read_value(&mut &delivered[..]).unwrap();

    let from = field(&value, "from").expect("every delivered message carries a sender");
    let peer = field(from, "peer").expect("the sender names a peer");
    assert_eq!(field(peer, "kind").unwrap().as_str(), Some("root"));
    assert_eq!(
        field(peer, "root_id").unwrap().as_str(),
        Some(root_id.to_string().as_str()),
        "a local sender names its own root, so one field answers 'who sent this'"
    );
    assert_eq!(
        field(from, "node").unwrap().as_str(),
        Some("root/intake"),
        "the local node form; the root is in `peer` already"
    );
    assert_eq!(
        field(&value, "op").unwrap().as_str(),
        Some("ping"),
        "and the message the sender wrote is untouched beside it"
    );

    // It really is the delivery path that attaches it, not this test.
    assert!(sw.push(root, "requests", &sender, &body).is_ok());
}

/// The runtime is the authority on who sent something, so a guest cannot
/// write the reserved key itself and be believed.
#[test]
fn a_sender_a_guest_wrote_itself_is_replaced() {
    let forged = rmpv::Value::Map(vec![
        ("from".into(), rmpv::Value::from("someone else entirely")),
        ("op".into(), "ping".into()),
    ]);
    let mut body = Vec::new();
    rmpv::encode::write_value(&mut body, &forged).unwrap();

    let sender = drt_config::peer::Sender::runtime(drt_config::project::NodePath::root());
    let value = rmpv::decode::read_value(&mut &drt_swarm::swarm::attach_sender(&body, &sender)[..])
        .unwrap();

    let from = field(&value, "from").unwrap();
    assert!(
        from.as_map().is_some(),
        "the forged string was replaced by the real sender: {from:?}"
    );
    let pairs = value.as_map().unwrap();
    assert_eq!(
        pairs
            .iter()
            .filter(|(k, _)| k.as_str() == Some("from"))
            .count(),
        1,
        "and exactly once"
    );
}

/// Acceptance 2: a write naming a peer is refused by name, whichever kind
/// of peer it names.
#[test]
fn a_write_to_a_peer_fails_by_name_for_a_root_and_for_a_plugin() {
    use drt_config::peer::{PeerRef, QueueAddress};

    let mut sw = swarm();
    let root = sw
        .root(SUPERVISOR.as_bytes(), lifecycle_caps(), Budget::default())
        .unwrap();
    sw.step();

    let peers = [
        PeerRef::root(drt_config::id::Uuid7::mint(1_757_707_440_000, [0x77; 10])),
        PeerRef::plugin("webauthn"),
    ];
    for peer in peers {
        let addr = QueueAddress::on(peer, "requests");
        let e = sw
            .push_to(root, &addr, &from(), b"\xc0")
            .expect_err("peer delivery is not supported in this build");
        let said = e.to_string();
        assert!(
            said.contains("peer delivery is not supported"),
            "refused by name: {said}"
        );
        assert!(said.contains("requests"), "and it names the queue: {said}");
    }

    // The same address without the peer component delivers.
    assert!(sw
        .push_to(root, &QueueAddress::local("requests"), &from(), b"\xa4stop")
        .is_ok());
}

/// Acceptance 7: a full queue is an immediate named failure, never a wait.
///
/// `PARKS` declares a queue of capacity 1 and then parks, so the second
/// message has nowhere to go and nothing will drain it. The assertion that
/// matters is that the call *returns* — a `push` that blocked here would
/// hang the test rather than fail it.
#[test]
fn a_write_to_a_full_queue_is_refused_immediately_rather_than_waiting() {
    let mut sw = swarm();
    let root = sw
        .root(PARKS.as_bytes(), lifecycle_caps(), Budget::default())
        .unwrap();
    sw.step();

    assert!(
        sw.push(root, "idle", &from(), b"\xc0").is_ok(),
        "the first message fits the capacity of one"
    );
    let e = sw
        .push(root, "idle", &from(), b"\xc0")
        .expect_err("the second has nowhere to go");
    assert!(
        matches!(e, SwarmError::Limit(_)),
        "a bounded thing being full is a Limit, not a wait: {e:?}"
    );
    assert!(
        e.to_string().contains("idle"),
        "and it names the queue that is full: {e}"
    );
}

/// A consumer that is gone and one that is full look the same to a sender,
/// which is the property that keeps a handler from hanging on a wedged
/// consumer: both are an immediate `Err`, and the caller decides inside its
/// own deadline.
#[test]
fn gone_and_full_are_both_immediate_refusals() {
    let mut sw = swarm();
    let root = sw
        .root(PARKS.as_bytes(), lifecycle_caps(), Budget::default())
        .unwrap();
    sw.step();

    sw.push(root, "idle", &from(), b"\xc0").unwrap();
    let full = sw.push(root, "idle", &from(), b"\xc0").unwrap_err();
    let gone = sw
        .push(InstanceId(4242), "idle", &from(), b"\xc0")
        .unwrap_err();

    for e in [&full, &gone] {
        assert!(
            matches!(e, SwarmError::Limit(_) | SwarmError::Gone),
            "{e:?}"
        );
    }
}

/// The map header grows format at 15 entries: `fixmap` holds up to 15, so
/// a message with exactly 15 becomes a `map16`. Splicing a header is only
/// correct if it crosses that boundary, and this is the message that would
/// arrive corrupt if it did not.
#[test]
fn attaching_a_sender_crosses_the_fixmap_boundary_correctly() {
    let sender = drt_config::peer::Sender::runtime(drt_config::project::NodePath::root());

    for entries in [0usize, 1, 14, 15, 16, 17] {
        let pairs: Vec<(rmpv::Value, rmpv::Value)> = (0..entries)
            .map(|i| {
                (
                    rmpv::Value::from(format!("k{i}")),
                    rmpv::Value::from(i as u64),
                )
            })
            .collect();
        let mut body = Vec::new();
        rmpv::encode::write_value(&mut body, &rmpv::Value::Map(pairs)).unwrap();

        let out = drt_swarm::swarm::attach_sender(&body, &sender);
        let value = rmpv::decode::read_value(&mut &out[..])
            .unwrap_or_else(|e| panic!("{entries} entries: the spliced map must decode: {e}"));
        let map = value.as_map().expect("still a map");

        assert_eq!(map.len(), entries + 1, "{entries} entries plus the sender");
        assert!(
            field(&value, "from").is_some(),
            "{entries} entries: the sender is there"
        );
        for i in 0..entries {
            assert_eq!(
                field(&value, &format!("k{i}")).and_then(|v| v.as_u64()),
                Some(i as u64),
                "{entries} entries: k{i} survived the splice"
            );
        }
    }
}

/// A message that is not a map is delivered byte for byte, and borrowed
/// rather than copied — the non-map path must not allocate.
#[test]
fn a_non_map_message_is_delivered_untouched() {
    let sender = drt_config::peer::Sender::runtime(drt_config::project::NodePath::root());
    for raw in [
        &b"\xc0"[..],         // nil
        &b"\x2a"[..],         // a positive fixint
        &b"\xa5hello"[..],    // a string
        &b"\x92\x01\x02"[..], // an array
        &b""[..],             // nothing at all
    ] {
        let out = drt_swarm::swarm::attach_sender(raw, &sender);
        assert_eq!(&out[..], raw, "delivered byte for byte");
        assert!(
            matches!(out, std::borrow::Cow::Borrowed(_)),
            "and borrowed, so the hot path does not allocate for it"
        );
    }
}
