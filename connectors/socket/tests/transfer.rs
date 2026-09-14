//! Transfer at the connector boundary (`doc/Plan-0.7.0.md` §3.2): offer and
//! claim, ownership single-valued at every instant, the number unchanged.
//!
//! Driven the way the pump drives it: polled with a no-op waker. A call
//! that must be *pending* is asserted pending after one poll, and a call
//! that must complete is polled to completion under a bound.

use std::future::Future;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::pin::Pin;
use std::task::{Context, Poll, Waker};

use drt_connector::{Asker, CallError, Caller, Connector};
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

fn poll_once<T>(fut: &mut Pin<Box<dyn Future<Output = T> + Send + '_>>) -> Poll<T> {
    fut.as_mut().poll(&mut Context::from_waker(Waker::noop()))
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

/// The same call, started and left for the caller to poll.
fn start<'a>(
    c: &'a SocketConnector,
    asker: &'a Asker<'a>,
    name: &'a str,
    a: rmpv::Value,
) -> Pin<Box<dyn Future<Output = Result<rmpv::Value, CallError>> + Send + 'a>> {
    c.call_as(asker, name, Some(a), None)
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

/// A listener under `holder` with one accepted connection, and the client
/// on its far end.
fn accepted(c: &SocketConnector, holder: Caller) -> (u64, u64, TcpStream) {
    let l = call(
        c,
        holder,
        "socket/listen",
        args(vec![("addr", "127.0.0.1:0".into())]),
    )
    .unwrap();
    let client = TcpStream::connect(field(&l, "addr").as_str().unwrap()).unwrap();
    let a = call(
        c,
        holder,
        "socket/accept",
        args(vec![("handle", handle(&l, "handle").into())]),
    )
    .unwrap();
    (handle(&l, "handle"), handle(&a, "handle"), client)
}

fn transfer(c: &SocketConnector, from: Caller, h: u64, to: u32) -> Result<rmpv::Value, String> {
    call(
        c,
        from,
        "socket/transfer",
        args(vec![("handle", h.into()), ("to", u64::from(to).into())]),
    )
}

fn claim(c: &SocketConnector, who: Caller, h: u64) -> Result<rmpv::Value, String> {
    call(c, who, "socket/claim", args(vec![("handle", h.into())]))
}

fn write(c: &SocketConnector, who: Caller, h: u64, data: &str) -> Result<rmpv::Value, String> {
    call(
        c,
        who,
        "socket/write",
        args(vec![("handle", h.into()), ("data", data.into())]),
    )
}

/// The whole contract in one pass: the holder keeps the handle through
/// the offer, the claim moves it under the same number, and the holder is
/// then a stranger to it.
#[test]
fn a_transfer_keeps_the_handle_until_the_claim_moves_it_whole() {
    let c = SocketConnector::new();
    let parent = Caller::Node(1);
    let child = Caller::Node(2);
    let (_l, conn, mut client) = accepted(&c, parent);

    assert_eq!(transfer(&c, parent, conn, 2).unwrap(), rmpv::Value::Nil);
    // Still the parent's: it may use it.
    write(&c, parent, conn, "from parent").unwrap();

    let got = claim(&c, child, conn).unwrap();
    assert_eq!(handle(&got, "handle"), conn, "the number does not change");
    assert_eq!(
        field(&got, "peer").as_str().unwrap(),
        client.local_addr().unwrap().to_string()
    );

    let err = write(&c, parent, conn, "too late").unwrap_err();
    assert_eq!(err, "no such handle");
    write(&c, child, conn, " and child").unwrap();

    let mut back = [0u8; 21];
    client.read_exact(&mut back).unwrap();
    assert_eq!(&back, b"from parent and child");

    call(
        &c,
        child,
        "socket/close",
        args(vec![("handle", conn.into())]),
    )
    .unwrap();
    assert_eq!(
        c.release(&parent).len(),
        0,
        "the parent held only a listener"
    );
}

/// An offer is matched on the dispatcher's caller. A third node naming the
/// offered handle gets the constant sentence, and its unnamed claim waits.
#[test]
fn a_claim_is_matched_on_the_caller_not_the_request() {
    let c = SocketConnector::new();
    let (_l, conn, _client) = accepted(&c, Caller::Node(1));
    transfer(&c, Caller::Node(1), conn, 2).unwrap();

    let stranger = Caller::Node(3);
    assert_eq!(claim(&c, stranger, conn).unwrap_err(), "no such handle");
    let asker = Asker {
        caller: stranger,
        grants: &[],
    };
    let mut waiting = start(&c, &asker, "socket/claim", args(vec![]));
    assert!(
        poll_once(&mut waiting).is_pending(),
        "nothing is addressed to 3"
    );

    claim(&c, Caller::Node(2), conn).unwrap();
    assert!(
        poll_once(&mut waiting).is_pending(),
        "and still nothing, after 2 took its own"
    );
}

/// `claim {}` waits for the first offer addressed to the caller, and takes
/// the oldest when there are several.
#[test]
fn an_unnamed_claim_waits_and_takes_the_oldest() {
    let c = SocketConnector::new();
    let parent = Caller::Node(1);
    let child = Caller::Node(2);
    let (l, first, a) = accepted(&c, parent);

    let asker = Asker {
        caller: child,
        grants: &[],
    };
    let mut waiting = start(&c, &asker, "socket/claim", args(vec![]));
    assert!(poll_once(&mut waiting).is_pending(), "no offer yet");

    // A second connection on the same listener, then both offered, in order.
    let _b = TcpStream::connect(a.peer_addr().unwrap()).unwrap();
    let second = call(
        &c,
        parent,
        "socket/accept",
        args(vec![("handle", l.into())]),
    )
    .unwrap();
    let second = handle(&second, "handle");
    transfer(&c, parent, first, 2).unwrap();
    transfer(&c, parent, second, 2).unwrap();

    let got = match poll_once(&mut waiting) {
        Poll::Ready(r) => r.unwrap(),
        Poll::Pending => panic!("an offer is addressed to 2 now"),
    };
    assert_eq!(handle(&got, "handle"), first, "oldest first");
    let next = call(&c, child, "socket/claim", args(vec![])).unwrap();
    assert_eq!(handle(&next, "handle"), second);
}

/// Closing an offered handle withdraws the offer; the claimant then gets
/// the constant sentence.
#[test]
fn closing_an_offered_handle_withdraws_the_offer() {
    let c = SocketConnector::new();
    let (_l, conn, _client) = accepted(&c, Caller::Node(1));
    transfer(&c, Caller::Node(1), conn, 2).unwrap();
    call(
        &c,
        Caller::Node(1),
        "socket/close",
        args(vec![("handle", conn.into())]),
    )
    .unwrap();
    assert_eq!(
        claim(&c, Caller::Node(2), conn).unwrap_err(),
        "no such handle"
    );
}

/// A holder that dies first takes its offers with it, and the release
/// reports the connection as its own loss; a claimant that dies first
/// leaves the holder holding, with the offer gone.
#[test]
fn a_death_on_either_side_sweeps_the_offer() {
    let c = SocketConnector::new();
    let (_l, conn, client) = accepted(&c, Caller::Node(1));
    transfer(&c, Caller::Node(1), conn, 2).unwrap();
    let lost = c.release(&Caller::Node(1));
    assert_eq!(
        lost,
        vec![format!(
            "a connection with {}, cut",
            client.local_addr().unwrap()
        )]
    );
    assert_eq!(
        claim(&c, Caller::Node(2), conn).unwrap_err(),
        "no such handle"
    );

    let (_l, conn, _client) = accepted(&c, Caller::Node(3));
    transfer(&c, Caller::Node(3), conn, 4).unwrap();
    assert!(c.release(&Caller::Node(4)).is_empty(), "4 held nothing yet");
    write(&c, Caller::Node(3), conn, "still mine").unwrap();
    let asker = Asker {
        caller: Caller::Node(4),
        grants: &[],
    };
    let mut waiting = start(&c, &asker, "socket/claim", args(vec![]));
    assert!(
        poll_once(&mut waiting).is_pending(),
        "the offer to 4 died with 4"
    );
}

#[test]
fn transfer_refuses_self_a_stranger_s_handle_and_a_missing_target() {
    let c = SocketConnector::new();
    let (_l, conn, _client) = accepted(&c, Caller::Node(1));
    assert_eq!(
        transfer(&c, Caller::Node(1), conn, 1).unwrap_err(),
        "transfer to self"
    );
    assert_eq!(
        transfer(&c, Caller::Node(2), conn, 3).unwrap_err(),
        "no such handle"
    );
    let err = call(
        &c,
        Caller::Node(1),
        "socket/transfer",
        args(vec![("handle", conn.into())]),
    )
    .unwrap_err();
    assert!(err.contains("args.to"), "{err}");
}

/// A second transfer of the same handle replaces the first.
#[test]
fn a_second_transfer_replaces_the_first() {
    let c = SocketConnector::new();
    let (_l, conn, _client) = accepted(&c, Caller::Node(1));
    transfer(&c, Caller::Node(1), conn, 2).unwrap();
    transfer(&c, Caller::Node(1), conn, 3).unwrap();
    assert_eq!(
        claim(&c, Caller::Node(2), conn).unwrap_err(),
        "no such handle"
    );
    claim(&c, Caller::Node(3), conn).unwrap();
}

/// A listener moves the same way, answers with its address, and accepts
/// for its new holder.
#[test]
fn a_listener_transfers_too() {
    let c = SocketConnector::new();
    let (l, _conn, _client) = accepted(&c, Caller::Node(1));
    transfer(&c, Caller::Node(1), l, 2).unwrap();
    let got = claim(&c, Caller::Node(2), l).unwrap();
    let addr = field(&got, "addr").as_str().unwrap().to_string();
    let _client2 = TcpStream::connect(&addr).unwrap();
    call(
        &c,
        Caller::Node(2),
        "socket/accept",
        args(vec![("handle", l.into())]),
    )
    .unwrap();
}

/// A read the holder left pending answers `no such handle` on the poll
/// after the claim: the handle is gone from under it, and it is told so.
#[test]
fn a_pending_read_on_a_claimed_handle_answers_no_such_handle() {
    let c = SocketConnector::new();
    let (_l, conn, mut client) = accepted(&c, Caller::Node(1));
    let asker = Asker {
        caller: Caller::Node(1),
        grants: &[],
    };
    let mut reading = start(
        &c,
        &asker,
        "socket/read",
        args(vec![("handle", conn.into())]),
    );
    assert!(poll_once(&mut reading).is_pending(), "nothing to read yet");

    transfer(&c, Caller::Node(1), conn, 2).unwrap();
    claim(&c, Caller::Node(2), conn).unwrap();
    client.write_all(b"for the child").unwrap();
    match poll_once(&mut reading) {
        Poll::Ready(Err(e)) => assert_eq!(e.to_string(), "no such handle"),
        other => panic!("expected the holder's read to be refused: {other:?}"),
    }
    let r = call(
        &c,
        Caller::Node(2),
        "socket/read",
        args(vec![("handle", conn.into())]),
    )
    .unwrap();
    assert_eq!(
        field(&r, "data"),
        rmpv::Value::Binary(b"for the child".to_vec())
    );
}
