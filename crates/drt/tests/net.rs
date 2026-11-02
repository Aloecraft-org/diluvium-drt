//! The `net` connector natively (issue #108): two `drt start` processes
//! link by address over WebSocket, with node code that is the page's.
//!
//! ## surface block
//!
//! - Entry points: the `#[test]`s below.
//! - Configurable: [`WAIT`], how long either process may take.
//! - Fan-out: [`LISTENER`] is process A (a root that listens, hands each
//!   connection to a child by `transfer`, and waits for the child);
//!   [`DIALLER`] is process B (opens, answers, closes). [`start`] runs
//!   one `drt start` on a program and a `net` scope.

#![cfg(feature = "connector-net")]

use std::path::Path;
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

const WAIT: Duration = Duration::from_secs(30);

/// A: listen with `queue`, take the first `accepted`, transfer the
/// connection to a child that claims it, then wait for the child to end.
/// The child speaks first: a listener's queue gets what arrives on a
/// connection until someone claims it, so the dialler waits to be spoken
/// to.
const LISTENER: &str = r#"
local inbox = queue.lookup('inbox')
local ev = queue.declare('system/events', {capacity = 16})
local l = host.call('net/listen', {address = ADDRESS, queue = true})
print('A listening')
local _, acc = queue.wait({inbox}, 20000)
print('A ' .. acc.event .. ' ' .. tostring(acc.handle == l.handle) .. ' ' .. tostring(acc.peer:match('^127%.0%.0%.1:%d+$') ~= nil))
local child = host.spawn{code = [==[
  local inbox = queue.lookup('inbox')
  local c = host.call('net/claim', {queue = true})
  host.call('net/send', {handle = c.handle, data = 'hello'})
  local _, d = queue.wait({inbox}, 20000)
  print('child ' .. d.event .. ' ' .. d.data .. ' ' .. tostring(d.handle == c.handle))
  local _, e = queue.wait({inbox}, 20000)
  print('child ' .. e.event .. ' ' .. tostring(e.reason))
]==], caps = {'host:net/claim', 'host:net/send'}}
host.call('net/transfer', {handle = acc.conn, to = child.id})
host.call('net/close', {handle = l.handle})
local _, gone = queue.wait({ev}, 20000)
print('A child ' .. gone.event)
"#;

/// B: open with `queue`, hear the child, answer, close.
const DIALLER: &str = r#"
local inbox = queue.lookup('inbox')
local a = host.call('net/open', {address = ADDRESS, queue = true})
local _, d = queue.wait({inbox}, 20000)
print('B ' .. d.event .. ' ' .. d.data .. ' ' .. tostring(d.handle == a.handle))
host.call('net/send', {handle = a.handle, data = 'ping'})
host.call('net/close', {handle = a.handle})
print('B closed')
"#;

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// `drt start` on `program` (with `ADDRESS` bound to `address`) under
/// `net`'s `scope`, in its own directory.
fn start(dir: &Path, program: &str, address: &str, scope: &str) -> Child {
    let source = format!("local ADDRESS = '{address}'\n{program}");
    std::fs::write(dir.join("app.dlua"), source).unwrap();
    std::fs::write(
        dir.join("app.json"),
        format!(
            r#"{{"program": {{"path": "app.dlua"}},
                "caps": [{{"capability": "host:net/*"}}, {{"capability": "lifecycle"}}],
                "connectors": {{"net": {{"scope": {scope}}}}}}}"#
        ),
    )
    .unwrap();
    Command::new(env!("CARGO_BIN_EXE_drt"))
        .env("HOME", dir)
        .current_dir(dir)
        .args(["start", "--config", "app.json"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap()
}

/// The child's output once it exits, or a panic naming who hung.
fn finish(mut child: Child, who: &str) -> Output {
    let end = Instant::now() + WAIT;
    while child.try_wait().unwrap().is_none() {
        if Instant::now() > end {
            let _ = child.kill();
            let out = child.wait_with_output().unwrap();
            panic!(
                "{who} did not finish in {WAIT:?}:\n{}{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            );
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    child.wait_with_output().unwrap()
}

fn text(out: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

/// Wait until A says it is listening: B dialling first would be refused.
fn until_listening(port: u16) {
    let end = Instant::now() + WAIT;
    while std::net::TcpStream::connect(("127.0.0.1", port)).is_err() {
        assert!(Instant::now() < end, "A never listened on {port}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// The issue's acceptance: accepted, data both ways, close reaching the
/// far end as `closed`, and the accepted connection moved to a child by
/// `transfer` and `claim` under the same number.
#[test]
fn two_processes_link_by_address_over_net() {
    let port = free_port();
    let (a_dir, b_dir) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let a = start(
        a_dir.path(),
        LISTENER,
        &format!("127.0.0.1:{port}"),
        r#"{"listen": ["127.0.0.1:0"]}"#,
    );
    until_listening(port);
    // The probe above was a connection that never upgraded: the listener
    // drops it unreported, so A's first `accepted` is still B's.
    let b = start(
        b_dir.path(),
        DIALLER,
        &format!("ws://127.0.0.1:{port}/"),
        &format!(r#"{{"open": {{"allow": ["ws://127.0.0.1:{port}"], "allow_private": true}}}}"#),
    );
    let b = finish(b, "B");
    let a = finish(a, "A");
    let (a, b) = (text(&a), text(&b));
    assert_eq!(b, "B data hello true\nB closed\n", "A said:\n{a}");
    assert_eq!(
        a,
        "A listening\nA accepted true true\nchild data ping true\nchild closed nil\nA child exited\n",
        "B said:\n{b}"
    );
}

/// What the scope does not name is refused, by name: a listen outside
/// `listen`, an open of an origin `open` does not grant, and either verb
/// when its key is absent. A scope granting neither is refused at boot.
#[test]
fn the_scope_bounds_both_verbs() {
    let dir = tempfile::tempdir().unwrap();
    let probe = r#"
local _, s1, d1 = host.try('net/listen', {address = '127.0.0.1:1'})
print(s1, d1)
local _, s2, d2 = host.try('net/open', {address = 'ws://127.0.0.1:1/'})
print(s2, d2)
local _, s3, d3 = host.try('net/listen', {address = '127.0.0.1:0'})
print(s3, d3)
"#;
    let out = finish(
        start(dir.path(), probe, "", r#"{"listen": ["127.0.0.2:0"]}"#),
        "the probe",
    );
    let said = text(&out);
    let lines: Vec<&str> = said.lines().collect();
    assert_eq!(lines.len(), 3, "{said}");
    assert!(
        lines[0].starts_with("error")
            && lines[0].contains("outside this deployment's net scope (127.0.0.2:0)"),
        "{said}"
    );
    assert!(
        lines[1].starts_with("error") && lines[1].contains("grants no open"),
        "{said}"
    );
    assert!(
        lines[2].starts_with("error") && lines[2].contains("name the port"),
        "{said}"
    );

    let out = finish(start(dir.path(), "", "", r#"{}"#), "an empty scope");
    assert!(!out.status.success());
    assert!(
        text(&out).contains("grants neither listen nor open"),
        "{}",
        text(&out)
    );
}
