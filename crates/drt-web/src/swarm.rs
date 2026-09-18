//! The swarm exports (doc/Wasm.md M5, doc/Browser.md): `dvs.c`'s table,
//! in this module, over the deployment `drt start` drives.
//!
//! The Lab's Instances panel calls sixteen `dvs_*` entry points on
//! `diluvium_swarm_wasi.wasm`, and `swarm.js`'s `swarmCapable(exports)` is
//! where a second backend is recognised. This is that second backend: the
//! same operations, named the way JavaScript names things, taking ids and
//! byte arrays rather than pointers. A page building p2p apps never
//! touches a pointer, so DRT does not impersonate a C ABI to be adopted --
//! a `drtCapable` beside `swarmCapable` is the migration.
//!
//! What a host gains by moving is everything a `Deployment` is over a bare
//! swarm: connectors behind the capability grants, hibernation and wake,
//! and the residency policy -- so the panel stops being a viewer of the C
//! swarm and becomes a host of this one.
//!
//! ## surface block
//!
//! - [`Swarm::new`]: the table's `dvsjs_new`, with the build's connectors
//!   or a config's.
//! - [`Swarm::root`]: the first instance; every other one is spawned by a
//!   program.
//! - [`Swarm::step`]: one round, answering how many are alive.
//! - The roster and its questions: [`Swarm::ids`], [`Swarm::parent`],
//!   [`Swarm::alive`], [`Swarm::slots_allocated`], [`Swarm::resident`],
//!   [`Swarm::cached_size`], [`Swarm::wake_on_message`].
//! - The capability questions: [`Swarm::caps`], [`Swarm::holds`],
//!   [`Swarm::may_grant`], [`Swarm::budget`].
//! - The verbs: [`Swarm::push`], [`Swarm::kill`], [`Swarm::hibernate`],
//!   [`Swarm::wake`].
//! - The switches: [`Swarm::allow_hibernation`], [`Swarm::allow_bytecode`],
//!   [`Swarm::allow_unsafe_stdlib`], [`Swarm::set_host_identity`].
//! - [`DEFAULT_CAPS`]: what `root` grants when a page names nothing.
//!
//! `dvs_last_error` has no twin: an error is thrown where it happens
//! rather than left for a host to poll, which is the one place this table
//! deliberately stops matching.

use std::sync::Arc;

use drt::start::Deployment;
use drt_caps::Grant;
use drt_config::Budget;
use drt_swarm::engine::diluvium_engine::DiluviumEngine;
use drt_swarm::pump::PumpHost;
use drt_swarm::swarm::Swarm as Inner;
use drt_swarm::InstanceId;

/// The ceiling `root` uses when a page passes no caps: the same
/// `host:*` a config-less `drt run` gets, and the same one
/// `drt::config::ceiling` hands a program whose config lists none.
pub const DEFAULT_CAPS: &str = r#"[{"capability":"host:*"}]"#;

/// The roster questions and the verbs, over a **borrowed** deployment.
///
/// Free functions rather than methods because a page can hold a
/// deployment two ways: [`Swarm`], which owns one, and a `drt start`
/// session, which is driving one. Both answer the same questions, and a
/// panel that could only see the first would be a panel that never showed
/// the agents someone started in the terminal -- which was the state of
/// things before this module existed.
///
/// Nothing here is browser-only; the marshalling lives in `bindings`.
pub mod view {
    use super::*;

    pub fn alive(d: &Deployment) -> usize {
        d.alive()
    }

    /// The roster, as ids: `dvs_instance` handed back a pointer and this
    /// hands back the ids a page can hold on to.
    pub fn ids(d: &Deployment) -> Vec<u32> {
        d.ids().into_iter().map(|id| id.0).collect()
    }

    pub fn slots_allocated(d: &Deployment) -> usize {
        d.slots_allocated()
    }

    /// Who spawned `id`: 0 for the root, whose parent is nobody, and
    /// `None` for an id that is not in the roster -- a distinction
    /// `dvs_parent` could not make, having only the one answer.
    pub fn parent(d: &Deployment, id: u32) -> Option<u32> {
        d.parent(InstanceId(id)).map(|p| p.0)
    }

    pub fn resident(d: &Deployment, id: u32) -> bool {
        d.resident(InstanceId(id))
    }

    pub fn cached_size(d: &Deployment, id: u32) -> usize {
        d.cached_size(InstanceId(id))
    }

    pub fn wake_on_message(d: &Deployment, id: u32) -> bool {
        d.wake_on_message(InstanceId(id))
    }

    /// What `id` may hold, as the JSON a config would have written.
    pub fn caps(d: &Deployment, id: u32) -> Option<String> {
        d.caps(InstanceId(id))
            .map(|set| serde_json::to_string(set.grants()).unwrap_or_else(|_| "[]".into()))
    }

    pub fn holds(d: &Deployment, id: u32, cap: &str) -> bool {
        d.holds(InstanceId(id), cap)
    }

    /// Whether `parent` could pass `cap` to something it spawns -- the
    /// question a panel asks before offering the button.
    pub fn may_grant(d: &Deployment, parent: u32, cap: &str) -> bool {
        d.may_grant(InstanceId(parent), cap)
    }

    pub fn budget(d: &Deployment, id: u32) -> Option<String> {
        d.budget(InstanceId(id))
            .map(|b| serde_json::to_string(&b).unwrap_or_else(|_| "{}".into()))
    }

    /// What `id` has spent and holds right now, as JSON.
    ///
    /// `None` for a hibernated instance, which is the answer rather than a
    /// gap -- see `Swarm::usage` in `drt-swarm`. A panel showing
    /// `bytes_now` beside `memory_kb_peak` is showing what an idle agent
    /// costs against its high-water mark, which is what hibernation is
    /// for.
    pub fn usage(d: &Deployment, id: u32) -> Option<String> {
        d.usage(InstanceId(id))
            .map(|u| serde_json::to_string(&u).unwrap_or_else(|_| "{}".into()))
    }

    /// A msgpack message onto one of `id`'s queues.
    ///
    /// The page is the runtime here: a browser root has no `.drt_root/`
    /// and so no id to name, and there are no peers to tell apart on the
    /// no-root path. The message still carries a sender, because "every
    /// delivered message carries one" is not a rule with a browser
    /// exception.
    pub fn push(d: &mut Deployment, id: u32, queue: &str, msg: &[u8]) -> Result<(), String> {
        let from = drt_config::peer::Sender::runtime(drt_config::project::NodePath::root());
        d.push(InstanceId(id), queue, &from, msg)
            .map_err(|e| e.to_string())
    }

    pub fn kill(d: &mut Deployment, id: u32) -> Result<(), String> {
        d.kill(InstanceId(id)).map_err(|e| e.to_string())
    }

    pub fn hibernate(d: &mut Deployment, id: u32) -> Result<(), String> {
        d.hibernate(InstanceId(id)).map_err(|e| e.to_string())
    }

    pub fn wake(d: &mut Deployment, id: u32) -> Result<(), String> {
        d.wake(InstanceId(id)).map_err(|e| e.to_string())
    }
}

/// A deployment a page drives.
pub struct Swarm {
    inner: Deployment,
}

impl Swarm {
    /// A swarm over this build's connectors, or over the ones `config`
    /// names.
    ///
    /// Zero for either limit means the swarm's own default, as `dvsjs_new`
    /// meant it. `config` is the same JSON `drt run --config` takes, so a
    /// page that wants `fs` scoped somewhere writes what it would write on
    /// disk; `None` is the zero-ceremony case, the connectors this build
    /// carries that need no scope of their own.
    pub fn new(
        max_instances: u32,
        spawns_per_step: u32,
        config: Option<&str>,
    ) -> Result<Self, String> {
        let config = match config {
            Some(text) => serde_json::from_str(text).map_err(|e| format!("the config: {e}"))?,
            None => {
                let mut config = drt::config::load(None)?;
                drt::cli::local_defaults(&mut config);
                config
            }
        };
        let registry = drt::cli::wire_connectors(&config)?;
        drt::config::validate_grants(&config, &registry)?;
        let engine = Arc::new(DiluviumEngine::new().map_err(|e| e.to_string())?);
        let host = PumpHost::new(
            drt::start::DeployHost::new(),
            drt_connector::Dispatcher::new(registry),
        );
        Ok(Swarm {
            inner: Inner::with_limits(engine, host, max_instances, spawns_per_step),
        })
    }

    /// The first instance, from source and the capabilities it may hold.
    ///
    /// `caps` is a config's `caps` array and `budget` its `budget` object,
    /// both as JSON, because a page that already writes a config should
    /// not learn a second dialect for the same two things.
    pub fn root(&mut self, code: &[u8], caps: &str, budget: &str) -> Result<u32, String> {
        let caps: Vec<Grant> = serde_json::from_str(caps).map_err(|e| format!("the caps: {e}"))?;
        let budget: Budget =
            serde_json::from_str(budget).map_err(|e| format!("the budget: {e}"))?;
        self.inner
            .root(code, caps, budget)
            .map(|id| id.0)
            .map_err(|e| e.to_string())
    }

    /// One round. Answers how many instances are alive, which is a host
    /// loop's own termination condition.
    pub fn step(&mut self) -> usize {
        self.inner.step()
    }

    pub fn alive(&self) -> usize {
        view::alive(&self.inner)
    }

    /// The roster, as ids: `dvs_instance` handed back a pointer and this
    /// hands back the ids a page can hold on to.
    pub fn ids(&self) -> Vec<u32> {
        view::ids(&self.inner)
    }

    pub fn slots_allocated(&self) -> usize {
        view::slots_allocated(&self.inner)
    }

    /// Who spawned `id`: 0 for the root, whose parent is nobody, and
    /// `None` for an id that is not in the roster -- a distinction
    /// `dvs_parent` could not make, having only the one answer.
    pub fn parent(&self, id: u32) -> Option<u32> {
        view::parent(&self.inner, id)
    }

    pub fn resident(&self, id: u32) -> bool {
        view::resident(&self.inner, id)
    }

    pub fn cached_size(&self, id: u32) -> usize {
        view::cached_size(&self.inner, id)
    }

    pub fn wake_on_message(&self, id: u32) -> bool {
        view::wake_on_message(&self.inner, id)
    }

    /// What `id` may hold, as the JSON a config would have written.
    pub fn caps(&self, id: u32) -> Option<String> {
        view::caps(&self.inner, id)
    }

    pub fn holds(&self, id: u32, cap: &str) -> bool {
        view::holds(&self.inner, id, cap)
    }

    /// Whether `parent` could pass `cap` to something it spawns -- the
    /// question a panel asks before offering the button.
    pub fn may_grant(&self, parent: u32, cap: &str) -> bool {
        view::may_grant(&self.inner, parent, cap)
    }

    pub fn budget(&self, id: u32) -> Option<String> {
        view::budget(&self.inner, id)
    }

    /// What `id` has spent and holds right now, as JSON.
    pub fn usage(&self, id: u32) -> Option<String> {
        view::usage(&self.inner, id)
    }

    /// A msgpack message onto one of `id`'s queues.
    ///
    /// The page is the runtime here: a browser root has no `.drt_root/`
    /// and so no id to name, and there are no peers to tell apart on the
    /// no-root path. The message still carries a sender, because "every
    /// delivered message carries one" is not a rule with a browser
    /// exception.
    pub fn push(&mut self, id: u32, queue: &str, msg: &[u8]) -> Result<(), String> {
        view::push(&mut self.inner, id, queue, msg)
    }

    pub fn kill(&mut self, id: u32) -> Result<(), String> {
        view::kill(&mut self.inner, id)
    }

    pub fn hibernate(&mut self, id: u32) -> Result<(), String> {
        view::hibernate(&mut self.inner, id)
    }

    pub fn wake(&mut self, id: u32) -> Result<(), String> {
        view::wake(&mut self.inner, id)
    }

    pub fn allow_hibernation(&mut self, allow: bool) {
        self.inner.allow_hibernation(allow);
    }

    pub fn allow_bytecode(&mut self, allow: bool) {
        self.inner.allow_bytecode(allow);
    }

    /// The whole `debug` library rather than the narrowed one. A debugger
    /// wants it, a deployment does not: see `LoadSpec::unsafe_debug` for
    /// the three escapes it puts back.
    pub fn allow_unsafe_debug(&mut self, allow: bool) {
        self.inner.allow_unsafe_debug(allow);
    }

    pub fn allow_unsafe_stdlib(&mut self, allow: bool) {
        self.inner.allow_unsafe_stdlib(allow);
    }

    pub fn set_host_identity(&mut self, identity: Option<&str>) {
        self.inner.set_host_identity(identity);
    }
}
