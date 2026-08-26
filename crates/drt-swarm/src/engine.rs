//! The Engine seam (SPEC.md §8): "a thing that produces instances speaking
//! dv ABI vN".
//!
//! v1 ships exactly one impl — current diluvium, statically linked over
//! `diluvium-sys` — and it lands here once the transcription upstream is
//! complete. The second impl (the C core as a wasm module under wasmtime,
//! building on `diluvium-wasmtime`) is deliberately deferred and pays twice
//! when it arrives: multi-version support and a strong-isolation tier for
//! untrusted bytecode.
//!
//! The surface mirrors `dv.h` — bytes in, bytes out; one instance, one
//! thread (`Instance` is deliberately not `Sync`-bound and implementations
//! are expected to be `Send + !Sync`, the safe `diluvium` crate's idiom); the
//! host drives; version first. It is a *seam*, not a second ABI: names and
//! shapes track `dv.h`, and it will firm up against `diluvium-sys` rather
//! than drift from it.

use drt_config::Budget;

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct EngineError(pub String);

/// What `run`/`resume` come back with — the `dv_run` contract: the engine
/// returns when the program parks or finishes, hands over what it is waiting
/// for, and leaves the decision to the caller. There is no scheduler and no
/// clock in here; park-with-timeout surfaces as data for the orchestrator's
/// timer, because DRT owns no clock either (SPEC.md §9).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Parked {
    /// `DV_IDLE`: waiting on the named queues, with an optional timeout the
    /// *caller* is responsible for measuring.
    Idle {
        queues: Vec<String>,
        timeout_ms: Option<u64>,
    },
    /// `DV_DONE`: ran to completion.
    Done,
    /// `DV_ERROR`: a guest error, with the message.
    Error(String),
}

/// One live instance. Bytes in, bytes out: messages are msgpack, and no
/// guest value crosses this trait in any other shape.
pub trait Instance: Send {
    fn load(&mut self, source: &[u8], name: &str) -> Result<(), EngineError>;
    fn run(&mut self) -> Result<Parked, EngineError>;
    fn resume(&mut self) -> Result<Parked, EngineError>;
    fn push(&mut self, queue: &str, message: &[u8]) -> Result<(), EngineError>;
    fn drain(&mut self, queue: &str) -> Result<Option<Vec<u8>>, EngineError>;
    fn set_budget(&mut self, budget: &Budget) -> Result<(), EngineError>;
    /// (instructions spent, memory high-water kb) — `dv_usage`.
    fn usage(&self) -> Result<(u64, u64), EngineError>;
    /// The whole parked state, host-stamped when `host` is `Some` —
    /// `dv_snapshot`. A stamped snapshot restores only under the same stamp.
    fn snapshot(&mut self, host: Option<&str>) -> Result<Vec<u8>, EngineError>;
    /// Restore into a fresh instance — `dv_restore`: refuses rather than
    /// raising, on *any* input; the budget is set before restoring.
    fn restore(&mut self, host: Option<&str>, snapshot: &[u8]) -> Result<(), EngineError>;
}

/// A producer of instances speaking one dv ABI version.
pub trait Engine: Send + Sync {
    /// `dv_abi_version`, checked first — a mismatch refuses to start.
    fn abi_version(&self) -> u32;
    fn create(&self) -> Result<Box<dyn Instance>, EngineError>;
}
