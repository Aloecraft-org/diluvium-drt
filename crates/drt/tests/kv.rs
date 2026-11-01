//! The kv store of `drt p2p --match --kv` (`doc/DRT-Signalling.md` §10),
//! end to end on loopback: the HTTP requests against the real server,
//! then `drt kv` and the `kv` connector against it.
//!
//! ## surface block
//!
//! - Entry points: the `#[test]`s below.
//! - Configurable: [`WAIT`], how long any one step may take.
//! - Fan-out: [`Server`] is the signalling server with kv on, a `drt p2p
//!   --match <port> --kv <dir>` subprocess, restartable on the same
//!   directory; [`Server::send`] is one request and its whole reply;
//!   [`Server::stream`] an event stream read as it arrives.

#![cfg(feature = "p2p")]

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const WAIT: Duration = Duration::from_secs(20);

fn drt(home: &Path) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_drt"));
    c.env("HOME", home);
    c
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// One reply: the status, the headers (names lowercased), the body.
#[derive(Debug)]
struct Reply {
    status: u16,
    headers: Vec<(String, String)>,
    body: String,
}

impl Reply {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }
    fn json(&self) -> serde_json::Value {
        serde_json::from_str(&self.body).unwrap_or_else(|e| panic!("{e}: {}", self.body))
    }
}

// depth: the server, as a subprocess

struct Server {
    child: Option<Child>,
    port: u16,
    dir: tempfile::TempDir,
    kv: Option<PathBuf>,
}

impl Server {
    fn start() -> Server {
        let dir = tempfile::tempdir().unwrap();
        let kv = Some(dir.path().join("kv"));
        let mut s = Server {
            child: None,
            port: free_port(),
            dir,
            kv,
        };
        s.spawn();
        s
    }

    /// A server with no `--kv`.
    fn without_kv() -> Server {
        let mut s = Server {
            child: None,
            port: free_port(),
            dir: tempfile::tempdir().unwrap(),
            kv: None,
        };
        s.spawn();
        s
    }

    fn spawn(&mut self) {
        let mut cmd = drt(self.dir.path());
        cmd.arg("p2p").arg("--match").arg(self.port.to_string());
        if let Some(kv) = &self.kv {
            cmd.arg("--kv").arg(kv);
        }
        let mut child = cmd
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut out = BufReader::new(child.stdout.take().unwrap()).lines();
        let line = out.next().expect("the server says what it serves").unwrap();
        assert!(line.contains("admission is by token"), "{line}");
        if self.kv.is_some() {
            let line = out.next().expect("the server says it keeps kv").unwrap();
            assert!(line.contains("kv in kv.db"), "{line}");
        }
        std::thread::spawn(move || for _ in out {});
        let stderr = child.stderr.take().unwrap();
        std::thread::spawn(move || for _ in BufReader::new(stderr).lines() {});
        self.child = Some(child);
    }

    /// Stop the process and start another on the same directory.
    fn restart(&mut self) {
        self.stop();
        self.spawn();
    }

    fn stop(&mut self) {
        if let Some(mut c) = self.child.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }

    fn send(&self, method: &str, path: &str, headers: &[(&str, &str)], body: &[u8]) -> Reply {
        let mut conn = TcpStream::connect(("127.0.0.1", self.port)).unwrap();
        conn.set_read_timeout(Some(WAIT * 2)).unwrap();
        let mut head = format!(
            "{method} {path} HTTP/1.1\r\nHost: t\r\nConnection: close\r\n\
             Content-Length: {}\r\n",
            body.len()
        );
        for (n, v) in headers {
            head.push_str(&format!("{n}: {v}\r\n"));
        }
        head.push_str("\r\n");
        conn.write_all(head.as_bytes()).unwrap();
        conn.write_all(body).unwrap();
        let mut raw = Vec::new();
        conn.read_to_end(&mut raw).unwrap();
        let raw = String::from_utf8_lossy(&raw).into_owned();
        let (head, body) = raw.split_once("\r\n\r\n").unwrap_or((&raw, ""));
        let mut lines = head.lines();
        let status = lines
            .next()
            .and_then(|l| l.split_whitespace().nth(1))
            .and_then(|c| c.parse().ok())
            .unwrap_or_else(|| panic!("not HTTP: {raw}"));
        let headers = lines
            .filter_map(|l| l.split_once(':'))
            .map(|(n, v)| (n.trim().to_ascii_lowercase(), v.trim().to_string()))
            .collect();
        Reply {
            status,
            headers,
            body: body.to_string(),
        }
    }

    fn get(&self, path: &str) -> Reply {
        self.send("GET", path, &[], b"")
    }

    fn put(&self, path: &str, value: &str) -> Reply {
        self.send("PUT", path, &[], value.as_bytes())
    }

    /// An event stream, with the lines it has sent so far readable as they
    /// arrive.
    fn stream(&self, path: &str, headers: &[(&str, &str)]) -> Stream {
        let mut conn = TcpStream::connect(("127.0.0.1", self.port)).unwrap();
        conn.set_read_timeout(Some(WAIT)).unwrap();
        let mut head = format!("GET {path} HTTP/1.1\r\nHost: t\r\n");
        for (n, v) in headers {
            head.push_str(&format!("{n}: {v}\r\n"));
        }
        head.push_str("\r\n");
        conn.write_all(head.as_bytes()).unwrap();
        Stream {
            reader: BufReader::new(conn),
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop();
    }
}

struct Stream {
    reader: BufReader<TcpStream>,
}

impl Stream {
    /// The `data:` of the next event, skipping the head, chunk sizes and
    /// keepalives.
    fn next_event(&mut self) -> (String, serde_json::Value) {
        let mut kind = String::new();
        loop {
            let mut line = String::new();
            assert!(
                self.reader.read_line(&mut line).unwrap() > 0,
                "the stream ended"
            );
            let line = line.trim_end();
            if let Some(k) = line.strip_prefix("event: ") {
                kind = k.to_string();
            }
            if let Some(data) = line.strip_prefix("data: ") {
                return (kind, serde_json::from_str(data).unwrap());
            }
        }
    }
}

const BASE: &str = "/v1/home/kv";

// depth: the requests of §7

#[test]
fn a_server_without_kv_says_how_to_turn_it_on() {
    let s = Server::without_kv();
    let r = s.get("/v1/home/kv/x");
    assert_eq!(r.status, 404);
    assert!(r.body.contains("--kv"), "{}", r.body);
}

#[test]
fn a_key_is_written_read_replaced_and_deleted_with_a_version_each_time() {
    let s = Server::start();
    let r = s.put("/v1/home/kv/bpi/info", "first");
    assert_eq!(r.status, 201, "{}", r.body);
    assert_eq!(r.header("etag"), Some("\"1\""));
    assert_eq!(r.json()["version"], 1);
    let r = s.get("/v1/home/kv/bpi/info");
    assert_eq!((r.status, r.body.as_str()), (200, "first"));
    assert_eq!(r.header("etag"), Some("\"1\""));
    assert!(r.header("content-type").unwrap().starts_with("text/plain"));
    let r = s.put("/v1/home/kv/bpi/info", "second");
    assert_eq!(r.status, 200);
    assert_eq!(r.header("etag"), Some("\"2\""));
    // Versions are the server's revision: a write elsewhere moves it too.
    assert_eq!(
        s.put("/v1/home/kv/other", "x").header("etag"),
        Some("\"3\"")
    );
    assert_eq!(
        s.send("DELETE", "/v1/home/kv/bpi/info", &[], b"").status,
        204
    );
    assert_eq!(s.get("/v1/home/kv/bpi/info").status, 404);
    assert_eq!(
        s.send("DELETE", "/v1/home/kv/bpi/info", &[], b"").status,
        404
    );
    // An empty value is a value.
    assert_eq!(s.put("/v1/home/kv/empty", "").status, 201);
    let r = s.get("/v1/home/kv/empty");
    assert_eq!((r.status, r.body.as_str()), (200, ""));
}

#[test]
fn compare_and_set_writes_only_over_the_version_named() {
    let s = Server::start();
    let k = "/v1/home/kv/lock";
    // If-None-Match: * creates only.
    assert_eq!(
        s.send("PUT", k, &[("If-None-Match", "*")], b"a").status,
        201
    );
    let r = s.send("PUT", k, &[("If-None-Match", "*")], b"b");
    assert_eq!(r.status, 412);
    assert_eq!(
        r.header("etag"),
        Some("\"1\""),
        "a lost race says where the key is"
    );
    // If-Match: a stale version loses and changes nothing; the current wins.
    let r = s.send("PUT", k, &[("If-Match", "\"7\"")], b"c");
    assert_eq!(r.status, 412);
    assert_eq!(s.get(k).body, "a");
    assert_eq!(s.send("PUT", k, &[("If-Match", "\"1\"")], b"d").status, 200);
    assert_eq!(
        s.send("PUT", k, &[("If-Match", "W/\"2\"")], b"e").status,
        200
    );
    // If-Match: * needs the key present.
    assert_eq!(
        s.send("PUT", "/v1/home/kv/absent", &[("If-Match", "*")], b"x")
            .status,
        412
    );
    assert_eq!(s.get("/v1/home/kv/absent").status, 404);
    // A delete takes If-Match too.
    assert_eq!(
        s.send("DELETE", k, &[("If-Match", "\"2\"")], b"").status,
        412
    );
    assert_eq!(s.get(k).body, "e");
    assert_eq!(
        s.send("DELETE", k, &[("If-Match", "\"3\"")], b"").status,
        204
    );
    assert_eq!(
        s.send("DELETE", k, &[("If-Match", "\"3\"")], b"").status,
        412
    );
}

#[test]
fn of_many_writers_racing_on_one_version_exactly_one_wins() {
    let s = Server::start();
    assert_eq!(s.put("/v1/home/kv/counter", "0").status, 201);
    let port = s.port;
    let racers: Vec<_> = (0..12)
        .map(|i| {
            std::thread::spawn(move || {
                let s = Server {
                    child: None,
                    port,
                    dir: tempfile::tempdir().unwrap(),
                    kv: None,
                };
                let body = format!("writer {i}");
                s.send(
                    "PUT",
                    "/v1/home/kv/counter",
                    &[("If-Match", "\"1\"")],
                    body.as_bytes(),
                )
                .status
            })
        })
        .collect();
    let statuses: Vec<u16> = racers.into_iter().map(|t| t.join().unwrap()).collect();
    assert_eq!(
        statuses.iter().filter(|s| **s == 200).count(),
        1,
        "{statuses:?}"
    );
    assert_eq!(
        statuses.iter().filter(|s| **s == 412).count(),
        11,
        "{statuses:?}"
    );
    assert_eq!(s.get("/v1/home/kv/counter").header("etag"), Some("\"2\""));
}

#[test]
fn a_ttl_key_expires_leaves_the_listing_and_says_so_to_a_watcher() {
    let s = Server::start();
    assert_eq!(s.put("/v1/home/kv/session/a?ttl=1", "alive").status, 201);
    assert_eq!(s.put("/v1/home/kv/session/b?ttl=1", "kept").status, 201);
    // A write without ttl makes a key permanent again.
    assert_eq!(s.put("/v1/home/kv/session/b", "kept").status, 200);
    let page = s.get("/v1/home/kv?prefix=session/").json();
    assert_eq!(page["keys"][0]["expires_in"], 1);
    assert!(page["keys"][1].get("expires_in").is_none());
    assert_eq!(s.get("/v1/home/kv/session/a").body, "alive");
    std::thread::sleep(Duration::from_millis(2500));
    assert_eq!(s.get("/v1/home/kv/session/a").status, 404);
    assert_eq!(s.get("/v1/home/kv/session/b").body, "kept");
    let page = s.get("/v1/home/kv?prefix=session/").json();
    assert_eq!(page["keys"].as_array().unwrap().len(), 1);
    let events = s.get("/v1/home/kv?watch=poll&since=3&wait=0").json();
    assert_eq!(events["events"][0]["op"], "expire");
    assert_eq!(events["events"][0]["key"], "session/a");
    // A refreshed TTL keeps a key alive past its first deadline.
    assert_eq!(s.put("/v1/home/kv/hb?ttl=2", "1").status, 201);
    std::thread::sleep(Duration::from_millis(1200));
    assert_eq!(s.put("/v1/home/kv/hb?ttl=2", "2").status, 200);
    std::thread::sleep(Duration::from_millis(1200));
    assert_eq!(s.get("/v1/home/kv/hb").body, "2");
    for bad in ["0", "-1", "1.5", "x", "31536001"] {
        let r = s.put(&format!("/v1/home/kv/t?ttl={bad}"), "v");
        assert_eq!(r.status, 400, "ttl={bad}: {}", r.body);
    }
}

#[test]
fn a_listing_pages_by_prefix_and_topics_do_not_see_each_other() {
    let s = Server::start();
    for k in ["a/1", "a/2", "a/3", "b/1"] {
        assert_eq!(s.put(&format!("{BASE}/{k}"), k).status, 201);
    }
    assert_eq!(s.put("/v1/work/kv/a/1", "work's").status, 201);
    let page = s.get(&format!("{BASE}?prefix=a/&limit=2")).json();
    let keys: Vec<&str> = page["keys"]
        .as_array()
        .unwrap()
        .iter()
        .map(|k| k["key"].as_str().unwrap())
        .collect();
    assert_eq!(keys, ["a/1", "a/2"]);
    assert_eq!(page["more"], true);
    assert_eq!(page["keys"][0]["value"], "a/1");
    let page = s.get(&format!("{BASE}?prefix=a/&after=a/2")).json();
    assert_eq!(page["keys"][0]["key"], "a/3");
    assert_eq!(page["more"], false);
    // Percent-encoded and plain spell the same prefix.
    let page = s.get(&format!("{BASE}?prefix=a%2F")).json();
    assert_eq!(page["keys"].as_array().unwrap().len(), 3);
    // An empty listing is an empty list, not an object.
    assert!(s
        .get(&format!("{BASE}?prefix=zz"))
        .body
        .contains("\"keys\":[]"));
    assert_eq!(
        s.get("/v1/work/kv").json()["keys"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(s.get("/v1/work/kv/a/1").body, "work's");
    assert_eq!(s.get(&format!("{BASE}/a/1")).body, "a/1");
    assert_eq!(s.get(&format!("{BASE}?limit=0")).status, 400);
    assert_eq!(s.get(&format!("{BASE}?limit=1001")).status, 400);
}

#[test]
fn what_is_not_a_key_or_a_value_is_refused_by_name() {
    let s = Server::start();
    for bad in [
        "/v1/home/kv/a//b",
        "/v1/home/kv//a",
        "/v1/home/kv/a%20b",
        "/v1/home/kv/a/../b",
    ] {
        let r = s.put(bad, "v");
        assert_eq!(r.status, 400, "{bad}: {}", r.body);
    }
    let long = format!("{BASE}/{}", "k".repeat(257));
    assert_eq!(s.put(&long, "v").status, 400);
    assert_eq!(
        s.put(&format!("{BASE}/{}", "k".repeat(256)), "v").status,
        201
    );
    assert_eq!(
        s.put(&format!("{BASE}/big"), &"v".repeat(16384)).status,
        201
    );
    let r = s.put(&format!("{BASE}/big"), &"v".repeat(16385));
    assert_eq!(r.status, 413, "{}", r.body);
    let r = s.send("PUT", &format!("{BASE}/bin"), &[], &[0xff, 0xfe]);
    assert_eq!(r.status, 400, "{}", r.body);
    assert_eq!(s.send("POST", &format!("{BASE}/x"), &[], b"v").status, 405);
    assert_eq!(s.send("PUT", BASE, &[], b"v").status, 405);
    assert_eq!(s.get("/v1/bad%20topic/kv/x").status, 400);
    // A preflight admits the methods and headers a page needs.
    let r = s.send("OPTIONS", &format!("{BASE}/x"), &[], b"");
    assert_eq!(r.status, 204);
    assert!(r
        .header("access-control-allow-methods")
        .unwrap()
        .contains("PUT"));
    assert!(r
        .header("access-control-allow-headers")
        .unwrap()
        .contains("if-match"));
    assert!(r
        .header("access-control-expose-headers")
        .unwrap()
        .contains("etag"));
    // kv beside the names of §2: a name called `kv` is not this.
    assert_eq!(s.get("/v1/home/kvx").status, 404);
}

#[test]
fn a_held_watch_answers_with_the_change_that_ends_it() {
    let s = Server::start();
    assert_eq!(s.put(&format!("{BASE}/cfg/a"), "1").status, 201);
    let port = s.port;
    let waiter = std::thread::spawn(move || {
        let s = Server {
            child: None,
            port,
            dir: tempfile::tempdir().unwrap(),
            kv: None,
        };
        let started = Instant::now();
        let r = s.get(&format!("{BASE}?watch=poll&since=1&prefix=cfg/"));
        (r, started.elapsed())
    });
    std::thread::sleep(Duration::from_millis(500));
    // Outside the prefix: the watch stays held.
    assert_eq!(s.put(&format!("{BASE}/other"), "x").status, 201);
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(s.put(&format!("{BASE}/cfg/b"), "2").status, 201);
    let (r, took) = waiter.join().unwrap();
    assert_eq!(r.status, 200, "{}", r.body);
    let v = r.json();
    assert_eq!(v["events"].as_array().unwrap().len(), 1, "{v}");
    assert_eq!(v["events"][0]["key"], "cfg/b");
    assert_eq!(v["events"][0]["value"], "2");
    assert!(
        took >= Duration::from_millis(700),
        "held, not answered at once"
    );
    // Changes already past `since` answer at once; a quiet wait answers empty.
    let v = s.get(&format!("{BASE}?watch=poll&since=0")).json();
    assert_eq!(v["events"].as_array().unwrap().len(), 3);
    let started = Instant::now();
    let v = s.get(&format!("{BASE}?watch=poll&since=3&wait=300")).json();
    assert!(v["events"].as_array().unwrap().is_empty());
    assert_eq!(v["revision"], 3);
    assert!(started.elapsed() < Duration::from_secs(5));
    assert_eq!(s.get(&format!("{BASE}?watch=poll")).status, 400);
    assert_eq!(
        s.get(&format!("{BASE}?watch=poll&since=0&wait=25001"))
            .status,
        400
    );
    assert_eq!(s.get(&format!("{BASE}?watch=sometimes")).status, 400);
}

#[test]
fn an_event_stream_carries_changes_and_resumes_from_last_event_id() {
    let s = Server::start();
    let mut live = s.stream(&format!("{BASE}?watch=stream&prefix=p/"), &[]);
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(s.put(&format!("{BASE}/q"), "not mine").status, 201);
    assert_eq!(s.put(&format!("{BASE}/p/1"), "one").status, 201);
    assert_eq!(
        s.send("DELETE", &format!("{BASE}/p/1"), &[], b"").status,
        204
    );
    let (kind, e) = live.next_event();
    assert_eq!(
        (kind.as_str(), &e["op"], &e["key"]),
        ("kv", &"put".into(), &"p/1".into())
    );
    assert_eq!(e["revision"], 2);
    let (_, e) = live.next_event();
    assert_eq!(e["op"], "delete");
    // A reconnect with Last-Event-ID gets what it missed, then live.
    let mut resumed = s.stream(
        &format!("{BASE}?watch=stream&prefix=p/"),
        &[("Last-Event-ID", "1")],
    );
    let (_, e) = resumed.next_event();
    assert_eq!((&e["revision"], &e["op"]), (&2.into(), &"put".into()));
    let (_, e) = resumed.next_event();
    assert_eq!(e["revision"], 3);
}

#[test]
fn keys_versions_and_recent_changes_survive_a_restart() {
    let mut s = Server::start();
    assert_eq!(s.put(&format!("{BASE}/keep"), "v1").status, 201);
    assert_eq!(s.put(&format!("{BASE}/keep"), "v2").status, 200);
    assert_eq!(s.put(&format!("{BASE}/brief?ttl=1"), "x").status, 201);
    s.restart();
    let r = s.get(&format!("{BASE}/keep"));
    assert_eq!(
        (r.status, r.body.as_str(), r.header("etag")),
        (200, "v2", Some("\"2\""))
    );
    // The revision goes on from where it was, never back to 1.
    assert_eq!(
        s.put(&format!("{BASE}/next"), "n").header("etag"),
        Some("\"4\"")
    );
    // A watcher resumes across the restart.
    let v = s.get(&format!("{BASE}?watch=poll&since=1&wait=0")).json();
    let revs: Vec<u64> = v["events"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["revision"].as_u64().unwrap())
        .collect();
    assert_eq!(&revs[..3], [2, 3, 4]);
    // A TTL is wall time, so it still runs out after a restart.
    std::thread::sleep(Duration::from_millis(1500));
    s.restart();
    assert_eq!(s.get(&format!("{BASE}/brief")).status, 404);
    assert!(s.dir.path().join("kv").join("kv.db").is_file());
}

// depth: `drt kv`, and the `kv` connector from a program

fn kv_cli(s: &Server, args: &[&str], stdin: &str) -> (i32, String, String) {
    let mut child = drt(s.dir.path())
        .arg("kv")
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(stdin.as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

#[test]
fn drt_kv_gets_puts_lists_and_removes_with_exit_statuses_a_script_can_read() {
    let s = Server::start();
    let at = |k: &str| format!("drt://127.0.0.1:{}/v1/home/kv/{k}", s.port);
    assert_eq!(
        kv_cli(&s, &["put", &at("bpi/info"), "fp=1"], ""),
        (0, "1\n".into(), String::new())
    );
    let (code, out, _) = kv_cli(&s, &["put", &at("bpi/ports"), "-"], "22,3389\n");
    assert_eq!((code, out.as_str()), (0, "2\n"));
    // A value is printed as it is: no newline added to a pipe.
    assert_eq!(kv_cli(&s, &["get", &at("bpi/info")], "").1, "fp=1");
    assert_eq!(kv_cli(&s, &["get", &at("bpi/ports")], "").1, "22,3389\n");
    let (_, out, _) = kv_cli(&s, &["get", "--json", &at("bpi/info")], "");
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!((&v["value"], &v["version"]), (&"fp=1".into(), &1.into()));
    let (code, _, err) = kv_cli(&s, &["put", "--if-version", "9", &at("bpi/info"), "x"], "");
    assert_eq!(code, 1);
    assert!(err.contains("version 1"), "{err}");
    assert_eq!(
        kv_cli(&s, &["put", "--if-version", "1", &at("bpi/info"), "x"], "").0,
        0
    );
    assert_eq!(
        kv_cli(&s, &["put", "--if-version", "0", &at("bpi/info"), "y"], "").0,
        1
    );
    assert_eq!(
        kv_cli(&s, &["put", "--ttl", "30", &at("tmp"), "t"], "").0,
        0
    );
    let (code, out, _) = kv_cli(&s, &["ls", &at("bpi/")], "");
    assert_eq!((code, out.as_str()), (0, "bpi/info\tv3\nbpi/ports\tv2\n"));
    let (_, out, _) = kv_cli(
        &s,
        &["ls", &format!("drt://127.0.0.1:{}/v1/home/kv", s.port)],
        "",
    );
    assert!(out.contains("tmp\tv4, expires in 30s"), "{out}");
    assert_eq!(kv_cli(&s, &["rm", &at("bpi/info")], "").0, 0);
    let (code, _, err) = kv_cli(&s, &["get", &at("bpi/info")], "");
    assert_eq!(code, 1);
    assert!(err.contains("404"), "{err}");
    assert_eq!(kv_cli(&s, &["rm", &at("bpi/info")], "").0, 1);
    // No answer at all is not a refusal.
    let (code, _, _) = kv_cli(&s, &["get", "drt://127.0.0.1:1/v1/home/kv/x"], "");
    assert_eq!(code, 2);
    let (code, _, err) = kv_cli(
        &s,
        &["get", &format!("drt://127.0.0.1:{}/v1/home/calls", s.port)],
        "",
    );
    assert_eq!(code, 2);
    assert!(err.contains("/v1/<topic>/kv"), "{err}");
}

#[test]
fn drt_kv_watch_prints_each_change_as_a_json_line() {
    let s = Server::start();
    assert_eq!(s.put(&format!("{BASE}/w/old"), "o").status, 201);
    let mut child = drt(s.dir.path())
        .arg("kv")
        .arg("watch")
        .arg("--since")
        .arg("0")
        .arg(format!("drt://127.0.0.1:{}/v1/home/kv/w/", s.port))
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    let first: serde_json::Value = serde_json::from_str(&lines.next().unwrap().unwrap()).unwrap();
    assert_eq!(first["key"], "w/old");
    assert_eq!(s.put(&format!("{BASE}/w/new"), "n").status, 201);
    let second: serde_json::Value = serde_json::from_str(&lines.next().unwrap().unwrap()).unwrap();
    assert_eq!(
        (&second["key"], &second["value"]),
        (&"w/new".into(), &"n".into())
    );
    let _ = child.kill();
    let _ = child.wait();
}

/// The connector, as a plain dlua program reaches it: `host.call` with no
/// DRT-specific library, under a config that names the topic.
#[test]
#[cfg(feature = "connector-kv")]
fn a_program_uses_the_kv_connector_through_host_calls() {
    let s = Server::start();
    let dir = s.dir.path();
    std::fs::write(
        dir.join("app.dlua"),
        r#"
local put = host.call('kv/put', {key = 'n/1', value = 'one'})
print('put', put.version)
local got = host.call('kv/get', {key = 'n/1'})
print('get', got.value, got.version)
local lost = host.call('kv/put', {key = 'n/1', value = 'x', version = 99})
print('lost', lost.conflict, lost.version)
print('won', host.call('kv/put', {key = 'n/1', value = 'two', version = got.version}).version)
print('create-only', host.call('kv/put', {key = 'n/1', value = 'x', version = 0}).conflict)
print('ttl', host.call('kv/put', {key = 'n/2', value = 't', ttl = 60}).version)
print('absent', host.call('kv/get', {key = 'n/none'}))
local page = host.call('kv/list', {prefix = 'n/'})
for _, k in ipairs(page.keys) do print('ls', k.key, k.version, k.expires_in) end
local quiet = host.call('kv/watch', {since = page.revision, wait_ms = 200}, 5000)
print('quiet', #quiet.events, quiet.revision)
local w = host.call('kv/watch', {since = 1, prefix = 'n/'}, 30000)
print('watch', #w.events, w.events[1].op, w.events[1].key, w.events[1].value)
print('stale delete', host.call('kv/delete', {key = 'n/1', version = 1}).conflict)
print('delete', host.call('kv/delete', {key = 'n/1'}).deleted, host.call('kv/delete', {key = 'n/1'}).deleted)
local _, status, detail = host.try('kv/get', {key = 'a b'})
print('refused', status, detail)
"#,
    )
    .unwrap();
    std::fs::write(
        dir.join("app.json"),
        format!(
            r#"{{"program": {{"path": "app.dlua"}},
                "caps": [{{"capability": "host:kv/*"}}],
                "connectors": {{"kv": {{"scope": {{"url": "drt://127.0.0.1:{}/v1/home", "token": "unused-until-topics"}}}}}}}}"#,
            s.port
        ),
    )
    .unwrap();
    let out = drt(dir)
        .current_dir(dir)
        .arg("run")
        .arg("--config")
        .arg("app.json")
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "{stdout}\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let want = "put\t1\nget\tone\t1\nlost\ttrue\t1\nwon\t2\ncreate-only\ttrue\nttl\t3\nabsent\tnil\n\
                ls\tn/1\t2\tnil\nls\tn/2\t3\t60\nquiet\t0\t3\nwatch\t2\tput\tn/1\ttwo\n\
                stale delete\ttrue\ndelete\ttrue\tfalse\n\
                refused\terror\tkv/get: the server answered 400: a key is 1 to 256 of A-Z a-z 0-9 . _ ~ - and /, with no empty, . or .. segment\n";
    assert_eq!(stdout, want);
    // The token rode as a bearer; the program never named it.
    assert_eq!(s.get(&format!("{BASE}/n/2")).body, "t");
}

#[test]
#[cfg(feature = "connector-kv")]
fn a_kv_scope_that_names_no_topic_is_refused_at_startup() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("app.dlua"), "print('ran')").unwrap();
    std::fs::write(
        dir.path().join("app.json"),
        r#"{"program": {"path": "app.dlua"}, "caps": [{"capability": "host:kv/*"}],
            "connectors": {"kv": {"scope": "http://127.0.0.1:1/v1/"}}}"#,
    )
    .unwrap();
    let out = drt(dir.path())
        .current_dir(dir.path())
        .arg("run")
        .arg("--config")
        .arg("app.json")
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(!String::from_utf8_lossy(&out.stdout).contains("ran"));
    assert!(String::from_utf8_lossy(&out.stderr).contains("topic"));
}
