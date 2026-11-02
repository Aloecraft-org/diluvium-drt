//! The connector over [`bus::Bus`]: two owners, one bus, and what each
//! sees. Calls are made as the dispatcher makes them, with the caller it
//! would name.

use std::future::Future;
use std::pin::pin;
use std::task::{Context, Poll, Waker};

use super::bus::Bus;
use super::*;

type Net = NetConnector<Bus>;

const A: Caller = Caller::Node(2);
const B: Caller = Caller::Node(3);

fn args(fields: Vec<(&str, rmpv::Value)>) -> Option<rmpv::Value> {
    Some(reply(fields))
}

fn asker(caller: Caller) -> Asker<'static> {
    Asker {
        caller,
        grants: &[],
    }
}

/// A call that must answer on its first poll, as `open` on a bus does.
fn now(net: &Net, caller: Caller, call: &str, a: Option<rmpv::Value>) -> CallResult {
    let who = asker(caller);
    let fut = net.call_as(&who, call, a, None);
    let mut fut = pin!(fut);
    match fut.as_mut().poll(&mut Context::from_waker(Waker::noop())) {
        Poll::Ready(r) => r,
        Poll::Pending => panic!("{call} waited; it should have answered at once"),
    }
}

fn ok(net: &Net, caller: Caller, call: &str, a: Option<rmpv::Value>) -> rmpv::Value {
    now(net, caller, call, a).unwrap_or_else(|e| panic!("{call}: {e}"))
}

fn get<'a>(v: &'a rmpv::Value, key: &str) -> &'a rmpv::Value {
    field(v, key).unwrap_or_else(|| panic!("no '{key}' in {v:?}"))
}

fn handle(v: &rmpv::Value) -> u64 {
    get(v, "handle").as_u64().unwrap()
}

fn bytes(v: &rmpv::Value) -> &[u8] {
    match get(v, "data") {
        rmpv::Value::Binary(b) => b,
        other => panic!("data is not bytes: {other:?}"),
    }
}

fn send(net: &Net, caller: Caller, h: u64, data: &[u8]) {
    ok(
        net,
        caller,
        "net/send",
        args(vec![
            ("handle", h.into()),
            ("data", rmpv::Value::Binary(data.to_vec())),
        ]),
    );
}

#[test]
fn buffered_open_accept_send_recv_and_eof() {
    let net = Net::new(Bus::new());
    let l = handle(&ok(
        &net,
        B,
        "net/listen",
        args(vec![("address", "b".into())]),
    ));
    let err = now(&net, A, "net/open", args(vec![("address", "c".into())])).unwrap_err();
    assert_eq!(err.detail, "nobody listens on 'c'");
    let a = handle(&ok(
        &net,
        A,
        "net/open",
        args(vec![("address", "b".into())]),
    ));
    let accepted = ok(&net, B, "net/accept", args(vec![("handle", l.into())]));
    let b = handle(&accepted);
    assert_eq!(get(&accepted, "peer").as_str(), Some("b-peer"));

    send(&net, A, a, b"hello");
    send(&net, A, a, b"again");
    let got = ok(&net, B, "net/recv", args(vec![("handle", b.into())]));
    assert_eq!(bytes(&got), b"hello", "one send is one message");
    let got = ok(&net, B, "net/recv", args(vec![("handle", b.into())]));
    assert_eq!(bytes(&got), b"again");
    send(&net, B, b, b"back");
    assert_eq!(
        bytes(&ok(&net, A, "net/recv", args(vec![("handle", a.into())]))),
        b"back"
    );

    // Nothing waiting: recv waits rather than answering empty.
    let who = asker(A);
    let fut = net.call_as(&who, "net/recv", args(vec![("handle", a.into())]), None);
    let mut fut = pin!(fut);
    assert!(fut
        .as_mut()
        .poll(&mut Context::from_waker(Waker::noop()))
        .is_pending());
    ok(&net, B, "net/close", args(vec![("handle", b.into())]));
    match fut.as_mut().poll(&mut Context::from_waker(Waker::noop())) {
        Poll::Ready(Ok(v)) => {
            assert_eq!(get(&v, "eof"), &rmpv::Value::Boolean(true));
            assert_eq!(get(&v, "reason").as_str(), Some("the far end closed"));
        }
        other => panic!("recv after the far end closed: {other:?}"),
    }
    let err = now(
        &net,
        A,
        "net/send",
        args(vec![("handle", a.into()), ("data", "x".into())]),
    )
    .unwrap_err();
    assert_eq!(
        err.detail,
        "the connection to b is closed: the far end closed"
    );
}

#[test]
fn a_queue_handle_pushes_what_arrives_to_its_owner() {
    let net = Net::new(Bus::new());
    let l = handle(&ok(
        &net,
        B,
        "net/listen",
        args(vec![("address", "b".into()), ("queue", true.into())]),
    ));
    let a = handle(&ok(
        &net,
        A,
        "net/open",
        args(vec![("address", "b".into()), ("queue", "net".into())]),
    ));
    let pushed = net.notices();
    assert_eq!(pushed.len(), 1, "{pushed:?}");
    assert_eq!(pushed[0].owner, B);
    assert_eq!(pushed[0].queue, "inbox");
    let m = &pushed[0].message;
    assert_eq!(get(m, "event").as_str(), Some("accepted"));
    assert_eq!(get(m, "handle").as_u64(), Some(l));
    let b = get(m, "conn").as_u64().unwrap();

    send(&net, A, a, b"ping");
    send(&net, B, b, b"pong");
    let pushed = net.notices();
    assert_eq!(pushed.len(), 2, "{pushed:?}");
    let to_b = pushed.iter().find(|n| n.owner == B).unwrap();
    assert_eq!(
        to_b.queue, "inbox",
        "an accepted connection pushes where its listener does"
    );
    assert_eq!(get(&to_b.message, "event").as_str(), Some("data"));
    assert_eq!(get(&to_b.message, "handle").as_u64(), Some(b));
    assert_eq!(bytes(&to_b.message), b"ping");
    let to_a = pushed.iter().find(|n| n.owner == A).unwrap();
    assert_eq!(to_a.queue, "net");
    assert_eq!(bytes(&to_a.message), b"pong");

    // A pushing handle has nothing to recv, and says where it went.
    let err = now(&net, A, "net/recv", args(vec![("handle", a.into())])).unwrap_err();
    assert!(err.detail.contains("pushes what arrives to 'net'"), "{err}");

    ok(&net, B, "net/close", args(vec![("handle", b.into())]));
    let pushed = net.notices();
    assert_eq!(pushed.len(), 1);
    assert_eq!(pushed[0].owner, A);
    assert_eq!(get(&pushed[0].message, "event").as_str(), Some("closed"));
    assert!(net.notices().is_empty(), "each fact is said once");
}

#[test]
fn a_handle_is_its_owners_and_nobody_elses() {
    let net = Net::new(Bus::new());
    ok(&net, B, "net/listen", args(vec![("address", "b".into())]));
    let a = handle(&ok(
        &net,
        A,
        "net/open",
        args(vec![("address", "b".into())]),
    ));
    let err = now(
        &net,
        B,
        "net/send",
        args(vec![("handle", a.into()), ("data", "x".into())]),
    )
    .unwrap_err();
    assert_eq!(err.detail, "no such handle");
    let err = now(&net, B, "net/close", args(vec![("handle", a.into())])).unwrap_err();
    assert_eq!(err.detail, "no such handle");
}

#[test]
fn release_closes_what_the_node_held_and_the_far_end_hears() {
    let net = Net::new(Bus::new());
    let l = handle(&ok(
        &net,
        B,
        "net/listen",
        args(vec![("address", "b".into())]),
    ));
    ok(&net, A, "net/open", args(vec![("address", "b".into())]));
    let b = handle(&ok(&net, B, "net/accept", args(vec![("handle", l.into())])));
    let lost = net.release(&A);
    assert_eq!(lost, vec!["a connection to b, cut".to_string()]);
    let v = ok(&net, B, "net/recv", args(vec![("handle", b.into())]));
    assert_eq!(get(&v, "eof"), &rmpv::Value::Boolean(true));
    // A listener closing is the end of its service and is not a loss.
    assert_eq!(net.release(&B), Vec::<String>::new());
    // And its name is free again.
    ok(&net, A, "net/listen", args(vec![("address", "b".into())]));
}

#[test]
fn a_vital_connection_ends_its_owner_when_it_ends() {
    let net = Net::new(Bus::new());
    let l = handle(&ok(
        &net,
        B,
        "net/listen",
        args(vec![("address", "b".into())]),
    ));
    ok(
        &net,
        A,
        "net/open",
        args(vec![("address", "b".into()), ("vital", true.into())]),
    );
    let b = handle(&ok(&net, B, "net/accept", args(vec![("handle", l.into())])));
    assert!(net.ended().is_empty());
    ok(&net, B, "net/close", args(vec![("handle", b.into())]));
    assert_eq!(
        net.ended(),
        vec![(A, "its vital connection to b ended".to_string())]
    );
    assert!(net.ended().is_empty(), "reported once");
}

#[test]
fn transfer_and_claim_move_a_connection_under_the_same_number() {
    let net = Net::new(Bus::new());
    let c = Caller::Node(4);
    let l = handle(&ok(
        &net,
        B,
        "net/listen",
        args(vec![("address", "b".into())]),
    ));
    let a = handle(&ok(
        &net,
        A,
        "net/open",
        args(vec![("address", "b".into())]),
    ));
    let b = handle(&ok(&net, B, "net/accept", args(vec![("handle", l.into())])));
    ok(
        &net,
        A,
        "net/transfer",
        args(vec![("handle", a.into()), ("to", 4u32.into())]),
    );
    // Still the holder's until claimed.
    send(&net, A, a, b"before");
    let claimed = ok(&net, c, "net/claim", args(vec![("queue", true.into())]));
    assert_eq!(handle(&claimed), a);
    assert_eq!(get(&claimed, "peer").as_str(), Some("b"));
    let err = now(
        &net,
        A,
        "net/send",
        args(vec![("handle", a.into()), ("data", "x".into())]),
    )
    .unwrap_err();
    assert_eq!(err.detail, "no such handle");
    send(&net, B, b, b"to the child");
    let pushed = net.notices();
    assert_eq!(pushed.len(), 1);
    assert_eq!(pushed[0].owner, c, "the claimant's queue, as it declared");
    assert_eq!(bytes(&pushed[0].message), b"to the child");
    assert_eq!(
        bytes(&ok(&net, B, "net/recv", args(vec![("handle", b.into())]))),
        b"before"
    );
}

#[test]
fn an_unread_backlog_past_the_bound_closes_the_connection() {
    let net = Net::new(Bus::new());
    let l = handle(&ok(
        &net,
        B,
        "net/listen",
        args(vec![("address", "b".into())]),
    ));
    let a = handle(&ok(
        &net,
        A,
        "net/open",
        args(vec![("address", "b".into())]),
    ));
    let b = handle(&ok(&net, B, "net/accept", args(vec![("handle", l.into())])));
    for _ in 0..=RECV_BACKLOG {
        send(&net, A, a, b"x");
    }
    for _ in 0..RECV_BACKLOG {
        ok(&net, B, "net/recv", args(vec![("handle", b.into())]));
    }
    let v = ok(&net, B, "net/recv", args(vec![("handle", b.into())]));
    assert_eq!(
        get(&v, "reason").as_str(),
        Some("more than 1024 messages arrived unread")
    );
    // The sender heard the close.
    let v = ok(&net, A, "net/recv", args(vec![("handle", a.into())]));
    assert_eq!(get(&v, "eof"), &rmpv::Value::Boolean(true));
}

#[test]
fn bad_arguments_are_named() {
    let net = Net::new(Bus::new());
    let err = now(&net, A, "net/open", args(vec![])).unwrap_err();
    assert_eq!(err.detail, "args.address must be a non-empty string");
    let err = now(
        &net,
        A,
        "net/open",
        args(vec![("address", "b".into()), ("queue", 3.into())]),
    )
    .unwrap_err();
    assert_eq!(
        err.detail,
        "args.queue must be a queue name, or true for the inbox"
    );
    let err = now(&net, A, "net/frob", None).unwrap_err();
    assert_eq!(err.detail, "net: unknown call 'net/frob'");
}

#[test]
fn dropping_the_connector_closes_what_is_still_open() {
    let bus = Bus::new();
    let far = Net::new(bus.clone());
    let l = handle(&ok(
        &far,
        B,
        "net/listen",
        args(vec![("address", "b".into())]),
    ));
    {
        let near = Net::new(bus.clone());
        ok(&near, A, "net/open", args(vec![("address", "b".into())]));
    }
    let b = handle(&ok(&far, B, "net/accept", args(vec![("handle", l.into())])));
    let v = ok(&far, B, "net/recv", args(vec![("handle", b.into())]));
    assert_eq!(get(&v, "eof"), &rmpv::Value::Boolean(true));
}
