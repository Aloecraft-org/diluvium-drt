//! Stream sockets at the connector boundary (`doc/Plan-0.7.0.md` §3.1):
//! the five verbs round-trip against a real peer, a handle is one node's,
//! a release closes what its owner held and says what that cut, and the
//! scope bounds what may be bound.
//!
//! Every call is driven the way the pump drives it: polled with a no-op
//! waker until ready. A call that never becomes ready fails the test by
//! poll count rather than hanging it.

use std::future::Future;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::Arc;
use std::task::{Context, Poll, Waker};

use drt_caps::Scope;
use drt_connector::{Asker, Caller, Connector, Registry};
use drt_connector_socket::{SocketConnector, READ_MAX};

/// Polls before a pending call is declared stuck. Loopback readiness is
/// immediate; this is a bound on diagnosis, not a wait.
const POLL_CAP: usize = 100_000;

fn drive<T>(fut: impl Future<Output = T>) -> T {
    let mut fut = std::pin::pin!(fut);
    let mut polls = 0;
    loop {
        match fut.as_mut().poll(&mut Context::from_waker(Waker::noop())) {
            Poll::Ready(v) => return v,
            Poll::Pending => {
                polls += 1;
                assert!(polls < POLL_CAP, "the call never became ready");
                std::thread::yield_now();
            }
        }
    }
}

fn args(entries: Vec<(&str, rmpv::Value)>) -> rmpv::Value {
    rmpv::Value::Map(
        entries
            .into_iter()
            .map(|(k, v)| (rmpv::Value::from(k), v))
            .collect(),
    )
}

fn call(
    c: &SocketConnector,
    caller: Caller,
    scope: Option<&Scope>,
    name: &str,
    a: rmpv::Value,
) -> Result<rmpv::Value, String> {
    let asker = Asker {
        caller,
        grants: &[],
    };
    drive(c.call_as(&asker, name, Some(a), scope)).map_err(|e| e.to_string())
}

fn field(v: &rmpv::Value, name: &str) -> rmpv::Value {
    v.as_map()
        .unwrap()
        .iter()
        .find(|(k, _)| k.as_str() == Some(name))
        .map(|(_, v)| v.clone())
        .unwrap_or_else(|| panic!("no field {name} in {v}"))
}

fn handle(v: &rmpv::Value, name: &str) -> u64 {
    field(v, name).as_u64().unwrap()
}

fn listen(c: &SocketConnector, caller: Caller) -> (u64, String) {
    let l = call(
        c,
        caller,
        None,
        "socket/listen",
        args(vec![("addr", "127.0.0.1:0".into())]),
    )
    .unwrap();
    (
        handle(&l, "handle"),
        field(&l, "addr").as_str().unwrap().to_string(),
    )
}

fn accept(c: &SocketConnector, caller: Caller, listener: u64) -> (u64, String) {
    let a = call(
        c,
        caller,
        None,
        "socket/accept",
        args(vec![("handle", listener.into())]),
    )
    .unwrap();
    (
        handle(&a, "handle"),
        field(&a, "peer").as_str().unwrap().to_string(),
    )
}

fn read(c: &SocketConnector, caller: Caller, conn: u64) -> (Vec<u8>, bool) {
    let r = call(
        c,
        caller,
        None,
        "socket/read",
        args(vec![("handle", conn.into())]),
    )
    .unwrap();
    let data = match field(&r, "data") {
        rmpv::Value::Binary(b) => b,
        rmpv::Value::String(s) => s.into_bytes(),
        other => panic!("data is {other}"),
    };
    (data, field(&r, "eof").as_bool().unwrap())
}

/// A listener the root still holds answers `accept` with "not yet" rather
/// than "no such handle": one poll is enough to tell the two apart.
fn accept_probe(c: &SocketConnector, listener: u64) {
    let asker = Asker {
        caller: Caller::Root,
        grants: &[],
    };
    let fut = c.call_as(
        &asker,
        "socket/accept",
        Some(args(vec![("handle", listener.into())])),
        None,
    );
    let mut fut = std::pin::pin!(fut);
    match fut.as_mut().poll(&mut Context::from_waker(Waker::noop())) {
        Poll::Pending => {}
        Poll::Ready(r) => panic!("expected a pending accept, got {r:?}"),
    }
}

/// §3.1: the five verbs, against a real peer, to completion.
#[test]
fn a_node_listens_accepts_reads_writes_and_closes() {
    let c = SocketConnector::new();
    let node = Caller::Node(1);
    let (l, addr) = listen(&c, node);
    let mut client = TcpStream::connect(&addr).unwrap();

    let (conn, peer) = accept(&c, node, l);
    assert_eq!(peer, client.local_addr().unwrap().to_string());

    client.write_all(b"hello").unwrap();
    let (data, eof) = read(&c, node, conn);
    assert_eq!(data, b"hello");
    assert!(!eof);

    let w = call(
        &c,
        node,
        None,
        "socket/write",
        args(vec![("handle", conn.into()), ("data", "world".into())]),
    )
    .unwrap();
    assert_eq!(handle(&w, "written"), 5);
    let mut back = [0u8; 5];
    client.read_exact(&mut back).unwrap();
    assert_eq!(&back, b"world");

    drop(client);
    let (data, eof) = read(&c, node, conn);
    assert!(data.is_empty());
    assert!(eof, "the far end closed, and the read says so");

    call(
        &c,
        node,
        None,
        "socket/close",
        args(vec![("handle", conn.into())]),
    )
    .unwrap();
    call(
        &c,
        node,
        None,
        "socket/close",
        args(vec![("handle", l.into())]),
    )
    .unwrap();
    let gone = call(
        &c,
        node,
        None,
        "socket/close",
        args(vec![("handle", l.into())]),
    )
    .unwrap_err();
    assert_eq!(gone, "no such handle");
}

/// Acceptance 2, at the socket: another node's handle does not exist from
/// here, and the sentence is the one a never-issued number gets.
#[test]
fn a_handle_is_invisible_to_a_node_that_does_not_own_it() {
    let c = SocketConnector::new();
    let (l, addr) = listen(&c, Caller::Node(1));
    let _client = TcpStream::connect(&addr).unwrap();
    let (conn, _) = accept(&c, Caller::Node(1), l);

    let other = Caller::Node(2);
    let said = call(
        &c,
        other,
        None,
        "socket/read",
        args(vec![("handle", conn.into())]),
    )
    .unwrap_err();
    assert_eq!(said, "no such handle");
    let never = call(
        &c,
        other,
        None,
        "socket/read",
        args(vec![("handle", 999u64.into())]),
    )
    .unwrap_err();
    assert_eq!(never, said);
    assert!(!said.contains("instance") && !said.contains('1'), "{said}");
}

/// Acceptance 5, the connector's half: a release reports the connection
/// whose peer was still there, and not the one already at end-of-file,
/// and not the listener. A sibling's sockets are untouched.
#[test]
fn a_release_reports_what_it_cut_and_leaves_a_sibling_alone() {
    let c = SocketConnector::new();
    let node = Caller::Node(1);
    let sibling = Caller::Node(2);
    let (l, addr) = listen(&c, node);

    let finished = TcpStream::connect(&addr).unwrap();
    let (fin_conn, _) = accept(&c, node, l);
    drop(finished);
    assert!(read(&c, node, fin_conn).1, "read to end-of-file");

    let live = TcpStream::connect(&addr).unwrap();
    let (_live_conn, live_peer) = accept(&c, node, l);

    let (sl, saddr) = listen(&c, sibling);
    let _sclient = TcpStream::connect(&saddr).unwrap();
    let (sconn, _) = accept(&c, sibling, sl);

    let lost = c.release(&node);
    assert_eq!(lost, vec![format!("a connection with {live_peer}, cut")]);

    let mut probe = [0u8; 1];
    let mut live = live;
    assert_eq!(
        live.read(&mut probe).unwrap(),
        0,
        "the far end of the cut connection sees it closed"
    );

    // The sibling still holds its connection: a write lands, and its own
    // release is what cuts it.
    call(
        &c,
        sibling,
        None,
        "socket/write",
        args(vec![("handle", sconn.into()), ("data", "x".into())]),
    )
    .unwrap();
    let lost = c.release(&sibling);
    assert_eq!(
        lost.len(),
        1,
        "the sibling's live connection, cut now: {lost:?}"
    );
}

/// The scope's `allow` bounds `listen` by name, before anything is bound,
/// and a malformed entry is refused at wiring.
#[test]
fn the_scope_bounds_what_may_be_bound() {
    let c = SocketConnector::new();
    let node = Caller::Node(1);
    let allow = |entries: &[&str]| {
        Scope(rmpv::Value::Map(vec![(
            "allow".into(),
            rmpv::Value::Array(entries.iter().map(|e| rmpv::Value::from(*e)).collect()),
        )]))
    };

    // Port 0 in the list is any port on that address.
    let ok = allow(&["127.0.0.1:0"]);
    call(
        &c,
        node,
        Some(&ok),
        "socket/listen",
        args(vec![("addr", "127.0.0.1:0".into())]),
    )
    .unwrap();

    let err = call(
        &c,
        node,
        Some(&ok),
        "socket/listen",
        args(vec![("addr", "[::1]:0".into())]),
    )
    .unwrap_err();
    assert!(err.contains("outside"), "{err}");
    assert!(err.contains("127.0.0.1:0"), "names the list: {err}");

    // A port the list does not name is refused by the port.
    let one_port = allow(&["127.0.0.1:65000"]);
    let err = call(
        &c,
        node,
        Some(&one_port),
        "socket/listen",
        args(vec![("addr", "127.0.0.1:0".into())]),
    )
    .unwrap_err();
    assert!(err.contains("outside"), "{err}");

    // A name is not an address, and the wiring says so at boot.
    let mut reg = Registry::new();
    let err = reg
        .wire(
            "socket",
            Arc::new(SocketConnector::new()),
            Some(allow(&["localhost:80"])),
        )
        .unwrap_err();
    assert!(err.detail.contains("localhost:80"), "{}", err.detail);
    reg.wire("socket", Arc::new(SocketConnector::new()), Some(ok))
        .expect("a literal wires");
    reg.wire("socket", Arc::new(SocketConnector::new()), None)
        .expect("no scope is any address");
}

/// The caller-blind path is the root: what it holds survives any node's
/// release and is drained at `finish`, which reports a connection it cut.
#[test]
fn a_root_held_socket_lives_to_finish() {
    let c = SocketConnector::new();
    let l = pollster::block_on(c.call(
        "socket/listen",
        Some(args(vec![("addr", "127.0.0.1:0".into())])),
        None,
    ))
    .unwrap();
    let addr = field(&l, "addr").as_str().unwrap().to_string();
    let lh = handle(&l, "handle");
    let client = TcpStream::connect(&addr).unwrap();
    let (_conn, peer) = accept(&c, Caller::Root, lh);

    assert!(c.release(&Caller::Node(1)).is_empty());
    // Still the root's after a node's release.
    accept_probe(&c, lh);
    let lost = c.finish();
    assert_eq!(lost, vec![format!("a connection with {peer}, cut")]);
    drop(client);
}

#[test]
fn a_wrong_kind_of_handle_is_refused_by_name() {
    let c = SocketConnector::new();
    let node = Caller::Node(1);
    let (l, addr) = listen(&c, node);
    let _client = TcpStream::connect(&addr).unwrap();
    let (conn, _) = accept(&c, node, l);

    let err = call(
        &c,
        node,
        None,
        "socket/accept",
        args(vec![("handle", conn.into())]),
    )
    .unwrap_err();
    assert!(err.contains("not a listener"), "{err}");
    let err = call(
        &c,
        node,
        None,
        "socket/write",
        args(vec![("handle", l.into()), ("data", "x".into())]),
    )
    .unwrap_err();
    assert!(err.contains("not a connection"), "{err}");
}

/// A read asks for less than the cap, never more, and gets at most what it
/// asked for.
#[test]
fn a_read_asks_for_less_never_more() {
    let c = SocketConnector::new();
    let node = Caller::Node(1);
    let (l, addr) = listen(&c, node);
    let mut client = TcpStream::connect(&addr).unwrap();
    let (conn, _) = accept(&c, node, l);
    client.write_all(b"abcdef").unwrap();

    let r = call(
        &c,
        node,
        None,
        "socket/read",
        args(vec![("handle", conn.into()), ("max", 2u64.into())]),
    )
    .unwrap();
    assert_eq!(field(&r, "data"), rmpv::Value::Binary(b"ab".to_vec()));
    let (rest, _) = read(&c, node, conn);
    assert_eq!(rest, b"cdef");

    let err = call(
        &c,
        node,
        None,
        "socket/read",
        args(vec![
            ("handle", conn.into()),
            ("max", (READ_MAX as u64 + 1).into()),
        ]),
    )
    .unwrap_err();
    assert!(err.contains("never more"), "{err}");
}

/// Issue #33: a socket a program is handed leaves Nagle off, so a pair of
/// small writes with a gap between them is not held for the first one's
/// delayed ACK -- 40 ms on Linux. The far end answers one byte once it
/// has read two; the node writes one byte, waits, writes the other, and
/// reads the answer. The median of nine rounds is the measurement.
#[test]
fn a_pair_of_small_writes_is_not_held_for_an_ack() {
    let c = SocketConnector::new();
    let node = Caller::Node(1);
    let (l, addr) = listen(&c, node);
    let mut client = TcpStream::connect(&addr).unwrap();
    client.set_nodelay(true).unwrap();
    let (conn, _) = accept(&c, node, l);
    let far = std::thread::spawn(move || {
        let mut two = [0u8; 2];
        for _ in 0..9 {
            client.read_exact(&mut two).unwrap();
            client.write_all(b"!").unwrap();
        }
    });
    let write = |byte: &str| {
        call(
            &c,
            node,
            None,
            "socket/write",
            args(vec![("handle", conn.into()), ("data", byte.into())]),
        )
        .unwrap();
    };
    let mut rounds = Vec::new();
    for _ in 0..9 {
        let started = std::time::Instant::now();
        write("a");
        std::thread::sleep(std::time::Duration::from_millis(5));
        write("b");
        let (data, _) = read(&c, node, conn);
        assert_eq!(data, b"!");
        rounds.push(started.elapsed());
    }
    far.join().unwrap();
    rounds.sort();
    let median = rounds[4];
    assert!(
        median < std::time::Duration::from_millis(30),
        "a two-write pair took {median:?} on a socket the node was handed: a delayed ACK is being paid"
    );
}
