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
//! [`PumpHost`] wraps any [`SwarmHost`]: pump, drive, pump again — so a
//! reply is already there when the driven program resumes, and a call made
//! while running is answered before the step ends. Dispatch is awaited on
//! the spot (`pollster`), which is right for the in-process connectors a
//! swarm wires today; a serve loop with genuinely async connectors brings
//! its own runtime and its own host.

use drt_caps::CapSet;
use drt_connector::Dispatcher;

use crate::engine::Instance;
use crate::swarm::{Driven, SwarmHost};
use crate::InstanceId;

/// The queue names `doc/Host.md` fixes so guests are portable between hosts.
const CALLS: &str = "host/calls";
const REPLIES: &str = "host/replies";

pub struct PumpHost<H: SwarmHost> {
    inner: H,
    dispatcher: Dispatcher,
}

impl<H: SwarmHost> PumpHost<H> {
    pub fn new(inner: H, dispatcher: Dispatcher) -> Self {
        PumpHost { inner, dispatcher }
    }

    /// Drain and answer everything pending on one instance. Looked up per
    /// pump, not once: the guest declares these queues at runtime.
    fn pump(&mut self, caps: &CapSet, inst: &mut dyn Instance) {
        let (Some(calls), Some(replies)) = (inst.queue(CALLS), inst.queue(REPLIES)) else {
            return;
        };
        while let Ok(Some(raw)) = inst.pop(calls) {
            let reply = pollster::block_on(self.dispatcher.dispatch(caps, &raw));
            let Ok(bytes) = drt_hostcall::to_bytes(&reply) else {
                continue;
            };
            // A reply queue with no room is the guest's own sizing to see;
            // the push answer is the only delivery guarantee there is.
            let _ = inst.push(replies, &bytes);
        }
    }
}

impl<H: SwarmHost> SwarmHost for PumpHost<H> {
    fn drive(&mut self, id: InstanceId, caps: &CapSet, inst: &mut dyn Instance) -> Driven {
        self.pump(caps, inst);
        let driven = self.inner.drive(id, caps, inst);
        if matches!(driven, Driven::Alive) {
            self.pump(caps, inst);
        }
        driven
    }

    fn attached(&mut self, id: InstanceId) {
        self.inner.attached(id);
    }

    fn detached(&mut self, id: InstanceId) {
        self.inner.detached(id);
    }
}
