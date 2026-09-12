//! `drt start` end to end: real guests, the deployment host, the clock.

use std::time::Duration;

#[cfg(feature = "listen")]
use drt::listen::Acceptor;
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
    // Both ways out, and the URL pinned: this is the one message a person
    // with an empty config ever sees, so a rotted link here is the whole
    // answer to "where do programs come from" going quiet.
    assert!(err.contains(r#""program": {"path": "..."}"#), "{err}");
    assert!(err.contains("https://dollup.aloecraft.org"), "{err}");
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

    /// A program that boots before it declares its queue, which is the
    /// ordinary shape: config, a schema migration, a control-plane read,
    /// *then* the queues. `queue.wait({boot}, N)` is that work — one park
    /// before the declare, which is all it takes.
    const LATE_DECLARE: &str = "\
        local boot = queue.declare('boot', {capacity = 1})\n\
        queue.wait({boot}, 250)\n\
        local q   = queue.declare('http_in',  {capacity = 8})\n\
        local out = queue.declare('http_out', {capacity = 8, exported = true})\n\
        while true do\n\
          local id, req = queue.wait({q})\n\
          queue.push(out, {conn = req.conn, status = 200, content_type = 'text/plain',\n\
                           body = 'ok'})\n\
        end\n";

    /// Issue #11: a request arriving before the program declared its queue
    /// was answered 503 on the spot, and `admit_timeout_ms` is the wait
    /// that makes it wait instead.
    ///
    /// The window is small — tens of milliseconds on a real deployment —
    /// and it lands on precisely the caller that cannot survive it: one
    /// that handshakes once at startup, takes the refusal as the answer,
    /// and never asks again. Two healthy services then never speak, with
    /// nothing in either log to say why.
    #[test]
    fn a_request_waits_for_a_queue_the_program_has_not_declared_yet() {
        let addr = served(
            r#"{"scheme": "http", "address": "127.0.0.1:0"}"#,
            LATE_DECLARE,
        );
        let began = std::time::Instant::now();
        let response = roundtrip(addr, "GET / HTTP/1.1\r\nHost: t\r\n\r\n");
        assert!(response.starts_with("HTTP/1.1 200 OK\r\n"), "{response}");
        // It waited for the declare rather than racing it, and the
        // default grace (2s) is what let it.
        assert!(
            began.elapsed() >= Duration::from_millis(200),
            "answered in {:?}, so it cannot have waited for the declare",
            began.elapsed()
        );
    }

    /// The wait is bounded, and what it ends with is the old answer: a
    /// program that has declared nothing by then declares nothing.
    #[test]
    fn a_program_with_no_request_queue_answers_503() {
        let addr = served(
            r#"{"scheme": "http", "address": "127.0.0.1:0", "admit_timeout_ms": 150}"#,
            "local hold = queue.declare('hold', {capacity = 1})\nqueue.wait({hold})\n",
        );
        let began = std::time::Instant::now();
        let response = roundtrip(addr, "GET / HTTP/1.1\r\nHost: t\r\n\r\n");
        assert!(response.starts_with("HTTP/1.1 503 "), "{response}");
        assert!(response.contains("declares no request queue"), "{response}");
        assert!(began.elapsed() >= Duration::from_millis(150), "{response}");
    }

    /// Zero is the old behaviour exactly, for a deployment that would
    /// rather its callers were told at once — and it is the setting the
    /// bug was, so it stays reachable.
    #[test]
    fn a_grace_of_zero_refuses_before_the_program_can_declare() {
        let addr = served(
            r#"{"scheme": "http", "address": "127.0.0.1:0", "admit_timeout_ms": 0}"#,
            LATE_DECLARE,
        );
        let began = std::time::Instant::now();
        let response = roundtrip(addr, "GET / HTTP/1.1\r\nHost: t\r\n\r\n");
        assert!(response.starts_with("HTTP/1.1 503 "), "{response}");
        assert!(response.contains("declares no request queue"), "{response}");
        assert!(began.elapsed() < Duration::from_millis(200), "{response}");
    }

    /// A connection that gives up first is dropped, not answered: its own
    /// `conn_deadline_ms` is the shorter wait, so the 504 is the acceptor's
    /// and the held request leaves without a second answer being written
    /// at a socket nobody is reading.
    #[test]
    fn a_held_request_whose_connection_expires_is_dropped_not_answered() {
        let addr = served(
            r#"{"scheme": "http", "address": "127.0.0.1:0",
                "conn_deadline_ms": 150, "admit_timeout_ms": 5000}"#,
            "local hold = queue.declare('hold', {capacity = 1})\nqueue.wait({hold})\n",
        );
        let response = roundtrip(addr, "GET / HTTP/1.1\r\nHost: t\r\n\r\n");
        assert!(response.starts_with("HTTP/1.1 504 "), "{response}");
        // One response, not a 504 with a 503 written after it.
        assert_eq!(response.matches("HTTP/1.1 ").count(), 1, "{response}");
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

/// Repeated allowlisted headers join with ", ", as the C host joins them —
/// a smuggled second x-df-sub arrives concatenated to the gateway's own,
/// visibly, never as a separable header the program might pick wrongly.
#[cfg(feature = "listen")]
#[test]
fn repeated_headers_join_rather_than_shadow() {
    use std::io::{Read, Write};
    let program = "\
        local q   = queue.declare('http_in',  {capacity = 8})\n\
        local out = queue.declare('http_out', {capacity = 8, exported = true})\n\
        while true do\n\
          local id, req = queue.wait({q})\n\
          queue.push(out, {conn = req.conn, status = 200,\n\
                           body = req.headers['x-df-sub'] or 'none'})\n\
        end\n";
    let program_json = serde_json::to_string(program).unwrap();
    let cfg: RootConfig = serde_json::from_str(&format!(
        r#"{{"program": {{"source": {program_json}}},
            "listeners": [{{"scheme": "http", "address": "127.0.0.1:0",
                           "headers": ["x-df-sub"]}}]}}"#
    ))
    .unwrap();
    let bound = drt::listen::bind(&cfg.listeners).unwrap();
    let addr = bound.addrs()[0];
    std::thread::spawn(move || {
        let _ = start::serve(&cfg, Dispatcher::new(Registry::new()), bound);
    });
    let mut conn = std::net::TcpStream::connect(addr).unwrap();
    conn.set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    conn.write_all(b"GET / HTTP/1.1\r\nHost: t\r\nX-Df-Sub: gateway\r\nX-Df-Sub: smuggled\r\n\r\n")
        .unwrap();
    let mut response = String::new();
    conn.read_to_string(&mut response).unwrap();
    assert!(response.ends_with("gateway, smuggled"), "{response}");
}

/// A deployment with no HTTP listener — a relay-only rendezvous fetchpoint
/// is exactly that — must still *sleep* between passes.
///
/// The drive loop's idle sleep and its request wakeup are the same call,
/// `Bound::next_within`. With at least one listener the acceptor thread
/// holds a sender, so that call waits. With none, the last sender used to
/// drop when `bind` returned, and `recv_timeout` on a disconnected channel
/// returns *immediately* — turning the idle sleep into a spin that burned
/// a whole core for the life of the process. `Bound` now keeps a sender of
/// its own, so an empty listener set idles like a full one.
#[cfg(feature = "listen")]
#[test]
fn a_deployment_with_no_listeners_sleeps_instead_of_spinning() {
    let mut bound = drt::listen::bind(&[]).unwrap();
    let start = std::time::Instant::now();
    assert!(bound.next_within(Duration::from_millis(150)).is_none());
    let waited = start.elapsed();
    assert!(
        waited >= Duration::from_millis(140),
        "next_within returned after {waited:?} instead of waiting out its \
         timeout: the ingress channel disconnected, and the drive loop is \
         spinning rather than idling"
    );
    // The polled acceptor, wasi's, has no channel to disconnect and must
    // idle the same way: a plain sleep, sliced by its poll tick.
    let mut polled = drt::listen::polled::Bound::bind(&[]).unwrap();
    let start = std::time::Instant::now();
    assert!(polled.next_within(Duration::from_millis(150)).is_none());
    let waited = start.elapsed();
    assert!(
        waited >= Duration::from_millis(140),
        "the polled acceptor idled {waited:?}"
    );
}

/// The deferred pump parks a connector's call only if the drive thread has
/// a runtime to await on. rc1 through rc3 entered none, so every
/// tokio-backed connector took its `block_on` fallback and a slow call
/// stalled every instance -- while 0.5.0's own notes said the opposite.
/// This is the measurement that found it, kept as the gate that keeps it
/// found: `crates/drt/src/runtime.rs` has the numbers.
#[cfg(all(
    feature = "listen",
    feature = "connector-rest",
    feature = "connector-time"
))]
mod parking {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};
    use std::time::Instant;

    /// How long the server sits on the request before answering: the length
    /// of the stall this test refuses.
    const SERVER_SLEEP: Duration = Duration::from_millis(600);

    /// One HTTP server that answers one request, slowly and then correctly.
    fn slow_server() -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let (mut sock, _) = listener.accept().unwrap();
            let mut buf = Vec::new();
            let mut chunk = [0u8; 1024];
            while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
                match sock.read(&mut chunk) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => buf.extend_from_slice(&chunk[..n]),
                }
            }
            std::thread::sleep(SERVER_SLEEP);
            sock.write_all(
                b"HTTP/1.1 200 OK\r\ncontent-type: text/plain\r\ncontent-length: 2\r\nconnection: close\r\n\r\nok",
            )
            .unwrap();
        });
        port
    }

    /// The root spawns a child whose one act is a slow `rest/get`, then
    /// parks itself for longer than that call. The child parks forever if
    /// the call fails, so a deployment that drains is a deployment whose
    /// parked call came back.
    fn program(port: u16) -> String {
        format!(
            "local child = host.spawn{{\n\
               code = [[\n\
                 local v, st = host.try('rest/get', {{url = 'http://127.0.0.1:{port}/x'}})\n\
                 if st ~= 'ok' then queue.wait({{queue.declare('never', {{capacity = 1}})}}) end\n\
               ]],\n\
               caps = {{'host:rest/*'}},\n\
               budget = {{instructions = 5000000}},\n\
             }}\n\
             local idle = queue.declare('idle', {{capacity = 1}})\n\
             queue.wait({{idle}}, 900)\n"
        )
    }

    #[test]
    fn a_parked_rest_call_does_not_stall_the_deployment() {
        let port = slow_server();
        let cfg = config_with_source(
            &program(port),
            &format!(
                r#", "caps": [{{"capability": "lifecycle"}}, {{"capability": "host:time"}}, {{"capability": "host:rest/*"}}],
                    "connectors": {{"time": {{}}, "rest": {{"scope": {{"allow": ["http://127.0.0.1:{port}"], "allow_private": true}}}}}},
                    "budget": {{"instructions": 50000000}}"#
            ),
        );
        let registry = drt::cli::wire_connectors(&cfg).unwrap();
        let bound = drt::listen::bind(&[]).unwrap();

        // The observer runs once per drive-loop pass on the drive thread:
        // a gap between two of its calls is exactly the time the loop
        // stood still.
        let passes: Arc<Mutex<Vec<Instant>>> = Arc::new(Mutex::new(Vec::new()));
        let seen = passes.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let outcome =
                start::serve_with_observer(&cfg, Dispatcher::new(registry), bound, move |_, _| {
                    seen.lock().unwrap().push(Instant::now())
                });
            let _ = tx.send(outcome);
        });
        rx.recv_timeout(Duration::from_secs(5))
            .expect("the deployment did not drain: the child's parked call never came back")
            .unwrap();

        let passes = passes.lock().unwrap();
        let longest = passes
            .windows(2)
            .map(|w| w[1].duration_since(w[0]))
            .max()
            .unwrap_or_default();
        assert!(
            longest < SERVER_SLEEP / 2,
            "the drive loop stood still for {longest:?} during a {SERVER_SLEEP:?} rest call: \
             the connector blocked the thread instead of parking in the pump"
        );
    }
}

/// The polled acceptor (doc/Wasm.md M6) is what wasip2 serves with: no
/// threads, non-blocking sockets stepped from the drive loop. It is
/// compiled natively so the same bridge can be proven here, through the
/// same `serve` loop, against the same requests the threaded one answers.
#[cfg(feature = "listen")]
mod polled {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpStream;

    const ECHO: &str = "\
        local q   = queue.declare('http_in',  {capacity = 8})\n\
        local out = queue.declare('http_out', {capacity = 8, exported = true})\n\
        while true do\n\
          local id, req = queue.wait({q})\n\
          queue.push(out, {conn = req.conn, status = 200, content_type = 'text/plain',\n\
                           body = req.method .. ' ' .. req.path .. ' ' .. #req.body})\n\
        end\n";

    fn served(program: &str, listener_json: &str) -> std::net::SocketAddr {
        let program_json = serde_json::to_string(program).unwrap();
        let cfg: RootConfig = serde_json::from_str(&format!(
            r#"{{"program": {{"source": {program_json}}}, "listeners": [{listener_json}]}}"#
        ))
        .unwrap();
        let bound = drt::listen::polled::Bound::bind(&cfg.listeners).unwrap();
        let addr = bound.addrs()[0];
        std::thread::spawn(move || {
            let _ = start::serve(&cfg, Dispatcher::new(Registry::new()), bound);
        });
        addr
    }

    fn roundtrip(addr: std::net::SocketAddr, request: &[u8]) -> String {
        let mut conn = TcpStream::connect(addr).unwrap();
        conn.set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        conn.write_all(request).unwrap();
        let mut response = String::new();
        conn.read_to_string(&mut response).unwrap();
        response
    }

    #[test]
    fn the_polled_acceptor_serves_the_bridge_without_a_thread() {
        let addr = served(ECHO, r#"{"scheme": "http", "address": "127.0.0.1:0"}"#);
        let response = roundtrip(addr, b"GET /hello?x=1 HTTP/1.1\r\nHost: t\r\n\r\n");
        assert!(response.starts_with("HTTP/1.1 200 OK\r\n"), "{response}");
        assert!(response.ends_with("GET /hello?x=1 0"), "{response}");
        assert!(response.contains("Connection: close\r\n"), "{response}");
        // A body, arriving in two pieces, across two polls.
        let mut conn = TcpStream::connect(addr).unwrap();
        conn.set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        conn.write_all(b"POST /in HTTP/1.1\r\nHost: t\r\nContent-Length: 6\r\n\r\nabc")
            .unwrap();
        std::thread::sleep(Duration::from_millis(20));
        conn.write_all(b"def").unwrap();
        let mut response = String::new();
        conn.read_to_string(&mut response).unwrap();
        assert!(response.ends_with("POST /in 6"), "{response}");
        // And the same refusals as the threaded one, in the C's words.
        let response = roundtrip(
            addr,
            b"POST / HTTP/1.1\r\nHost: t\r\nTransfer-Encoding: chunked\r\n\r\n",
        );
        assert!(response.starts_with("HTTP/1.1 400 "), "{response}");
        assert!(
            response.contains("chunked bodies are not spoken here"),
            "{response}"
        );
    }

    /// Issue #11 through the acceptor wasip2 uses. The wait lives in the
    /// drive loop, above both acceptors, and this is what says so.
    #[test]
    fn the_polled_acceptor_waits_for_a_late_declare_too() {
        let addr = served(
            "\
             local boot = queue.declare('boot', {capacity = 1})\n\
             queue.wait({boot}, 250)\n\
             local q   = queue.declare('http_in',  {capacity = 8})\n\
             local out = queue.declare('http_out', {capacity = 8, exported = true})\n\
             while true do\n\
               local id, req = queue.wait({q})\n\
               queue.push(out, {conn = req.conn, status = 200,\n\
                                content_type = 'text/plain', body = 'ok'})\n\
             end\n",
            r#"{"scheme": "http", "address": "127.0.0.1:0"}"#,
        );
        let response = roundtrip(addr, b"GET / HTTP/1.1\r\nHost: t\r\n\r\n");
        assert!(response.starts_with("HTTP/1.1 200 OK\r\n"), "{response}");
    }

    #[test]
    fn a_program_that_never_answers_is_timed_out_by_the_polled_acceptor() {
        let addr = served(
            "local q = queue.declare('http_in', {capacity = 8})\nwhile true do queue.wait({q}) end\n",
            r#"{"scheme": "http", "address": "127.0.0.1:0", "conn_deadline_ms": 200}"#,
        );
        let started = std::time::Instant::now();
        let response = roundtrip(addr, b"GET / HTTP/1.1\r\nHost: t\r\n\r\n");
        assert!(response.starts_with("HTTP/1.1 504 "), "{response}");
        assert!(
            response.contains("did not answer within the deadline"),
            "{response}"
        );
        assert!(started.elapsed() < Duration::from_secs(5));
    }
}

// depth: a rooted start, against a real filesystem

/// Everything above this point drives a config straight into `start`. This
/// block drives the *root* path instead — `.drt_root/` on a real disk, a
/// profile resolved, consent accepted, a program found — because the unit tests
/// for it run against `MemFs` and `StdFs` is what ships. A path jail, a
/// `create_dir_all`, and a relative `dlua_dir` all behave slightly differently
/// on the two, and only one of them is what an operator has.
mod rooted {
    use std::path::Path;

    use drt::boot;
    use drt::consent_gate::{Ask, Flags};

    /// Nobody to ask, and a record of what it was told.
    #[derive(Default)]
    struct Quiet {
        said: Vec<String>,
    }

    impl Ask for Quiet {
        fn interactive(&self) -> bool {
            false
        }
        fn say(&mut self, line: &str) {
            self.said.push(line.to_string());
        }
        fn confirm(&mut self, _: &str) -> std::io::Result<bool> {
            unreachable!("no test here answers a prompt")
        }
    }

    /// A root on a real disk.
    fn lay_out(dir: &Path, project: &str) {
        std::fs::create_dir_all(dir.join(".drt_root/profile")).unwrap();
        std::fs::create_dir_all(dir.join("dlua")).unwrap();
        std::fs::write(dir.join(".drt_root/project.json"), project).unwrap();
        std::fs::write(
            dir.join(".drt_root/profile/debug.config.json"),
            r#"{"dlua_dir":"dlua/","entry":"app.dlua","args":{"verbose":false}}"#,
        )
        .unwrap();
        std::fs::write(
            dir.join(".drt_root/profile/preflight.config.json"),
            r#"{"entry":"stdlib:preflight"}"#,
        )
        .unwrap();
        std::fs::write(dir.join("dlua/app.dlua"), "print('hello from app.dlua')\n").unwrap();
    }

    const PROJECT: &str = r#"{
        "root_id": "0192f0c1-8000-7000-8000-00000000abcd",
        "project_name": "my_drt_project",
        "project_version": "0.0.0",
        "drt": "0.5.0",
        "caps": [{"effect":"grant","capability":"host:time"}],
        "default_profile": "debug",
        "profiles": ["debug.config.json", "preflight.config.json"]
    }"#;

    /// A first start accepts the ceiling with `-y`, writes `consent.json`, and
    /// resolves the entry to a real path on disk. A second start is silent.
    #[test]
    fn a_first_rooted_start_accepts_and_the_second_is_silent() {
        let tmp = tempfile::tempdir().unwrap();
        lay_out(tmp.path(), PROJECT);

        let mut ask = Quiet::default();
        let booted = boot::boot(
            tmp.path(),
            None,
            None,
            None,
            Flags {
                yes: true,
                accept_changes: false,
            },
            &mut ask,
        )
        .expect("it boots");

        assert_eq!(
            booted.config.root.program,
            Some(drt_config::Program::Path(tmp.path().join("dlua/app.dlua"))),
            "dlua_dir + entry resolved against the root's own directory"
        );
        assert!(
            tmp.path().join(".drt_root/consent.json").exists(),
            "-y wrote the acceptance"
        );

        // Second start: the hash matches, so nothing is said and nothing asked.
        let mut again = Quiet::default();
        boot::boot(tmp.path(), None, None, None, Flags::default(), &mut again)
            .expect("a matching ceiling needs no flag at all");
        assert!(again.said.is_empty(), "{:?}", again.said);
    }

    /// consent.md acceptance 1's second half, on a real disk: widen the
    /// declared ceiling and `-y` is not enough.
    #[test]
    fn widening_the_ceiling_is_not_covered_by_minus_y() {
        let tmp = tempfile::tempdir().unwrap();
        lay_out(tmp.path(), PROJECT);
        let yes = Flags {
            yes: true,
            accept_changes: false,
        };
        boot::boot(tmp.path(), None, None, None, yes, &mut Quiet::default()).unwrap();

        std::fs::write(
            tmp.path().join(".drt_root/project.json"),
            PROJECT.replace(
                r#"[{"effect":"grant","capability":"host:time"}]"#,
                r#"[{"effect":"grant","capability":"host:time"},
                    {"effect":"grant","capability":"host:fs/read"}]"#,
            ),
        )
        .unwrap();

        let mut ask = Quiet::default();
        let e = boot::boot(tmp.path(), None, None, None, yes, &mut ask).unwrap_err();
        assert!(e.contains("--accept-changes"), "{e}");
        assert!(e.contains("host:fs/read"), "the refusal names it: {e}");

        // And `--accept-changes` is.
        boot::boot(
            tmp.path(),
            None,
            None,
            None,
            Flags {
                yes: false,
                accept_changes: true,
            },
            &mut Quiet::default(),
        )
        .expect("the deliberate act works");
    }

    /// `--root` reaches a root from somewhere else, which is what a systemd
    /// unit needs because systemd's default working directory is `/`.
    #[test]
    fn the_root_flag_reaches_a_root_from_outside_it() {
        let tmp = tempfile::tempdir().unwrap();
        lay_out(tmp.path(), PROJECT);
        let elsewhere = tempfile::tempdir().unwrap();

        // Discovery does not walk up, and there is no root where we are.
        let mut ask = Quiet::default();
        let booted = boot::boot(
            elsewhere.path(),
            None,
            None,
            None,
            Flags::default(),
            &mut ask,
        )
        .expect("no root is the no-root path");
        assert!(booted.root.is_none());

        // Named explicitly, it is found.
        let booted = boot::boot(
            elsewhere.path(),
            Some(tmp.path()),
            None,
            None,
            Flags {
                yes: true,
                accept_changes: false,
            },
            &mut Quiet::default(),
        )
        .expect("--root finds it");
        assert_eq!(booted.root.as_ref().unwrap().dir, tmp.path());
    }

    /// `drt start preflight` resolves and reports, and the report is the
    /// resolution -- the same value `start` would act on.
    #[test]
    fn the_preflight_profile_reports_what_start_would_do() {
        let tmp = tempfile::tempdir().unwrap();
        lay_out(tmp.path(), PROJECT);
        let booted = boot::boot(
            tmp.path(),
            None,
            None,
            Some("preflight"),
            Flags {
                yes: true,
                accept_changes: false,
            },
            &mut Quiet::default(),
        )
        .expect("it boots");

        let resolution = booted.resolution.as_ref().unwrap();
        let mut out = Vec::new();
        drt::stdlib::preflight(
            resolution,
            resolution.pin.as_ref().map(|p| p.value.as_str()),
            "0.5.0",
            resolution.consent.as_ref(),
            &mut out,
        )
        .unwrap();
        let report = String::from_utf8(out).unwrap();

        assert!(
            report.contains("profile: preflight (named on the command line)"),
            "{report}"
        );
        assert!(report.contains("ceiling: 1 caps"), "{report}");
        assert!(report.contains("stdlib:preflight"), "{report}");
        assert!(
            report.ends_with("start would run. nothing started.\n"),
            "{report}"
        );
    }
}
