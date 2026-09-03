//! The hostcall pump for a swarm — Host.md §5's duties, per instance: drain
//! each guest's `host/calls`, dispatch on `call` against **that guest's**
//! granted capabilities, run the connector, push the reply with `tok` echoed
//! verbatim into `host/replies`. Every drained request is answered.
//!
//! The attenuation story lands here end to end: a parent holding
//! `host:time*` and a child it spawned without that grant push the same
//! request bytes and get different answers — `ok` and `denied` — with no
//! special case anywhere, because the dispatcher asks each instance's own
//! [`CapSet`] the question the swarm asks about queues.
//!
//! **A connector may answer later** (doc/Wasm.md D6). A request is routed
//! synchronously — the capability check and the connector lookup are the
//! dispatcher's, and cost nothing to await — and the connector's future is
//! polled once. Nearly every future is ready on that poll: `time`, `fs`,
//! `crypto` and `sql` do no real awaiting, and the connectors that carry a
//! runtime of their own (`ssh`, `rest`, `ssmtp`) block inside the call. A
//! future that is not ready is parked in [`Pump`]'s in-flight table with
//! its request already consumed, polled again on every later pump, and its
//! answer lands on the guest's reply queue when it comes. Nothing here ever
//! blocks, which is the one thing a browser thread cannot do, and it is the
//! Lab's `_inflight`/`_settled` shape in Rust.
//!
//! Three rules, all load-bearing. A request is not drained until its reply
//! has room to land: answering, failing to deliver and retrying would apply
//! a stateful connector's write twice. A reply whose queue is full when it
//! arrives waits, in order, for a later pump. And an answer owed to an
//! instance that died is dropped, while one owed to an instance that merely
//! hibernated is held for its return.
//!
//! [`PumpHost`] wraps any [`SwarmHost`]: pump, drive, pump again — so a
//! reply is already there when the driven program resumes, and a call made
//! while running is answered before the step ends.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll, Waker};

use drt_caps::CapSet;
use drt_connector::{Dispatcher, Routed};
use drt_hostcall::Reply;

use crate::engine::{Instance, QueueHandle};
use crate::swarm::{Driven, SwarmHost};
use crate::InstanceId;

/// The queue names `doc/Host.md` fixes so guests are portable between hosts.
pub const CALLS: &str = "host/calls";
pub const REPLIES: &str = "host/replies";

/// A reply on its way: the connector's future, owned. `Send` where there
/// are threads, because a native embedding may move a swarm between them;
/// not on wasm, where a page's connector holds JS values — the distinction
/// `engine::MaybeSend` draws, drawn again here because a trait alias
/// cannot be added to a `dyn Future`.
#[cfg(not(target_arch = "wasm32"))]
type ReplyFuture = Pin<Box<dyn Future<Output = Reply> + Send>>;
#[cfg(target_arch = "wasm32")]
type ReplyFuture = Pin<Box<dyn Future<Output = Reply>>>;

struct InFlight {
    id: InstanceId,
    future: ReplyFuture,
}

/// An answer that arrived, encoded, waiting for room on its reply queue.
struct Settled {
    id: InstanceId,
    bytes: Vec<u8>,
}

/// The in-flight table: what has been asked and not yet answered, and what
/// has been answered and not yet landed, for any number of instances.
#[derive(Default)]
pub struct Pump {
    inflight: Vec<InFlight>,
    settled: Vec<Settled>,
}

impl Pump {
    pub fn new() -> Self {
        Self::default()
    }

    /// Everything one instance is owed and asks: land what settled, drain
    /// what it pushed, park what cannot be answered yet. Returns how many
    /// replies landed. Looked up per pump, not once: the guest declares
    /// these queues at runtime.
    pub fn pump(
        &mut self,
        id: InstanceId,
        caps: &CapSet,
        dispatcher: &Dispatcher,
        inst: &mut dyn Instance,
    ) -> usize {
        let (Some(calls), Some(replies)) = (inst.queue(CALLS), inst.queue(REPLIES)) else {
            return 0;
        };
        let mut landed = self.deliver(id, replies, inst);
        while room(inst, replies) {
            let Ok(Some(raw)) = inst.pop(calls) else {
                break;
            };
            match dispatcher.route(caps, &raw) {
                Routed::Answered(reply) => landed += self.land(id, replies, inst, &reply),
                Routed::Call(call) => {
                    let mut future: ReplyFuture = Box::pin(call.answer());
                    match future
                        .as_mut()
                        .poll(&mut Context::from_waker(Waker::noop()))
                    {
                        Poll::Ready(reply) => landed += self.land(id, replies, inst, &reply),
                        Poll::Pending => self.inflight.push(InFlight { id, future }),
                    }
                }
            }
        }
        landed
    }

    /// Poll every in-flight answer once. What is ready joins the settled
    /// queue and lands on the next pump of its instance.
    ///
    /// A no-op waker, deliberately: the pump is polled by the drive loop's
    /// own cadence (doc/Wasm.md §4.3's `POLL_TICK`), and a future built on
    /// a JS promise keeps its result until the poll that collects it.
    pub fn poll(&mut self) {
        let mut i = 0;
        while i < self.inflight.len() {
            let polled = self.inflight[i]
                .future
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop()));
            match polled {
                Poll::Ready(reply) => {
                    let InFlight { id, .. } = self.inflight.remove(i);
                    if let Ok(bytes) = drt_hostcall::to_bytes(&reply) {
                        self.settled.push(Settled { id, bytes });
                    }
                }
                Poll::Pending => i += 1,
            }
        }
    }

    /// Answers outstanding for `id`: in flight, or settled and waiting for
    /// room on the reply queue.
    pub fn outstanding(&self, id: InstanceId) -> usize {
        self.inflight.iter().filter(|f| f.id == id).count()
            + self.settled.iter().filter(|s| s.id == id).count()
    }

    /// Answers outstanding for every instance.
    pub fn in_flight(&self) -> usize {
        self.inflight.len() + self.settled.len()
    }

    /// The instance is gone; drop what was owed to it.
    pub fn forget(&mut self, id: InstanceId) {
        self.inflight.retain(|f| f.id != id);
        self.settled.retain(|s| s.id != id);
    }

    /// Land the settled answers for `id`, oldest first, while there is
    /// room. The first that does not fit stops the rest: order is part of
    /// the contract a guest with several requests outstanding relies on.
    fn deliver(&mut self, id: InstanceId, replies: QueueHandle, inst: &mut dyn Instance) -> usize {
        let mut landed = 0;
        let mut i = 0;
        while i < self.settled.len() {
            if self.settled[i].id != id {
                i += 1;
                continue;
            }
            match inst.push(replies, &self.settled[i].bytes) {
                Ok(outcome) if outcome.is_accepted() => {
                    self.settled.remove(i);
                    landed += 1;
                }
                _ => break,
            }
        }
        landed
    }

    /// Push one reply now, or keep it for a later pump when it does not fit.
    fn land(
        &mut self,
        id: InstanceId,
        replies: QueueHandle,
        inst: &mut dyn Instance,
        reply: &Reply,
    ) -> usize {
        let Ok(bytes) = drt_hostcall::to_bytes(reply) else {
            return 0;
        };
        match inst.push(replies, &bytes) {
            Ok(outcome) if outcome.is_accepted() => 1,
            _ => {
                self.settled.push(Settled { id, bytes });
                0
            }
        }
    }
}

/// Whether one more reply fits. A disabled queue has no room either: the
/// request stays where it is rather than being consumed for an answer that
/// can never land.
fn room(inst: &mut dyn Instance, replies: QueueHandle) -> bool {
    inst.queue_info(replies)
        .map(|q| q.enabled && q.len < q.capacity)
        .unwrap_or(false)
}

/// Any [`SwarmHost`], with the hostcall pump around each drive.
pub struct PumpHost<H: SwarmHost> {
    inner: H,
    dispatcher: Arc<Dispatcher>,
    pump: Pump,
}

impl<H: SwarmHost> PumpHost<H> {
    pub fn new(inner: H, dispatcher: Dispatcher) -> Self {
        Self::shared(inner, Arc::new(dispatcher))
    }

    /// Over a dispatcher something else also holds — a driver that needs
    /// it back for `Dispatcher::finish` at shutdown.
    pub fn shared(inner: H, dispatcher: Arc<Dispatcher>) -> Self {
        PumpHost {
            inner,
            dispatcher,
            pump: Pump::new(),
        }
    }

    /// The wrapped host — the pump adds hostcalls, it does not hide what
    /// it wraps.
    pub fn inner(&self) -> &H {
        &self.inner
    }

    /// The dispatcher this pump answers through, so a host that owns a
    /// swarm can still reach its connectors — `Dispatcher::finish` at
    /// shutdown is the reason, and a caller that has handed its dispatcher
    /// to a swarm has no other way back to it.
    pub fn dispatcher(&self) -> &Dispatcher {
        &self.dispatcher
    }

    /// The in-flight table, for a driver deciding whether to keep polling.
    pub fn pump(&self) -> &Pump {
        &self.pump
    }
}

impl<H: SwarmHost> SwarmHost for PumpHost<H> {
    fn drive(&mut self, id: InstanceId, caps: &CapSet, inst: &mut dyn Instance) -> Driven {
        self.pump.poll();
        self.pump.pump(id, caps, &self.dispatcher, inst);
        let driven = self.inner.drive(id, caps, inst);
        if matches!(driven, Driven::Alive) {
            self.pump.pump(id, caps, &self.dispatcher, inst);
        }
        driven
    }

    fn attached(&mut self, id: InstanceId) {
        self.inner.attached(id);
    }

    fn detached(&mut self, id: InstanceId) {
        self.inner.detached(id);
    }

    fn released(&mut self, id: InstanceId) {
        self.pump.forget(id);
        self.inner.released(id);
    }
}
