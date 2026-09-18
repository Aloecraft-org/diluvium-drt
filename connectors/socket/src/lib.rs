//! The socket connector: a stream socket a node holds by handle and the
//! host holds by descriptor (`doc/Plan-0.7.0.md` §3.1).
//!
//! ```text
//!   socket/listen {addr, vital?, wake?}   -> {handle = n, addr = "ip:port"}
//!   socket/accept {handle, vital?, wake?} -> {handle = m, peer = "ip:port"}
//!   socket/read   {handle, max?}  -> {data = <bytes>, eof = bool}
//!   socket/write  {handle, data}  -> {written = n}
//!   socket/close  {handle}        -> nil
//!   socket/transfer {handle, to}  -> nil                   (the holder)
//!   socket/claim  {handle?, vital?, wake?} -> {handle, peer | addr} (the target)
//! ```
//!
//! A guest never holds a descriptor; it holds a number the host issued to
//! it and to no one else (§2.2). "Node-owned" is a statement about
//! lifetime and reach (§3.1): the socket lives while its owner does, closes
//! when its owner is released (§2.4), and survives its owner's hibernation,
//! because hibernation is not release.
//!
//! **What waits, waits in the host.** `accept` and `read` answer when there
//! is something to answer with, `write` when every byte is out. Until then
//! the call is pending in the hostcall pump, polled on the drive loop's own
//! cadence, and the guest is suspended in the call — the shape `rest` and
//! `ssh` have parked in since 0.5.0rc4. A guest that wants to *hibernate*
//! while it waits parks on its queue instead, and the readiness push (§3.4)
//! is what wakes it.
//!
//! **No `connect`.** The verb set is listen, accept, read, write, close
//! (§3.1). A node-owned socket is for protocols the node *serves*; TLS
//! terminates outside and proxies in (§10).
//!
//! **A `wake` handle pushes readiness to its owner's queue (§3.4).**
//! `wake = true` names the owner's `inbox`, `wake = "<queue>"` another of
//! its queues. When the handle becomes readable — bytes or end-of-file on
//! a connection, a connection waiting on a listener — the connector says
//! `{handle = n, ready = "read" | "accept"}` on that queue, once, and says
//! it again only after the owner has read or accepted: level-triggered,
//! re-armed by use, so a queue never fills with the same fact. The push is
//! what wakes a hibernated owner, by the swarm's own `wake_on_message`
//! path — an owner that hibernated without asking to be woken is, to this
//! push as to any other, not there. A service that wants to park for
//! hours waits on its queue rather than in `read`; this is how it hears.
//!
//! **A `vital` handle ends its owner (§3.3).** Declared at creation or at
//! claim — by the node whose lifetime is being tied, since the flag is a
//! property of the entry under its owner. When a vital connection ends
//! (the far end hangs up, or the owner closes it) the connector reports
//! the owner through [`Connector::ended`], the swarm kills it where it
//! lies, resident or hibernating, and the release path closes whatever
//! else it held. The guarantee runs socket → node; §2.4 already gave node
//! → socket. A vital listener ends only when closed: nothing ends it from
//! outside.
//!
//! **Transfer is offer and claim (§3.2), and ownership is single-valued at
//! every instant.** `transfer {handle, to}` addresses an offer to instance
//! `to`; the handle is **still the holder's** — usable, closable (`close`
//! withdraws the offer), released with the holder if it dies first. At
//! `claim` the entry moves to the claimant under the **same number** and
//! the holder's every use of it is `no such handle` from then on. An offer
//! is matched on the dispatcher's caller, never on anything in the
//! request, so a node claims only what was addressed to it. `claim {}`
//! with no handle **waits** for the first offer addressed to the caller —
//! the per-connection child cannot be told its handle before it exists,
//! and a retry loop in guest code is the wrong fix — while `claim
//! {handle}` is immediate. No descendancy check: the connector cannot make
//! one, and the ownership tree is not the membership graph. Nothing is
//! copied and no descriptor moves; a `to` that never claims costs the
//! holder nothing past its own lifetime.
//!
//! **One DRT bound, the scope's:** `allow`, the addresses a `listen` may
//! bind, in the shape `connectors/exec`'s `allow` established. Absent, any
//! address. Present, a `listen` naming anything else is refused by name,
//! and the list is checked at wiring so a typo is a refusal at boot.
//!
//! Replay: a reply is a message like any other, logged and replayed. A
//! replay does **not** re-bind, re-accept, or re-send.

use std::collections::VecDeque;
use std::future::poll_fn;
use std::io::{ErrorKind, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::Mutex;
use std::task::Poll;

use serde::Deserialize;

use drt_caps::{Scope, ScopeType};
use drt_connector::{Asker, CallError, CallResult, Caller, Connector, HandleId, Handles, Notice};

// ---------------------------------------------------------------------------
// Surface. [`SocketConnector`] answers the seven calls the `impl Connector`
// below dispatches, under the scope [`SocketScopeType`] describes. The two
// values are the caps a deployment does not tune.
// ---------------------------------------------------------------------------

/// The most one `read` hands back, and the default when the call names no
/// `max`: 64 KiB. A call may ask for less, never more.
pub const READ_MAX: usize = 64 * 1024;
/// The most one `write` takes: 1 MiB, exec's own output cap. Past it the
/// call is refused rather than truncated, like every other cap in this tree.
pub const WRITE_MAX: usize = 1024 * 1024;
/// How many waiting connections one readiness sweep takes into a `wake`
/// listener's backlog per step, so a flood is bounded per tick.
pub const ACCEPT_SWEEP: usize = 16;

/// The connector. One table of sockets, each keyed by who holds it; the
/// table is what makes §2's rules hold here without restating them.
pub struct SocketConnector {
    sockets: Handles<Socket>,
    /// Offers in flight, oldest first. Each names a handle its holder
    /// still owns and the instance it is addressed to.
    offers: Mutex<Vec<Offer>>,
    /// Owners whose vital handle was closed by their own hand, reported on
    /// the next [`Connector::ended`] alongside the ones the far end ended.
    ended: Mutex<Vec<(Caller, String)>>,
}

impl Default for SocketConnector {
    fn default() -> Self {
        Self::new()
    }
}

impl SocketConnector {
    pub fn new() -> Self {
        SocketConnector {
            sockets: Handles::new("socket"),
            offers: Mutex::new(Vec::new()),
            ended: Mutex::new(Vec::new()),
        }
    }
}

/// What the deployment bounds: the addresses a `listen` may bind.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct SocketScope {
    /// `ip:port` literals, never names: the connector resolves nothing, so
    /// the scope does not either. Port `0` in an entry allows any port on
    /// that address — the ephemeral-port shape a bind-then-advertise
    /// service wants.
    #[serde(default)]
    allow: Option<Vec<String>>,
}

pub struct SocketScopeType;

impl ScopeType for SocketScopeType {
    fn describe(&self) -> &str {
        "{allow?}: the ip:port literals a listen may bind (port 0 for any), or no scope for any address"
    }

    fn validate(&self, scope: Option<&Scope>) -> Result<(), String> {
        SocketScope::parse(scope).map(|_| ())
    }
}

#[async_trait::async_trait]
impl Connector for SocketConnector {
    fn scope_type(&self) -> Box<dyn ScopeType> {
        Box::new(SocketScopeType)
    }

    /// The caller-blind path is the root with no grants: what it creates
    /// is the root's and lives to [`Connector::finish`].
    async fn call(
        &self,
        call: &str,
        args: Option<rmpv::Value>,
        scope: Option<&Scope>,
    ) -> CallResult {
        let asker = Asker {
            caller: Caller::Root,
            grants: &[],
        };
        self.call_as(&asker, call, args, scope).await
    }

    async fn call_as(
        &self,
        asker: &Asker<'_>,
        call: &str,
        args: Option<rmpv::Value>,
        scope: Option<&Scope>,
    ) -> CallResult {
        let scope = SocketScope::parse(scope).map_err(CallError)?;
        let caller = asker.caller;
        match call {
            "socket/listen" => self.listen(caller, &scope, args),
            "socket/accept" => self.accept(caller, args).await,
            "socket/read" => self.read(caller, args).await,
            "socket/write" => self.write(caller, args).await,
            "socket/close" => self.close(caller, args),
            "socket/transfer" => self.transfer(caller, args),
            "socket/claim" => self.claim(caller, args).await,
            other => Err(CallError::new(format!("socket: unknown call '{other}'"))),
        }
    }

    /// The owner is gone for good: every socket it held closes now, and
    /// each connection whose far end was still there is one line of the
    /// report (§2.5). A listener closing is the intended end of its
    /// service and says nothing.
    fn release(&self, caller: &Caller) -> Vec<String> {
        // Offers it made die with what it held; offers addressed to it
        // have no one left to claim them.
        self.offers()
            .retain(|o| o.from != *caller && o.to != *caller);
        self.sockets
            .release(*caller)
            .into_iter()
            .filter_map(|(_, socket)| socket.lost())
            .collect()
    }

    /// Owners whose vital connection ended since the last ask: closed by
    /// them, or hung up on by the far end (one non-blocking peek per vital
    /// connection per ask). Each ending is reported once, and the entry is
    /// gone with the report — the owner is about to be, too.
    fn ended(&self) -> Vec<(Caller, String)> {
        let mut ended = std::mem::take(&mut *self.ended.lock().unwrap_or_else(|e| e.into_inner()));
        for (owner, _, socket) in self
            .sockets
            .take_where(|_, _, s| s.is_vital() && s.has_ended())
        {
            ended.push((owner, format!("its vital {} ended", socket.name())));
        }
        ended
    }

    /// Every `wake` handle that became readable since its owner last used
    /// it, as one notice each on the queue the owner named.
    fn notices(&self) -> Vec<Notice> {
        let mut out = Vec::new();
        self.sockets.each(|owner, handle, socket| {
            if let Some((queue, ready)) = socket.became_ready() {
                out.push(Notice {
                    owner,
                    queue,
                    message: reply(vec![("handle", handle.0.into()), ("ready", ready.into())]),
                });
            }
        });
        out
    }

    fn finish(&self) -> Vec<String> {
        self.offers()
            .retain(|o| o.from != Caller::Root && o.to != Caller::Root);
        self.sockets
            .drain_root()
            .into_iter()
            .filter_map(|(_, socket)| socket.lost())
            .collect()
    }
}

// depth: what the host holds behind a handle

/// One `transfer` not yet claimed: `handle` is still `from`'s.
struct Offer {
    from: Caller,
    to: Caller,
    handle: HandleId,
}

/// `wake` is the queue readiness goes to, if the owner asked; `notified`
/// is whether it has been told and has not used the handle since.
enum Socket {
    Listener {
        listener: TcpListener,
        vital: bool,
        wake: Option<String>,
        notified: bool,
        /// Connections a readiness sweep already took, handed out by the
        /// next `accept` before the listener is asked again.
        backlog: VecDeque<(TcpStream, SocketAddr)>,
    },
    /// `eof` once the far end has finished sending: closing such a
    /// connection cuts nothing, and the report says nothing.
    Stream {
        stream: TcpStream,
        peer: SocketAddr,
        eof: bool,
        vital: bool,
        wake: Option<String>,
        notified: bool,
    },
}

impl Socket {
    fn set_wake(&mut self, w: Option<String>) {
        match self {
            Socket::Listener { wake, notified, .. } | Socket::Stream { wake, notified, .. } => {
                *wake = w;
                *notified = false;
            }
        }
    }

    /// Whether this became readable and its owner has not been told: the
    /// queue to say it on and the word for it. A look, under the table's
    /// lock: one non-blocking peek or up to [`ACCEPT_SWEEP`] accepts. A
    /// connection at end-of-file is said once and then never again — the
    /// owner's next read sees the end, and there is nothing after it.
    fn became_ready(&mut self) -> Option<(String, &'static str)> {
        match self {
            Socket::Listener {
                listener,
                wake: Some(queue),
                notified,
                backlog,
                ..
            } => {
                if *notified {
                    return None;
                }
                while backlog.len() < ACCEPT_SWEEP {
                    match accept_interactive(listener) {
                        Ok(accepted) => backlog.push_back(accepted),
                        Err(_) => break,
                    }
                }
                if backlog.is_empty() {
                    return None;
                }
                *notified = true;
                Some((queue.clone(), "accept"))
            }
            Socket::Stream {
                stream,
                eof,
                wake: Some(queue),
                notified,
                ..
            } => {
                if *notified || *eof {
                    return None;
                }
                let readable = match stream.peek(&mut [0u8; 1]) {
                    Ok(0) => {
                        *eof = true;
                        true
                    }
                    Ok(_) => true,
                    Err(e) if e.kind() == ErrorKind::WouldBlock => false,
                    Err(_) => true,
                };
                if !readable {
                    return None;
                }
                *notified = true;
                Some((queue.clone(), "read"))
            }
            _ => None,
        }
    }

    fn is_vital(&self) -> bool {
        match self {
            Socket::Listener { vital, .. } | Socket::Stream { vital, .. } => *vital,
        }
    }

    fn set_vital(&mut self, v: bool) {
        match self {
            Socket::Listener { vital, .. } | Socket::Stream { vital, .. } => *vital = v,
        }
    }

    /// Whether this has ended on its own: a connection whose far end is
    /// gone, seen by a peek that costs one syscall. A listener never ends
    /// on its own.
    fn has_ended(&mut self) -> bool {
        match self {
            Socket::Listener { .. } => false,
            Socket::Stream { eof: true, .. } => true,
            Socket::Stream { stream, eof, .. } => match stream.peek(&mut [0u8; 1]) {
                Ok(0) => {
                    *eof = true;
                    true
                }
                Ok(_) => false,
                Err(e) if e.kind() == ErrorKind::WouldBlock => false,
                Err(_) => true,
            },
        }
    }

    /// What this is, for a sentence about it.
    fn name(&self) -> String {
        match self {
            Socket::Stream { peer, .. } => format!("connection with {peer}"),
            Socket::Listener { listener, .. } => format!(
                "listener on {}",
                listener
                    .local_addr()
                    .map(|a| a.to_string())
                    .unwrap_or_default()
            ),
        }
    }

    /// The reply that names this socket to whoever now holds it: a
    /// connection by its peer, a listener by its address.
    fn describe(&self, handle: HandleId) -> rmpv::Value {
        match self {
            Socket::Stream { peer, .. } => reply(vec![
                ("handle", handle.0.into()),
                ("peer", peer.to_string().into()),
            ]),
            Socket::Listener { listener, .. } => reply(vec![
                ("handle", handle.0.into()),
                (
                    "addr",
                    listener
                        .local_addr()
                        .map(|a| a.to_string())
                        .unwrap_or_default()
                        .into(),
                ),
            ]),
        }
    }

    /// What closing this loses, if anything. Consumes the socket, so the
    /// descriptor is gone by the time the sentence is read.
    fn lost(self) -> Option<String> {
        match self {
            Socket::Stream {
                peer, eof: false, ..
            } => Some(format!("a connection with {peer}, cut")),
            _ => None,
        }
    }
}

// depth: the scope, and its startup validation

impl SocketScope {
    fn parse(scope: Option<&Scope>) -> Result<Self, String> {
        let Some(Scope(value)) = scope else {
            return Ok(SocketScope::default());
        };
        if !value.is_map() {
            return Err("scope must be a table of bounds".into());
        }
        let parsed: SocketScope = rmpv::ext::from_value(value.clone())
            .map_err(|e| format!("scope does not parse: {e}"))?;
        for entry in parsed.allow.iter().flatten() {
            entry.parse::<SocketAddr>().map_err(|e| {
                format!("allow names '{entry}', which is not an ip:port literal: {e}")
            })?;
        }
        Ok(parsed)
    }

    /// Whether `addr` may be bound under this scope. Refuses by name, with
    /// the list, before anything is bound.
    fn permit(&self, addr: SocketAddr) -> Result<(), CallError> {
        let Some(allow) = &self.allow else {
            return Ok(());
        };
        let permitted = allow
            .iter()
            .filter_map(|entry| entry.parse::<SocketAddr>().ok())
            .any(|a| a.ip() == addr.ip() && (a.port() == 0 || a.port() == addr.port()));
        if permitted {
            Ok(())
        } else {
            Err(CallError::new(format!(
                "listen on {addr}: outside this deployment's allow list ({})",
                allow.join(", ")
            )))
        }
    }
}

// depth: the verbs

/// One try at a non-blocking operation. `Ok(None)` is "not yet": the
/// pump's cue to poll again next step.
type Try<T> = Result<Option<T>, CallError>;

/// Issue #33: a socket handed to a program is one hop of whatever that
/// program splices, and a splice is interactive -- with Nagle on, every
/// second small write waited for the first one's delayed ACK, 40 ms on
/// Linux. TCP_NODELAY goes on at accept. A failure to set it is a latency
/// cost rather than a refusal, and not one a supported target produces on
/// a connected socket.
fn accept_interactive(
    listener: &std::net::TcpListener,
) -> std::io::Result<(std::net::TcpStream, std::net::SocketAddr)> {
    let accepted = listener.accept()?;
    let _ = accepted.0.set_nodelay(true);
    Ok(accepted)
}

fn not_yet<T>(e: std::io::Error, what: &str) -> Try<T> {
    match e.kind() {
        ErrorKind::WouldBlock | ErrorKind::Interrupted => Ok(None),
        _ => Err(CallError::new(format!("{what}: {e}"))),
    }
}

fn not_a_connection(what: &str) -> CallError {
    CallError::new(format!(
        "{what}: the handle is a listener, not a connection"
    ))
}

impl SocketConnector {
    fn listen(&self, caller: Caller, scope: &SocketScope, args: Option<rmpv::Value>) -> CallResult {
        let args = args.ok_or_else(|| CallError::new("args.addr must be the address to bind"))?;
        let addr = string_field(&args, "addr")?
            .ok_or_else(|| CallError::new("args.addr must be the address to bind"))?;
        let addr: SocketAddr = addr
            .parse()
            .map_err(|e| CallError::new(format!("addr '{addr}' is not an ip:port literal: {e}")))?;
        scope.permit(addr)?;
        let listener = TcpListener::bind(addr)
            .map_err(|e| CallError::new(format!("listen on {addr}: {e}")))?;
        listener
            .set_nonblocking(true)
            .map_err(|e| CallError::new(format!("listen on {addr}: {e}")))?;
        let bound = listener
            .local_addr()
            .map_err(|e| CallError::new(format!("listen on {addr}: {e}")))?;
        let vital = vital_arg(&args);
        let wake = wake_arg(&args);
        let handle = self.sockets.insert(
            caller,
            Socket::Listener {
                listener,
                vital,
                wake,
                notified: false,
                backlog: VecDeque::new(),
            },
        );
        Ok(reply(vec![
            ("handle", handle.0.into()),
            ("addr", bound.to_string().into()),
        ]))
    }

    async fn accept(&self, caller: Caller, args: Option<rmpv::Value>) -> CallResult {
        let handle = handle_arg(args.as_ref())?;
        let vital = args.as_ref().is_some_and(vital_arg);
        let wake = args.as_ref().and_then(wake_arg);
        let (stream, peer) = self
            .until_ready(caller, handle, |socket| match socket {
                Socket::Listener {
                    listener,
                    backlog,
                    notified,
                    ..
                } => {
                    // Using the listener re-arms its readiness.
                    *notified = false;
                    if let Some(taken) = backlog.pop_front() {
                        return Ok(Some(taken));
                    }
                    match accept_interactive(listener) {
                        Ok(accepted) => Ok(Some(accepted)),
                        Err(e) => not_yet(e, "accept"),
                    }
                }
                Socket::Stream { .. } => Err(CallError::new(
                    "accept: the handle is a connection, not a listener",
                )),
            })
            .await?;
        // Not inherited from the listener on every platform, so said again.
        stream
            .set_nonblocking(true)
            .map_err(|e| CallError::new(format!("accept: {e}")))?;
        let handle = self.sockets.insert(
            caller,
            Socket::Stream {
                stream,
                peer,
                eof: false,
                vital,
                wake,
                notified: false,
            },
        );
        Ok(reply(vec![
            ("handle", handle.0.into()),
            ("peer", peer.to_string().into()),
        ]))
    }

    async fn read(&self, caller: Caller, args: Option<rmpv::Value>) -> CallResult {
        let handle = handle_arg(args.as_ref())?;
        let max = match args.as_ref().and_then(|a| field(a, "max")) {
            None | Some(rmpv::Value::Nil) => READ_MAX,
            Some(value) => {
                let Some(asked) = value.as_u64().filter(|n| *n > 0) else {
                    return Err(CallError::new("args.max must be a positive integer"));
                };
                if asked as usize > READ_MAX {
                    return Err(CallError::new(format!(
                        "max passed the host's cap ({READ_MAX} bytes); a call may ask for less, \
                         never more"
                    )));
                }
                asked as usize
            }
        };
        let mut buf = vec![0u8; max];
        let n = self
            .until_ready(caller, handle, |socket| match socket {
                Socket::Stream {
                    stream,
                    eof,
                    notified,
                    ..
                } => match stream.read(&mut buf) {
                    Ok(0) => {
                        *eof = true;
                        *notified = false;
                        Ok(Some(0))
                    }
                    Ok(n) => {
                        // Reading re-arms readiness: the next bytes are news.
                        *notified = false;
                        Ok(Some(n))
                    }
                    Err(e) => not_yet(e, "read"),
                },
                Socket::Listener { .. } => Err(not_a_connection("read")),
            })
            .await?;
        buf.truncate(n);
        Ok(reply(vec![
            ("data", rmpv::Value::Binary(buf)),
            ("eof", (n == 0).into()),
        ]))
    }

    async fn write(&self, caller: Caller, args: Option<rmpv::Value>) -> CallResult {
        let handle = handle_arg(args.as_ref())?;
        let data = args
            .as_ref()
            .and_then(|a| field(a, "data"))
            .and_then(string_bytes)
            .ok_or_else(|| CallError::new("args.data must be a string"))?
            .to_vec();
        if data.len() > WRITE_MAX {
            return Err(CallError::new(format!(
                "data is bigger than the host's cap ({WRITE_MAX} bytes)"
            )));
        }
        // Progress is kept across polls: a partial write leaves the call
        // pending with `off` where it got to.
        let mut off = 0usize;
        let len = data.len();
        self.until_ready(caller, handle, |socket| match socket {
            Socket::Stream { stream, .. } => loop {
                if off == len {
                    return Ok(Some(()));
                }
                match stream.write(&data[off..]) {
                    Ok(0) => {
                        return Err(CallError::new("write: the connection took no bytes"));
                    }
                    Ok(n) => off += n,
                    Err(e) => return not_yet(e, "write"),
                }
            },
            Socket::Listener { .. } => Err(not_a_connection("write")),
        })
        .await?;
        Ok(reply(vec![("written", (len as u64).into())]))
    }

    fn close(&self, caller: Caller, args: Option<rmpv::Value>) -> CallResult {
        let handle = handle_arg(args.as_ref())?;
        let socket = self
            .sockets
            .remove(caller, handle)
            .map_err(|e| CallError::new(e.to_string()))?;
        // Closing one's own vital handle is ending it: the owner ends too.
        if socket.is_vital() {
            self.ended.lock().unwrap_or_else(|e| e.into_inner()).push((
                caller,
                format!("its vital {} was closed by its owner", socket.name()),
            ));
        }
        // Closing withdraws any offer of it: there is nothing left to claim.
        self.offers()
            .retain(|o| !(o.from == caller && o.handle == handle));
        Ok(rmpv::Value::Nil)
    }

    fn transfer(&self, caller: Caller, args: Option<rmpv::Value>) -> CallResult {
        let handle = handle_arg(args.as_ref())?;
        let to = args
            .as_ref()
            .and_then(|a| field(a, "to"))
            .and_then(|v| v.as_u64())
            .and_then(|n| u32::try_from(n).ok())
            .map(Caller::Node)
            .ok_or_else(|| CallError::new("args.to must be an instance id"))?;
        if to == caller {
            return Err(CallError::new("transfer to self"));
        }
        // The holder's, or the constant: an offer of someone else's handle
        // must read exactly like an offer of nothing.
        self.sockets
            .with(caller, handle, |_| ())
            .map_err(|e| CallError::new(e.to_string()))?;
        let mut offers = self.offers();
        // A second transfer of the same handle replaces the first: the
        // holder may change its mind until someone claims.
        offers.retain(|o| !(o.from == caller && o.handle == handle));
        offers.push(Offer {
            from: caller,
            to,
            handle,
        });
        Ok(rmpv::Value::Nil)
    }

    async fn claim(&self, caller: Caller, args: Option<rmpv::Value>) -> CallResult {
        let named = match args.as_ref().and_then(|a| field(a, "handle")) {
            None | Some(rmpv::Value::Nil) => None,
            Some(v) => Some(
                v.as_u64()
                    .map(HandleId)
                    .ok_or_else(|| CallError::new("args.handle must be a handle number"))?,
            ),
        };
        // Named: that offer or nothing, now. Unnamed: the oldest offer
        // addressed here, and until one exists the call is pending.
        let (from, handle) = poll_fn(|_| {
            let mut offers = self.offers();
            let at = offers
                .iter()
                .position(|o| o.to == caller && named.is_none_or(|h| o.handle == h));
            match at {
                Some(i) => {
                    let o = offers.remove(i);
                    Poll::Ready(Ok((o.from, o.handle)))
                }
                None if named.is_some() => Poll::Ready(Err(CallError::new("no such handle"))),
                None => Poll::Pending,
            }
        })
        .await?;
        self.sockets
            .rekey(from, caller, handle)
            .map_err(|e| CallError::new(e.to_string()))?;
        // The claimant's declaration, fresh: the flag is the owner's, and
        // the owner just changed.
        let vital = args.as_ref().is_some_and(vital_arg);
        let wake = args.as_ref().and_then(wake_arg);
        self.sockets
            .with(caller, handle, |s| {
                s.set_vital(vital);
                s.set_wake(wake);
                s.describe(handle)
            })
            .map_err(|e| CallError::new(e.to_string()))
    }

    fn offers(&self) -> std::sync::MutexGuard<'_, Vec<Offer>> {
        self.offers.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Drive one non-blocking operation to completion in the pump: every
    /// poll is one try against the caller's socket, and a try that would
    /// block leaves the call pending for the next step. A handle the caller
    /// does not hold is "no such handle" on the first poll, and stays so.
    async fn until_ready<T>(
        &self,
        caller: Caller,
        handle: HandleId,
        mut op: impl FnMut(&mut Socket) -> Try<T> + Send,
    ) -> Result<T, CallError> {
        poll_fn(|_| match self.sockets.with(caller, handle, &mut op) {
            Ok(Ok(Some(value))) => Poll::Ready(Ok(value)),
            Ok(Ok(None)) => Poll::Pending,
            Ok(Err(e)) => Poll::Ready(Err(e)),
            Err(no_such) => Poll::Ready(Err(CallError::new(no_such.to_string()))),
        })
        .await
    }
}

// depth: reading the request

/// `vital = true`, or nothing. Anything else is nothing: a flag that ties
/// a lifetime is not inferred from a truthy value.
/// `wake = true` is the owner's `inbox`; `wake = "<queue>"` is that queue;
/// anything else is no wake.
fn wake_arg(args: &rmpv::Value) -> Option<String> {
    match field(args, "wake") {
        Some(rmpv::Value::Boolean(true)) => Some("inbox".into()),
        Some(rmpv::Value::String(s)) => s.as_str().map(String::from),
        _ => None,
    }
}

fn vital_arg(args: &rmpv::Value) -> bool {
    field(args, "vital").and_then(|v| v.as_bool()) == Some(true)
}

fn handle_arg(args: Option<&rmpv::Value>) -> Result<HandleId, CallError> {
    args.and_then(|a| field(a, "handle"))
        .and_then(|v| v.as_u64())
        .map(HandleId)
        .ok_or_else(|| CallError::new("args.handle must be a handle number"))
}

fn string_field<'a>(map: &'a rmpv::Value, name: &str) -> Result<Option<&'a str>, CallError> {
    match field(map, name) {
        None | Some(rmpv::Value::Nil) => Ok(None),
        Some(rmpv::Value::String(s)) => s
            .as_str()
            .map(Some)
            .ok_or_else(|| CallError::new(format!("args.{name} must be a string"))),
        Some(_) => Err(CallError::new(format!("args.{name} must be a string"))),
    }
}

fn field<'a>(map: &'a rmpv::Value, name: &str) -> Option<&'a rmpv::Value> {
    map.as_map()?
        .iter()
        .find(|(k, _)| k.as_str() == Some(name))
        .map(|(_, v)| v)
}

/// The bytes of a msgpack `str` or `bin`. The guest's codec reads both into
/// one token, so both are accepted here.
fn string_bytes(value: &rmpv::Value) -> Option<&[u8]> {
    match value {
        rmpv::Value::String(s) => Some(s.as_bytes()),
        rmpv::Value::Binary(b) => Some(b),
        _ => None,
    }
}

fn reply(entries: Vec<(&str, rmpv::Value)>) -> rmpv::Value {
    rmpv::Value::Map(
        entries
            .into_iter()
            .map(|(k, v)| (rmpv::Value::from(k), v))
            .collect(),
    )
}
