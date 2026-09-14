//! Vital handles at the connector boundary (`doc/Plan-0.7.0.md` §3.3):
//! the connector notices a vital connection ending and names its owner
//! once; a plain connection ending is nobody's business; the flag is the
//! claimant's to set; a listener ends only by its owner's hand.

use std::future::Future;
use std::net::TcpStream;
use std::task::{Context, Poll, Waker};

use drt_connector::{Asker, Caller, Connector};
use drt_connector_socket::SocketConnector;

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
    name: &str,
    a: rmpv::Value,
) -> Result<rmpv::Value, String> {
    let asker = Asker {
        caller,
        grants: &[],
    };
    drive(c.call_as(&asker, name, Some(a), None)).map_err(|e| e.to_string())
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

fn listen(c: &SocketConnector, who: Caller, vital: bool) -> (u64, String) {
    let l = call(
        c,
        who,
        "socket/listen",
        args(vec![
            ("addr", "127.0.0.1:0".into()),
            ("vital", vital.into()),
        ]),
    )
    .unwrap();
    (
        handle(&l, "handle"),
        field(&l, "addr").as_str().unwrap().to_string(),
    )
}

fn accept(c: &SocketConnector, who: Caller, l: u64, vital: bool) -> (u64, String) {
    let a = call(
        c,
        who,
        "socket/accept",
        args(vec![("handle", l.into()), ("vital", vital.into())]),
    )
    .unwrap();
    (
        handle(&a, "handle"),
        field(&a, "peer").as_str().unwrap().to_string(),
    )
}

/// The far end hangs up on a vital connection: its owner is reported,
/// once, and the entry is gone with the report.
#[test]
fn a_vital_connection_that_ends_names_its_owner_once() {
    let c = SocketConnector::new();
    let node = Caller::Node(1);
    let (l, addr) = listen(&c, node, false);
    let client = TcpStream::connect(&addr).unwrap();
    let (conn, peer) = accept(&c, node, l, true);

    assert!(c.ended().is_empty(), "alive: nothing has ended");
    drop(client);
    assert_eq!(
        c.ended(),
        vec![(node, format!("its vital connection with {peer} ended"))]
    );
    assert!(c.ended().is_empty(), "reported once");
    let err = call(&c, node, "socket/read", args(vec![("handle", conn.into())])).unwrap_err();
    assert_eq!(err, "no such handle", "the entry went with the report");
    assert!(c.release(&node).is_empty(), "only the listener was left");
}

/// A plain connection ending is the guest's to notice on its own read.
#[test]
fn a_plain_connection_ending_reports_nothing() {
    let c = SocketConnector::new();
    let node = Caller::Node(1);
    let (l, addr) = listen(&c, node, false);
    let client = TcpStream::connect(&addr).unwrap();
    let (conn, _) = accept(&c, node, l, false);
    drop(client);
    assert!(c.ended().is_empty());
    let r = call(&c, node, "socket/read", args(vec![("handle", conn.into())])).unwrap();
    assert_eq!(field(&r, "eof"), rmpv::Value::Boolean(true));
    assert!(c.ended().is_empty(), "still nothing, at end-of-file");
}

/// Closing one's own vital handle ends it, so it ends its owner too.
#[test]
fn closing_a_vital_handle_ends_its_owner() {
    let c = SocketConnector::new();
    let node = Caller::Node(1);
    let (l, addr) = listen(&c, node, false);
    let _client = TcpStream::connect(&addr).unwrap();
    let (conn, peer) = accept(&c, node, l, true);
    call(
        &c,
        node,
        "socket/close",
        args(vec![("handle", conn.into())]),
    )
    .unwrap();
    assert_eq!(
        c.ended(),
        vec![(
            node,
            format!("its vital connection with {peer} was closed by its owner")
        )]
    );
}

/// The flag is the claimant's declaration: set at claim it binds the
/// claimant; set by the holder before transfer it binds nobody after.
#[test]
fn vital_is_the_claimants_declaration() {
    let c = SocketConnector::new();
    let parent = Caller::Node(1);
    let child = Caller::Node(2);
    let (l, addr) = listen(&c, parent, false);

    // Plain at accept, vital at claim: the child is bound.
    let a = TcpStream::connect(&addr).unwrap();
    let (conn, peer) = accept(&c, parent, l, false);
    call(
        &c,
        parent,
        "socket/transfer",
        args(vec![("handle", conn.into()), ("to", 2u64.into())]),
    )
    .unwrap();
    call(
        &c,
        child,
        "socket/claim",
        args(vec![("handle", conn.into()), ("vital", true.into())]),
    )
    .unwrap();
    drop(a);
    assert_eq!(
        c.ended(),
        vec![(child, format!("its vital connection with {peer} ended"))]
    );

    // Vital at accept, plain at claim: nobody is bound.
    let b = TcpStream::connect(&addr).unwrap();
    let (conn, _) = accept(&c, parent, l, true);
    call(
        &c,
        parent,
        "socket/transfer",
        args(vec![("handle", conn.into()), ("to", 2u64.into())]),
    )
    .unwrap();
    call(
        &c,
        child,
        "socket/claim",
        args(vec![("handle", conn.into())]),
    )
    .unwrap();
    drop(b);
    assert!(c.ended().is_empty(), "the holder's flag did not travel");
    let r = call(
        &c,
        child,
        "socket/read",
        args(vec![("handle", conn.into())]),
    )
    .unwrap();
    assert_eq!(field(&r, "eof"), rmpv::Value::Boolean(true));
}

/// Nothing ends a listener from outside; its owner's close is the end.
#[test]
fn a_vital_listener_ends_only_when_closed() {
    let c = SocketConnector::new();
    let node = Caller::Node(1);
    let (l, addr) = listen(&c, node, true);
    let _client = TcpStream::connect(&addr).unwrap();
    assert!(c.ended().is_empty());
    call(&c, node, "socket/close", args(vec![("handle", l.into())])).unwrap();
    assert_eq!(
        c.ended(),
        vec![(
            node,
            format!("its vital listener on {addr} was closed by its owner")
        )]
    );
}

/// `vital` is `true` or nothing; a truthy string does not tie a lifetime.
#[test]
fn vital_is_a_boolean_or_nothing() {
    let c = SocketConnector::new();
    let node = Caller::Node(1);
    let l = call(
        &c,
        node,
        "socket/listen",
        args(vec![
            ("addr", "127.0.0.1:0".into()),
            ("vital", "yes".into()),
        ]),
    )
    .unwrap();
    call(
        &c,
        node,
        "socket/close",
        args(vec![("handle", field(&l, "handle"))]),
    )
    .unwrap();
    assert!(c.ended().is_empty());
}
