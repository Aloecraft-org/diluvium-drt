//! The control endpoint (SPEC.md §13a): introspection and the few
//! lifecycle orders a running deployment takes, from inside the process.
//!
//! The drive loop owns the swarm, so everything here is a question handed
//! to that loop and answered on its next pass: a REPL served over SSH
//! asks with `:ps`, the sshd `drt` subsystem asks for a client on another
//! machine (`drt ps ssh://host:port`), and both get the same answer. One
//! process is one deployment, so the loop's end is a process-wide handle.
//!
//! What an order may mean is §13a's: `pause <id>` hibernates an instance
//! that is parked and refuses by name one that is not, since nothing
//! swaps an instance out behind its back; `resume <id>` wakes it; `stop`
//! hibernates everything parked and ends the loop. Introspection is open
//! to any key the listener admits; the three orders need the ceiling,
//! `host:*`, because they are the process's to give.
//!
//! ## surface block
//!
//! - Entry points: [`channel`], the loop's end and the handle; [`install`]
//!   and [`handle`], the process-wide handle; [`Control::ask`];
//!   [`drain`], what the loop calls each pass; [`parse`], a line to an
//!   [`Ask`]; [`render`], an answer as text; [`frame`] and [`unframe`],
//!   the subsystem's wire; `drt ps`: [`PsArgs`], [`run`].
//! - Configurable: [`REPLY_WAIT`], [`STOP_GRACE`], [`SUBSYSTEM`],
//!   [`ENDPOINT_FILE`], [`MAX_FRAME`].
//! - Fan-out: [`Ask`], one variant per verb §13a names; [`answer`], the
//!   match on it.

use std::sync::mpsc;
use std::sync::OnceLock;
use std::time::Duration;

use drt_swarm::InstanceId;

use crate::start::Deployment;

/// How long an asker waits for the loop's next pass.
pub const REPLY_WAIT: Duration = Duration::from_secs(10);
/// How long the loop, told to stop, waits for the asker to say the
/// answer was delivered before it ends anyway. The process exiting is
/// what would otherwise lose a `stopping` already handed to the runtime
/// and not yet on the wire.
pub const STOP_GRACE: Duration = Duration::from_secs(2);
/// The SSH subsystem name a client requests.
pub const SUBSYSTEM: &str = "drt";
/// Under `.drt_root/live`: where `drt start` writes its endpoint, so a
/// `drt ps` in the project finds it with nothing said.
pub const ENDPOINT_FILE: &str = "control";
/// One framed request or reply, at most.
pub const MAX_FRAME: usize = 1 << 20;

/// What may be asked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ask {
    Ps,
    Status,
    Caps(u32),
    Pause(u32),
    Resume(u32),
    Stop,
}

impl Ask {
    /// The three orders: the ceiling's to give.
    pub fn is_order(&self) -> bool {
        matches!(self, Ask::Pause(_) | Ask::Resume(_) | Ask::Stop)
    }

    pub fn verb(&self) -> &'static str {
        match self {
            Ask::Ps => "ps",
            Ask::Status => "status",
            Ask::Caps(_) => "caps",
            Ask::Pause(_) => "pause",
            Ask::Resume(_) => "resume",
            Ask::Stop => "stop",
        }
    }

    /// As the subsystem carries it: `{"ask": verb, "id": n}`.
    pub fn to_value(&self) -> rmpv::Value {
        let mut m = vec![(rmpv::Value::from("ask"), rmpv::Value::from(self.verb()))];
        if let Ask::Caps(id) | Ask::Pause(id) | Ask::Resume(id) = self {
            m.push((rmpv::Value::from("id"), rmpv::Value::from(*id)));
        }
        rmpv::Value::Map(m)
    }

    pub fn from_value(v: &rmpv::Value) -> Result<Ask, String> {
        let verb = get(v, "ask")
            .and_then(|a| a.as_str())
            .ok_or("a request names its `ask`")?;
        let id = || {
            get(v, "id")
                .and_then(|i| i.as_u64())
                .map(|i| i as u32)
                .ok_or_else(|| format!("`{verb}` takes an instance id"))
        };
        Ok(match verb {
            "ps" => Ask::Ps,
            "status" => Ask::Status,
            "caps" => Ask::Caps(id()?),
            "pause" => Ask::Pause(id()?),
            "resume" => Ask::Resume(id()?),
            "stop" => Ask::Stop,
            other => return Err(format!("no such ask: {other}")),
        })
    }
}

/// `ps`, `status`, `caps <id>`, `pause <id>`, `resume <id>`, `stop`, as
/// typed at a REPL after the colon or on `drt ps`'s flags.
pub fn parse(words: &[&str]) -> Result<Ask, String> {
    let id = |w: Option<&&str>| -> Result<u32, String> {
        w.and_then(|w| w.parse().ok())
            .ok_or_else(|| format!("{} takes an instance id", words[0]))
    };
    match words.first().copied() {
        Some("ps") => Ok(Ask::Ps),
        Some("status") => Ok(Ask::Status),
        Some("caps") => Ok(Ask::Caps(id(words.get(1))?)),
        Some("pause") => Ok(Ask::Pause(id(words.get(1))?)),
        Some("resume") => Ok(Ask::Resume(id(words.get(1))?)),
        Some("stop") => Ok(Ask::Stop),
        _ => Err("ps, status, caps <id>, pause <id>, resume <id>, or stop".into()),
    }
}

/// A question on its way to the loop, with where the answer goes, and
/// the asker's [`Delivered`] end: the loop reads a `stop` as done only
/// when it is dropped, or [`STOP_GRACE`] has passed.
pub struct Request {
    pub ask: Ask,
    reply: mpsc::SyncSender<rmpv::Value>,
    delivered: mpsc::Receiver<()>,
}

/// Held by the asker until its answer has reached whoever asked; dropped,
/// it tells the loop so. Every answer comes with one, and only `stop`
/// waits on it.
pub struct Delivered(#[allow(dead_code)] mpsc::Sender<()>);

/// The asker's end.
#[derive(Clone)]
pub struct Control {
    tx: mpsc::Sender<Request>,
}

/// The loop's end.
pub struct Inbox(mpsc::Receiver<Request>);

pub fn channel() -> (Control, Inbox) {
    let (tx, rx) = mpsc::channel();
    (Control { tx }, Inbox(rx))
}

static HANDLE: OnceLock<Control> = OnceLock::new();

/// The process's deployment, once `drt start` is driving one.
pub fn install(control: Control) {
    let _ = HANDLE.set(control);
}

pub fn handle() -> Option<Control> {
    HANDLE.get().cloned()
}

impl Control {
    /// Ask, and wait for the loop's next pass. Blocking: a tokio task
    /// wraps it in `spawn_blocking`. The [`Delivered`] is dropped once the
    /// answer has been handed on, which a `stop` waits for.
    pub fn ask(&self, ask: Ask) -> Result<(rmpv::Value, Delivered), String> {
        let (reply, answer) = mpsc::sync_channel(1);
        let (done, delivered) = mpsc::channel();
        self.tx
            .send(Request {
                ask,
                reply,
                delivered,
            })
            .map_err(|_| "the deployment has ended".to_string())?;
        let answer = answer
            .recv_timeout(REPLY_WAIT)
            .map_err(|_| "the deployment did not answer in time".to_string())?;
        Ok((answer, Delivered(done)))
    }
}

/// Answer everything asked since the last pass. True when `stop` was
/// asked: the loop hibernates what is parked and ends.
pub fn drain(inbox: &Inbox, sw: &mut Deployment, root: InstanceId) -> bool {
    let mut stop = false;
    while let Ok(req) = inbox.0.try_recv() {
        let _ = req.reply.send(answer(sw, root, &req.ask));
        if req.ask == Ask::Stop {
            stop = true;
            // The asker drops its `Delivered` once `stopping` has reached
            // the client; the loop ending first would take the answer
            // with it. Bounded: an asker that never says is not a veto.
            let _ = req.delivered.recv_timeout(STOP_GRACE);
        }
    }
    stop
}

// depth: the answers, on the drive thread

fn answer(sw: &mut Deployment, root: InstanceId, ask: &Ask) -> rmpv::Value {
    match ask {
        Ask::Ps => {
            let rows = sw
                .ids()
                .into_iter()
                .map(|id| instance_row(sw, root, id))
                .collect();
            ok(vec![("instances", rmpv::Value::Array(rows))])
        }
        Ask::Status => {
            let ids = sw.ids();
            let resident = ids.iter().filter(|id| sw.resident(**id)).count();
            ok(vec![
                ("root", rmpv::Value::from(root.0)),
                ("instances", rmpv::Value::from(ids.len() as u64)),
                ("resident", rmpv::Value::from(resident as u64)),
                (
                    "hibernated",
                    rmpv::Value::from((ids.len() - resident) as u64),
                ),
            ])
        }
        Ask::Caps(id) => match sw.caps(InstanceId(*id)) {
            Some(set) => ok(vec![(
                "caps",
                rmpv::Value::from(
                    serde_json::to_string(set.grants()).unwrap_or_else(|_| "[]".into()),
                ),
            )]),
            None => err(format!("no such instance: {id}")),
        },
        Ask::Pause(id) => {
            if InstanceId(*id) == root {
                return err("the root is not paused: that is `stop`".into());
            }
            match sw.hibernate(InstanceId(*id)) {
                Ok(()) => ok(vec![]),
                Err(e) => err(format!("{id} was not paused: {e}")),
            }
        }
        Ask::Resume(id) => match sw.wake(InstanceId(*id)) {
            Ok(()) => ok(vec![]),
            Err(e) => err(format!("{id} was not resumed: {e}")),
        },
        Ask::Stop => {
            // What is parked goes to the cache now; the loop ends after
            // this pass. A running instance is left to the process's end.
            let mut hibernated = 0u64;
            for id in sw.ids() {
                if id != root && sw.hibernate(id).is_ok() {
                    hibernated += 1;
                }
            }
            ok(vec![("hibernated", rmpv::Value::from(hibernated))])
        }
    }
}

fn instance_row(sw: &Deployment, root: InstanceId, id: InstanceId) -> rmpv::Value {
    let mut m = vec![
        ("id", rmpv::Value::from(id.0)),
        (
            "parent",
            sw.parent(id)
                .filter(|_| id != root)
                .map_or(rmpv::Value::Nil, |p| rmpv::Value::from(p.0)),
        ),
        (
            "path",
            rmpv::Value::from(sw.path(id).map_or("", |p| p.as_str())),
        ),
        ("resident", rmpv::Value::from(sw.resident(id))),
        (
            "caps",
            rmpv::Value::from(sw.caps(id).map_or(0, |c| c.grants().len() as u64)),
        ),
    ];
    if let Some(u) = sw.usage(id) {
        m.push(("instructions", rmpv::Value::from(u.instructions)));
        m.push(("memory_kb_peak", rmpv::Value::from(u.memory_kb_peak)));
        m.push(("bytes_now", rmpv::Value::from(u.bytes_now)));
    }
    rmpv::Value::Map(
        m.into_iter()
            .map(|(k, v)| (rmpv::Value::from(k), v))
            .collect(),
    )
}

fn ok(fields: Vec<(&str, rmpv::Value)>) -> rmpv::Value {
    let mut m = vec![(rmpv::Value::from("ok"), rmpv::Value::from(true))];
    m.extend(fields.into_iter().map(|(k, v)| (rmpv::Value::from(k), v)));
    rmpv::Value::Map(m)
}

pub fn err(why: String) -> rmpv::Value {
    rmpv::Value::Map(vec![
        (rmpv::Value::from("ok"), rmpv::Value::from(false)),
        (rmpv::Value::from("error"), rmpv::Value::from(why)),
    ])
}

pub fn get<'a>(v: &'a rmpv::Value, key: &str) -> Option<&'a rmpv::Value> {
    v.as_map()?
        .iter()
        .find(|(k, _)| k.as_str() == Some(key))
        .map(|(_, v)| v)
}

/// An answer as a terminal shows it.
pub fn render(ask: &Ask, v: &rmpv::Value) -> String {
    if get(v, "ok").and_then(|o| o.as_bool()) != Some(true) {
        return format!(
            "{}: {}",
            ask.verb(),
            get(v, "error")
                .and_then(|e| e.as_str())
                .unwrap_or("refused")
        );
    }
    match ask {
        Ask::Ps => {
            let mut out = format!(
                "{:>4} {:>6} {:<24} {:<9} {:>4} {:>12} {:>8} {:>10}\n",
                "ID", "PARENT", "PATH", "STATE", "CAPS", "INSTRUCTIONS", "PEAK_KB", "BYTES"
            );
            let rows = get(v, "instances").and_then(|r| r.as_array());
            for row in rows.into_iter().flatten() {
                let s = |k: &str| {
                    get(row, k)
                        .and_then(|x| x.as_str())
                        .unwrap_or("")
                        .to_string()
                };
                let n = |k: &str| get(row, k).and_then(|x| x.as_u64());
                let resident = get(row, "resident")
                    .and_then(|x| x.as_bool())
                    .unwrap_or(false);
                out.push_str(&format!(
                    "{:>4} {:>6} {:<24} {:<9} {:>4} {:>12} {:>8} {:>10}\n",
                    n("id").unwrap_or(0),
                    n("parent").map_or("-".to_string(), |p| p.to_string()),
                    s("path"),
                    if resident { "resident" } else { "hibernated" },
                    n("caps").unwrap_or(0),
                    n("instructions").map_or("-".to_string(), |x| x.to_string()),
                    n("memory_kb_peak").map_or("-".to_string(), |x| x.to_string()),
                    n("bytes_now").map_or("-".to_string(), |x| x.to_string()),
                ));
            }
            out
        }
        Ask::Status => {
            let n = |k: &str| get(v, k).and_then(|x| x.as_u64()).unwrap_or(0);
            format!(
                "root {}: {} instance(s), {} resident, {} hibernated\n",
                n("root"),
                n("instances"),
                n("resident"),
                n("hibernated")
            )
        }
        Ask::Caps(_) => format!(
            "{}\n",
            get(v, "caps").and_then(|c| c.as_str()).unwrap_or("[]")
        ),
        Ask::Pause(id) => format!("{id} paused\n"),
        Ask::Resume(id) => format!("{id} resumed\n"),
        Ask::Stop => format!(
            "stopping: {} hibernated\n",
            get(v, "hibernated").and_then(|x| x.as_u64()).unwrap_or(0)
        ),
    }
}

// depth: the subsystem's wire, framed msgpack (§13a)

/// A frame: a 4-byte big-endian length, then one msgpack value.
pub fn frame(v: &rmpv::Value) -> Vec<u8> {
    let mut body = Vec::new();
    let _ = rmpv::encode::write_value(&mut body, v);
    let mut out = (body.len() as u32).to_be_bytes().to_vec();
    out.extend(body);
    out
}

/// The first whole frame in `buf`, taken off it; `None` while it is
/// still arriving. A length past [`MAX_FRAME`] is an error.
pub fn unframe(buf: &mut Vec<u8>) -> Result<Option<rmpv::Value>, String> {
    if buf.len() < 4 {
        return Ok(None);
    }
    let len = u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;
    if len > MAX_FRAME {
        return Err(format!("a frame of {len} bytes is past the limit"));
    }
    if buf.len() < 4 + len {
        return Ok(None);
    }
    let body: Vec<u8> = buf.drain(..4 + len).skip(4).collect();
    rmpv::decode::read_value(&mut &body[..])
        .map(Some)
        .map_err(|e| format!("a frame that is not msgpack: {e}"))
}

/// Whether a key's grants hold the ceiling, which the orders need.
pub fn may_order(grants: &[drt_caps::Grant]) -> bool {
    grants
        .iter()
        .any(|g| g.effect == drt_caps::Effect::Grant && g.capability == "host:*")
}

/// The endpoint a `drt ps` in this project finds: the one `drt start`
/// wrote under `.drt_root/live`.
pub fn endpoint_path() -> Option<std::path::PathBuf> {
    let cwd = std::env::current_dir().ok()?;
    let root = crate::drt_root::discover(&cwd, None)?;
    Some(root.live().join(ENDPOINT_FILE))
}

// depth: `drt ps`, the client

/// `drt ps [ENDPOINT]`: ask a running deployment, over its ssh listener.
#[derive(clap::Args, Debug, Clone, Default)]
pub struct PsArgs {
    /// The deployment's ssh listener, `ssh://host:port` or `host:port`.
    /// Absent, the one `drt start` wrote under this project's
    /// .drt_root/live/control.
    #[arg(value_name = "ENDPOINT")]
    pub endpoint: Option<String>,
    /// Counts instead of the table.
    #[arg(long)]
    pub status: bool,
    /// One instance's grants, as its config would spell them.
    #[arg(long, value_name = "ID")]
    pub caps: Option<u32>,
    /// Hibernate an instance that is parked; one that is not is refused.
    #[arg(long, value_name = "ID")]
    pub pause: Option<u32>,
    /// Wake a hibernated instance.
    #[arg(long, value_name = "ID")]
    pub resume: Option<u32>,
    /// Hibernate everything parked and end the deployment.
    #[arg(long)]
    pub stop: bool,
    /// The answer as JSON, for a program.
    #[arg(long)]
    pub json: bool,
    /// The user to sign in as; the key decides what is allowed.
    #[arg(short = 'l', long = "login", short_alias = 'u', value_name = "USER")]
    pub login: Option<String>,
    /// A private key to sign in with, in place of ~/.ssh's. Repeatable.
    #[arg(short = 'i', value_name = "FILE")]
    pub identity: Vec<std::path::PathBuf>,
    /// Trust exactly this host key (`SHA256:…`). Repeatable.
    #[arg(long = "hostkey", value_name = "SHA256:…")]
    pub hostkey: Vec<String>,
    /// The known_hosts file; ~/.ssh/known_hosts if omitted.
    #[arg(long = "known-hosts", value_name = "FILE")]
    pub known_hosts: Option<std::path::PathBuf>,
    /// Refuse a host not already in known_hosts, rather than asking.
    #[arg(long)]
    pub strict: bool,
}

impl PsArgs {
    pub fn ask(&self) -> Result<Ask, String> {
        let chosen = [
            self.status,
            self.caps.is_some(),
            self.pause.is_some(),
            self.resume.is_some(),
            self.stop,
        ]
        .iter()
        .filter(|c| **c)
        .count();
        if chosen > 1 {
            return Err("one of --status, --caps, --pause, --resume or --stop".into());
        }
        Ok(if self.status {
            Ask::Status
        } else if let Some(id) = self.caps {
            Ask::Caps(id)
        } else if let Some(id) = self.pause {
            Ask::Pause(id)
        } else if let Some(id) = self.resume {
            Ask::Resume(id)
        } else if self.stop {
            Ask::Stop
        } else {
            Ask::Ps
        })
    }
}

#[cfg(feature = "connector-ssh")]
pub fn run(args: &PsArgs) -> Result<(), String> {
    use drt_connector_ssh::interactive::{self, Login, Trust};
    let ask = args.ask()?;
    let endpoint = match &args.endpoint {
        Some(e) => e.clone(),
        None => {
            let path = endpoint_path().ok_or(
                "no endpoint: give ssh://host:port, or run this inside a project a `drt start` \
                 with an ssh listener is running in",
            )?;
            std::fs::read_to_string(&path)
                .map_err(|_| {
                    format!(
                        "no deployment is running here ({} is absent); give ssh://host:port",
                        path.display()
                    )
                })?
                .trim()
                .to_string()
        }
    };
    let (host, port) = split_endpoint(&endpoint)?;
    let user = args
        .login
        .clone()
        .or_else(|| std::env::var("USER").ok())
        .or_else(|| std::env::var("USERNAME").ok())
        .unwrap_or_else(|| "drt".to_string());
    let trust = if args.hostkey.is_empty() {
        Trust::KnownHosts {
            file: args
                .known_hosts
                .clone()
                .or_else(interactive::known_hosts)
                .ok_or("no home directory for ~/.ssh/known_hosts; give --known-hosts")?,
            ask: !args.strict,
        }
    } else {
        Trust::Pinned {
            fingerprints: args.hostkey.clone(),
            keys: Vec::new(),
        }
    };
    let login = Login {
        user,
        host: host.clone(),
        port,
        trust,
        credentials: interactive::discover(args.identity.clone()),
    };
    let runtime = tokio::runtime::Runtime::new().map_err(|e| format!("a tokio runtime: {e}"))?;
    let answer = runtime.block_on(async {
        let tcp = tokio::time::timeout(
            crate::ssh::DIAL_TIMEOUT,
            tokio::net::TcpStream::connect((host.as_str(), port)),
        )
        .await
        .map_err(|_| format!("{host}:{port} did not answer in time"))?
        .map_err(|e| format!("{host}:{port}: {e}"))?;
        let conn =
            interactive::connect(tcp, login, std::sync::Arc::new(interactive::TtyPrompt)).await?;
        let whole = |bytes: &[u8]| matches!(unframe(&mut bytes.to_vec()), Ok(Some(_)) | Err(_));
        let reply =
            interactive::subsystem(conn, SUBSYSTEM, &frame(&ask.to_value()), &whole).await?;
        let mut buf = reply;
        unframe(&mut buf)?.ok_or_else(|| "the deployment answered nothing".to_string())
    })?;
    std::mem::forget(runtime);
    if args.json {
        println!("{}", msgpack_to_json(&answer));
    } else {
        print!("{}", render(&ask, &answer));
    }
    if get(&answer, "ok").and_then(|o| o.as_bool()) == Some(true) {
        Ok(())
    } else {
        Err(String::new())
    }
}

#[cfg(not(feature = "connector-ssh"))]
pub fn run(_args: &PsArgs) -> Result<(), String> {
    Err(
        "drt ps reaches a deployment over its ssh listener, and this build does not carry \
         `connector-ssh` (it is in `full`)"
            .into(),
    )
}

/// `ssh://host:port`, `host:port`, `[v6]:port`; the port is required.
pub fn split_endpoint(text: &str) -> Result<(String, u16), String> {
    let rest = text.strip_prefix("ssh://").unwrap_or(text);
    let rest = rest.trim_end_matches('/');
    let (host, port) = if let Some(inner) = rest.strip_prefix('[') {
        let (h, after) = inner
            .split_once(']')
            .ok_or_else(|| format!("{text}: an unclosed '['"))?;
        (h, after.strip_prefix(':').unwrap_or(""))
    } else {
        rest.rsplit_once(':')
            .ok_or_else(|| format!("{text}: an endpoint is ssh://host:port"))?
    };
    let port = port
        .parse()
        .map_err(|_| format!("{text}: not a port: {port}"))?;
    Ok((host.to_string(), port))
}

#[cfg(feature = "connector-ssh")]
fn msgpack_to_json(v: &rmpv::Value) -> String {
    fn conv(v: &rmpv::Value) -> serde_json::Value {
        match v {
            rmpv::Value::Nil => serde_json::Value::Null,
            rmpv::Value::Boolean(b) => serde_json::Value::Bool(*b),
            rmpv::Value::Integer(i) => i
                .as_i64()
                .map(serde_json::Value::from)
                .or_else(|| i.as_u64().map(serde_json::Value::from))
                .unwrap_or(serde_json::Value::Null),
            rmpv::Value::F32(f) => serde_json::Value::from(*f),
            rmpv::Value::F64(f) => serde_json::Value::from(*f),
            rmpv::Value::String(s) => serde_json::Value::from(s.as_str().unwrap_or("")),
            rmpv::Value::Binary(b) => serde_json::Value::from(String::from_utf8_lossy(b)),
            rmpv::Value::Array(a) => serde_json::Value::Array(a.iter().map(conv).collect()),
            rmpv::Value::Map(m) => serde_json::Value::Object(
                m.iter()
                    .map(|(k, v)| (k.as_str().unwrap_or("").to_string(), conv(v)))
                    .collect(),
            ),
            rmpv::Value::Ext(..) => serde_json::Value::Null,
        }
    }
    conv(v).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn asks_round_trip_the_wire_and_parse_from_words() {
        for ask in [
            Ask::Ps,
            Ask::Status,
            Ask::Caps(3),
            Ask::Pause(4),
            Ask::Resume(5),
            Ask::Stop,
        ] {
            let mut buf = frame(&ask.to_value());
            buf.extend([0, 0]); // a next frame, still arriving
            let got = unframe(&mut buf).unwrap().unwrap();
            assert_eq!(Ask::from_value(&got).unwrap(), ask);
            assert_eq!(buf, [0, 0]);
        }
        assert_eq!(parse(&["caps", "7"]).unwrap(), Ask::Caps(7));
        assert!(parse(&["caps"]).is_err());
        assert!(parse(&["dance"]).is_err());
        assert!(Ask::Stop.is_order() && !Ask::Ps.is_order());
        assert_eq!(
            split_endpoint("ssh://box:2222").unwrap(),
            ("box".into(), 2222)
        );
        assert_eq!(split_endpoint("[::1]:22").unwrap(), ("::1".into(), 22));
        assert!(split_endpoint("box").is_err());
    }

    #[test]
    fn a_refusal_renders_its_reason_and_a_ps_its_table() {
        assert_eq!(render(&Ask::Stop, &err("no".into())), "stop: no");
        let v = ok(vec![(
            "instances",
            rmpv::Value::Array(vec![rmpv::Value::Map(vec![
                (rmpv::Value::from("id"), rmpv::Value::from(1u32)),
                (rmpv::Value::from("path"), rmpv::Value::from("root")),
                (rmpv::Value::from("resident"), rmpv::Value::from(true)),
                (rmpv::Value::from("caps"), rmpv::Value::from(2u64)),
            ])]),
        )]);
        let text = render(&Ask::Ps, &v);
        assert!(text.contains("root") && text.contains("resident"), "{text}");
    }
}
