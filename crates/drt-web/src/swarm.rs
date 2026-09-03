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
        self.inner.alive()
    }

    /// The roster, as ids: `dvs_instance` handed back a pointer and this
    /// hands back the ids a page can hold on to.
    pub fn ids(&self) -> Vec<u32> {
        self.inner.ids().into_iter().map(|id| id.0).collect()
    }

    pub fn slots_allocated(&self) -> usize {
        self.inner.slots_allocated()
    }

    /// Who spawned `id`: 0 for the root, whose parent is nobody, and
    /// `None` for an id that is not in the roster -- a distinction
    /// `dvs_parent` could not make, having only the one answer.
    pub fn parent(&self, id: u32) -> Option<u32> {
        self.inner.parent(InstanceId(id)).map(|p| p.0)
    }

    pub fn resident(&self, id: u32) -> bool {
        self.inner.resident(InstanceId(id))
    }

    pub fn cached_size(&self, id: u32) -> usize {
        self.inner.cached_size(InstanceId(id))
    }

    pub fn wake_on_message(&self, id: u32) -> bool {
        self.inner.wake_on_message(InstanceId(id))
    }

    /// What `id` may hold, as the JSON a config would have written.
    pub fn caps(&self, id: u32) -> Option<String> {
        self.inner
            .caps(InstanceId(id))
            .map(|set| serde_json::to_string(set.grants()).unwrap_or_else(|_| "[]".into()))
    }

    pub fn holds(&self, id: u32, cap: &str) -> bool {
        self.inner.holds(InstanceId(id), cap)
    }

    /// Whether `parent` could pass `cap` to something it spawns -- the
    /// question a panel asks before offering the button.
    pub fn may_grant(&self, parent: u32, cap: &str) -> bool {
        self.inner.may_grant(InstanceId(parent), cap)
    }

    pub fn budget(&self, id: u32) -> Option<String> {
        self.inner
            .budget(InstanceId(id))
            .map(|b| serde_json::to_string(&b).unwrap_or_else(|_| "{}".into()))
    }

    /// A msgpack message onto one of `id`'s queues.
    pub fn push(&mut self, id: u32, queue: &str, msg: &[u8]) -> Result<(), String> {
        self.inner
            .push(InstanceId(id), queue, msg)
            .map_err(|e| e.to_string())
    }

    pub fn kill(&mut self, id: u32) -> Result<(), String> {
        self.inner.kill(InstanceId(id)).map_err(|e| e.to_string())
    }

    pub fn hibernate(&mut self, id: u32) -> Result<(), String> {
        self.inner
            .hibernate(InstanceId(id))
            .map_err(|e| e.to_string())
    }

    pub fn wake(&mut self, id: u32) -> Result<(), String> {
        self.inner.wake(InstanceId(id)).map_err(|e| e.to_string())
    }

    pub fn allow_hibernation(&mut self, allow: bool) {
        self.inner.allow_hibernation(allow);
    }

    pub fn allow_bytecode(&mut self, allow: bool) {
        self.inner.allow_bytecode(allow);
    }

    pub fn allow_unsafe_stdlib(&mut self, allow: bool) {
        self.inner.allow_unsafe_stdlib(allow);
    }

    pub fn set_host_identity(&mut self, identity: Option<&str>) {
        self.inner.set_host_identity(identity);
    }
}
