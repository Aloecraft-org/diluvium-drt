//! Node-owned sockets at the swarm boundary (`doc/Plan-0.7.0.md` §3): a
//! real instance holds a socket by handle, and what happens to the socket
//! when the swarm ends the instance.
//!
//! # Surface
//!
//! Entry points: the tests. Each builds the shape `drt start` runs —
//! `DiluviumEngine` under a `PumpHost` over `DeployHost` — with the socket
//! connector wired, so a hostcall that waits parks in the pump.
//!
//! Configurable values:
//! - `STEPS` — how far to drive before the program is expected somewhere.
//! - `DILUVIUM_VERIFIED` — the diluvium revision acceptance 13 was last
//!   proved against. Bump it only after re-running that test on the new
//!   pin: the fact it rests on is an implementation note, not a contract.
//!
//! Fan-out: none.
//!
//! The socket count is process-wide, so this file is its own binary,
//! its tests hold `SERIAL` for their whole body so they never overlap, and
//! each takes its baseline after the engine is up.

#![cfg(all(feature = "connector-socket", target_os = "linux"))]

use std::io::{ErrorKind, Read, Write};
use std::net::TcpStream;
use std::sync::{Arc, Mutex};

use drt::start::DeployHost;
use drt_caps::Grant;
use drt_config::Budget;
use drt_connector::{Dispatcher, Registry};
use drt_connector_socket::SocketConnector;
use drt_swarm::engine::diluvium_engine::DiluviumEngine;
use drt_swarm::pump::PumpHost;
use drt_swarm::swarm::Swarm;
use drt_swarm::InstanceId;

const STEPS: usize = 256;
const DILUVIUM_VERIFIED: &str = "d8497b0dd917548c3e2bfa88ed539a6f527ba369";

/// Held by every test for its whole body: two tests counting one
/// process's descriptors at once would read each other's sockets.
static SERIAL: Mutex<()> = Mutex::new(());

/// The sockets this process holds. Sockets only, because that is what
/// every assertion here is about, and because a descriptor another thread
/// opens and closes in passing would otherwise land in a baseline and be
/// missed by every check after it. That happened: glibc reads
/// `/proc/sys/vm/overcommit_memory` once, at the first thread teardown
/// that trims a heap — the previous test's thread exiting just after it
/// released `SERIAL` — and the read overlapped the next test's baseline
/// about once in fifty runs.
fn open_sockets() -> usize {
    std::fs::read_dir("/proc/self/fd")
        .unwrap()
        .filter(|entry| {
            entry
                .as_ref()
                .ok()
                .and_then(|entry| std::fs::read_link(entry.path()).ok())
                .is_some_and(|target| target.to_string_lossy().starts_with("socket:"))
        })
        .count()
}

fn socket_dispatcher() -> Dispatcher {
    let mut reg = Registry::new();
    reg.wire("socket", Arc::new(SocketConnector::new()), None)
        .unwrap();
    Dispatcher::new(reg)
}

fn socket_caps() -> Vec<Grant> {
    [
        "listen", "accept", "read", "write", "close", "transfer", "claim",
    ]
    .iter()
    .map(|verb| Grant::grant(format!("host:socket/{verb}")))
    .collect()
}

type Deployment = Swarm<PumpHost<DeployHost>>;

fn deployment() -> Deployment {
    let engine = Arc::new(DiluviumEngine::new().unwrap());
    Swarm::new(
        engine,
        PumpHost::new(DeployHost::new(), socket_dispatcher()),
    )
}

/// Pop one decoded message from an instance's exported queue, if any.
fn pop(sw: &mut Deployment, id: InstanceId, queue: &str) -> Option<rmpv::Value> {
    let inst = sw.instance_mut(id)?;
    let q = inst.queue(queue)?;
    let raw = inst.pop(q).ok()??;
    Some(rmpv::decode::read_value(&mut raw.as_slice()).unwrap())
}

fn field(v: &rmpv::Value, name: &str) -> Option<rmpv::Value> {
    v.as_map()?
        .iter()
        .find(|(k, _)| k.as_str() == Some(name))
        .map(|(_, v)| v.clone())
}

/// Drive until some child of `root` says anything on its outbox, and
/// return which child and what.
fn drive_until_child_says(sw: &mut Deployment, root: InstanceId) -> (InstanceId, rmpv::Value) {
    for _ in 0..STEPS {
        sw.step();
        let children: Vec<InstanceId> = sw.ids().into_iter().filter(|id| *id != root).collect();
        for child in children {
            if let Some(m) = pop(sw, child, "outbox") {
                return (child, m);
            }
        }
    }
    panic!("no child said anything in {STEPS} steps");
}

/// Drive until some child of `root` announces `subject` on its outbox,
/// and return which.
fn drive_until_child_joins(sw: &mut Deployment, root: InstanceId, subject: &str) -> InstanceId {
    let (child, m) = drive_until_child_says(sw, root);
    let said = field(&m, "subject").and_then(|v| v.as_str().map(String::from));
    assert_eq!(
        said.as_deref(),
        Some(subject),
        "a child joined the wrong subject"
    );
    child
}

/// Drive until the far end has read exactly `want` from a non-blocking
/// client.
fn drive_until_read(sw: &mut Deployment, client: &mut TcpStream, want: &[u8]) {
    let mut got = Vec::new();
    for _ in 0..STEPS {
        sw.step();
        let mut buf = [0u8; 256];
        match client.read(&mut buf) {
            Ok(0) => panic!("the far end closed while {want:?} was expected"),
            Ok(n) => {
                got.extend_from_slice(&buf[..n]);
                if got == want {
                    return;
                }
                assert!(want.starts_with(&got), "read {got:?}, wanted {want:?}");
            }
            Err(e) if e.kind() == ErrorKind::WouldBlock => {}
            Err(e) => panic!("client read: {e}"),
        }
    }
    panic!("nothing more arrived; have {got:?}, wanted {want:?}");
}

/// Drive until a non-blocking client sees end-of-file.
fn drive_until_eof(sw: &mut Deployment, client: &mut TcpStream) {
    for _ in 0..STEPS {
        sw.step();
        let mut buf = [0u8; 16];
        match client.read(&mut buf) {
            Ok(0) => return,
            Ok(n) => panic!("unexpected bytes {:?}", &buf[..n]),
            Err(e) if e.kind() == ErrorKind::WouldBlock => {}
            Err(e) if e.kind() == ErrorKind::ConnectionReset => return,
            Err(e) => panic!("client read: {e}"),
        }
    }
    panic!("the far end never closed in {STEPS} steps");
}

fn connect(addr: &str, subject: &str) -> TcpStream {
    let mut client = TcpStream::connect(addr).unwrap();
    client.write_all(subject.as_bytes()).unwrap();
    client.set_nonblocking(true).unwrap();
    client
}

/// Drive until the instance's `outbox` yields a string, and return it.
fn drive_until_outbox(sw: &mut Deployment, id: InstanceId) -> String {
    for _ in 0..STEPS {
        sw.step();
        assert!(
            sw.ids().contains(&id),
            "the program faulted; its reason is on stderr"
        );
        if let Some(v) = pop(sw, id, "outbox") {
            return v.as_str().expect("a string").to_string();
        }
    }
    panic!("nothing reached the outbox in {STEPS} steps");
}

/// Drive until the instance is parked on a wait set.
fn drive_until_parked(sw: &mut Deployment, id: InstanceId) {
    for _ in 0..STEPS {
        sw.step();
        assert!(
            sw.ids().contains(&id),
            "the program faulted; its reason is on stderr"
        );
        if sw
            .instance_mut(id)
            .is_some_and(|inst| inst.current_wait().is_some())
        {
            return;
        }
    }
    panic!("the program did not park in {STEPS} steps");
}

/// Acceptance 3 and 5, through the swarm: a killed node's sockets close,
/// the descriptor count returns to its starting value, the far end sees
/// the connection go, and the host is told what was lost, attributed.
#[test]
fn a_killed_node_has_its_sockets_closed_and_the_loss_is_attributed() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let mut sw = deployment();
    let before = open_sockets();

    // Listen, tell the test where, accept one connection, park.
    const PROGRAM: &str = "\
        local inbox = queue.lookup('inbox')\n\
        local outbox = queue.lookup('outbox')\n\
        local l, s, d = host.try('socket/listen', {addr = '127.0.0.1:0'})\n\
        assert(s == 'ok', 'listen: ' .. tostring(s) .. ' ' .. tostring(d))\n\
        queue.push(outbox, l.addr)\n\
        local c, s2, d2 = host.try('socket/accept', {handle = l.handle})\n\
        assert(s2 == 'ok', 'accept: ' .. tostring(s2) .. ' ' .. tostring(d2))\n\
        queue.wait({inbox})\n";
    let root = sw
        .root(PROGRAM.as_bytes(), socket_caps(), Budget::default())
        .unwrap();

    let addr = drive_until_outbox(&mut sw, root);
    let mut client = TcpStream::connect(addr.as_str()).unwrap();
    drive_until_parked(&mut sw, root);
    // The listener, the accepted side, and this test's own client.
    assert_eq!(open_sockets(), before + 3);

    sw.kill(root).unwrap();
    assert_eq!(sw.alive(), 0);

    let lost = sw.host_mut().take_lost();
    assert_eq!(lost.len(), 1, "{lost:?}");
    assert_eq!(lost[0].0 .0, root.0, "attributed to the node that held it");
    assert_eq!(
        lost[0].1,
        format!("a connection with {}, cut", client.local_addr().unwrap())
    );

    assert_eq!(open_sockets(), before + 1, "only the client remains");
    let mut probe = [0u8; 1];
    assert_eq!(
        client.read(&mut probe).unwrap(),
        0,
        "the far end sees the connection closed"
    );
}

/// The per-connection child (§3.2). It claims whatever was addressed to
/// it, reads the first message as its subject and says so on its outbox —
/// joining by message, which is the only channel between nodes — then
/// echoes until the far end closes. `trap` is the one word it faults on.
const CHILD: &str = "\
    local outbox = queue.lookup('outbox')\n\
    local c, s, d = host.try('socket/claim', {})\n\
    assert(s == 'ok', 'claim: ' .. tostring(s) .. ' ' .. tostring(d))\n\
    local first, s2, d2 = host.try('socket/read', {handle = c.handle})\n\
    assert(s2 == 'ok', 'read: ' .. tostring(s2) .. ' ' .. tostring(d2))\n\
    queue.push(outbox, {subject = first.data, peer = c.peer})\n\
    while true do\n\
      local r, s3, d3 = host.try('socket/read', {handle = c.handle})\n\
      assert(s3 == 'ok', 'read: ' .. tostring(s3) .. ' ' .. tostring(d3))\n\
      if r.eof then break end\n\
      if r.data == 'trap' then error('boom') end\n\
      local _, s4, d4 = host.try('socket/write', {handle = c.handle, data = r.data})\n\
      assert(s4 == 'ok', 'write: ' .. tostring(s4) .. ' ' .. tostring(d4))\n\
    end\n\
    host.try('socket/close', {handle = c.handle})\n";

/// One acceptor per root (§3.2): it binds once, and for each of three
/// connections accepts, spawns a child with only the verbs a served
/// connection needs, and transfers the connection to it.
fn acceptor() -> String {
    format!(
        "\
        local inbox = queue.lookup('inbox')\n\
        local outbox = queue.lookup('outbox')\n\
        local l, s, d = host.try('socket/listen', {{addr = '127.0.0.1:0'}})\n\
        assert(s == 'ok', 'listen: ' .. tostring(s) .. ' ' .. tostring(d))\n\
        queue.push(outbox, l.addr)\n\
        for i = 1, 3 do\n\
          local c, s2, d2 = host.try('socket/accept', {{handle = l.handle}})\n\
          assert(s2 == 'ok', 'accept: ' .. tostring(s2) .. ' ' .. tostring(d2))\n\
          local child = host.spawn{{code = [==[{CHILD}]==],\n\
            caps = {{'host:socket/claim', 'host:socket/read', 'host:socket/write', 'host:socket/close'}}}}\n\
          local _, s3, d3 = host.try('socket/transfer', {{handle = c.handle, to = child.id}})\n\
          assert(s3 == 'ok', 'transfer: ' .. tostring(s3) .. ' ' .. tostring(d3))\n\
        end\n\
        queue.wait({{inbox}})\n"
    )
}

/// Acceptance 6, 7 and 14 in one deployment. 14: one listener for the
/// whole run, and each child joins its subject by message. 6: a child
/// serves its connection to completion and exits clean, losing nothing.
/// 7: a child that traps mid-connection is killed, its socket closes, the
/// loss is attributed to it, and its sibling and its parent carry on.
#[test]
fn an_acceptor_spawns_a_child_per_connection_and_each_serves_its_own() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let mut sw = deployment();
    let before = open_sockets();
    let mut caps = socket_caps();
    caps.push(Grant::grant("lifecycle"));
    let root = sw
        .root(acceptor().as_bytes(), caps, Budget::default())
        .unwrap();
    let addr = drive_until_outbox(&mut sw, root);

    // 14: two subjects, two children, still one listener.
    let mut a = connect(&addr, "room:1");
    let child_a = drive_until_child_joins(&mut sw, root, "room:1");
    let mut b = connect(&addr, "room:2");
    let child_b = drive_until_child_joins(&mut sw, root, "room:2");
    assert_ne!(child_a, child_b);
    assert_eq!(
        open_sockets(),
        before + 1 + 2 + 2,
        "one listener, two accepted sides, two clients: no listener per subject"
    );

    a.write_all(b"hello").unwrap();
    drive_until_read(&mut sw, &mut a, b"hello");

    // 7: b's child traps. Its socket closes, b sees it, the loss is b's
    // child's, and a's child and the parent are untouched.
    b.write_all(b"trap").unwrap();
    drive_until_eof(&mut sw, &mut b);
    let lost = sw.host_mut().take_lost();
    assert_eq!(lost.len(), 1, "{lost:?}");
    assert_eq!(
        lost[0].0 .0, child_b.0,
        "attributed to the child that held it"
    );
    assert_eq!(
        lost[0].1,
        format!("a connection with {}, cut", b.local_addr().unwrap())
    );
    drop(b);
    assert!(sw.ids().contains(&child_a), "the sibling lives");
    assert!(sw.ids().contains(&root), "and so does the parent");
    a.write_all(b"again").unwrap();
    drive_until_read(&mut sw, &mut a, b"again");

    // The parent is still accepting: a third connection gets its child.
    let c = connect(&addr, "room:3");
    let child_c = drive_until_child_joins(&mut sw, root, "room:3");
    assert_ne!(child_c, child_a);
    assert_eq!(
        open_sockets(),
        before + 1 + 2 + 2,
        "b's pair is gone, c's is here"
    );

    // 6: a's child serves to completion. The client hangs up, the child
    // reads end-of-file, closes, and exits — nothing to release, nothing lost.
    drop(a);
    let alive = sw.alive();
    for _ in 0..STEPS {
        sw.step();
        if sw.alive() < alive {
            break;
        }
    }
    assert!(!sw.ids().contains(&child_a), "a's child exited on its own");
    assert!(
        sw.host_mut().take_lost().is_empty(),
        "a clean end loses nothing"
    );
    assert_eq!(open_sockets(), before + 1 + 2, "the listener and c's pair");

    drop(c);
    sw.kill(root).unwrap();
    assert_eq!(sw.alive(), 0);
    assert_eq!(open_sockets(), before);
}

/// A service node (§3.3): it claims its first connection as vital and a
/// second plainly, says what it holds, and parks — the state a parked
/// service is in when the far end finally hangs up.
const SERVICE: &str = "\
    local inbox = queue.lookup('inbox')\n\
    local outbox = queue.lookup('outbox')\n\
    local v, s, d = host.try('socket/claim', {vital = true})\n\
    assert(s == 'ok', 'claim: ' .. tostring(s) .. ' ' .. tostring(d))\n\
    local o, s2, d2 = host.try('socket/claim', {})\n\
    assert(s2 == 'ok', 'claim: ' .. tostring(s2) .. ' ' .. tostring(d2))\n\
    queue.push(outbox, 'holding ' .. v.peer .. ' and ' .. o.peer)\n\
    queue.wait({inbox})\n";

/// A supervisor: spawns the service first (its vital claim waits), then
/// accepts two connections and hands both over, then forwards every
/// lifecycle event it hears to its outbox as `<event> <id>`.
fn supervisor() -> String {
    format!(
        "\
        local outbox = queue.lookup('outbox')\n\
        local l, s, d = host.try('socket/listen', {{addr = '127.0.0.1:0'}})\n\
        assert(s == 'ok', 'listen: ' .. tostring(s) .. ' ' .. tostring(d))\n\
        queue.push(outbox, l.addr)\n\
        local child = host.spawn{{code = [==[{SERVICE}]==],\n\
          caps = {{'host:socket/claim', 'host:socket/read', 'host:socket/write', 'host:socket/close'}}}}\n\
        for i = 1, 2 do\n\
          local c, s2, d2 = host.try('socket/accept', {{handle = l.handle}})\n\
          assert(s2 == 'ok', 'accept: ' .. tostring(s2) .. ' ' .. tostring(d2))\n\
          local _, s3, d3 = host.try('socket/transfer', {{handle = c.handle, to = child.id}})\n\
          assert(s3 == 'ok', 'transfer: ' .. tostring(s3) .. ' ' .. tostring(d3))\n\
        end\n\
        while true do\n\
          local ev = host.events(60000)\n\
          if ev then queue.push(outbox, ev.event .. ' ' .. ev.id) end\n\
        end\n"
    )
}

/// Acceptance 11 and 23. The owner is **hibernating** when the far end
/// of its vital connection hangs up: it is ended without a line of its
/// own logic running and with no guest resident, its other connection is
/// released with it and the loss attributed, and its parent hears
/// `ended` — its own variant, not `faulted`, not `exited`.
#[test]
fn a_vital_handle_ends_its_hibernating_owner_and_the_parent_hears_ended() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let mut sw = deployment();
    let before = open_sockets();
    let mut caps = socket_caps();
    caps.push(Grant::grant("lifecycle"));
    let root = sw
        .root(supervisor().as_bytes(), caps, Budget::default())
        .unwrap();
    let addr = drive_until_outbox(&mut sw, root);

    // First in is the vital one: offers are claimed oldest first.
    let vital = TcpStream::connect(addr.as_str()).unwrap();
    let mut other = TcpStream::connect(addr.as_str()).unwrap();
    other.set_nonblocking(true).unwrap();
    let (service, said) = drive_until_child_says(&mut sw, root);
    assert_eq!(
        said.as_str().unwrap(),
        format!(
            "holding {} and {}",
            vital.local_addr().unwrap(),
            other.local_addr().unwrap()
        )
    );
    assert_eq!(open_sockets(), before + 1 + 2 + 2);

    drive_until_parked(&mut sw, service);
    sw.hibernate(service).unwrap();
    assert!(!sw.resident(service), "a parked service, hours into quiet");

    // The far end hangs up.
    drop(vital);
    for _ in 0..STEPS {
        sw.step();
        if !sw.ids().contains(&service) {
            break;
        }
    }
    assert!(
        !sw.ids().contains(&service),
        "ended where it lay, with no guest rebuilt to notice"
    );

    // Its other handle went with it, attributed.
    drive_until_eof(&mut sw, &mut other);
    let lost = sw.host_mut().take_lost();
    assert_eq!(lost.len(), 1, "{lost:?}");
    assert_eq!(lost[0].0 .0, service.0);
    assert_eq!(
        lost[0].1,
        format!("a connection with {}, cut", other.local_addr().unwrap())
    );

    // The supervisor heard the variant.
    let heard = drive_until_outbox(&mut sw, root);
    assert_eq!(heard, format!("ended {}", service.0));

    drop(other);
    assert_eq!(
        open_sockets(),
        before + 1,
        "the listener, and nothing of the service's"
    );
}

/// A service that parks between messages (§3.4): it claims with `wake`,
/// keeps the `inbox` handle it looked up before its first park and the
/// socket handle it was given, and after every wake takes the notice off
/// the one (`queue.wait` returns the message) and reads with the other —
/// no re-`lookup`, no re-resolve.
const PARKED_SERVICE: &str = "\
    local inbox = queue.lookup('inbox')\n\
    local outbox = queue.lookup('outbox')\n\
    local c, s, d = host.try('socket/claim', {wake = true})\n\
    assert(s == 'ok', 'claim: ' .. tostring(s) .. ' ' .. tostring(d))\n\
    queue.push(outbox, 'holding ' .. c.handle)\n\
    while true do\n\
      local _, n = queue.wait({inbox})\n\
      assert(n.handle == c.handle and n.ready == 'read', 'the notice names the handle')\n\
      local r, s2, d2 = host.try('socket/read', {handle = c.handle})\n\
      assert(s2 == 'ok', 'read: ' .. tostring(s2) .. ' ' .. tostring(d2))\n\
      if r.eof then break end\n\
      local _, s3, d3 = host.try('socket/write', {handle = c.handle, data = r.data})\n\
      assert(s3 == 'ok', 'write: ' .. tostring(s3) .. ' ' .. tostring(d3))\n\
    end\n\
    host.try('socket/close', {handle = c.handle})\n";

/// Spawns the parked service with `wake_on_message` — the swarm's own
/// wake path, which the readiness push rides — accepts one connection,
/// hands it over, and parks for good.
fn parking_supervisor() -> String {
    format!(
        "\
        local inbox = queue.lookup('inbox')\n\
        local outbox = queue.lookup('outbox')\n\
        local l, s, d = host.try('socket/listen', {{addr = '127.0.0.1:0'}})\n\
        assert(s == 'ok', 'listen: ' .. tostring(s) .. ' ' .. tostring(d))\n\
        queue.push(outbox, l.addr)\n\
        local child = host.spawn{{code = [==[{PARKED_SERVICE}]==], wake_on_message = true,\n\
          caps = {{'host:socket/claim', 'host:socket/read', 'host:socket/write', 'host:socket/close'}}}}\n\
        local c, s2, d2 = host.try('socket/accept', {{handle = l.handle}})\n\
        assert(s2 == 'ok', 'accept: ' .. tostring(s2) .. ' ' .. tostring(d2))\n\
        local _, s3, d3 = host.try('socket/transfer', {{handle = c.handle, to = child.id}})\n\
        assert(s3 == 'ok', 'transfer: ' .. tostring(s3) .. ' ' .. tostring(d3))\n\
        queue.wait({{inbox}})\n"
    )
}

/// Acceptance 4 and 13. A hibernated service keeps its socket, a write
/// from the far end wakes it, and after waking it uses both the queue
/// handle and the socket handle it held before — twice, so the second
/// wake proves the readiness re-armed. Pinned to the diluvium revision
/// it was proved against, because §3.5 rests on an implementation note.
#[test]
fn a_hibernated_service_is_woken_by_bytes_and_keeps_both_kinds_of_handle() {
    assert_eq!(
        env!("DRT_DILUVIUM_REV"),
        DILUVIUM_VERIFIED,
        "the diluvium pin moved: re-prove §3.5 (handles survive a whole-instance \
         snapshot) on the new revision, then bump DILUVIUM_VERIFIED"
    );
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let mut sw = deployment();
    let before = open_sockets();
    let mut caps = socket_caps();
    caps.push(Grant::grant("lifecycle"));
    let root = sw
        .root(parking_supervisor().as_bytes(), caps, Budget::default())
        .unwrap();
    let addr = drive_until_outbox(&mut sw, root);

    let mut client = TcpStream::connect(addr.as_str()).unwrap();
    client.set_nonblocking(true).unwrap();
    let (service, said) = drive_until_child_says(&mut sw, root);
    let said = said.as_str().unwrap().to_string();
    let socket_number: u64 = said
        .strip_prefix("holding ")
        .and_then(|n| n.parse().ok())
        .expect("the service names its handle");
    assert_eq!(open_sockets(), before + 1 + 2);

    for round in [b"first" as &[u8], b"second"] {
        drive_until_parked(&mut sw, service);
        sw.hibernate(service).unwrap();
        assert!(!sw.resident(service), "parked and swapped out");

        client.write_all(round).unwrap();
        drive_until_read(&mut sw, &mut client, round);
        assert!(
            sw.resident(service),
            "the bytes woke it, and it served them with the handles it had"
        );
    }
    let _ = socket_number;

    // To completion: the far end hangs up, the wake carries end-of-file,
    // the service closes and exits, and nothing is lost.
    drop(client);
    for _ in 0..STEPS {
        sw.step();
        if !sw.ids().contains(&service) {
            break;
        }
    }
    assert!(!sw.ids().contains(&service), "exited on end-of-file");
    assert!(sw.host_mut().take_lost().is_empty());
    assert_eq!(open_sockets(), before + 1, "the listener, and nothing else");
}
