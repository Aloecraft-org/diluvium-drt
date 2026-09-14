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
//!
//! Fan-out: none.
//!
//! The descriptor count is process-wide, so this file is its own binary
//! and every test in it takes its baseline after the engine is up.

#![cfg(all(feature = "connector-socket", target_os = "linux"))]

use std::io::Read;
use std::net::TcpStream;
use std::sync::Arc;

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

fn open_fds() -> usize {
    std::fs::read_dir("/proc/self/fd").unwrap().count()
}

fn socket_dispatcher() -> Dispatcher {
    let mut reg = Registry::new();
    reg.wire("socket", Arc::new(SocketConnector::new()), None)
        .unwrap();
    Dispatcher::new(reg)
}

fn socket_caps() -> Vec<Grant> {
    ["listen", "accept", "read", "write", "close"]
        .iter()
        .map(|verb| Grant::grant(format!("host:socket/{verb}")))
        .collect()
}

/// Drive until the instance's `outbox` yields a string, and return it.
fn drive_until_outbox(sw: &mut Swarm<PumpHost<DeployHost>>, id: InstanceId) -> String {
    for _ in 0..STEPS {
        sw.step();
        assert_eq!(
            sw.alive(),
            1,
            "the program faulted; its reason is on stderr"
        );
        let inst = sw.instance_mut(id).expect("resident");
        let Some(outbox) = inst.queue("outbox") else {
            continue;
        };
        if let Ok(Some(raw)) = inst.pop(outbox) {
            let v = rmpv::decode::read_value(&mut raw.as_slice()).unwrap();
            return v.as_str().expect("a string").to_string();
        }
    }
    panic!("nothing reached the outbox in {STEPS} steps");
}

/// Drive until the instance is parked on a wait set.
fn drive_until_parked(sw: &mut Swarm<PumpHost<DeployHost>>, id: InstanceId) {
    for _ in 0..STEPS {
        sw.step();
        assert_eq!(
            sw.alive(),
            1,
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
    let engine = Arc::new(DiluviumEngine::new().unwrap());
    let mut sw = Swarm::new(
        engine,
        PumpHost::new(DeployHost::new(), socket_dispatcher()),
    );
    let before = open_fds();

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
    assert_eq!(open_fds(), before + 3);

    sw.kill(root).unwrap();
    assert_eq!(sw.alive(), 0);

    let lost = sw.host_mut().take_lost();
    assert_eq!(lost.len(), 1, "{lost:?}");
    assert_eq!(lost[0].0 .0, root.0, "attributed to the node that held it");
    assert_eq!(
        lost[0].1,
        format!("a connection with {}, cut", client.local_addr().unwrap())
    );

    assert_eq!(open_fds(), before + 1, "only the client remains");
    let mut probe = [0u8; 1];
    assert_eq!(
        client.read(&mut probe).unwrap(),
        0,
        "the far end sees the connection closed"
    );
}
