//! The JS contract, as a Rust trait.
//!
//! `doc/Browser.md` specifies fifteen functions the JS host supplies. This
//! is that surface in Rust terms, and making it a **trait rather than a
//! direct `extern` block** is the load-bearing decision in this crate:
//!
//! - The engine, the host and every export are written against
//!   `HostBridge`, so they compile and run natively. A mock bridge fakes a
//!   diluvium instance well enough to drive a real [`drt_swarm::swarm::Swarm`],
//!   which means the browser tier gets ordinary `cargo test` coverage
//!   instead of only being exercisable in a browser.
//! - The wasm-bindgen implementation is then one small file whose only job
//!   is marshalling, with no logic to get wrong that tests would not see.
//!
//! Handles are opaque `u32`s minted by the JS side — an index into its own
//! table is the obvious choice. Rust never interprets one.
//!
//! Errors are `String`, because they arrive as a thrown JS value and end up
//! in [`EngineError::Engine`] either way. Nothing on this boundary tries to
//! preserve a typed error across two languages.

use drt_config::Budget;
use drt_swarm::engine::{PushOutcome, QueueHandle, QueueStatus, Step, UsageReport};

/// A JS-side instance, opaque here.
pub type InstanceHandle = u32;

/// What the JS host must provide. See `doc/Browser.md` for the JS-facing
/// spelling of the same fifteen operations.
///
/// `Clone` because every [`crate::engine::BrowserInstance`] holds one; an
/// implementation shares its state internally (`Rc` in a browser, `Arc`
/// under test) and clones cheaply.
pub trait HostBridge:
    Clone + drt_swarm::engine::MaybeSend + drt_swarm::engine::MaybeSync + 'static
{
    // --- Engine ---------------------------------------------------------
    fn abi_version(&self) -> u32;
    /// `program` is source text; bytecode is refused before reaching here
    /// (the browser tier has no verifier either — GUARANTEES.md).
    fn load(
        &self,
        program: &str,
        name: &str,
        budget: Budget,
        unsafe_stdlib: bool,
    ) -> Result<InstanceHandle, String>;
    fn restore(
        &self,
        snapshot: &[u8],
        host_stamp: Option<&str>,
        budget: Budget,
        unsafe_stdlib: bool,
    ) -> Result<InstanceHandle, String>;
    /// Called when a `BrowserInstance` drops, so JS can release its own
    /// entry. A swarm that hibernates and kills leaks the JS table without
    /// it.
    fn release(&self, instance: InstanceHandle);

    // --- Instance -------------------------------------------------------
    fn queue(&self, instance: InstanceHandle, name: &str) -> Option<QueueHandle>;
    fn queue_info(
        &self,
        instance: InstanceHandle,
        queue: QueueHandle,
    ) -> Result<QueueStatus, String>;
    fn push(
        &self,
        instance: InstanceHandle,
        queue: QueueHandle,
        msgpack: &[u8],
    ) -> Result<PushOutcome, String>;
    fn pop(&self, instance: InstanceHandle, queue: QueueHandle) -> Result<Option<Vec<u8>>, String>;
    fn run(&self, instance: InstanceHandle) -> Result<Step, String>;
    fn resume(&self, instance: InstanceHandle, fired: QueueHandle) -> Result<Step, String>;
    fn resume_timeout(&self, instance: InstanceHandle) -> Result<Step, String>;
    fn current_wait(&self, instance: InstanceHandle) -> Option<drt_swarm::engine::WaitSet>;
    fn usage(&self, instance: InstanceHandle) -> UsageReport;
    fn exceeded(&self, instance: InstanceHandle) -> bool;
    fn snapshot(
        &self,
        instance: InstanceHandle,
        host_stamp: Option<&str>,
    ) -> Result<Vec<u8>, String>;

    // --- Host -----------------------------------------------------------
    /// One drive of one instance, synchronously.
    ///
    /// Synchronous is the design, not a limitation: the swarm drives
    /// instances synchronously, which is why the native pump uses
    /// `pollster`. In a browser that is exactly what the Lab's deferred
    /// pattern already answers — a hostcall that cannot reply now returns
    /// pending, and the reply lands on a later step. See `doc/Browser.md`.
    fn drive(&self, id: u32, instance: InstanceHandle) -> Driven;
}

/// What one drive produced. Mirrors [`drt_swarm::swarm::Driven`] without
/// borrowing it, so the bridge stays a plain data boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Driven {
    Alive,
    Exited,
    Faulted(String),
}

impl From<Driven> for drt_swarm::swarm::Driven {
    fn from(d: Driven) -> Self {
        match d {
            Driven::Alive => drt_swarm::swarm::Driven::Alive,
            Driven::Exited => drt_swarm::swarm::Driven::Exited,
            Driven::Faulted(why) => drt_swarm::swarm::Driven::Faulted(why),
        }
    }
}
