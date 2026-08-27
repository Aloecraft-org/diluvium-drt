//! The swarm: `dvs.c`'s semantics, ported faithfully over the [`Engine`]
//! seam (SPEC.md §8). `dvs.c` stays frozen upstream as the differential
//! reference until the ported capability suite passes against this.
//!
//! The six owned things are the whole of what lives here — instance table,
//! parentage, per-instance caps, lifecycle drain, budget enforcement,
//! snapshot cache + `wake_on_message`. There is still no supervisor type:
//! restart, backoff, and topology are programs holding the lifecycle
//! capability, and nothing here distinguishes such a program from any other.
//!
//! Deliberate deviations from `dvs.c`, each an addition rather than a
//! change in guest-visible behavior:
//!
//! - Capability sets are [`drt_caps::CapSet`]s with provenance — every
//!   set records who granted it, attenuated from what, back to the root.
//!   With no `deny` grants in play the checks reduce exactly to
//!   `dvs_holds`/`dvs_may_grant`; a root config that *does* carry denies has
//!   them carried onto every descendant automatically, so a spawn request
//!   (which names capabilities, nothing more) can never shed one.
//! - Bytecode spawns are refused by default (`allow_bytecode` is the
//!   opt-in): the verifier does not exist (GUARANTEES.md), so code arriving
//!   as a message is loaded as source, text-only. `dvs.c` accepted compiled
//!   chunks; a swarm that wants that behavior states it.
//! - The rate-limit "leave the request queued" trick uses a one-slot
//!   deferred buffer instead of a queue peek (the seam has no non-consuming
//!   peek): a throttled spawn is popped, held, and processed first on the
//!   next step. Order and rate are preserved; the only observable difference
//!   is the requester's own queue depth.

use std::collections::HashMap;
use std::sync::Arc;

use drt_caps::{CapSet, Effect, Grant, Principal};
use drt_config::Budget;

use crate::engine::{
    Engine, Instance, LoadSpec, ProgramBytes, PushOutcome, QueueHandle, RestoreSpec, WaitSet,
};
use crate::InstanceId;

/// The capability that gates `system/lifecycle`.
pub const CAP_LIFECYCLE: &str = "lifecycle";

const DEFAULT_MAX_INSTANCES: u32 = 256;
const DEFAULT_SPAWN_RATE: u32 = 8;
const MAX_CAPS: usize = 32;
const MAX_CAP_LEN: usize = 96;
/// How many messages may wait for a non-resident instance. Bounded on
/// purpose: an unbounded wake buffer would be the one place in the system
/// where backpressure was invisible.
const MAX_PENDING: usize = 16;
const MAX_QNAME: usize = 64;
/// Event details are clamped exactly as `emit_event` clamps them.
const MAX_EVENT_DETAIL: usize = 191;

/// The four-row delivery table's answers, plus the refusals around it.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SwarmError {
    /// Dead, unknown, or cached without `wake_on_message`: from a sender's
    /// point of view, not there. Immediate, never blocking.
    #[error("gone")]
    Gone,
    /// No such instance.
    #[error("no such instance")]
    Unknown,
    /// The named queue does not exist on the destination.
    #[error("no such queue")]
    UnknownQueue,
    /// A bounded thing is full: the destination queue, the wake buffer, the
    /// instance table, or the capability count.
    #[error("{0}")]
    Limit(String),
    /// Everything else, worded to be read.
    #[error("{0}")]
    Error(String),
}

/// The outcome of driving one instance, reported by the host. The swarm
/// classifies a stop as `exceeded`/`faulted`/`exited` — three different
/// events because a supervisor's response to each is different.
#[derive(Debug)]
pub enum Driven {
    /// Still going (running or parked).
    Alive,
    /// Ran to completion.
    Exited,
    /// A guest error, with the message.
    Faulted(String),
}

/// The host vtable (11.5's shape, in Rust): the swarm cannot drive anything
/// by itself, because driving means something different in every
/// environment — a thread, a task, an orchestrator tick. The portable part
/// is bookkeeping.
///
/// `attached`/`detached` are `create`/`destroy` for whatever the host keeps
/// per instance: called on build and wake, and on hibernate and death. A
/// host that keeps nothing implements neither.
pub trait SwarmHost {
    /// `caps` is the instance's own attenuated set — what a host mediating
    /// its own resources (hostcalls above all) gates against, the same
    /// question the swarm asks about queues.
    fn drive(&mut self, id: InstanceId, caps: &CapSet, inst: &mut dyn Instance) -> Driven;
    fn attached(&mut self, _id: InstanceId) {}
    fn detached(&mut self, _id: InstanceId) {}
}

/// A host whose `drive` is one `run` or `resume` — the single-threaded
/// legitimate host `dvs.h` describes, not a stub. A parked instance is
/// resumed when one of its waited queues is ready; otherwise it stays parked
/// (the swarm owns no clock, so a timeout is someone else's to honour —
/// unlike the C bench harness, which times its own parks).
///
/// **Ready** is not the same question for both kinds of park. A program
/// waiting for a *message* is ready when its queue is non-empty; one waiting
/// for *space* in a full queue is ready when the queue has room. Answering
/// the wrong one resumes a program straight back into a push that fails
/// again, which is why `dv.h` §8.3 makes the host say which it means and why
/// [`WaitSet::for_space`] exists.
#[derive(Default)]
pub struct StepHost {
    /// The wait set each parked instance was last handed.
    ///
    /// `run` and `resume` already return the park as `Step::Parked`, so
    /// asking `current_wait()` for it again on the next step is asking the
    /// engine to repeat itself — a second `dv_waitset_get` across the FFI
    /// per message round trip, where the C harness's `host_drive` makes
    /// exactly one. An entry is dropped whenever residency changes, because
    /// queue handles are interned per residency and a woken instance's are
    /// not the ones it parked on.
    parked: HashMap<u32, WaitSet>,
}

impl StepHost {
    pub fn new() -> StepHost {
        StepHost::default()
    }
}

impl SwarmHost for StepHost {
    fn drive(&mut self, id: InstanceId, _caps: &CapSet, inst: &mut dyn Instance) -> Driven {
        // Cached, or asked for — the second is the restored-instance path,
        // whose park was never returned by anything here.
        let wait = match self.parked.get(&id.0) {
            Some(wait) => Some(*wait),
            None => inst.current_wait(),
        };
        let step = match wait {
            None => inst.run(),
            Some(wait) => {
                let ready = |info: crate::engine::QueueStatus| {
                    if wait.for_space {
                        info.len < info.capacity
                    } else {
                        info.len > 0
                    }
                };
                let fired = wait
                    .queues()
                    .iter()
                    .copied()
                    .find(|&q| inst.queue_info(q).map(ready).unwrap_or(false));
                match fired {
                    Some(q) => inst.resume(q),
                    None => return Driven::Alive,
                }
            }
        };
        match step {
            Ok(crate::engine::Step::Parked(wait)) => {
                self.parked.insert(id.0, wait);
                Driven::Alive
            }
            Ok(crate::engine::Step::Done) => {
                self.parked.remove(&id.0);
                Driven::Exited
            }
            Err(e) => {
                self.parked.remove(&id.0);
                Driven::Faulted(e.to_string())
            }
        }
    }

    // Build, wake, hibernate and death all change residency, and handles do
    // not survive it.
    fn attached(&mut self, id: InstanceId) {
        self.parked.remove(&id.0);
    }

    fn detached(&mut self, id: InstanceId) {
        self.parked.remove(&id.0);
    }
}

struct Pending {
    queue: String,
    msg: Vec<u8>,
}

struct Slot {
    /// 0 when the slot is free. A handle is never reused.
    id: u32,
    parent: u32,
    inst: Option<Box<dyn Instance>>,
    caps: Arc<CapSet>,
    budget: Budget,
    unsafe_stdlib: bool,
    wake_on_message: bool,
    alive: bool,
    /// Scratch, used only by `kill_subtree`.
    doomed: bool,
    /// The cache: the instance's whole state while it is not resident.
    snap: Option<Vec<u8>>,
    /// What arrived for it in the meantime, oldest first.
    pending: Vec<Pending>,
    /// A lifecycle request popped but throttled: processed first next step.
    deferred: Option<Vec<u8>>,
    /// Queue handles already resolved for *this residency*. A handle is
    /// runtime identity — valid only for the instance that issued it — so
    /// this is cleared wherever `inst` changes: build, hibernate, wake.
    /// A short `Vec` rather than a map: an instance uses a handful of queue
    /// names, and a map's allocation would cost more than the scan saves.
    handles: Vec<(String, QueueHandle)>,
}

impl Slot {
    fn free() -> Self {
        Slot {
            id: 0,
            parent: 0,
            inst: None,
            caps: CapSet::root(Vec::new()),
            budget: Budget::default(),
            unsafe_stdlib: false,
            wake_on_message: false,
            alive: false,
            doomed: false,
            snap: None,
            pending: Vec::new(),
            deferred: None,
            handles: Vec::new(),
        }
    }
}

pub struct Swarm<H: SwarmHost> {
    engine: Arc<dyn Engine>,
    host: H,
    slots: Vec<Slot>,
    /// Handle to slot index. `dvs.c` scans the table on every call that
    /// takes a handle — its own bench notes that "a host walking its own
    /// roster is quadratic in the swarm size" — and `Swarm::push` pays that
    /// scan for every message. A slot never moves (the `Vec` only grows,
    /// and `release` blanks in place), so an index is safe to hold.
    index: HashMap<u32, usize>,
    max_instances: usize,
    next_id: u32,
    spawn_rate: u32,
    spawns_this_step: u32,
    allow_hibernation: bool,
    allow_bytecode: bool,
    unsafe_stdlib: bool,
    host_identity: Option<String>,
}

impl<H: SwarmHost> Swarm<H> {
    pub fn new(engine: Arc<dyn Engine>, host: H) -> Self {
        Self::with_limits(engine, host, 0, 0)
    }

    /// `0` means the built-in defaults rather than no limit — an unbounded
    /// default is the wrong shape for something whose failure mode is a
    /// fork bomb.
    pub fn with_limits(
        engine: Arc<dyn Engine>,
        host: H,
        max_instances: u32,
        spawns_per_step: u32,
    ) -> Self {
        Swarm {
            engine,
            host,
            slots: Vec::new(),
            index: HashMap::new(),
            max_instances: if max_instances != 0 {
                max_instances as usize
            } else {
                DEFAULT_MAX_INSTANCES as usize
            },
            next_id: 1,
            spawn_rate: if spawns_per_step != 0 {
                spawns_per_step
            } else {
                DEFAULT_SPAWN_RATE
            },
            spawns_this_step: 0,
            allow_hibernation: true,
            allow_bytecode: false,
            unsafe_stdlib: false,
            host_identity: None,
        }
    }

    /// Hibernation is on by default; this is a host's opt-out, refused by
    /// name at the call rather than merely unused.
    pub fn allow_hibernation(&mut self, allow: bool) {
        self.allow_hibernation = allow;
    }

    /// Let spawn requests carry compiled chunks. Off by default: the
    /// bytecode verifier does not exist, so this is the GUARANTEES.md
    /// decision, made once per deployment rather than per request.
    pub fn allow_bytecode(&mut self, allow: bool) {
        self.allow_bytecode = allow;
    }

    /// `io`/`os`/`package` for this swarm's instances. The root takes this,
    /// children inherit and may narrow (`sealed = true` in a spawn request),
    /// never widen.
    pub fn allow_unsafe_stdlib(&mut self, allow: bool) {
        self.unsafe_stdlib = allow;
    }

    /// Give this swarm an identity, and every snapshot it takes carries it:
    /// a foreign or missing stamp is then refused at wake. Set it once,
    /// before anything hibernates. `None` clears it.
    pub fn set_host_identity(&mut self, identity: Option<&str>) {
        self.host_identity = match identity {
            Some(s) if !s.is_empty() => Some(s.to_string()),
            _ => None,
        };
    }

    // ---------------------------------------------------------- the table --

    fn find(&self, id: InstanceId) -> Option<usize> {
        if id.0 == 0 {
            return None;
        }
        self.index.get(&id.0).copied()
    }

    fn claim(&mut self) -> Option<usize> {
        let index = match self.slots.iter().position(|s| s.id == 0) {
            Some(i) => i,
            None if self.slots.len() < self.max_instances => {
                self.slots.push(Slot::free());
                self.slots.len() - 1
            }
            None => return None,
        };
        self.slots[index] = Slot::free();
        self.slots[index].id = self.next_id;
        self.index.insert(self.next_id, index);
        self.next_id += 1;
        Some(index)
    }

    fn release(&mut self, index: usize) {
        let id = self.slots[index].id;
        // A cached instance's host context was already detached when it
        // hibernated — the same contract a spawn has, in reverse.
        if self.slots[index].inst.is_some() {
            self.host.detached(InstanceId(id));
        }
        self.index.remove(&id);
        self.slots[index] = Slot::free();
        // The slot is free; the handle is not reused.
    }

    /// Make, load and budget an instance for an already-claimed slot.
    fn build(&mut self, index: usize, code: &[u8]) -> Result<(), String> {
        let slot = &self.slots[index];
        let spec_budget = slot.budget;
        let unsafe_stdlib = slot.unsafe_stdlib;
        let program = match std::str::from_utf8(code) {
            Ok(text) => ProgramBytes::Source(text),
            Err(_) if self.allow_bytecode => ProgramBytes::Bytecode(code),
            Err(_) => {
                return Err(
                    "the code is not source text, and bytecode spawns are switched off for \
                     this swarm (allow_bytecode)"
                        .to_string(),
                )
            }
        };
        let inst = self
            .engine
            .load(LoadSpec {
                program,
                name: "=agent",
                budget: spec_budget,
                unsafe_stdlib,
            })
            .map_err(|e| format!("the program would not load: {e}"))?;
        let slot = &mut self.slots[index];
        slot.inst = Some(inst);
        slot.handles.clear();
        slot.alive = true;
        let id = slot.id;
        self.host.attached(InstanceId(id));
        Ok(())
    }

    /// Put a program in as the root. Its capabilities are whatever the
    /// caller grants and are the ceiling for everything below; a root
    /// granted nothing gets a swarm that can never spawn, which is a
    /// legitimate configuration and not a mistake.
    pub fn root(
        &mut self,
        code: &[u8],
        caps: Vec<Grant>,
        budget: Budget,
    ) -> Result<InstanceId, SwarmError> {
        check_grant_names(&caps).map_err(SwarmError::Error)?;
        let index = self.claim().ok_or_else(|| {
            SwarmError::Limit(format!(
                "the instance table is full ({})",
                self.max_instances
            ))
        })?;
        {
            let slot = &mut self.slots[index];
            slot.parent = 0;
            slot.caps = CapSet::root(caps);
            slot.budget = budget;
            slot.unsafe_stdlib = self.unsafe_stdlib;
        }
        match self.build(index, code) {
            Ok(()) => Ok(InstanceId(self.slots[index].id)),
            Err(why) => {
                self.release(index);
                Err(SwarmError::Error(why))
            }
        }
    }

    pub fn alive(&self) -> usize {
        self.slots.iter().filter(|s| s.id != 0 && s.alive).count()
    }

    /// Every live handle, in table order — the roster `drt ps` walks.
    pub fn ids(&self) -> Vec<InstanceId> {
        self.slots
            .iter()
            .filter(|s| s.id != 0 && s.alive)
            .map(|s| InstanceId(s.id))
            .collect()
    }

    /// One slot's cost. Unlike `dvs.c`, which allocates `max_instances`
    /// slots up front, the table here grows to what has been claimed — so
    /// the memory a swarm reserves is [`Swarm::slots_allocated`] × this,
    /// not the bound × this.
    pub const fn slot_bytes() -> usize {
        std::mem::size_of::<Slot>()
    }

    /// How many slots the table actually holds, used or free.
    pub fn slots_allocated(&self) -> usize {
        self.slots.len()
    }

    pub fn parent(&self, id: InstanceId) -> Option<InstanceId> {
        let index = self.find(id)?;
        Some(InstanceId(self.slots[index].parent))
    }

    /// Live instance access, for a host that pumps queues directly.
    pub fn instance_mut(&mut self, id: InstanceId) -> Option<&mut (dyn Instance + '_)> {
        let index = self.find(id)?;
        self.slots[index].inst.as_deref_mut().map(|inst| inst as _)
    }

    pub fn resident(&self, id: InstanceId) -> bool {
        self.find(id)
            .map(|i| self.slots[i].alive && self.slots[i].inst.is_some())
            .unwrap_or(false)
    }

    pub fn cached_size(&self, id: InstanceId) -> usize {
        self.find(id)
            .and_then(|i| self.slots[i].snap.as_ref().map(Vec::len))
            .unwrap_or(0)
    }

    pub fn budget(&self, id: InstanceId) -> Option<Budget> {
        self.find(id).map(|i| self.slots[i].budget)
    }

    /// The capability set, with its provenance — for a host that logs or
    /// audits. Enforcement asks [`Swarm::holds`]; auditability needs the set
    /// readable out.
    pub fn caps(&self, id: InstanceId) -> Option<Arc<CapSet>> {
        self.find(id).map(|i| Arc::clone(&self.slots[i].caps))
    }

    pub fn holds(&self, id: InstanceId, cap: &str) -> bool {
        self.find(id)
            .map(|i| self.slots[i].caps.holds(cap))
            .unwrap_or(false)
    }

    /// Attenuation only, no exceptions: granting is holding.
    pub fn may_grant(&self, parent: InstanceId, cap: &str) -> bool {
        self.holds(parent, cap)
    }

    // -------------------------------------------------------------- events --

    /// Push an event into an instance's `system/events`, if it declared one.
    /// Monitor semantics only, one-directional: a parent hears about a
    /// child; a child hears nothing about a parent. A full events queue is
    /// not an error and not a retry — a supervisor that does not drain its
    /// events has chosen to miss them.
    fn emit(&mut self, to: u32, what: &str, about: u32, detail: Option<&str>) {
        let Some(index) = self.find(InstanceId(to)) else {
            return;
        };
        let Some(inst) = self.slots[index].inst.as_deref_mut() else {
            return;
        };
        let Some(q) = inst.queue("system/events") else {
            return; // it did not declare one; nothing to say
        };
        let mut map = vec![
            ("event".into(), rmpv::Value::from(what)),
            ("id".into(), rmpv::Value::from(about)),
        ];
        if let Some(detail) = detail {
            let clamped = clamp_utf8(detail, MAX_EVENT_DETAIL);
            map.push(("detail".into(), rmpv::Value::from(clamped)));
        }
        let mut buf = Vec::new();
        if rmpv::encode::write_value(&mut buf, &rmpv::Value::Map(map)).is_ok() {
            let _ = inst.push(q, &buf);
        }
    }

    // -------------------------------------------------------- subtree kill --

    /// Kill an instance and everything below it, by mark and sweep over the
    /// flat table — no recursion, so unbounded delegation depth cannot meet
    /// the stack, and a parentage cycle cannot hang it.
    fn kill_subtree(&mut self, id: InstanceId, notify_parent: bool) {
        let Some(target) = self.find(id) else { return };
        let parent = self.slots[target].parent;
        for slot in &mut self.slots {
            slot.doomed = false;
        }
        self.slots[target].doomed = true;
        loop {
            let mut changed = false;
            for i in 0..self.slots.len() {
                let slot = &self.slots[i];
                if slot.id == 0 || slot.doomed || slot.parent == 0 {
                    continue;
                }
                let parent_doomed = self
                    .find(InstanceId(slot.parent))
                    .map(|p| self.slots[p].doomed)
                    .unwrap_or(false);
                if parent_doomed {
                    self.slots[i].doomed = true;
                    changed = true;
                }
            }
            if !changed {
                break;
            }
        }
        // Release after marking, never during: a slot freed mid-walk would
        // break the parent lookups the walk still depends on.
        for i in 0..self.slots.len() {
            if self.slots[i].id != 0 && self.slots[i].doomed {
                self.release(i);
            }
        }
        if notify_parent && parent != 0 {
            self.emit(parent, "exited", id.0, Some("killed"));
        }
    }

    pub fn kill(&mut self, id: InstanceId) -> Result<(), SwarmError> {
        if self.find(id).is_none() {
            return Err(SwarmError::Unknown);
        }
        self.kill_subtree(id, true);
        Ok(())
    }

    // ---------------------------------------------------- hibernate / wake --

    /// Swap an instance out to the cache. It must be parked; a running or
    /// already non-resident instance is refused rather than forced.
    pub fn hibernate(&mut self, id: InstanceId) -> Result<(), SwarmError> {
        let Some(index) = self.find(id) else {
            return Err(SwarmError::Gone);
        };
        if !self.slots[index].alive {
            return Err(SwarmError::Gone);
        }
        if !self.allow_hibernation {
            return Err(SwarmError::Error(
                "hibernation is switched off for this swarm: this host called \
                 allow_hibernation(false). Call it with true to switch it back on."
                    .to_string(),
            ));
        }
        if self.slots[index].inst.is_none() {
            return Ok(()); // already cached; asking twice is not an error
        }
        let identity = self.host_identity.clone();
        let inst = self.slots[index]
            .inst
            .as_deref_mut()
            .expect("checked above");
        let snap = inst
            .snapshot(identity.as_deref())
            .map_err(|e| SwarmError::Error(format!("instance {} will not hibernate: {e}", id.0)))?;
        let slot = &mut self.slots[index];
        slot.snap = Some(snap);
        // The host's context goes with the instance; waking calls `attached`
        // again, the same contract a spawn has. The handles go with it too:
        // they named queues in an instance that no longer exists.
        slot.inst = None;
        slot.handles.clear();
        self.host.detached(id);
        Ok(())
    }

    pub fn wake(&mut self, id: InstanceId) -> Result<(), SwarmError> {
        let Some(index) = self.find(id) else {
            return Err(SwarmError::Gone);
        };
        if !self.slots[index].alive {
            return Err(SwarmError::Gone);
        }
        if self.slots[index].inst.is_some() {
            return Ok(()); // already resident
        }
        let Some(snap) = self.slots[index].snap.take() else {
            return Err(SwarmError::Error(format!(
                "instance {} has no cached snapshot",
                id.0
            )));
        };
        let identity = self.host_identity.clone();
        let budget = self.slots[index].budget;
        let unsafe_stdlib = self.slots[index].unsafe_stdlib;
        let mut inst = match self.engine.restore(RestoreSpec {
            snapshot: &snap,
            host_stamp: identity.as_deref(),
            budget,
            unsafe_stdlib,
        }) {
            Ok(inst) => inst,
            Err(e) => {
                // Put the bytes back: the caller decides the instance's fate.
                self.slots[index].snap = Some(snap);
                return Err(SwarmError::Error(format!(
                    "instance {} will not restore: {e}",
                    id.0
                )));
            }
        };
        // The buffer drains here, before any live push can reach the
        // instance. A message refused now is dropped rather than kept: the
        // queue it names is bounded, the sender was already told Ok, and
        // holding it would be an unbounded buffer wearing a different name.
        for pending in self.slots[index].pending.drain(..) {
            if let Some(q) = inst.queue(&pending.queue) {
                let _ = inst.push(q, &pending.msg);
            }
        }
        self.slots[index].inst = Some(inst);
        self.slots[index].handles.clear();
        self.host.attached(id);
        Ok(())
    }

    // ----------------------------------------------------------- delivery --

    /// Resolve a queue by name for a resident instance, remembering the
    /// answer. Every message would otherwise pay a `dv_queue_lookup` (a
    /// string lookup inside the guest) plus the engine's own intern scan —
    /// measured at 20-25% of a round trip (`bench/README.md`).
    fn resolve_queue(&mut self, index: usize, queue: &str) -> Option<QueueHandle> {
        if let Some((_, handle)) = self.slots[index]
            .handles
            .iter()
            .find(|(name, _)| name == queue)
        {
            return Some(*handle);
        }
        let handle = self.slots[index].inst.as_deref_mut()?.queue(queue)?;
        self.slots[index].handles.push((queue.to_string(), handle));
        Some(handle)
    }

    /// The host's side of the delivery table: resident → the queue; dead or
    /// unknown → [`SwarmError::Gone`], immediately; cached with
    /// `wake_on_message` → a bounded buffer, drained ahead of live pushes on
    /// the next step; cached without → gone.
    pub fn push(&mut self, id: InstanceId, queue: &str, msg: &[u8]) -> Result<(), SwarmError> {
        let Some(index) = self.find(id) else {
            return Err(SwarmError::Gone);
        };
        if !self.slots[index].alive {
            return Err(SwarmError::Gone);
        }
        if self.slots[index].inst.is_some() {
            let Some(q) = self.resolve_queue(index, queue) else {
                return Err(SwarmError::UnknownQueue);
            };
            let inst = self.slots[index]
                .inst
                .as_deref_mut()
                .expect("checked above");
            return match inst.push(q, msg) {
                Ok(PushOutcome::Accepted) => Ok(()),
                Ok(_) => Err(SwarmError::Limit(format!(
                    "'{queue}' did not accept the message"
                ))),
                Err(e) => Err(SwarmError::Error(e.to_string())),
            };
        }
        // Cached. An agent that did not ask to be woken is, from a sender's
        // point of view, not there.
        if !self.slots[index].wake_on_message {
            return Err(SwarmError::Gone);
        }
        if queue.is_empty() || queue.len() >= MAX_QNAME {
            return Err(SwarmError::Error(format!(
                "'{queue}' is not a usable queue name"
            )));
        }
        if self.slots[index].pending.len() >= MAX_PENDING {
            return Err(SwarmError::Limit(format!(
                "the wake buffer holds {MAX_PENDING} messages and is full"
            )));
        }
        self.slots[index].pending.push(Pending {
            queue: queue.to_string(),
            msg: msg.to_vec(),
        });
        Ok(())
    }

    // ----------------------------------------------- draining the lifecycle --

    /// Run the swarm one step: wake anything with buffered messages, drain
    /// every instance's `system/lifecycle`, then drive each resident
    /// instance once. One step rather than a loop — there is no scheduler in
    /// here, and a host that wants one writes it. Returns how many instances
    /// are alive, so a caller's loop condition is obvious.
    pub fn step(&mut self) -> usize {
        self.spawns_this_step = 0;
        // Waking first, so a woken instance gets a whole step in the same
        // step its message arrived in. A wake that fails is fatal: the
        // alternative is a handle alive, non-resident, and permanently
        // unreachable.
        for i in 0..self.slots.len() {
            let slot = &self.slots[i];
            if slot.id == 0 || !slot.alive || slot.inst.is_some() || slot.pending.is_empty() {
                continue;
            }
            let id = InstanceId(slot.id);
            let parent = slot.parent;
            if let Err(e) = self.wake(id) {
                let why = e.to_string();
                self.kill_subtree(id, false);
                if parent != 0 {
                    self.emit(parent, "faulted", id.0, Some(&why));
                }
            }
        }
        for i in 0..self.slots.len() {
            if self.slots[i].id != 0 && self.slots[i].alive {
                self.drain(i);
            }
        }
        for i in 0..self.slots.len() {
            let slot = &self.slots[i];
            // A cached instance is not driven: there is nothing to drive. It
            // is still alive and counted, because a handle that names a
            // snapshot is a handle a sender may legitimately push to.
            if slot.id == 0 || !slot.alive || slot.inst.is_none() {
                continue;
            }
            let id = slot.id;
            let driven = {
                let (host, slots) = (&mut self.host, &mut self.slots);
                let caps = Arc::clone(&slots[i].caps);
                let inst = slots[i].inst.as_deref_mut().expect("checked above");
                host.drive(InstanceId(id), &caps, inst)
            };
            // Driving can free slots (a supervisor may kill its own subtree
            // through a later host call); re-find before acting.
            let Some(index) = self.find(InstanceId(id)) else {
                continue;
            };
            match driven {
                Driven::Alive => {}
                outcome => {
                    let parent = self.slots[index].parent;
                    let over = self.slots[index]
                        .inst
                        .as_deref()
                        .map(|inst| inst.exceeded())
                        .unwrap_or(false);
                    let (what, why) = match (&outcome, over) {
                        (_, true) => ("exceeded", None),
                        (Driven::Faulted(msg), false) => ("faulted", Some(msg.clone())),
                        _ => ("exited", None),
                    };
                    self.kill_subtree(InstanceId(id), false);
                    if parent != 0 {
                        self.emit(parent, what, id, why.as_deref());
                    }
                }
            }
        }
        self.alive()
    }

    /// Drain one instance's lifecycle queue. The capability check is here
    /// and not at declare time: a program without the lifecycle capability
    /// may declare the queue and write to it all it likes, and nothing will
    /// ever read it — refusal by mechanism, not by special case.
    fn drain(&mut self, index: usize) {
        let id = self.slots[index].id;
        if self.slots[index].inst.is_none() || !self.slots[index].alive {
            return;
        }
        if !self.slots[index].caps.holds(CAP_LIFECYCLE) {
            return;
        }
        loop {
            // A throttled spawn from a previous step goes first, in order.
            let msg: Vec<u8> = if let Some(deferred) = self.slots[index].deferred.take() {
                deferred
            } else {
                let Some(inst) = self.slots[index].inst.as_deref_mut() else {
                    return;
                };
                let Some(q) = inst.queue("system/lifecycle") else {
                    return; // it never declared one
                };
                match inst.pop(q) {
                    Ok(Some(msg)) => msg,
                    _ => return,
                }
            };
            if msg.len() > crate::REQUEST_CAP_BYTES {
                self.emit(id, "denied", 0, Some("the request is too large"));
                continue;
            }
            let request = read_request(&msg);
            let op = match request.as_ref().and_then(|r| field_str(r, "op")) {
                Some(op) => op,
                None => {
                    self.emit(id, "denied", 0, Some("no op in the request"));
                    continue;
                }
            };
            // The rate limit is a rate, not a filter: a spawn the limit
            // refuses is held and re-tried next step, in order, and one
            // "throttled" event per step tells the requester to back off.
            if op == "spawn" && self.spawns_this_step >= self.spawn_rate {
                self.slots[index].deferred = Some(msg);
                self.emit(id, "throttled", 0, Some("spawn rate limit"));
                return;
            }
            let request = request.expect("op was read from it");
            match op.as_str() {
                "spawn" => self.do_spawn(index, &request),
                "kill" => self.do_kill(index, &request),
                "query" => self.do_query(index, &request),
                "hibernate" => self.do_hibernate(index, &request),
                other => self.emit(id, "denied", 0, Some(other)),
            }
            // Acting on a request may have freed this slot or swapped its
            // instance out (a program may hibernate itself). Re-check.
            let slot = &self.slots[index];
            if slot.id != id || !slot.alive || slot.inst.is_none() {
                return;
            }
        }
    }

    fn do_spawn(&mut self, parent_index: usize, request: &rmpv::Value) {
        let parent_id = self.slots[parent_index].id;
        if self.spawns_this_step >= self.spawn_rate {
            // Normally unreachable (drain defers first), kept because a
            // limit enforced only in the caller is one refactor from gone.
            self.emit(parent_id, "denied", 0, Some("spawn rate limit"));
            return;
        }
        let Some(code) = field_bytes(request, "code") else {
            self.emit(parent_id, "denied", 0, Some("no code in the spawn request"));
            return;
        };
        let names = match field_caps(request) {
            Ok(names) => names,
            Err(()) => {
                self.emit(parent_id, "denied", 0, Some("malformed caps"));
                return;
            }
        };
        // Attenuation, before anything is built: a denied spawn costs
        // nothing and leaves nothing behind. The named capability comes back
        // in the event so the requester knows which grant was refused.
        let mut grants: Vec<Grant> = names.iter().map(|n| Grant::grant(n.clone())).collect();
        // Parent denies are carried automatically: a spawn request names
        // capabilities, and a name must never shed a deny.
        grants.extend(
            self.slots[parent_index]
                .caps
                .grants()
                .iter()
                .filter(|g| g.effect == Effect::Deny)
                .cloned(),
        );
        let parent_caps = Arc::clone(&self.slots[parent_index].caps);
        let child_caps =
            match parent_caps.attenuate(Principal(format!("instance-{parent_id}")), grants) {
                Ok(set) => set,
                Err(drt_caps::AttenuationError::NotHeldByParent { capability }) => {
                    self.emit(parent_id, "denied", 0, Some(&capability));
                    return;
                }
                Err(e) => {
                    self.emit(parent_id, "denied", 0, Some(&e.to_string()));
                    return;
                }
            };
        let budget = field_budget(request);
        let Some(child_index) = self.claim() else {
            self.emit(parent_id, "denied", 0, Some("the instance table is full"));
            return;
        };
        // Flags attenuate: inherit the parent's set; `sealed = true` drops
        // the stdlib, and nothing adds it.
        let child_stdlib = self.slots[parent_index].unsafe_stdlib && !field_bool(request, "sealed");
        {
            let slot = &mut self.slots[child_index];
            slot.parent = parent_id;
            slot.caps = child_caps;
            slot.budget = budget;
            slot.wake_on_message = field_bool(request, "wake_on_message");
            slot.unsafe_stdlib = child_stdlib;
        }
        match self.build(child_index, &code) {
            Ok(()) => {
                self.spawns_this_step += 1;
                let child_id = self.slots[child_index].id;
                self.emit(parent_id, "spawned", child_id, None);
            }
            Err(why) => {
                let gone = self.slots[child_index].id;
                self.release(child_index);
                self.emit(parent_id, "faulted", gone, Some(&why));
            }
        }
    }

    /// Only an ancestor may act on another instance. Parentage is the only
    /// relation this layer knows, and "any instance may kill any other"
    /// would make the capability set meaningless.
    fn is_ancestor(&self, ancestor: u32, of: usize) -> bool {
        let mut walk = self.slots[of].parent;
        while walk != 0 {
            if walk == ancestor {
                return true;
            }
            walk = self
                .find(InstanceId(walk))
                .map(|i| self.slots[i].parent)
                .unwrap_or(0);
        }
        false
    }

    fn do_kill(&mut self, parent_index: usize, request: &rmpv::Value) {
        let parent_id = self.slots[parent_index].id;
        let Some(target) = field_id(request, "id") else {
            self.emit(
                parent_id,
                "denied",
                0,
                Some("no usable id in the kill request"),
            );
            return;
        };
        let Some(target_index) = self.find(InstanceId(target)) else {
            self.emit(parent_id, "denied", target, Some("no such instance"));
            return;
        };
        if self.is_ancestor(parent_id, target_index) {
            self.kill_subtree(InstanceId(target), true);
        } else {
            self.emit(parent_id, "denied", target, Some("not a descendant"));
        }
    }

    /// `{op = "hibernate"}` swaps the requester out; with an `id`, a
    /// descendant — who still chooses the moment by choosing when to park.
    fn do_hibernate(&mut self, parent_index: usize, request: &rmpv::Value) {
        let parent_id = self.slots[parent_index].id;
        let parent_parent = self.slots[parent_index].parent;
        let mut who = parent_id;
        if let Some(target) = field_id(request, "id") {
            if target != parent_id {
                let Some(target_index) = self.find(InstanceId(target)) else {
                    self.emit(parent_id, "denied", target, Some("no such instance"));
                    return;
                };
                if !self.is_ancestor(parent_id, target_index) {
                    self.emit(parent_id, "denied", target, Some("not a descendant"));
                    return;
                }
                who = target;
            }
        }
        // `wake_on_message` may be set here as well as at spawn time, and
        // this is the better place: the program going to sleep is what knows
        // whether it wants to be woken. Absent, the spawn-time value stands.
        if field_bool(request, "wake_on_message") {
            if let Some(subject) = self.find(InstanceId(who)) {
                self.slots[subject].wake_on_message = true;
            }
        }
        if let Err(e) = self.hibernate(InstanceId(who)) {
            // The event goes to the requester: the thing that needs to know
            // is whatever asked.
            let why = e.to_string();
            self.emit(parent_id, "denied", who, Some(&why));
            return;
        }
        // An instance that hibernated itself is about to stop reading, so
        // its own parent is told instead.
        if who != parent_id {
            self.emit(parent_id, "hibernated", who, None);
        } else if parent_parent != 0 {
            self.emit(parent_parent, "hibernated", who, None);
        }
    }

    fn do_query(&mut self, parent_index: usize, request: &rmpv::Value) {
        let parent_id = self.slots[parent_index].id;
        let Some(target) = field_id(request, "id") else {
            self.emit(parent_id, "denied", 0, Some("no usable id in the query"));
            return;
        };
        let Some(target_index) = self.find(InstanceId(target)) else {
            self.emit(parent_id, "status", target, Some("gone"));
            return;
        };
        let slot = &self.slots[target_index];
        let (state, usage) = match slot.inst.as_deref() {
            None => ("cached", None),
            Some(inst) => (
                if slot.alive { "alive" } else { "dead" },
                Some(inst.usage()),
            ),
        };
        let usage = usage.unwrap_or_default();
        let detail = format!(
            "{state} insns={} mem_kb={}",
            usage.instructions, usage.memory_kb_peak
        );
        self.emit(parent_id, "status", target, Some(&detail));
    }
}

// ------------------------------------------------------- request reading --

/// Read one msgpack value; trailing bytes are tolerated, exactly as the C
/// cursor tolerates them.
fn read_request(msg: &[u8]) -> Option<rmpv::Value> {
    let mut cursor = msg;
    rmpv::decode::read_value(&mut cursor).ok()
}

fn field<'a>(request: &'a rmpv::Value, key: &str) -> Option<&'a rmpv::Value> {
    request
        .as_map()?
        .iter()
        .find(|(k, _)| k.as_str() == Some(key))
        .map(|(_, v)| v)
}

fn field_str(request: &rmpv::Value, key: &str) -> Option<String> {
    match field(request, key)? {
        rmpv::Value::String(s) => Some(String::from_utf8_lossy(s.as_bytes()).into_owned()),
        _ => None,
    }
}

/// A string field's raw bytes: a compiled chunk is full of zeroes and need
/// not be UTF-8, so the length is the length and never `strlen`.
fn field_bytes(request: &rmpv::Value, key: &str) -> Option<Vec<u8>> {
    match field(request, key)? {
        rmpv::Value::String(s) => Some(s.as_bytes().to_vec()),
        _ => None,
    }
}

/// Integers, and floats too: a budget written `5e6` arrives as a float,
/// which is what the design's own example shows.
fn field_int(request: &rmpv::Value, key: &str) -> Option<u64> {
    match field(request, key)? {
        rmpv::Value::Integer(n) => n.as_u64(),
        rmpv::Value::F64(f) if *f >= 0.0 && *f < 1e18 => Some(*f as u64),
        rmpv::Value::F32(f) if *f >= 0.0 => Some(*f as u64),
        _ => None,
    }
}

/// An instance handle: 32 bits on a wire that carries 64, so a value that
/// does not survive the round trip is refused rather than truncated onto
/// a different live instance. Zero is refused here too.
fn field_id(request: &rmpv::Value, key: &str) -> Option<u32> {
    let v = field_int(request, key)?;
    if v == 0 || v > u64::from(u32::MAX) {
        return None;
    }
    Some(v as u32)
}

fn field_bool(request: &rmpv::Value, key: &str) -> bool {
    matches!(field(request, key), Some(rmpv::Value::Boolean(true)))
}

/// The budget, read the way the design writes it: nested
/// `budget = {instructions=…, memory_kb=…}` wins; the flat top-level form is
/// still accepted because it was understood historically.
fn field_budget(request: &rmpv::Value) -> Budget {
    let mut instructions = field_int(request, "instructions");
    let mut memory_kb = field_int(request, "memory_kb");
    if let Some(nested) = field(request, "budget") {
        if nested.is_map() {
            instructions = field_int(nested, "instructions").or(instructions);
            memory_kb = field_int(nested, "memory_kb").or(memory_kb);
        }
    }
    Budget {
        instructions,
        memory_kb,
    }
}

/// The `caps` array. Absent is an empty set; so is an empty map, because an
/// empty Lua table is a map on the wire. A non-empty map, a non-array, an
/// over-long name, or too many entries is malformed — refused rather than
/// silently taken as nothing.
fn field_caps(request: &rmpv::Value) -> Result<Vec<String>, ()> {
    let Some(value) = field(request, "caps") else {
        return Ok(Vec::new());
    };
    if let Some(map) = value.as_map() {
        return if map.is_empty() {
            Ok(Vec::new())
        } else {
            Err(())
        };
    }
    let Some(entries) = value.as_array() else {
        return Err(());
    };
    if entries.len() > MAX_CAPS {
        return Err(());
    }
    let mut names = Vec::with_capacity(entries.len());
    for entry in entries {
        let rmpv::Value::String(s) = entry else {
            return Err(());
        };
        let bytes = s.as_bytes();
        if bytes.is_empty() || bytes.len() >= MAX_CAP_LEN {
            return Err(());
        }
        names.push(String::from_utf8_lossy(bytes).into_owned());
    }
    Ok(names)
}

fn check_grant_names(grants: &[Grant]) -> Result<(), String> {
    if grants.len() > MAX_CAPS {
        return Err(format!(
            "a capability set of {} is beyond the {MAX_CAPS} this layer holds",
            grants.len()
        ));
    }
    for grant in grants {
        let n = grant.capability.len();
        if n == 0 || n >= MAX_CAP_LEN {
            return Err(format!("a capability name of {n} characters is not usable"));
        }
    }
    Ok(())
}

/// Clamp to at most `max` bytes on a character boundary — a lie about the
/// length would be worse than a shorter detail.
fn clamp_utf8(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}
