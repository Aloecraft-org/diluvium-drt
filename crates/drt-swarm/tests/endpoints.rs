//! Instances reaching each other through endpoint references: what a
//! `spawned` event and the `ref` op hand out, what `endpoint.bind` makes
//! of one, and how the swarm routes what is pushed through it.

#![cfg(feature = "engine-diluvium")]

use std::sync::Arc;

use drt_caps::Grant;
use drt_config::Budget;
use drt_swarm::engine::diluvium_engine::DiluviumEngine;
use drt_swarm::swarm::{StepHost, Swarm};
use drt_swarm::InstanceId;

fn swarm() -> Swarm<StepHost> {
    let engine = Arc::new(DiluviumEngine::new().unwrap());
    Swarm::new(engine, StepHost::new())
}

fn settle(sw: &mut Swarm<StepHost>, steps: usize) {
    for _ in 0..steps {
        sw.step();
    }
}

fn drain(sw: &mut Swarm<StepHost>, id: InstanceId, queue: &str) -> Vec<rmpv::Value> {
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

fn field<'a>(v: &'a rmpv::Value, name: &str) -> Option<&'a rmpv::Value> {
    v.as_map()?
        .iter()
        .find(|(k, _)| k.as_str() == Some(name))
        .map(|(_, v)| v)
}

fn text<'a>(v: &'a rmpv::Value, name: &str) -> &'a str {
    field(v, name).and_then(|v| v.as_str()).unwrap_or("?")
}

/// A root that spawns the children it is given, in order, and keeps their
/// spawned events in `kids` (indexed from 1) for the `then` code to use.
/// It has `lifecycle`; the children get nothing.
fn root_program(children: &[&str], then: &str) -> String {
    let mut spawns = String::new();
    for code in children {
        spawns.push_str(&format!("kids[#kids + 1] = spawn([==[{code}]==])\n"));
    }
    format!(
        r#"
        local lc = queue.declare("system/lifecycle", {{ capacity = 8 }})
        local ev = queue.declare("system/events", {{ capacity = 16 }})
        local out = queue.declare("out", {{ capacity = 16, exported = true }})
        local idle = queue.declare("idle", {{ capacity = 1 }})
        -- The test's knock: StepHost keeps no clock, so a program that
        -- must let the router run a step waits for one of these.
        local go = queue.declare("go", {{ capacity = 4, exported = true }})
        local function spawn(code)
            queue.push(lc, {{ op = "spawn", code = code, caps = {{ "queue:*" }} }})
            local _, e = queue.wait({{ ev }})
            assert(e.event == "spawned", e.event)
            return e
        end
        local function ask(request)
            queue.push(lc, request)
            local _, e = queue.wait({{ ev }})
            return e
        end
        local kids = {{}}
        {spawns}
        {then}
        queue.wait({{ idle }})
        "#
    )
}

/// Settle, then knock on the root's `go` queue and settle again, `times`
/// times over.
fn knock(sw: &mut Swarm<StepHost>, root: InstanceId, times: usize) {
    let from = drt_config::peer::Sender::runtime(drt_config::project::NodePath::root());
    let mut buf = Vec::new();
    rmpv::encode::write_value(&mut buf, &rmpv::Value::from(true)).unwrap();
    settle(sw, 4);
    for _ in 0..times {
        let _ = sw.push(root, "go", &from, &buf);
        settle(sw, 4);
    }
}

fn root(sw: &mut Swarm<StepHost>, code: &str) -> InstanceId {
    sw.root(
        code.as_bytes(),
        vec![Grant::grant("lifecycle"), Grant::grant("queue:*")],
        Budget::default(),
    )
    .unwrap()
}

/// Parks on its inbox, and reports what arrives to an exported `out`.
const REPORTER: &str = r#"
    local inbox = queue.lookup("inbox")
    local out = queue.declare("out", { capacity = 8, exported = true })
    while true do
        local _, m = queue.wait({ inbox })
        queue.push(out, { said = m.said, from = m.from.node })
    end
"#;

#[test]
fn a_spawned_event_carries_a_reference_and_a_parent_reaches_its_child() {
    let mut sw = swarm();
    let code = root_program(
        &[REPORTER],
        r#"
        assert(endpoint.is_endpoint ~= nil)
        local to_kid = endpoint.bind(kids[1].ref, "to_kid")
        local ok, why = queue.push(to_kid, { said = "hello child" })
        queue.push(out, { pushed = tostring(ok) .. "/" .. tostring(why) })
        "#,
    );
    let parent = root(&mut sw, &code);
    settle(&mut sw, 6);

    let report = drain(&mut sw, parent, "out");
    assert_eq!(text(&report[0], "pushed"), "true/ok", "{report:?}");
    let child = InstanceId(parent.0 + 1);
    let got = drain(&mut sw, child, "out");
    assert_eq!(got.len(), 1, "{got:?}");
    assert_eq!(text(&got[0], "said"), "hello child");
    assert_eq!(
        text(&got[0], "from"),
        "root",
        "the runtime attaches who sent it"
    );
}

#[test]
fn siblings_meet_by_introduction_and_talk_directly() {
    let mut sw = swarm();
    // The introducer: told a peer's reference, it binds it and speaks.
    let speaker = r#"
        local inbox = queue.lookup("inbox")
        local idle = queue.declare("idle", { capacity = 1 })
        local _, m = queue.wait({ inbox })
        local peer = endpoint.bind(m.peer, "peer")
        queue.push(peer, { said = "hello sibling" })
        queue.wait({ idle })
    "#;
    let code = root_program(
        &[speaker, REPORTER],
        r#"
        local to_speaker = endpoint.bind(kids[1].ref, "to_speaker")
        queue.push(to_speaker, { peer = kids[2].ref })
        "#,
    );
    let parent = root(&mut sw, &code);
    settle(&mut sw, 8);

    let speaker_id = InstanceId(parent.0 + 1);
    let reporter_id = InstanceId(parent.0 + 2);
    let got = drain(&mut sw, reporter_id, "out");
    assert_eq!(got.len(), 1, "{got:?}");
    assert_eq!(text(&got[0], "said"), "hello sibling");
    assert_eq!(
        text(&got[0], "from"),
        sw.path(speaker_id).unwrap().as_str(),
        "it came from the sibling, not the parent"
    );
}

#[test]
fn a_push_to_a_child_that_has_exited_answers_gone() {
    let mut sw = swarm();
    let code = root_program(
        &["return 1"],
        r#"
        local to_kid = endpoint.bind(kids[1].ref, "to_kid")
        local _, e = queue.wait({ ev })            -- its exit
        queue.push(to_kid, { said = "anyone?" })   -- accepted into the endpoint
        queue.wait({ go })                         -- the router has had a step
        local ok, why = queue.push(to_kid, { said = "anyone?" })
        queue.push(out, { exit = e.event, status = endpoint.status(to_kid),
                          second = tostring(ok) .. "/" .. tostring(why) })
        "#,
    );
    let parent = root(&mut sw, &code);
    knock(&mut sw, parent, 6);
    let report = drain(&mut sw, parent, "out");
    assert_eq!(report.len(), 1, "{report:?}");
    assert_eq!(text(&report[0], "exit"), "exited");
    assert_eq!(text(&report[0], "status"), "gone");
    assert_eq!(text(&report[0], "second"), "false/gone");
}

#[test]
fn the_ref_op_answers_self_and_descendants_and_refuses_a_sibling() {
    let mut sw = swarm();
    let code = root_program(
        &[REPORTER, REPORTER],
        r#"
        local mine = ask({ op = "ref" })
        local kid = ask({ op = "ref", id = kids[1].id })
        queue.push(out, { mine = mine.event, mine_id = mine.id,
                          kid = kid.event, kid_id = kid.id })
        -- The first child asks about its sibling, and is refused.
        local to_kid = endpoint.bind(kid.ref, "to_kid")
        queue.push(to_kid, { said = "ok" })
        "#,
    );
    let parent = root(&mut sw, &code);
    settle(&mut sw, 6);
    let report = drain(&mut sw, parent, "out");
    assert_eq!(text(&report[0], "mine"), "ref");
    assert_eq!(
        field(&report[0], "mine_id").unwrap().as_u64(),
        Some(parent.0 as u64)
    );
    assert_eq!(text(&report[0], "kid"), "ref");
    let got = drain(&mut sw, InstanceId(parent.0 + 1), "out");
    assert_eq!(text(&got[0], "said"), "ok", "the ref op's reference binds");

    // A child holding lifecycle may not ask for a sibling's reference.
    let asker = r#"
        local lc = queue.declare("system/lifecycle", { capacity = 4 })
        local ev = queue.declare("system/events", { capacity = 4 })
        local out = queue.declare("out", { capacity = 4, exported = true })
        local idle = queue.declare("idle", { capacity = 1 })
        queue.push(lc, { op = "ref", id = SIBLING })
        local _, e = queue.wait({ ev })
        queue.push(out, { event = e.event, detail = e.detail })
        queue.wait({ idle })
    "#;
    let mut sw = swarm();
    let code = format!(
        r#"
        local lc = queue.declare("system/lifecycle", {{ capacity = 8 }})
        local ev = queue.declare("system/events", {{ capacity = 16 }})
        local idle = queue.declare("idle", {{ capacity = 1 }})
        queue.push(lc, {{ op = "spawn", code = [==[{REPORTER}]==], caps = {{ "queue:*" }} }})
        local _, a = queue.wait({{ ev }})
        local asker = [==[{asker}]==]
        asker = asker:gsub("SIBLING", tostring(a.id))
        queue.push(lc, {{ op = "spawn", code = asker, caps = {{ "lifecycle", "queue:*" }} }})
        queue.wait({{ ev }})
        queue.wait({{ idle }})
        "#
    );
    let parent = root(&mut sw, &code);
    settle(&mut sw, 8);
    let got = drain(&mut sw, InstanceId(parent.0 + 2), "out");
    assert_eq!(got.len(), 1, "{got:?}");
    assert_eq!(text(&got[0], "event"), "denied");
    assert_eq!(text(&got[0], "detail"), "not a descendant");
}

#[test]
fn a_full_inbox_holds_the_message_and_order_survives() {
    let mut sw = swarm();
    // Never reads, so its inbox (64, the core's default) fills.
    let deaf = r#"
        local idle = queue.declare("idle", { capacity = 1 })
        queue.wait({ idle })
    "#;
    // Two endpoint-fuls (64 each): the second cannot all fit, so one is
    // held and the rest wait in the sender's endpoint.
    let code = root_program(
        &[deaf],
        r#"
        local to_kid = endpoint.bind(kids[1].ref, "to_kid")
        local n, refused = 0, 0
        for _ = 1, 2 do
            for _ = 1, 64 do
                n = n + 1
                if not queue.push(to_kid, { n = n }) then refused = refused + 1 end
            end
            queue.wait({ go })
        end
        queue.push(out, { refused = refused })
        "#,
    );
    let parent = root(&mut sw, &code);
    knock(&mut sw, parent, 6);
    let report = drain(&mut sw, parent, "out");
    assert_eq!(
        field(&report[0], "refused").unwrap().as_u64(),
        Some(0),
        "the endpoint had room both times: {report:?}"
    );
    let kid = InstanceId(parent.0 + 1);
    let mut seen = Vec::new();
    for _ in 0..8 {
        for m in drain(&mut sw, kid, "inbox") {
            seen.push(field(&m, "n").unwrap().as_u64().unwrap());
        }
        sw.step();
    }
    assert_eq!(seen, (1..=128).collect::<Vec<_>>(), "none lost, in order");
}

#[test]
fn a_push_wakes_a_parked_child_that_asked_to_be_woken() {
    let mut sw = swarm();
    let sleeper = r#"
        local lc = queue.declare("system/lifecycle", { capacity = 4 })
        local inbox = queue.lookup("inbox")
        local out = queue.declare("out", { capacity = 8, exported = true })
        queue.push(lc, { op = "hibernate", wake_on_message = true })
        local _, m = queue.wait({ inbox })
        queue.push(out, { said = m.said })
        queue.wait({ queue.declare("idle", { capacity = 1 }) })
    "#;
    let code = format!(
        r#"
        local lc = queue.declare("system/lifecycle", {{ capacity = 8 }})
        local ev = queue.declare("system/events", {{ capacity = 16 }})
        local go = queue.declare("go", {{ capacity = 1, exported = true }})
        queue.push(lc, {{ op = "spawn", code = [==[{sleeper}]==],
                          caps = {{ "lifecycle", "queue:*" }}, wake_on_message = true }})
        local _, e = queue.wait({{ ev }})
        local to_kid = endpoint.bind(e.ref, "to_kid")
        queue.wait({{ go }})
        queue.push(to_kid, {{ said = "wake up" }})
        queue.wait({{ queue.declare("idle", {{ capacity = 1 }}) }})
        "#
    );
    let parent = root(&mut sw, &code);
    settle(&mut sw, 6);
    let kid = InstanceId(parent.0 + 1);
    assert!(!sw.resident(kid), "it parked itself");
    let mut buf = Vec::new();
    rmpv::encode::write_value(&mut buf, &rmpv::Value::from(true)).unwrap();
    sw.push(
        parent,
        "go",
        &drt_config::peer::Sender::runtime(drt_config::project::NodePath::root()),
        &buf,
    )
    .unwrap();
    settle(&mut sw, 6);
    assert!(sw.resident(kid), "the push woke it");
    let got = drain(&mut sw, kid, "out");
    assert_eq!(text(&got[0], "said"), "wake up", "{got:?}");
}

#[test]
fn a_reference_minted_under_another_identity_does_not_reach_this_swarm() {
    let mut sw = swarm();
    sw.set_host_identity(Some("here"));
    // The only way to carry one in is from the host, which is trusted: the
    // shape of a snapshot woken in a process other than the one that took
    // it, still holding the reference it was given there.
    let code = root_program(
        &[REPORTER],
        r#"
        local _, m = queue.wait({ queue.lookup("inbox") })
        local there = endpoint.bind(m.peer, "there")
        queue.push(there, { said = "wrong swarm" })
        queue.wait({ go })
        local ok, why = queue.push(there, { said = "wrong swarm" })
        queue.push(out, { second = tostring(ok) .. "/" .. tostring(why) })
        "#,
    );
    let parent = root(&mut sw, &code);
    settle(&mut sw, 4);
    let child = InstanceId(parent.0 + 1);
    let elsewhere =
        drt_swarm::refs::EndpointRef::local(format!("{}/inbox", child.0), Some("elsewhere".into()));
    let msg = rmpv::Value::Map(vec![("peer".into(), elsewhere.to_value())]);
    let mut buf = Vec::new();
    rmpv::encode::write_value(&mut buf, &msg).unwrap();
    let from = drt_config::peer::Sender::runtime(drt_config::project::NodePath::root());
    sw.push(parent, "inbox", &from, &buf).unwrap();
    knock(&mut sw, parent, 2);

    assert!(drain(&mut sw, child, "out").is_empty(), "nothing arrived");
    let report = drain(&mut sw, parent, "out");
    assert_eq!(text(&report[0], "second"), "false/gone", "{report:?}");
}

#[test]
fn a_sender_that_hibernates_binds_again_and_keeps_reaching_its_peer() {
    let mut sw = swarm();
    // Binds, sends, parks itself, and after the wake binds the same
    // reference under the same name: the core adopts the queue it had, and
    // the swarm, whose token table did not ride the snapshot, learns it
    // again.
    let sender = r#"
        local lc = queue.declare("system/lifecycle", { capacity = 4 })
        local inbox = queue.lookup("inbox")
        local _, m = queue.wait({ inbox })
        local peer = endpoint.bind(m.peer, "peer")
        queue.push(peer, { said = "before" })
        queue.push(lc, { op = "hibernate", wake_on_message = true })
        queue.wait({ inbox })
        peer = endpoint.bind(m.peer, "peer")
        queue.push(peer, { said = "after" })
        queue.wait({ queue.declare("idle", { capacity = 1 }) })
    "#;
    let code = format!(
        r#"
        local lc = queue.declare("system/lifecycle", {{ capacity = 8 }})
        local ev = queue.declare("system/events", {{ capacity = 16 }})
        local go = queue.declare("go", {{ capacity = 4, exported = true }})
        local function spawn(code, caps, wake)
            queue.push(lc, {{ op = "spawn", code = code, caps = caps, wake_on_message = wake }})
            local _, e = queue.wait({{ ev }})
            assert(e.event == "spawned", e.event)
            return e
        end
        local s = spawn([==[{sender}]==], {{ "lifecycle", "queue:*" }}, true)
        local r = spawn([==[{REPORTER}]==], {{ "queue:*" }}, false)
        local to_s = endpoint.bind(s.ref, "to_s")
        queue.push(to_s, {{ peer = r.ref }})
        queue.wait({{ go }})
        queue.push(to_s, {{ wake = true }})
        queue.wait({{ queue.declare("idle", {{ capacity = 1 }}) }})
        "#
    );
    let parent = root(&mut sw, &code);
    settle(&mut sw, 6);
    let s = InstanceId(parent.0 + 1);
    assert!(!sw.resident(s), "the sender parked itself");
    knock(&mut sw, parent, 1);
    let got: Vec<String> = drain(&mut sw, InstanceId(parent.0 + 2), "out")
        .iter()
        .map(|m| text(m, "said").to_string())
        .collect();
    assert_eq!(got, ["before", "after"]);
}

#[test]
fn what_a_child_pushes_just_before_it_returns_still_arrives() {
    let mut sw = swarm();
    let last_words = r#"
        local _, m = queue.wait({ queue.lookup("inbox") })
        queue.push(endpoint.bind(m.peer, "peer"), { said = "goodbye" })
    "#;
    let code = root_program(
        &[last_words, REPORTER],
        r#"
        queue.push(endpoint.bind(kids[1].ref, "to_first"), { peer = kids[2].ref })
        "#,
    );
    let parent = root(&mut sw, &code);
    settle(&mut sw, 8);
    let got = drain(&mut sw, InstanceId(parent.0 + 2), "out");
    assert_eq!(got.len(), 1, "{got:?}");
    assert_eq!(text(&got[0], "said"), "goodbye");
}
