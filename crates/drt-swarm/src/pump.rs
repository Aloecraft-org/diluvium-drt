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
//! Encoding goes through [`drt_hostcall::to_wire`] rather than `to_bytes`,
//! here and in [`Pump::poll`], because those are the two places a reply
//! becomes bytes and the blob lane is resolved at exactly that step
//! (`doc/Plan-2026-09.md` §3.2). A reply carrying no column encodes
//! identically either way.
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
use drt_connector::{Caller, Dispatcher, Routed};
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
            match dispatcher.route_as(Caller::Node(id.0), caps, &raw) {
                Routed::Answered(reply) => landed += self.land(id, replies, inst, reply),
                Routed::Call(call) => {
                    let mut future: ReplyFuture = Box::pin(call.answer());
                    match future
                        .as_mut()
                        .poll(&mut Context::from_waker(Waker::noop()))
                    {
                        Poll::Ready(reply) => landed += self.land(id, replies, inst, reply),
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
                    if let Ok(bytes) = drt_hostcall::to_wire(reply) {
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
        reply: Reply,
    ) -> usize {
        let Ok(bytes) = drt_hostcall::to_wire(reply) else {
            return 0;
        };
        match inst.push(replies, &bytes) {
            Ok(outcome) if outcome.is_accepted() => 1,
            // Unreachable, and held rather than dropped if it ever is not.
            //
            // `land` is only called inside `pump`'s `while room(..)`, and
            // `room` has just established `enabled && len < capacity` on
            // this very queue. Nothing runs the guest in between, so the
            // slot `room` saw is still free: `Full` and `Disabled` cannot
            // come back, and the engine's only error here is a queue it
            // does not know, for a handle `pump` re-read from it moments
            // ago.
            //
            // The assertion is how a future change that breaks that
            // ordering announces itself in a test run rather than in a
            // deployment. The release behaviour is deliberately the
            // conservative one: hold the answer for a later pump. A reply
            // held too long is a bounded cost; a reply dropped is a guest
            // waiting forever for an answer that was thrown away.
            other => {
                debug_assert!(
                    false,
                    "a reply did not land on a queue `room` just found space on: {other:?}"
                );
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
    /// What connectors reported lost when a node died, attributed to it,
    /// waiting for the driver to take it (`doc/Plan-0.7.0.md` §2.5). Kept
    /// here rather than written anywhere because this crate has no stderr
    /// and no supervisor queue of its own; the driver that does is the one
    /// to say it.
    lost: Vec<(InstanceId, String)>,
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
            lost: Vec::new(),
        }
    }

    /// Everything connectors reported lost since the last take, each
    /// attributed to the node that owned it. Empty is the ordinary answer.
    pub fn take_lost(&mut self) -> Vec<(InstanceId, String)> {
        std::mem::take(&mut self.lost)
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

    /// Death, and only death. Hibernation goes through `detached`, which
    /// deliberately does not reach the dispatcher: a parked node keeps what
    /// it holds (§2.4).
    /// What the connectors say ended, as instances; the root is never an
    /// instance and the wrapped host may have its own.
    fn ended(&mut self) -> Vec<(InstanceId, String)> {
        let mut ended = self.inner.ended();
        for (caller, why) in self.dispatcher.ended() {
            if let Caller::Node(id) = caller {
                ended.push((InstanceId(id), why));
            }
        }
        ended
    }

    /// What the connectors have to say, encoded; the root is never an
    /// instance and the wrapped host may have its own.
    fn notices(&mut self) -> Vec<(InstanceId, String, Vec<u8>)> {
        let mut notices = self.inner.notices();
        for notice in self.dispatcher.notices() {
            let Caller::Node(id) = notice.owner else {
                continue;
            };
            let mut bytes = Vec::new();
            if rmpv::encode::write_value(&mut bytes, &notice.message).is_ok() {
                notices.push((InstanceId(id), notice.queue, bytes));
            }
        }
        notices
    }

    fn released(&mut self, id: InstanceId) {
        self.pump.forget(id);
        for what in self.dispatcher.release(&Caller::Node(id.0)) {
            self.lost.push((id, what));
        }
        self.inner.released(id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::{EngineError, PushOutcome, QueueStatus, Step, UsageReport, WaitSet};
    use crate::swarm::StepHost;
    use drt_connector::{CallResult, Connector, Handles, Registry};
    use drt_hostcall::Request;
    use std::sync::Mutex;

    /// A connector that holds one thing per node and records every release
    /// it is asked for, so the test can see which hook reached it.
    struct Holding {
        table: Handles<&'static str>,
        releases: Mutex<Vec<Caller>>,
    }

    #[async_trait::async_trait]
    impl Connector for Holding {
        async fn call(
            &self,
            _: &str,
            _: Option<rmpv::Value>,
            _: Option<&drt_caps::Scope>,
        ) -> CallResult {
            Ok(rmpv::Value::Nil)
        }
        fn release(&self, caller: &Caller) -> Vec<String> {
            self.releases.lock().unwrap().push(*caller);
            self.table
                .release(*caller)
                .into_iter()
                .map(|(_, what)| format!("{caller} lost {what}"))
                .collect()
        }
    }

    fn host_with(holding: Arc<Holding>) -> PumpHost<StepHost> {
        let mut reg = Registry::new();
        reg.wire("hold", holding, None).unwrap();
        PumpHost::new(StepHost::new(), Dispatcher::new(reg))
    }

    /// §2.4: hibernation must not release. `detached` is the hibernation
    /// hook and `released` is the death hook; only the second reaches the
    /// dispatcher, and the report it collects names the node.
    #[test]
    fn detached_keeps_a_nodes_resources_and_released_takes_them() {
        let holding = Arc::new(Holding {
            table: Handles::new("thing"),
            releases: Mutex::new(Vec::new()),
        });
        holding.table.insert(Caller::Node(4), "a quiet socket");
        let mut host = host_with(holding.clone());

        host.detached(InstanceId(4));
        assert!(
            holding.releases.lock().unwrap().is_empty(),
            "hibernation reached the dispatcher's release"
        );
        assert_eq!(
            holding.table.count(Caller::Node(4)),
            1,
            "still held while parked"
        );
        assert!(host.take_lost().is_empty());

        host.released(InstanceId(4));
        assert_eq!(*holding.releases.lock().unwrap(), vec![Caller::Node(4)]);
        assert_eq!(
            holding.table.count(Caller::Node(4)),
            0,
            "gone with the node"
        );
        let lost = host.take_lost();
        assert_eq!(lost.len(), 1);
        assert_eq!(lost[0].0, InstanceId(4));
        assert_eq!(lost[0].1, "instance 4 lost a quiet socket");
        assert!(host.take_lost().is_empty(), "taking drains");
    }

    // depth: the held-answer fixtures, for the four tests below

    /// A connector that answers only once the test says so, which is the
    /// one way an answer reaches the settled queue: the request is drained
    /// and its future parked, and the answer arrives after the pump that
    /// asked for it has already returned.
    struct Slow {
        ready: Arc<std::sync::atomic::AtomicBool>,
    }

    #[async_trait::async_trait]
    impl Connector for Slow {
        async fn call(
            &self,
            _: &str,
            args: Option<rmpv::Value>,
            _: Option<&drt_caps::Scope>,
        ) -> CallResult {
            let ready = self.ready.clone();
            std::future::poll_fn(move |_| {
                if ready.load(std::sync::atomic::Ordering::SeqCst) {
                    Poll::Ready(())
                } else {
                    Poll::Pending
                }
            })
            .await;
            // Echoed so a test can tell one held answer from another.
            Ok(args.unwrap_or(rmpv::Value::Nil))
        }
    }

    /// One instance with the two queues `doc/Host.md` fixes. `replies` has
    /// a capacity the test sets, so "there is room" and "there is not" are
    /// both reachable without an engine.
    struct Fake {
        calls: Vec<Vec<u8>>,
        replies: Vec<Vec<u8>>,
        capacity: u32,
    }

    const H_CALLS: QueueHandle = QueueHandle(1);
    const H_REPLIES: QueueHandle = QueueHandle(2);

    impl Fake {
        fn with(requests: Vec<Request>) -> Self {
            Self {
                calls: requests
                    .into_iter()
                    .map(|r| drt_hostcall::to_bytes(&r).unwrap())
                    .collect(),
                replies: Vec::new(),
                capacity: 8,
            }
        }

        /// The tokens landed so far, in the order they landed.
        fn landed_toks(&self) -> Vec<u64> {
            self.replies
                .iter()
                .filter_map(|b| drt_hostcall::from_bytes::<Reply>(b).unwrap().tok)
                .collect()
        }
    }

    impl Instance for Fake {
        fn queue(&mut self, name: &str) -> Option<QueueHandle> {
            match name {
                CALLS => Some(H_CALLS),
                REPLIES => Some(H_REPLIES),
                _ => None,
            }
        }
        fn queue_info(&mut self, queue: QueueHandle) -> Result<QueueStatus, EngineError> {
            let (len, capacity) = match queue {
                H_CALLS => (self.calls.len() as u32, 8),
                _ => (self.replies.len() as u32, self.capacity),
            };
            Ok(QueueStatus {
                len,
                capacity,
                enabled: true,
                exported: false,
            })
        }
        fn push(&mut self, queue: QueueHandle, msgpack: &[u8]) -> Result<PushOutcome, EngineError> {
            if queue != H_REPLIES {
                return Err(EngineError::Engine("not the reply queue".into()));
            }
            if self.replies.len() as u32 >= self.capacity {
                return Ok(PushOutcome::Full);
            }
            self.replies.push(msgpack.to_vec());
            Ok(PushOutcome::Accepted)
        }
        fn pop(&mut self, queue: QueueHandle) -> Result<Option<Vec<u8>>, EngineError> {
            if queue != H_CALLS || self.calls.is_empty() {
                return Ok(None);
            }
            Ok(Some(self.calls.remove(0)))
        }
        fn run(&mut self) -> Result<Step, EngineError> {
            Ok(Step::Done)
        }
        fn resume(&mut self, _: QueueHandle) -> Result<Step, EngineError> {
            Ok(Step::Done)
        }
        fn resume_timeout(&mut self) -> Result<Step, EngineError> {
            Ok(Step::Done)
        }
        fn current_wait(&mut self) -> Option<WaitSet> {
            None
        }
        fn usage(&self) -> UsageReport {
            UsageReport::default()
        }
        fn exceeded(&self) -> bool {
            false
        }
        fn snapshot(&mut self, _: Option<&str>) -> Result<Vec<u8>, EngineError> {
            Ok(Vec::new())
        }
    }

    /// A pump wired to one slow connector, and the switch that answers it.
    fn slow_pump() -> (
        Pump,
        Dispatcher,
        Arc<CapSet>,
        Arc<std::sync::atomic::AtomicBool>,
    ) {
        let ready = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let mut reg = Registry::new();
        reg.wire(
            "slow",
            Arc::new(Slow {
                ready: ready.clone(),
            }),
            None,
        )
        .unwrap();
        let caps = CapSet::root(vec![drt_caps::Grant::grant("host:slow/*")]);
        (Pump::new(), Dispatcher::new(reg), caps, ready)
    }

    /// The whole reason `settled` exists, and the reason it is safe to let
    /// it wait: an answer that arrives after its pump is held, is counted
    /// while it waits, and lands on the next pump.
    ///
    /// The counting is the part worth pinning. `outstanding` is how an
    /// operator sees held answers accumulating at all; without it the only
    /// symptom of a guest that stopped reading its inbox is memory.
    #[test]
    fn an_answer_that_arrives_late_is_held_counted_and_then_landed() {
        let (mut pump, disp, caps, ready) = slow_pump();
        let mut inst = Fake::with(vec![Request {
            tok: 7,
            call: "slow/thing".into(),
            args: None,
        }]);

        assert_eq!(pump.pump(InstanceId(1), &caps, &disp, &mut inst), 0);
        assert_eq!(
            pump.outstanding(InstanceId(1)),
            1,
            "in flight, not yet ready"
        );
        assert!(inst.replies.is_empty(), "nothing landed yet");

        ready.store(true, std::sync::atomic::Ordering::SeqCst);
        pump.poll();
        assert_eq!(
            pump.outstanding(InstanceId(1)),
            1,
            "settled now, still owed, still counted"
        );
        assert!(inst.replies.is_empty(), "poll does not deliver");

        assert_eq!(pump.pump(InstanceId(1), &caps, &disp, &mut inst), 1);
        assert_eq!(
            pump.outstanding(InstanceId(1)),
            0,
            "delivered and forgotten"
        );
        assert_eq!(inst.landed_toks(), vec![7]);
    }

    /// The bound on how long an answer can be held: the instance's life.
    ///
    /// This is what makes holding safe rather than an unbounded promise.
    /// A guest that dies owing answers does not strand them -- `released`
    /// reaches `forget`, and what it was owed goes with it.
    #[test]
    fn answers_owed_to_a_dead_instance_are_dropped_with_it() {
        let (mut pump, disp, caps, ready) = slow_pump();
        let mut inst = Fake::with(vec![Request {
            tok: 1,
            call: "slow/thing".into(),
            args: None,
        }]);

        pump.pump(InstanceId(3), &caps, &disp, &mut inst);
        ready.store(true, std::sync::atomic::Ordering::SeqCst);
        pump.poll();
        assert_eq!(pump.outstanding(InstanceId(3)), 1, "held for a live node");

        pump.forget(InstanceId(3));
        assert_eq!(pump.outstanding(InstanceId(3)), 0);
        assert_eq!(pump.in_flight(), 0, "nothing of a dead node's is retained");

        // And the answer is not resurrected by a later pump.
        assert_eq!(pump.pump(InstanceId(3), &caps, &disp, &mut inst), 0);
        assert!(inst.replies.is_empty());
    }

    /// One node's death leaves another node's held answers alone. The
    /// settled queue is shared, so this is the test that it is keyed.
    #[test]
    fn forgetting_one_node_leaves_another_nodes_held_answers() {
        let (mut pump, disp, caps, ready) = slow_pump();
        let mut a = Fake::with(vec![Request {
            tok: 1,
            call: "slow/thing".into(),
            args: None,
        }]);
        let mut b = Fake::with(vec![Request {
            tok: 2,
            call: "slow/thing".into(),
            args: None,
        }]);

        pump.pump(InstanceId(1), &caps, &disp, &mut a);
        pump.pump(InstanceId(2), &caps, &disp, &mut b);
        ready.store(true, std::sync::atomic::Ordering::SeqCst);
        pump.poll();
        assert_eq!(pump.in_flight(), 2);

        pump.forget(InstanceId(1));
        assert_eq!(pump.outstanding(InstanceId(2)), 1, "2's answer survives 1");
        assert_eq!(pump.pump(InstanceId(2), &caps, &disp, &mut b), 1);
        assert_eq!(b.landed_toks(), vec![2]);
    }

    /// Held answers land oldest-first. Order is part of the contract a
    /// guest with several requests outstanding relies on, and `deliver`
    /// stopping at the first that does not fit is what preserves it: the
    /// rest wait behind it rather than overtaking it.
    #[test]
    fn held_answers_land_in_the_order_they_settled() {
        let (mut pump, disp, caps, ready) = slow_pump();
        let mut inst = Fake::with(vec![
            Request {
                tok: 10,
                call: "slow/thing".into(),
                args: Some(rmpv::Value::from(10)),
            },
            Request {
                tok: 11,
                call: "slow/thing".into(),
                args: Some(rmpv::Value::from(11)),
            },
            Request {
                tok: 12,
                call: "slow/thing".into(),
                args: Some(rmpv::Value::from(12)),
            },
        ]);

        pump.pump(InstanceId(1), &caps, &disp, &mut inst);
        assert_eq!(pump.outstanding(InstanceId(1)), 3, "all three parked");

        ready.store(true, std::sync::atomic::Ordering::SeqCst);
        pump.poll();

        // Room for one. The other two wait, in order, behind it.
        inst.capacity = 1;
        assert_eq!(pump.pump(InstanceId(1), &caps, &disp, &mut inst), 1);
        assert_eq!(inst.landed_toks(), vec![10], "the oldest went first");
        assert_eq!(pump.outstanding(InstanceId(1)), 2);

        inst.capacity = 8;
        assert_eq!(pump.pump(InstanceId(1), &caps, &disp, &mut inst), 2);
        assert_eq!(inst.landed_toks(), vec![10, 11, 12], "and then in order");
        assert_eq!(pump.outstanding(InstanceId(1)), 0);
    }

    /// A node that held nothing releases nothing, and a sibling's holdings
    /// are not touched by its death.
    #[test]
    fn releasing_one_node_leaves_a_siblings_resources() {
        let holding = Arc::new(Holding {
            table: Handles::new("thing"),
            releases: Mutex::new(Vec::new()),
        });
        holding.table.insert(Caller::Node(5), "sibling's");
        let mut host = host_with(holding.clone());

        host.released(InstanceId(6));
        assert!(host.take_lost().is_empty(), "nothing of 6's to lose");
        assert_eq!(holding.table.count(Caller::Node(5)), 1);
    }
}
