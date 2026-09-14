//! Readiness at the connector boundary (`doc/Plan-0.7.0.md` §3.4): a
//! `wake` handle that becomes readable is one notice on the queue its
//! owner named, said once per edge and re-armed by use.

use std::future::Future;
use std::io::Write;
use std::net::TcpStream;
use std::task::{Context, Poll, Waker};

use drt_connector::{Asker, Caller, Connector, Notice};
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

fn call(c: &SocketConnector, caller: Caller, name: &str, a: rmpv::Value) -> rmpv::Value {
    let asker = Asker {
        caller,
        grants: &[],
    };
    drive(c.call_as(&asker, name, Some(a), None)).unwrap()
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

fn notice(owner: Caller, queue: &str, handle: u64, ready: &str) -> Notice {
    Notice {
        owner,
        queue: queue.into(),
        message: args(vec![("handle", handle.into()), ("ready", ready.into())]),
    }
}

/// Acceptance 4's connector half: bytes from the far end are one notice
/// on the inbox, silence until the owner reads, then the next bytes are
/// news again.
#[test]
fn bytes_arriving_are_one_notice_until_the_owner_reads() {
    let c = SocketConnector::new();
    let node = Caller::Node(1);
    let l = call(
        &c,
        node,
        "socket/listen",
        args(vec![("addr", "127.0.0.1:0".into())]),
    );
    let mut client = TcpStream::connect(field(&l, "addr").as_str().unwrap()).unwrap();
    let a = call(
        &c,
        node,
        "socket/accept",
        args(vec![("handle", field(&l, "handle")), ("wake", true.into())]),
    );
    let conn = handle(&a, "handle");

    assert!(c.notices().is_empty(), "nothing to say while quiet");
    client.write_all(b"hello").unwrap();
    assert_eq!(c.notices(), vec![notice(node, "inbox", conn, "read")]);
    assert!(c.notices().is_empty(), "said once");
    client.write_all(b" more").unwrap();
    assert!(c.notices().is_empty(), "still once: the owner has not read");

    let r = call(&c, node, "socket/read", args(vec![("handle", conn.into())]));
    assert_eq!(
        field(&r, "data"),
        rmpv::Value::Binary(b"hello more".to_vec())
    );
    assert!(c.notices().is_empty(), "read everything; quiet again");
    client.write_all(b"again").unwrap();
    assert_eq!(c.notices(), vec![notice(node, "inbox", conn, "read")]);

    // End-of-file is readable, once, and never again after that.
    call(&c, node, "socket/read", args(vec![("handle", conn.into())]));
    drop(client);
    assert_eq!(c.notices(), vec![notice(node, "inbox", conn, "read")]);
    let r = call(&c, node, "socket/read", args(vec![("handle", conn.into())]));
    assert_eq!(field(&r, "eof"), rmpv::Value::Boolean(true));
    assert!(c.notices().is_empty());
    assert!(c.notices().is_empty(), "there is nothing after the end");
}

/// A `wake` listener says `accept` when a connection is waiting, and the
/// next `accept` hands that connection out.
#[test]
fn a_waiting_connection_is_one_notice_and_the_next_accept_takes_it() {
    let c = SocketConnector::new();
    let node = Caller::Node(1);
    let l = call(
        &c,
        node,
        "socket/listen",
        args(vec![
            ("addr", "127.0.0.1:0".into()),
            ("wake", "conns".into()),
        ]),
    );
    let lh = handle(&l, "handle");
    assert!(c.notices().is_empty());
    let client = TcpStream::connect(field(&l, "addr").as_str().unwrap()).unwrap();
    assert_eq!(c.notices(), vec![notice(node, "conns", lh, "accept")]);
    assert!(c.notices().is_empty(), "said once");

    let a = call(&c, node, "socket/accept", args(vec![("handle", lh.into())]));
    assert_eq!(
        field(&a, "peer").as_str().unwrap(),
        client.local_addr().unwrap().to_string(),
        "the connection the sweep took"
    );
    assert!(c.notices().is_empty(), "accepted; quiet again");
    let _second = TcpStream::connect(field(&l, "addr").as_str().unwrap()).unwrap();
    assert_eq!(c.notices(), vec![notice(node, "conns", lh, "accept")]);
}

/// Without `wake` a handle says nothing, however readable it is; the
/// claimant's `wake` is its own, like `vital`.
#[test]
fn wake_is_opt_in_and_the_claimants_own() {
    let c = SocketConnector::new();
    let parent = Caller::Node(1);
    let child = Caller::Node(2);
    let l = call(
        &c,
        parent,
        "socket/listen",
        args(vec![("addr", "127.0.0.1:0".into())]),
    );
    let mut client = TcpStream::connect(field(&l, "addr").as_str().unwrap()).unwrap();
    let a = call(
        &c,
        parent,
        "socket/accept",
        args(vec![("handle", field(&l, "handle")), ("wake", true.into())]),
    );
    let conn = handle(&a, "handle");
    call(
        &c,
        parent,
        "socket/transfer",
        args(vec![("handle", conn.into()), ("to", 2u64.into())]),
    );
    call(
        &c,
        child,
        "socket/claim",
        args(vec![("handle", conn.into()), ("wake", "sock".into())]),
    );
    client.write_all(b"x").unwrap();
    assert_eq!(c.notices(), vec![notice(child, "sock", conn, "read")]);

    // A plain claim of a second connection: nothing, ever.
    let mut other = TcpStream::connect(field(&l, "addr").as_str().unwrap()).unwrap();
    let b = call(
        &c,
        parent,
        "socket/accept",
        args(vec![("handle", field(&l, "handle"))]),
    );
    other.write_all(b"y").unwrap();
    assert!(c.notices().is_empty());
    let _ = b;
}
