//! The net connector: a connection to an opaque address, which a node
//! holds by handle and a host-supplied [`Transport`] carries (issue #80).
//!
//! ```text
//!   net/open   {address, queue?, vital?}  -> {handle = n}
//!   net/listen {address, queue?}          -> {handle = n}
//!   net/accept {handle, queue?, vital?}   -> {handle = m, peer = "..."}
//!   net/send   {handle, data}             -> nil
//!   net/recv   {handle}                   -> {data = <bytes>} | {eof = true, reason?}
//!   net/close  {handle}                   -> nil
//!   net/transfer {handle, to}             -> nil                      (the holder)
//!   net/claim  {handle?, queue?, vital?}  -> {handle, address | peer} (the target)
//! ```
//!
//! **The address means nothing to DRT.** It is a string the transport
//! resolves: a card's name on an in-page bus, a peer id the browser
//! access client dials, anything. That is what lets a node's code be the
//! same whichever transport the host handed in; the node only ever says
//! `open(address)` and `send(handle, bytes)`.
//!
//! **Messages, not a byte stream.** One `send` arrives as one `data` at
//! the far end, whole. A transport that cannot keep that (a raw stream)
//! is not a transport for this connector.
//!
//! **The table is `drt-connector`'s [`Handles`].** A connection belongs to
//! the node that opened or accepted it, closes when that node is released,
//! survives its hibernation, moves to another node by `transfer` and
//! `claim` under the same number, and with `vital` ends the node when it
//! ends. The rules and their reasons are the socket connector's
//! (`connectors/socket`, `doc/Plan-0.7.0.md` §2-§3), and the verbs are its
//! verbs wherever the two mean the same thing.
//!
//! **Inbound has two shapes, chosen per handle.** With `queue` (a queue
//! name, or `true` for `inbox`), what arrives is pushed to the owner's
//! queue as it arrives: `{event = "data", handle, data}`,
//! `{event = "closed", handle, reason?}`, and on a listener
//! `{event = "accepted", handle, conn, peer}`, where `conn` is the new
//! connection's handle, already the owner's and pushing to the same
//! queue. A push is what wakes an owner parked with `wake_on_message`.
//! Without `queue`, inbound waits in the host for `recv` and `accept`,
//! which wait in turn until there is something; that is the shape for a
//! program `drt run` drives, which has no queue sweep.
//!
//! A push the swarm cannot deliver (the queue full, the owner parked
//! without asking to be woken) is dropped like any push, and for `data`
//! that is the message gone. A queue is for an owner that keeps up; a
//! buffered handle is held to [`RECV_BACKLOG`] unread messages, past which
//! the connection is closed and `recv` says why.
//!
//! **The transport is the host's.** In a page it is a JavaScript object
//! (`crates/drt-web`'s `js` module); in a test it is [`bus::Bus`]. Its four
//! methods are below, and the [`Link`] and [`Acceptor`] it is handed are
//! how it reports what arrives.

use std::collections::VecDeque;
use std::future::poll_fn;
use std::sync::{Arc, Mutex, MutexGuard};
use std::task::Poll;

use drt_caps::Scope;
use drt_connector::{
    Asker, CallError, CallResult, Caller, Connector, HandleId, Handles, NoSuchHandle, Notice,
};

// ---------------------------------------------------------------------------
// Surface. [`NetConnector`] answers the eight calls `Connector::call_as`
// dispatches below, over whatever [`Transport`] it was built with; the
// three values are the bounds a deployment does not tune.
// ---------------------------------------------------------------------------

/// Unread messages one buffered connection holds before the connector
/// closes it. The transport cannot be told to slow down, so the bound is
/// a refusal rather than a wait.
pub const RECV_BACKLOG: usize = 1024;
/// Unaccepted connections one buffered listener holds; past it a new one
/// is closed as it arrives.
pub const ACCEPT_BACKLOG: usize = 64;
/// The most one `send` takes: 1 MiB, the socket connector's write cap.
pub const SEND_MAX: usize = 1024 * 1024;

/// What carries the bytes. The host implements it; the connector holds
/// the handles, the owners and the queues, and asks the transport only to
/// move bytes.
///
/// `Conn` is whatever the transport uses to name one connection or one
/// listener: a JavaScript object, an index. The connector clones it to
/// hand it back on `send` and `close`, and never looks inside.
#[async_trait::async_trait]
pub trait Transport: Send + Sync + 'static {
    type Conn: Clone + Send + Sync + 'static;

    /// Connect to `address`. Bytes from the far end go to `link`, from the
    /// moment this is called: a transport may deliver before it answers.
    async fn open(&self, address: &str, link: Link<Self::Conn>) -> Result<Self::Conn, String>;

    /// Accept connections at `address`. Each one is reported through
    /// `accept`, which hands back the [`Link`] its bytes go to.
    async fn listen(
        &self,
        address: &str,
        accept: Acceptor<Self::Conn>,
    ) -> Result<Self::Conn, String>;

    /// Send one message on a connection `open` or an [`Acceptor`] gave the
    /// connector.
    async fn send(&self, conn: &Self::Conn, data: Vec<u8>) -> Result<(), String>;

    /// Close a connection or a listener. Called once per `Conn`, and never
    /// for one the transport already reported closed.
    fn close(&self, conn: &Self::Conn);
}

/// The connector over one transport.
pub struct NetConnector<T: Transport> {
    transport: T,
    inbox: Arc<Inbox<T::Conn>>,
    entries: Handles<Entry<T::Conn>>,
    /// `transfer`s not yet claimed, oldest first.
    offers: Mutex<Vec<Offer>>,
    /// Owners whose vital connection ended, for the next `ended`.
    ended: Mutex<Vec<(Caller, String)>>,
    /// Pushes a sweep produced, for the next `notices`.
    pushes: Mutex<Vec<Notice>>,
}

impl<T: Transport> NetConnector<T> {
    pub fn new(transport: T) -> Self {
        NetConnector {
            transport,
            inbox: Arc::new(Inbox::default()),
            entries: Handles::new("connection"),
            offers: Mutex::new(Vec::new()),
            ended: Mutex::new(Vec::new()),
            pushes: Mutex::new(Vec::new()),
        }
    }

    /// The transport, for a host that has to reach it again.
    pub fn transport(&self) -> &T {
        &self.transport
    }
}

#[async_trait::async_trait]
impl<T: Transport> Connector for NetConnector<T> {
    /// The caller-blind path is the root: what it opens is the root's and
    /// lives to [`Connector::finish`].
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
        _scope: Option<&Scope>,
    ) -> CallResult {
        let caller = asker.caller;
        let args = args.unwrap_or(rmpv::Value::Nil);
        match call {
            "net/open" => self.open(caller, &args).await,
            "net/listen" => self.listen(caller, &args).await,
            "net/accept" => self.accept(caller, &args).await,
            "net/send" => self.send(caller, &args).await,
            "net/recv" => self.recv(caller, &args).await,
            "net/close" => self.close(caller, &args),
            "net/transfer" => self.transfer(caller, &args),
            "net/claim" => self.claim(caller, &args).await,
            other => Err(CallError::new(format!("net: unknown call '{other}'"))),
        }
    }

    /// The owner is gone: everything it held closes, and each connection
    /// whose far end was still there is one line of the report.
    fn release(&self, caller: &Caller) -> Vec<String> {
        self.offers()
            .retain(|o| o.from != *caller && o.to != *caller);
        let released = self.entries.release(*caller);
        self.close_all(released.into_iter().map(|(_, e)| e))
    }

    fn ended(&self) -> Vec<(Caller, String)> {
        self.sweep();
        std::mem::take(&mut *lock(&self.ended))
    }

    fn notices(&self) -> Vec<Notice> {
        self.sweep();
        std::mem::take(&mut *lock(&self.pushes))
    }

    fn finish(&self) -> Vec<String> {
        self.offers()
            .retain(|o| o.from != Caller::Root && o.to != Caller::Root);
        let drained = self.entries.drain_root();
        self.close_all(drained.into_iter().map(|(_, e)| e))
    }
}

/// The connector going away (the command that wired it is over) closes
/// whatever is still open, whoever held it: a transport's connections are
/// not the page's to find and close by hand.
impl<T: Transport> Drop for NetConnector<T> {
    fn drop(&mut self) {
        let left = self.entries.take_where(|_, _, _| true);
        self.close_all(left.into_iter().map(|(_, _, e)| e));
    }
}

// depth: what the transport reports, and how it reaches an owner

/// One connection's report line. Cloned into the transport; each method
/// is a look, never a wait, so a transport may call it from a callback.
pub struct Link<C> {
    key: u64,
    inbox: Arc<Inbox<C>>,
}

impl<C> Clone for Link<C> {
    fn clone(&self) -> Self {
        Link {
            key: self.key,
            inbox: Arc::clone(&self.inbox),
        }
    }
}

impl<C> Link<C> {
    /// One message from the far end.
    pub fn receive(&self, data: Vec<u8>) {
        self.inbox.push(Event::Data {
            key: self.key,
            data,
        });
    }

    /// The far end is gone, or the transport gave up on it. `reason` is
    /// shown to the owner as it is.
    pub fn closed(&self, reason: Option<String>) {
        self.inbox.push(Event::Closed {
            key: self.key,
            reason,
        });
    }
}

/// One listener's report line.
pub struct Acceptor<C> {
    key: u64,
    inbox: Arc<Inbox<C>>,
}

impl<C> Clone for Acceptor<C> {
    fn clone(&self) -> Self {
        Acceptor {
            key: self.key,
            inbox: Arc::clone(&self.inbox),
        }
    }
}

impl<C> Acceptor<C> {
    /// A connection arrived from `peer`. `conn` is what the transport will
    /// be handed back to send on it and close it; the answer is where its
    /// bytes go.
    pub fn connection(&self, conn: C, peer: String) -> Link<C> {
        let key = self.inbox.key();
        self.inbox.push(Event::Accepted {
            listener: self.key,
            key,
            conn,
            peer,
        });
        Link {
            key,
            inbox: Arc::clone(&self.inbox),
        }
    }
}

/// Everything the transport has said, in order, until a sweep reads it.
/// Its own lock, so a transport reporting from inside a `send` or `close`
/// never waits on the handle table.
struct Inbox<C> {
    state: Mutex<InboxState<C>>,
}

struct InboxState<C> {
    next: u64,
    events: VecDeque<Event<C>>,
}

impl<C> Default for Inbox<C> {
    fn default() -> Self {
        Inbox {
            state: Mutex::new(InboxState {
                next: 1,
                events: VecDeque::new(),
            }),
        }
    }
}

impl<C> Inbox<C> {
    fn key(&self) -> u64 {
        let mut state = lock(&self.state);
        let key = state.next;
        state.next += 1;
        key
    }

    fn push(&self, event: Event<C>) {
        lock(&self.state).events.push_back(event);
    }

    fn drain(&self) -> VecDeque<Event<C>> {
        std::mem::take(&mut lock(&self.state).events)
    }
}

enum Event<C> {
    Data {
        key: u64,
        data: Vec<u8>,
    },
    Closed {
        key: u64,
        reason: Option<String>,
    },
    Accepted {
        listener: u64,
        key: u64,
        conn: C,
        peer: String,
    },
}

/// What one handle is. `conn` is `None` only while `open` or `listen` is
/// still waiting on the transport, which is what lets a transport deliver
/// before it answers: the entry, and so the owner, exists first.
struct Entry<C> {
    key: u64,
    conn: Option<C>,
    listener: bool,
    /// The address opened or listened on, or the peer an accept named.
    name: String,
    queue: Option<String>,
    vital: bool,
    inbound: VecDeque<Vec<u8>>,
    /// An accepted connection no `accept` has taken yet: the listener it
    /// waits under. Already an entry, so what arrives on it before the
    /// accept is kept, but no handle to it has been issued, and every verb
    /// but `accept` reads it as no such handle.
    waiting: Option<HandleId>,
    /// `Some` once the far end is gone, with the transport's reason.
    gone: Option<Option<String>>,
}

impl<C> Entry<C> {
    fn new(key: u64, listener: bool, name: String, queue: Option<String>, vital: bool) -> Self {
        Entry {
            key,
            conn: None,
            listener,
            name,
            queue,
            vital,
            inbound: VecDeque::new(),
            waiting: None,
            gone: None,
        }
    }

    fn what(&self) -> String {
        if self.listener {
            format!("listener on {}", self.name)
        } else {
            format!("connection to {}", self.name)
        }
    }

    /// What arrived while the entry buffered, as pushes, now that its
    /// owner has named a queue (an `accept` or `claim` that says `queue`).
    fn flush(&mut self, owner: Caller, handle: HandleId) -> Vec<Notice> {
        let Some(queue) = self.queue.clone() else {
            return Vec::new();
        };
        let mut pushes: Vec<Notice> = self
            .inbound
            .drain(..)
            .map(|data| Notice {
                owner,
                queue: queue.clone(),
                message: reply(vec![
                    ("event", "data".into()),
                    ("handle", handle.0.into()),
                    ("data", rmpv::Value::Binary(data)),
                ]),
            })
            .collect();
        if let Some(why) = &self.gone {
            let mut fields = vec![("event", "closed".into()), ("handle", handle.0.into())];
            if let Some(why) = why {
                fields.push(("reason", why.as_str().into()));
            }
            pushes.push(Notice {
                owner,
                queue,
                message: reply(fields),
            });
        }
        pushes
    }

    fn describe(&self, handle: HandleId) -> rmpv::Value {
        let key = if self.listener { "address" } else { "peer" };
        reply(vec![
            ("handle", handle.0.into()),
            (key, self.name.as_str().into()),
        ])
    }
}

struct Offer {
    from: Caller,
    to: Caller,
    handle: HandleId,
}

impl<T: Transport> NetConnector<T> {
    /// Read what the transport said and put each thing where it belongs:
    /// a push for an owner that named a queue, a buffer for one that did
    /// not, an ending for an owner whose vital connection went. Called by
    /// every sweep and every wait, so `drt run`, which sweeps nothing,
    /// still sees its bytes.
    fn sweep(&self) {
        let events = self.inbox.drain();
        if events.is_empty() {
            return;
        }
        let mut close = Vec::new();
        let mut pushes = Vec::new();
        let mut ended = Vec::new();
        for event in events {
            match event {
                Event::Data { key, data } => self.entries.each(|owner, handle, e| {
                    if e.key != key || e.gone.is_some() {
                        return;
                    }
                    if let Some(queue) = &e.queue {
                        pushes.push(Notice {
                            owner,
                            queue: queue.clone(),
                            message: reply(vec![
                                ("event", "data".into()),
                                ("handle", handle.0.into()),
                                ("data", rmpv::Value::Binary(data.clone())),
                            ]),
                        });
                    } else if e.inbound.len() >= RECV_BACKLOG {
                        let why = format!("more than {RECV_BACKLOG} messages arrived unread");
                        if let Some(conn) = &e.conn {
                            close.push(conn.clone());
                        }
                        if e.vital {
                            ended.push((owner, format!("its vital {} ended: {why}", e.what())));
                        }
                        e.gone = Some(Some(why));
                    } else {
                        e.inbound.push_back(data.clone());
                    }
                }),
                Event::Closed { key, reason } => self.entries.each(|owner, handle, e| {
                    if e.key != key || e.gone.is_some() {
                        return;
                    }
                    if let Some(queue) = &e.queue {
                        let mut fields =
                            vec![("event", "closed".into()), ("handle", handle.0.into())];
                        if let Some(why) = &reason {
                            fields.push(("reason", why.as_str().into()));
                        }
                        pushes.push(Notice {
                            owner,
                            queue: queue.clone(),
                            message: reply(fields),
                        });
                    }
                    if e.vital {
                        ended.push((owner, format!("its vital {} ended", e.what())));
                    }
                    e.gone = Some(reason.clone());
                }),
                Event::Accepted {
                    listener,
                    key,
                    conn,
                    peer,
                } => {
                    let mut taken = None;
                    self.entries.each(|owner, handle, e| {
                        if e.key == listener && e.listener && e.gone.is_none() {
                            taken = Some((owner, handle, e.queue.clone()));
                        }
                    });
                    let Some((owner, handle, queue)) = taken else {
                        // The listener is closed or gone: nobody will ever
                        // read this one.
                        close.push(conn);
                        continue;
                    };
                    if queue.is_none() && self.waiting(owner, handle) >= ACCEPT_BACKLOG {
                        close.push(conn);
                        continue;
                    }
                    // Inserted now, outside the walk, so the bytes behind
                    // it in this same drain find their entry.
                    let mut entry = Entry::new(key, false, peer.clone(), queue.clone(), false);
                    entry.conn = Some(conn);
                    match queue {
                        Some(queue) => {
                            let conn = self.entries.insert(owner, entry);
                            pushes.push(Notice {
                                owner,
                                queue,
                                message: reply(vec![
                                    ("event", "accepted".into()),
                                    ("handle", handle.0.into()),
                                    ("conn", conn.0.into()),
                                    ("peer", peer.into()),
                                ]),
                            });
                        }
                        None => {
                            entry.waiting = Some(handle);
                            self.entries.insert(owner, entry);
                        }
                    }
                }
            }
        }
        lock(&self.pushes).extend(pushes);
        lock(&self.ended).extend(ended);
        for conn in close {
            self.transport.close(&conn);
        }
    }

    /// How many connections wait under `owner`'s listener `listener`.
    fn waiting(&self, owner: Caller, listener: HandleId) -> usize {
        let mut n = 0;
        self.entries.each(|o, _, e| {
            if o == owner && e.waiting == Some(listener) {
                n += 1;
            }
        });
        n
    }

    /// One of `caller`'s issued handles. An accepted connection still
    /// waiting for `accept` reads exactly like a number never issued.
    fn mine<R>(
        &self,
        caller: Caller,
        handle: HandleId,
        f: impl FnOnce(&mut Entry<T::Conn>) -> R,
    ) -> Result<R, CallError> {
        self.entries
            .with(caller, handle, |e| match e.waiting {
                Some(_) => None,
                None => Some(f(e)),
            })
            .ok()
            .flatten()
            .ok_or_else(|| CallError::new(NoSuchHandle.to_string()))
    }

    async fn open(&self, caller: Caller, args: &rmpv::Value) -> CallResult {
        let address = address_arg(args)?;
        let queue = queue_arg(args)?;
        let key = self.inbox.key();
        let handle = self.entries.insert(
            caller,
            Entry::new(key, false, address.clone(), queue, vital_arg(args)),
        );
        let link = Link {
            key,
            inbox: Arc::clone(&self.inbox),
        };
        let opened = self.transport.open(&address, link).await;
        self.settle(caller, handle, opened)?;
        Ok(reply(vec![("handle", handle.0.into())]))
    }

    async fn listen(&self, caller: Caller, args: &rmpv::Value) -> CallResult {
        let address = address_arg(args)?;
        let queue = queue_arg(args)?;
        let key = self.inbox.key();
        let handle = self
            .entries
            .insert(caller, Entry::new(key, true, address.clone(), queue, false));
        let accept = Acceptor {
            key,
            inbox: Arc::clone(&self.inbox),
        };
        let listening = self.transport.listen(&address, accept).await;
        self.settle(caller, handle, listening)?;
        Ok(reply(vec![("handle", handle.0.into())]))
    }

    /// Put the transport's answer into the entry made before asking it.
    /// A refusal removes the entry; an answer for an entry that is gone
    /// (the owner died while it waited) is closed at once.
    fn settle(
        &self,
        caller: Caller,
        handle: HandleId,
        answer: Result<T::Conn, String>,
    ) -> Result<(), CallError> {
        match answer {
            Ok(conn) => {
                let kept = self
                    .entries
                    .with(caller, handle, |e| e.conn = Some(conn.clone()));
                if kept.is_err() {
                    self.transport.close(&conn);
                }
                Ok(())
            }
            Err(why) => {
                let _ = self.entries.remove(caller, handle);
                Err(CallError::new(why))
            }
        }
    }

    async fn accept(&self, caller: Caller, args: &rmpv::Value) -> CallResult {
        let listener = handle_arg(args)?;
        let queue = queue_arg(args)?;
        let vital = vital_arg(args);
        let handle = poll_fn(|_| {
            self.sweep();
            let looked = self.mine(caller, listener, |e| {
                if !e.listener {
                    return Err(CallError::new(
                        "accept on a connection; it takes a listener",
                    ));
                }
                if let Some(q) = &e.queue {
                    return Err(CallError::new(format!(
                        "this listener pushes its connections to '{q}'; there is nothing to accept"
                    )));
                }
                Ok(e.gone.as_ref().map(|why| gone_sentence(e, why)))
            });
            let gone = match looked {
                Err(e) | Ok(Err(e)) => return Poll::Ready(Err(e)),
                Ok(Ok(gone)) => gone,
            };
            // The oldest waiting connection: handles only grow.
            let mut oldest = None;
            self.entries.each(|owner, handle, e| {
                if owner == caller && e.waiting == Some(listener) && oldest.is_none() {
                    oldest = Some(handle);
                }
            });
            match (oldest, gone) {
                (Some(handle), _) => Poll::Ready(Ok(handle)),
                (None, Some(why)) => Poll::Ready(Err(CallError::new(why))),
                (None, None) => Poll::Pending,
            }
        })
        .await?;
        // Taken: the accept's declarations, and anything that arrived
        // while it waited goes where the accept now says.
        let peer = self
            .entries
            .with(caller, handle, |e| {
                e.waiting = None;
                e.vital = vital;
                e.queue = queue;
                lock(&self.pushes).extend(e.flush(caller, handle));
                e.name.clone()
            })
            .map_err(|e| CallError::new(e.to_string()))?;
        Ok(reply(vec![
            ("handle", handle.0.into()),
            ("peer", peer.into()),
        ]))
    }

    async fn send(&self, caller: Caller, args: &rmpv::Value) -> CallResult {
        let handle = handle_arg(args)?;
        let data = match field(args, "data") {
            Some(rmpv::Value::Binary(b)) => b.clone(),
            Some(rmpv::Value::String(s)) => s.as_bytes().to_vec(),
            _ => return Err(CallError::new("args.data must be bytes or a string")),
        };
        if data.len() > SEND_MAX {
            return Err(CallError::new(format!(
                "a send of {} bytes is over the {SEND_MAX}-byte limit",
                data.len()
            )));
        }
        let conn = self.mine(caller, handle, |e| {
            if e.listener {
                return Err(CallError::new("send on a listener; it takes a connection"));
            }
            if let Some(why) = &e.gone {
                return Err(CallError::new(gone_sentence(e, why)));
            }
            e.conn
                .clone()
                .ok_or_else(|| CallError::new("the connection is still opening"))
        })??;
        self.transport
            .send(&conn, data)
            .await
            .map_err(CallError::new)?;
        Ok(rmpv::Value::Nil)
    }

    async fn recv(&self, caller: Caller, args: &rmpv::Value) -> CallResult {
        let handle = handle_arg(args)?;
        poll_fn(|_| {
            self.sweep();
            let looked = self.mine(caller, handle, |e| {
                if e.listener {
                    return Poll::Ready(Err(CallError::new(
                        "recv on a listener; it takes a connection",
                    )));
                }
                if let Some(q) = &e.queue {
                    return Poll::Ready(Err(CallError::new(format!(
                        "this connection pushes what arrives to '{q}'; there is nothing to recv"
                    ))));
                }
                if let Some(data) = e.inbound.pop_front() {
                    return Poll::Ready(Ok(reply(vec![("data", rmpv::Value::Binary(data))])));
                }
                match &e.gone {
                    Some(why) => {
                        let mut fields = vec![("eof", rmpv::Value::Boolean(true))];
                        if let Some(why) = why {
                            fields.push(("reason", why.as_str().into()));
                        }
                        Poll::Ready(Ok(reply(fields)))
                    }
                    None => Poll::Pending,
                }
            });
            match looked {
                Ok(answer) => answer,
                Err(e) => Poll::Ready(Err(e)),
            }
        })
        .await
    }

    fn close(&self, caller: Caller, args: &rmpv::Value) -> CallResult {
        let handle = handle_arg(args)?;
        self.mine(caller, handle, |_| ())?;
        let entry = self
            .entries
            .remove(caller, handle)
            .map_err(|e| CallError::new(e.to_string()))?;
        // A listener's waiting connections go with it.
        let waiting = self
            .entries
            .take_where(|owner, _, e| owner == caller && e.waiting == Some(handle));
        self.close_all(waiting.into_iter().map(|(_, _, e)| e));
        // A close withdraws the holder's offer of it, if it made one.
        self.offers()
            .retain(|o| !(o.from == caller && o.handle == handle));
        if entry.vital {
            lock(&self.ended).push((caller, format!("it closed its vital {}", entry.what())));
        }
        self.close_all(std::iter::once(entry));
        Ok(rmpv::Value::Nil)
    }

    fn transfer(&self, caller: Caller, args: &rmpv::Value) -> CallResult {
        let handle = handle_arg(args)?;
        let to = field(args, "to")
            .and_then(|v| v.as_u64())
            .and_then(|n| u32::try_from(n).ok())
            .map(Caller::Node)
            .ok_or_else(|| CallError::new("args.to must be an instance id"))?;
        if to == caller {
            return Err(CallError::new("transfer to self"));
        }
        // The holder's, or the constant: an offer of someone else's handle
        // reads exactly like an offer of nothing.
        self.mine(caller, handle, |_| ())?;
        let mut offers = self.offers();
        offers.retain(|o| !(o.from == caller && o.handle == handle));
        offers.push(Offer {
            from: caller,
            to,
            handle,
        });
        Ok(rmpv::Value::Nil)
    }

    async fn claim(&self, caller: Caller, args: &rmpv::Value) -> CallResult {
        let named = match field(args, "handle") {
            None | Some(rmpv::Value::Nil) => None,
            Some(v) => Some(
                v.as_u64()
                    .map(HandleId)
                    .ok_or_else(|| CallError::new("args.handle must be a handle number"))?,
            ),
        };
        let queue = queue_arg(args)?;
        let vital = vital_arg(args);
        // Named: that offer or nothing, now. Unnamed: the oldest offer
        // addressed here, and until one exists the call waits.
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
        self.entries
            .rekey(from, caller, handle)
            .map_err(|e| CallError::new(e.to_string()))?;
        // The claimant's declarations, fresh: both are the owner's, and the
        // owner just changed.
        self.entries
            .with(caller, handle, |e| {
                e.vital = vital;
                e.queue = queue;
                lock(&self.pushes).extend(e.flush(caller, handle));
                e.describe(handle)
            })
            .map_err(|e| CallError::new(e.to_string()))
    }

    /// Close what was taken out of the table, and say which connections
    /// were cut with their far end still there.
    fn close_all(&self, entries: impl Iterator<Item = Entry<T::Conn>>) -> Vec<String> {
        let mut lost = Vec::new();
        for entry in entries {
            if entry.gone.is_some() {
                continue;
            }
            if let Some(conn) = &entry.conn {
                self.transport.close(conn);
            }
            // Nobody accepted it, so nothing of anyone's was cut.
            if !entry.listener && entry.waiting.is_none() {
                lost.push(format!("a {}, cut", entry.what()));
            }
        }
        lost
    }

    fn offers(&self) -> MutexGuard<'_, Vec<Offer>> {
        lock(&self.offers)
    }
}

// depth: argument reading

fn gone_sentence<C>(e: &Entry<C>, why: &Option<String>) -> String {
    match why {
        Some(why) => format!("the {} is closed: {why}", e.what()),
        None => format!("the {} is closed", e.what()),
    }
}

fn address_arg(args: &rmpv::Value) -> Result<String, CallError> {
    match field(args, "address") {
        Some(rmpv::Value::String(s)) if s.as_str().is_some_and(|s| !s.is_empty()) => {
            Ok(s.as_str().unwrap_or_default().to_string())
        }
        _ => Err(CallError::new("args.address must be a non-empty string")),
    }
}

/// `queue = "<name>"` pushes to that queue, `queue = true` to `inbox`,
/// absent or `false` buffers for `recv` and `accept`.
fn queue_arg(args: &rmpv::Value) -> Result<Option<String>, CallError> {
    match field(args, "queue") {
        None | Some(rmpv::Value::Nil) | Some(rmpv::Value::Boolean(false)) => Ok(None),
        Some(rmpv::Value::Boolean(true)) => Ok(Some("inbox".into())),
        Some(rmpv::Value::String(s)) if s.as_str().is_some_and(|s| !s.is_empty()) => {
            Ok(Some(s.as_str().unwrap_or_default().to_string()))
        }
        _ => Err(CallError::new(
            "args.queue must be a queue name, or true for the inbox",
        )),
    }
}

fn vital_arg(args: &rmpv::Value) -> bool {
    field(args, "vital").and_then(|v| v.as_bool()) == Some(true)
}

fn handle_arg(args: &rmpv::Value) -> Result<HandleId, CallError> {
    field(args, "handle")
        .and_then(|v| v.as_u64())
        .map(HandleId)
        .ok_or_else(|| CallError::new("args.handle must be a handle number"))
}

fn field<'a>(map: &'a rmpv::Value, name: &str) -> Option<&'a rmpv::Value> {
    map.as_map()?
        .iter()
        .find(|(k, _)| k.as_str() == Some(name))
        .map(|(_, v)| v)
}

fn reply(entries: Vec<(&str, rmpv::Value)>) -> rmpv::Value {
    rmpv::Value::Map(
        entries
            .into_iter()
            .map(|(k, v)| (rmpv::Value::from(k), v))
            .collect(),
    )
}

/// A poisoned lock is a panic mid-operation; every structure here is
/// consistent at each panic point, so recover rather than propagate.
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

pub mod bus {
    //! An in-process transport: every address is a name on one bus, and a
    //! connection is two ends that hand each other's messages straight
    //! across. The reference a host's transport is checked against, and
    //! what the tests here and in `drt-web` run on. The same shape as the
    //! in-page bus a page writes in JavaScript.

    use std::collections::BTreeMap;
    use std::sync::{Arc, Mutex};

    use super::{lock, Acceptor, Link, Transport};

    /// One end of a bus connection, or a listener's name.
    #[derive(Clone)]
    pub struct End(Arc<Mutex<EndState>>);

    struct EndState {
        /// The other end's link: where this end's sends arrive.
        far: Option<Link<End>>,
        /// This end's own link, so closing it tells the other side.
        near: Option<Link<End>>,
        other: Option<End>,
        listening: Option<String>,
    }

    impl End {
        fn new() -> Self {
            End(Arc::new(Mutex::new(EndState {
                far: None,
                near: None,
                other: None,
                listening: None,
            })))
        }
    }

    /// The bus: names to listeners, shared by every [`Bus`] cloned from
    /// one, so two connectors on one bus reach each other.
    #[derive(Clone, Default)]
    pub struct Bus {
        listeners: Arc<Mutex<BTreeMap<String, Acceptor<End>>>>,
    }

    impl Bus {
        pub fn new() -> Self {
            Self::default()
        }
    }

    #[async_trait::async_trait]
    impl Transport for Bus {
        type Conn = End;

        async fn open(&self, address: &str, link: Link<End>) -> Result<End, String> {
            let accept = lock(&self.listeners)
                .get(address)
                .cloned()
                .ok_or_else(|| format!("nobody listens on '{address}'"))?;
            let mine = End::new();
            let theirs = End::new();
            let their_link = accept.connection(theirs.clone(), format!("{address}-peer"));
            {
                let mut m = lock(&mine.0);
                m.far = Some(their_link.clone());
                m.near = Some(link.clone());
                m.other = Some(theirs.clone());
            }
            {
                let mut t = lock(&theirs.0);
                t.far = Some(link);
                t.near = Some(their_link);
                t.other = Some(mine.clone());
            }
            Ok(mine)
        }

        async fn listen(&self, address: &str, accept: Acceptor<End>) -> Result<End, String> {
            let mut listeners = lock(&self.listeners);
            if listeners.contains_key(address) {
                return Err(format!("'{address}' already has a listener"));
            }
            listeners.insert(address.to_string(), accept);
            let end = End::new();
            lock(&end.0).listening = Some(address.to_string());
            Ok(end)
        }

        async fn send(&self, conn: &End, data: Vec<u8>) -> Result<(), String> {
            let far = lock(&conn.0).far.clone();
            match far {
                Some(far) => {
                    far.receive(data);
                    Ok(())
                }
                None => Err("the far end is gone".into()),
            }
        }

        fn close(&self, conn: &End) {
            let (listening, other) = {
                let mut s = lock(&conn.0);
                s.far = None;
                s.near = None;
                (s.listening.take(), s.other.take())
            };
            if let Some(name) = listening {
                lock(&self.listeners).remove(&name);
            }
            if let Some(other) = other {
                let near = {
                    let mut o = lock(&other.0);
                    o.far = None;
                    o.other = None;
                    o.near.take()
                };
                if let Some(near) = near {
                    near.closed(Some("the far end closed".into()));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests;
