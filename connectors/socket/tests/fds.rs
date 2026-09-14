//! Acceptance 3 (`doc/Plan-0.7.0.md` §9): a released owner's sockets close,
//! and the descriptor count returns to where it started.
//!
//! Its own binary, on purpose: the count is process-wide, and a test that
//! shares a process with other socket-opening tests reads their descriptors
//! as its own. Counted in `/proc`, so Linux only.

#![cfg(target_os = "linux")]

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
        .unwrap()
}

fn open_fds() -> usize {
    std::fs::read_dir("/proc/self/fd").unwrap().count()
}

#[test]
fn a_released_nodes_sockets_close_and_the_fd_count_returns() {
    let c = SocketConnector::new();
    let node = Caller::Node(1);
    let before = open_fds();

    let l = call(
        &c,
        node,
        "socket/listen",
        rmpv::Value::Map(vec![("addr".into(), "127.0.0.1:0".into())]),
    );
    let addr = field(&l, "addr").as_str().unwrap().to_string();
    let client = TcpStream::connect(&addr).unwrap();
    call(
        &c,
        node,
        "socket/accept",
        rmpv::Value::Map(vec![("handle".into(), field(&l, "handle"))]),
    );
    // The listener, the accepted side, and this test's own client.
    assert_eq!(open_fds(), before + 3);

    let lost = c.release(&node);
    assert_eq!(
        lost,
        vec![format!(
            "a connection with {}, cut",
            client.local_addr().unwrap()
        )],
        "the live connection is the one thing that did not end well"
    );
    assert_eq!(open_fds(), before + 1, "only the client remains");
    drop(client);
    assert_eq!(open_fds(), before);
}
