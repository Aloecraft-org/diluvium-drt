//! The Engine seam (SPEC.md §8): "a thing that produces instances speaking
//! dv ABI vN".
//!
//! v1 ships exactly one impl — current diluvium, statically linked over
//! `diluvium-sys` ([`diluvium_engine::DiluviumEngine`], behind the
//! `engine-diluvium` feature). The second impl (the C core as a wasm module
//! under wasmtime, building on `diluvium-wasmtime`) is deliberately deferred
//! and pays twice when it arrives: multi-version support and a
//! strong-isolation tier for untrusted bytecode. The upstream wasm spike
//! (`bindings/rust/WASM-SPIKE.md`) answered yes on all three targets, so
//! nothing here is provisional against that outcome.
//!
//! The surface mirrors the safe `diluvium` crate, which mirrors `dv.h` —
//! bytes in, bytes out; one instance, one thread (implementations are
//! `Send + !Sync` behind a `Send` box); the host drives; version first.
//! Where the two could drift, the safe crate wins: this trait exists to let
//! a second engine in, not to re-describe the first.

use std::time::Duration;

use drt_config::Budget;

/// Everything that can go wrong at the seam.
#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    /// The engine and its library disagree about the ABI. Refusing to start
    /// is the point.
    #[error("ABI mismatch: library speaks v{library}, engine expects v{expected}")]
    AbiMismatch { library: u32, expected: u32 },
    /// A guest-program failure: load rejected, or an error raised while
    /// running. The instance's fate, not the engine's.
    #[error("{0}")]
    Program(String),
    /// A snapshot whose header was refused: different build, permanents,
    /// capability set, or host stamp.
    #[error("{0}")]
    SnapshotMismatch(String),
    /// Anything else the engine can say about itself.
    #[error("{0}")]
    Engine(String),
}

/// A queue handle, valid only for the instance that issued it and only for
/// that instance's residency — runtime identity, never durable identity.
/// (Durable identity is an endpoint ref, `crate::refs`.)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct QueueHandle(pub u32);

/// The ABI's own bound on how many queues one `wait` can name
/// (`DV_WAIT_MAX` in `dv.h`). It is part of the ABI, so it is a constant
/// here rather than a capacity hint.
pub const WAIT_MAX: usize = 32;

/// What a parked program is waiting for. The caller owns the clock: honour
/// the timeout, ignore it, or answer immediately — the program only learns
/// which handle the caller says fired.
///
/// The handles live **inline**, in the fixed array `dv_waitset` already
/// hands over, because this type is built on every step of every parked
/// instance — twice, once for the wait a host asks about and once for the
/// `Parked` a step returns. A `Vec` here was two heap allocations per
/// message round trip for the sake of moving at most 32 `u32`s.
#[derive(Debug, Clone, Copy)]
pub struct WaitSet {
    queues: [QueueHandle; WAIT_MAX],
    len: u8,
    pub timeout: Option<Duration>,
    /// Waiting for *space* in a full queue rather than for a message:
    /// drain it rather than pushing to it.
    pub for_space: bool,
}

impl WaitSet {
    /// Build one. Handles past [`WAIT_MAX`] are dropped, which is what the
    /// ABI does with them too — the array is the bound, on both sides.
    pub fn new(
        queues: impl IntoIterator<Item = QueueHandle>,
        timeout: Option<Duration>,
        for_space: bool,
    ) -> WaitSet {
        let mut set = WaitSet {
            queues: [QueueHandle(0); WAIT_MAX],
            len: 0,
            timeout,
            for_space,
        };
        for q in queues {
            if (set.len as usize) < WAIT_MAX {
                set.queues[set.len as usize] = q;
                set.len += 1;
            }
        }
        set
    }

    /// The handles, in the order the program named them.
    pub fn queues(&self) -> &[QueueHandle] {
        &self.queues[..self.len as usize]
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

/// Where a program got to.
#[derive(Debug, Clone)]
pub enum Step {
    /// Parked. Decide what fired and call [`Instance::resume`] (or
    /// [`Instance::resume_timeout`] when the wait's timeout elapsed).
    Parked(WaitSet),
    /// Ran to completion.
    Done,
}

/// What happened to a push. A full or disabled queue is an ordinary answer,
/// not a failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PushOutcome {
    Accepted,
    /// Accepted, and the oldest message was evicted to make room.
    DroppedOldest,
    Full,
    Disabled,
}

impl PushOutcome {
    pub fn is_accepted(self) -> bool {
        matches!(self, PushOutcome::Accepted | PushOutcome::DroppedOldest)
    }
}

/// What an instance has spent against its budget, and what it holds.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct UsageReport {
    /// VM instructions consumed.
    pub instructions: u64,
    /// Heap high-water mark, kilobytes — a supervisor's number.
    pub memory_kb_peak: u64,
    /// Held right now, bytes — what an idle agent costs.
    pub bytes_now: u64,
}

/// What to load. Source is the norm and is loaded text-only; passing
/// `Bytecode` is the explicit decision GUARANTEES.md warns about — the
/// loader's checks are not a verifier.
#[derive(Debug, Clone, Copy)]
pub enum ProgramBytes<'a> {
    Source(&'a str),
    Bytecode(&'a [u8]),
}

#[derive(Debug, Clone, Copy)]
pub struct LoadSpec<'a> {
    pub program: ProgramBytes<'a>,
    /// Appears in error messages and tracebacks; give it something a human
    /// can act on.
    pub name: &'a str,
    /// Applied before the first step; an unstated bound is unlimited here —
    /// resolve against the parent ceiling first (`Budget::resolved_against`).
    pub budget: Budget,
    /// `DV_FLAG_UNSAFE_STDLIB`: give the program `io`, `os` and `package`.
    /// Off by default — sealed — and under the swarm it attenuates like any
    /// other authority: a child inherits its parent's setting and may drop
    /// it, never add it. Turning it on costs replayability and makes the
    /// budget approximate (dv.h says why).
    pub unsafe_stdlib: bool,
}

#[derive(Debug, Clone, Copy)]
pub struct RestoreSpec<'a> {
    pub snapshot: &'a [u8],
    /// The identity stamp: a stamped snapshot restores only under the same
    /// string, and passing `Some` refuses an unstamped snapshot — stamping
    /// is never advisory.
    pub host_stamp: Option<&'a str>,
    pub budget: Budget,
    /// Must match the set the snapshot was captured under — a snapshot does
    /// not cross the stdlib seal (the permanents fingerprint differs).
    pub unsafe_stdlib: bool,
}

/// How a queue is configured and how full it is — the introspection number
/// behind queue-depth reporting, and what a driver uses to decide which
/// waited queue fired.
#[derive(Debug, Clone, Copy)]
pub struct QueueStatus {
    pub len: u32,
    /// Always bounded; there is no unbounded option anywhere in Diluvium.
    pub capacity: u32,
    pub enabled: bool,
    pub exported: bool,
}

/// One live instance. Bytes in, bytes out: messages are msgpack, and no
/// guest value crosses this trait in any other shape.
pub trait Instance: Send {
    /// Look up a queue the program declared. `None` is an answer (the
    /// program has no such queue), not an error. `&mut` because handles are
    /// interned per residency, and the host drives one thread anyway.
    fn queue(&mut self, name: &str) -> Option<QueueHandle>;
    fn queue_info(&mut self, queue: QueueHandle) -> Result<QueueStatus, EngineError>;
    fn push(&mut self, queue: QueueHandle, msgpack: &[u8]) -> Result<PushOutcome, EngineError>;
    fn pop(&mut self, queue: QueueHandle) -> Result<Option<Vec<u8>>, EngineError>;
    /// First step of a loaded program. A restored instance is *continuing*,
    /// not starting: use [`Instance::current_wait`] + [`Instance::resume`].
    fn run(&mut self) -> Result<Step, EngineError>;
    fn resume(&mut self, fired: QueueHandle) -> Result<Step, EngineError>;
    /// Resume because the wait's timeout elapsed, on the caller's clock.
    fn resume_timeout(&mut self) -> Result<Step, EngineError>;
    /// What a parked instance is waiting for — the restored-instance entry
    /// point, and `None` when the instance is not parked.
    fn current_wait(&mut self) -> Option<WaitSet>;
    fn usage(&self) -> UsageReport;
    /// Whether the budget has been exceeded.
    fn exceeded(&self) -> bool;
    /// The whole parked state, stamped when `host_stamp` is `Some`.
    fn snapshot(&mut self, host_stamp: Option<&str>) -> Result<Vec<u8>, EngineError>;
}

/// A producer of instances speaking one dv ABI version.
pub trait Engine: Send + Sync {
    /// `dv_abi_version`, checked before anything else.
    fn abi_version(&self) -> u32;
    fn load(&self, spec: LoadSpec<'_>) -> Result<Box<dyn Instance>, EngineError>;
    fn restore(&self, spec: RestoreSpec<'_>) -> Result<Box<dyn Instance>, EngineError>;
}

#[cfg(feature = "engine-diluvium")]
pub mod diluvium_engine {
    //! The one v1 engine: current diluvium, statically linked.

    use super::*;

    pub struct DiluviumEngine;

    impl DiluviumEngine {
        /// Version first: refuse an ABI mismatch at construction, not at the
        /// first misread message.
        pub fn new() -> Result<Self, EngineError> {
            let library = diluvium::library_abi_version();
            let expected = diluvium::abi_version();
            if library != expected {
                return Err(EngineError::AbiMismatch { library, expected });
            }
            Ok(DiluviumEngine)
        }
    }

    /// The wrapped instance, interning the safe crate's opaque `QueueId`s
    /// behind stable `QueueHandle`s.
    struct DiluviumInstance {
        inner: diluvium::Instance,
        ids: Vec<diluvium::QueueId>,
    }

    impl DiluviumInstance {
        fn intern(&mut self, id: diluvium::QueueId) -> QueueHandle {
            let index = self.ids.iter().position(|k| *k == id).unwrap_or_else(|| {
                self.ids.push(id);
                self.ids.len() - 1
            });
            QueueHandle(index as u32 + 1)
        }

        fn resolve(&self, q: QueueHandle) -> Result<diluvium::QueueId, EngineError> {
            self.ids
                .get(q.0.checked_sub(1).map(|i| i as usize).unwrap_or(usize::MAX))
                .copied()
                .ok_or_else(|| EngineError::Engine(format!("{q:?} is not this instance's handle")))
        }

        fn lift_wait(&mut self, wait: &diluvium::Wait) -> WaitSet {
            // No allocation: `WaitSet` carries the same fixed array
            // `dv_waitset` does. This lift used to build a `Vec`, and
            // `drive` lifts twice per step — once for `current_wait`, once
            // for the `Parked` the step returns — so it was two of the four
            // allocations the drive loop showed per message round trip.
            //
            // The other two are `diluvium::Wait`'s own `Vec<QueueId>`,
            // built inside the safe wrapper before this ever sees it. They
            // close upstream, by giving `Wait` the same treatment.
            let mut set = WaitSet {
                queues: [QueueHandle(0); WAIT_MAX],
                len: 0,
                timeout: wait.timeout(),
                for_space: wait.is_waiting_for_space(),
            };
            for id in wait.ids().iter().take(WAIT_MAX) {
                set.queues[set.len as usize] = self.intern(*id);
                set.len += 1;
            }
            set
        }

        fn lift_step(&mut self, step: diluvium::Step) -> Step {
            match step {
                diluvium::Step::Parked(w) => Step::Parked(self.lift_wait(&w)),
                diluvium::Step::Done => Step::Done,
            }
        }
    }

    fn lift_error(e: diluvium::Error) -> EngineError {
        match e {
            diluvium::Error::AbiMismatch { library, wrapper } => EngineError::AbiMismatch {
                library,
                expected: wrapper,
            },
            diluvium::Error::Program(msg) => EngineError::Program(msg),
            diluvium::Error::SnapshotMismatch(msg) => EngineError::SnapshotMismatch(msg),
            other => EngineError::Engine(other.to_string()),
        }
    }

    fn config_for(budget: &Budget, unsafe_stdlib: bool) -> diluvium::Config {
        let mut cfg = diluvium::Config::new();
        if unsafe_stdlib {
            cfg = cfg.unsafe_stdlib(true);
        }
        // The counting hook goes on only when a bound is stated; 0 is the
        // ABI's "no limit", so an unstated half of a stated budget maps to it.
        if budget.instructions.is_some() || budget.memory_kb.is_some() {
            cfg.budget(
                budget.instructions.unwrap_or(0),
                budget.memory_kb.unwrap_or(0),
            )
        } else {
            cfg
        }
    }

    impl Engine for DiluviumEngine {
        fn abi_version(&self) -> u32 {
            diluvium::library_abi_version()
        }

        fn load(&self, spec: LoadSpec<'_>) -> Result<Box<dyn Instance>, EngineError> {
            let cfg = config_for(&spec.budget, spec.unsafe_stdlib);
            let inner = match spec.program {
                ProgramBytes::Source(text) => {
                    // Source only unless bytecode was explicit: GUARANTEES.md,
                    // the verifier that does not exist yet.
                    cfg.text_only(true).load_source(text, spec.name)
                }
                ProgramBytes::Bytecode(code) => cfg.load_bytecode(code, spec.name),
            }
            .map_err(lift_error)?;
            Ok(Box::new(DiluviumInstance {
                inner,
                ids: Vec::new(),
            }))
        }

        fn restore(&self, spec: RestoreSpec<'_>) -> Result<Box<dyn Instance>, EngineError> {
            let inner = config_for(&spec.budget, spec.unsafe_stdlib)
                .restore(spec.snapshot, spec.host_stamp)
                .map_err(lift_error)?;
            Ok(Box::new(DiluviumInstance {
                inner,
                ids: Vec::new(),
            }))
        }
    }

    impl Instance for DiluviumInstance {
        fn queue(&mut self, name: &str) -> Option<QueueHandle> {
            let id = self.inner.queue(name)?;
            Some(self.intern(id))
        }

        fn queue_info(&mut self, queue: QueueHandle) -> Result<QueueStatus, EngineError> {
            let id = self.resolve(queue)?;
            let info = self.inner.queue_info(id).map_err(lift_error)?;
            Ok(QueueStatus {
                len: info.len,
                capacity: info.capacity,
                enabled: info.enabled,
                exported: info.exported,
            })
        }

        fn push(&mut self, queue: QueueHandle, msgpack: &[u8]) -> Result<PushOutcome, EngineError> {
            let id = self.resolve(queue)?;
            let accepted = self.inner.push_raw(id, msgpack).map_err(lift_error)?;
            Ok(match accepted {
                diluvium::Accepted::Yes => PushOutcome::Accepted,
                diluvium::Accepted::DroppedOldest => PushOutcome::DroppedOldest,
                diluvium::Accepted::Full => PushOutcome::Full,
                diluvium::Accepted::Disabled => PushOutcome::Disabled,
            })
        }

        fn pop(&mut self, queue: QueueHandle) -> Result<Option<Vec<u8>>, EngineError> {
            let id = self.resolve(queue)?;
            self.inner.pop_raw(id).map_err(lift_error)
        }

        fn run(&mut self) -> Result<Step, EngineError> {
            let step = self.inner.run().map_err(lift_error)?;
            Ok(self.lift_step(step))
        }

        fn resume(&mut self, fired: QueueHandle) -> Result<Step, EngineError> {
            let id = self.resolve(fired)?;
            let step = self.inner.resume(id).map_err(lift_error)?;
            Ok(self.lift_step(step))
        }

        fn resume_timeout(&mut self) -> Result<Step, EngineError> {
            let step = self.inner.resume_timeout().map_err(lift_error)?;
            Ok(self.lift_step(step))
        }

        fn current_wait(&mut self) -> Option<WaitSet> {
            let wait = self.inner.current_wait()?;
            Some(self.lift_wait(&wait))
        }

        fn usage(&self) -> UsageReport {
            let usage = self.inner.usage();
            let memory = self.inner.memory();
            UsageReport {
                instructions: usage.instructions,
                memory_kb_peak: usage.memory_kb_peak,
                bytes_now: memory.bytes_now,
            }
        }

        fn exceeded(&self) -> bool {
            self.inner.exceeded()
        }

        fn snapshot(&mut self, host_stamp: Option<&str>) -> Result<Vec<u8>, EngineError> {
            self.inner.snapshot(host_stamp).map_err(lift_error)
        }
    }
}
