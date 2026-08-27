//! `drt start` end to end: real guests, the deployment host, the clock.

use std::time::Duration;

use drt::start;
use drt_config::RootConfig;
use drt_connector::{Dispatcher, Registry};

fn config(json: &str) -> RootConfig {
    serde_json::from_str(json).unwrap()
}

/// A config whose program is inline source — JSON strings cannot carry the
/// newlines a readable program wants, so the program is spliced in encoded.
fn config_with_source(source: &str, rest: &str) -> RootConfig {
    let program = serde_json::to_string(source).unwrap();
    serde_json::from_str(&format!(r#"{{"program": {{"source": {program}}}{rest}}}"#)).unwrap()
}

/// Run `start` on its own thread with a guard: a deployment that should
/// drain but does not is a hang, and a hung test explains nothing.
fn start_guarded(config: RootConfig, within: Duration) -> Result<(), String> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(start::start(&config, Dispatcher::new(Registry::new())));
    });
    rx.recv_timeout(within)
        .expect("the deployment did not drain in time — a park was never answered")
}

/// A deployment is config + a program: the root spawns a child, hears its
/// exit through `system/events`, and returns. The swarm drains and `start`
/// comes home — the batch-shaped contract.
#[test]
fn a_deployment_that_drains_returns() {
    let cfg = config_with_source(
        "local sys = queue.declare('system/lifecycle', {capacity = 4})\n\
         local ev  = queue.declare('system/events', {capacity = 16})\n\
         assert(queue.push(sys, {op = 'spawn', code = 'local x = 1', caps = {}}))\n\
         queue.wait({ev})\n",
        r#", "caps": [{"capability": "lifecycle"}, {"capability": "queue:*"}]"#,
    );
    start_guarded(cfg, Duration::from_secs(10)).unwrap();
}

/// The one thing the deployment host owes that the clockless bench host
/// does not: a park with a timeout is resumed when the timeout elapses.
/// Without the clock this test does not fail — it hangs, which is exactly
/// what `queue.wait({q}, 25)` would do to a real deployment.
#[test]
fn a_park_timeout_fires_on_the_host_clock() {
    let cfg = config_with_source(
        "local q = queue.declare('nothing-pushes-here', {capacity = 1})\n\
         queue.wait({q}, 25)\n",
        "",
    );
    let begun = std::time::Instant::now();
    start_guarded(cfg, Duration::from_secs(10)).unwrap();
    assert!(
        begun.elapsed() >= Duration::from_millis(25),
        "the wait came back before its own timeout"
    );
}

/// A scheme this build cannot serve is refused, not accepted-and-ignored:
/// an operator who wrote a listener block believes a port is being served.
#[test]
fn an_unserved_scheme_is_refused() {
    let cfg = config(
        r#"{
            "program": {"source": "local x = 1"},
            "listeners": [{"scheme": "ssh", "address": "127.0.0.1:0"}]
        }"#,
    );
    let err = start::start(&cfg, Dispatcher::new(Registry::new())).unwrap_err();
    assert!(err.contains("'ssh'"), "{err}");
    assert!(err.contains("only 'http'"), "{err}");
}

#[test]
fn a_config_with_no_program_says_what_is_missing() {
    let err = start::start(&config("{}"), Dispatcher::new(Registry::new())).unwrap_err();
    assert!(err.contains("names no program"), "{err}");
}

// ---------------------------------------------------------------------------
// The http listener: dhost_http.c's queue bridge, end to end over a socket
// ---------------------------------------------------------------------------

#[cfg(feature = "listen")]
mod listener {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpStream;

    /// A fetchpoint-shaped guest: reads `http_in`, answers on `http_out`
    /// with the path, the one allowlisted request header it can see, and a
    /// reply-header pair of which only one is allowlisted back out.
    const FETCHPOINT: &str = "\
        local q   = queue.declare('http_in',  {capacity = 8})\n\
        local out = queue.declare('http_out', {capacity = 8, exported = true})\n\
        while true do\n\
          local id, req = queue.wait({q})\n\
          local port = (req.headers and req.headers['x-real-port']) or 'unobserved'\n\
          local secret = (req.headers and req.headers['x-secret']) or 'unseen'\n\
          queue.push(out, {\n\
            conn = req.conn,\n\
            status = 200,\n\
            content_type = 'text/plain',\n\
            body = req.method .. ' ' .. req.path .. ' port=' .. port .. ' secret=' .. secret,\n\
            headers = {['X-Answer'] = '42', ['x-not-allowed'] = 'leak'},\n\
          })\n\
        end\n";

    /// Bind, serve on a thread, hand back the address. The serve thread
    /// runs for the life of the test process — foreground-forever is the
    /// deployment's own contract.
    fn served(listener_json: &str, program: &str) -> std::net::SocketAddr {
        let program_json = serde_json::to_string(program).unwrap();
        let cfg: RootConfig = serde_json::from_str(&format!(
            r#"{{"program": {{"source": {program_json}}}, "listeners": [{listener_json}]}}"#
        ))
        .unwrap();
        let bound = drt::listen::bind(&cfg.listeners).unwrap();
        let addr = bound.addrs()[0];
        std::thread::spawn(move || {
            let _ = start::serve(&cfg, Dispatcher::new(Registry::new()), bound);
        });
        addr
    }

    fn roundtrip(addr: std::net::SocketAddr, request: &str) -> String {
        let mut conn = TcpStream::connect(addr).unwrap();
        conn.set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        conn.write_all(request.as_bytes()).unwrap();
        let mut response = String::new();
        conn.read_to_string(&mut response).unwrap();
        response
    }

    #[test]
    fn a_request_crosses_the_bridge_and_the_allowlists_hold() {
        let addr = served(
            r#"{"scheme": "http", "address": "127.0.0.1:0",
                "headers": ["x-real-port"], "resp_headers": ["x-answer"]}"#,
            FETCHPOINT,
        );
        let response = roundtrip(
            addr,
            "GET /reflect?format=text HTTP/1.1\r\n\
             Host: t\r\n\
             X-Real-Port: 54321\r\n\
             X-Secret: hunter2\r\n\
             \r\n",
        );
        // The program saw the method, the whole request-target (query
        // included), and the allowlisted header — under the allowlist's
        // lowercased spelling.
        assert!(response.starts_with("HTTP/1.1 200 OK\r\n"), "{response}");
        assert!(
            response.contains("GET /reflect?format=text port=54321"),
            "{response}"
        );
        // A header the deployment did not name never reached the program.
        assert!(response.contains("secret=unseen"), "{response}");
        // The allowlisted reply header came through under config's
        // spelling; the one not named was dropped whole.
        assert!(response.contains("x-answer: 42\r\n"), "{response}");
        assert!(!response.to_lowercase().contains("leak"), "{response}");
        assert!(
            response.contains("Content-Type: text/plain\r\n"),
            "{response}"
        );
        assert!(response.contains("Connection: close\r\n"), "{response}");
    }

    #[test]
    fn a_program_with_no_request_queue_answers_503() {
        let addr = served(
            r#"{"scheme": "http", "address": "127.0.0.1:0"}"#,
            "local hold = queue.declare('hold', {capacity = 1})\nqueue.wait({hold})\n",
        );
        let response = roundtrip(addr, "GET / HTTP/1.1\r\nHost: t\r\n\r\n");
        assert!(response.starts_with("HTTP/1.1 503 "), "{response}");
        assert!(response.contains("declares no request queue"), "{response}");
    }

    /// The request-smuggling refusals, straight from the C: chunked is not
    /// spoken, and Content-Length is digits-only with no duplicates.
    #[test]
    fn smuggling_shaped_requests_are_refused() {
        let addr = served(
            r#"{"scheme": "http", "address": "127.0.0.1:0"}"#,
            FETCHPOINT,
        );
        for (req, expect) in [
            (
                "POST / HTTP/1.1\r\nHost: t\r\nTransfer-Encoding: chunked\r\n\r\n",
                "chunked bodies are not spoken here",
            ),
            (
                "POST / HTTP/1.1\r\nHost: t\r\nContent-Length: 5\r\nContent-Length: 5\r\n\r\nhello",
                "duplicated Content-Length",
            ),
            (
                "POST / HTTP/1.1\r\nHost: t\r\nContent-Length: +5\r\n\r\nhello",
                "digits only",
            ),
        ] {
            let response = roundtrip(addr, req);
            assert!(
                response.starts_with("HTTP/1.1 400 "),
                "{req:?} -> {response}"
            );
            assert!(response.contains(expect), "{req:?} -> {response}");
        }
    }

    #[test]
    fn a_body_past_the_cap_is_413() {
        let addr = served(
            r#"{"scheme": "http", "address": "127.0.0.1:0", "max_body": 8}"#,
            FETCHPOINT,
        );
        let response = roundtrip(
            addr,
            "POST / HTTP/1.1\r\nHost: t\r\nContent-Length: 9\r\n\r\nteninelet",
        );
        assert!(response.starts_with("HTTP/1.1 413 "), "{response}");
    }

    /// A program that never answers costs the connection its deadline and
    /// nothing else: 504, and the deployment keeps serving.
    #[test]
    fn a_silent_program_is_a_504_not_a_hang() {
        let addr = served(
            r#"{"scheme": "http", "address": "127.0.0.1:0", "conn_deadline_ms": 300}"#,
            "local q = queue.declare('http_in', {capacity = 8})\n\
             local hold = queue.declare('hold', {capacity = 1})\n\
             queue.wait({hold})\n",
        );
        let begun = std::time::Instant::now();
        let response = roundtrip(addr, "GET / HTTP/1.1\r\nHost: t\r\n\r\n");
        assert!(response.starts_with("HTTP/1.1 504 "), "{response}");
        assert!(
            response.contains("did not answer within the deadline"),
            "{response}"
        );
        assert!(begun.elapsed() >= Duration::from_millis(300));
    }
}

// ---------------------------------------------------------------------------
// The residency policy: LRU under a budget, with the deployment's exemptions
// ---------------------------------------------------------------------------

mod residency {
    use drt::start::{enforce_residency, DeployHost, Deployment};
    use drt_caps::Grant;
    use drt_connector::{Dispatcher, Registry};
    use drt_swarm::engine::diluvium_engine::DiluviumEngine;
    use drt_swarm::pump::PumpHost;
    use drt_swarm::swarm::Swarm;
    use std::sync::Arc;

    /// A root that spawns `n` idle children and parks forever — the shape
    /// of a served deployment's supervisor.
    fn deployment_of(n: usize, wake: bool) -> (Deployment, drt_swarm::InstanceId) {
        let src = format!(
            "local sys  = queue.declare('system/lifecycle', {{capacity = 8}})\n\
             local hold = queue.declare('hold', {{capacity = 1}})\n\
             local WORKER = [[\n\
               local inbox = queue.declare('work', {{capacity = 4}})\n\
               while true do queue.wait({{inbox}}) end\n\
             ]]\n\
             for i = 1, {n} do\n\
               assert(queue.push(sys, {{op = 'spawn', code = WORKER,\n\
                                       caps = {{'queue:work'}},\n\
                                       wake_on_message = {wake}}}))\n\
             end\n\
             queue.wait({{hold}})\n"
        );
        let engine = Arc::new(DiluviumEngine::new().unwrap());
        let mut sw = Swarm::new(
            engine,
            PumpHost::new(DeployHost::new(), Dispatcher::new(Registry::new())),
        );
        let root = sw
            .root(
                src.as_bytes(),
                vec![Grant::grant("lifecycle"), Grant::grant("queue:*")],
                Default::default(),
            )
            .unwrap();
        for _ in 0..16 {
            sw.step();
            if sw.alive() > n {
                break;
            }
        }
        assert_eq!(sw.alive(), n + 1, "the children did not come up");
        (sw, root)
    }

    fn resident_children(sw: &Deployment, root: drt_swarm::InstanceId) -> usize {
        sw.ids()
            .into_iter()
            .filter(|id| *id != root && sw.resident(*id))
            .count()
    }

    #[test]
    fn the_budget_is_held_and_a_message_brings_one_back() {
        let (mut sw, root) = deployment_of(3, true);
        sw.step(); // children reach their parks

        enforce_residency(&mut sw, root, 1);
        assert_eq!(resident_children(&sw, root), 1, "the budget was not held");
        assert!(sw.resident(root), "the root is exempt and must stay");

        // A message to a hibernated child wakes it on the next step — the
        // whole point of requiring wake_on_message before hibernating.
        let sleeping = sw
            .ids()
            .into_iter()
            .find(|id| *id != root && !sw.resident(*id))
            .unwrap();
        // msgpack uint 1 is the single byte 0x01 — no encoder needed.
        sw.push(sleeping, "work", &[0x01]).unwrap();
        sw.step();
        assert!(sw.resident(sleeping), "the message did not wake it");

        // Over budget again; the policy evicts back down — and not the
        // one that just did work.
        enforce_residency(&mut sw, root, 1);
        assert_eq!(resident_children(&sw, root), 1);
        assert!(
            sw.resident(sleeping),
            "the LRU evicted the most recently active instance"
        );
    }

    /// An instance that did not ask to be woken is never hibernated by the
    /// policy: the delivery table makes a cached instance without the flag
    /// `Gone` to every sender, so hibernating it would disconnect its
    /// mailbox, not park it.
    #[test]
    fn an_instance_without_wake_on_message_is_never_evicted() {
        let (mut sw, root) = deployment_of(3, false);
        sw.step();
        enforce_residency(&mut sw, root, 0);
        assert_eq!(
            resident_children(&sw, root),
            3,
            "the policy hibernated a mailbox it would have disconnected"
        );
    }
}
