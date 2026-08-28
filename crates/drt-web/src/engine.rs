//! [`Engine`] and [`Instance`] over a [`HostBridge`].
//!
//! Every method is a call across the boundary. That is a lot of crossings
//! per step compared to the native engine, which reaches the C core through
//! one FFI call — and it is the right trade here, because the browser is
//! not the performance tier and the alternative (linking the C core into
//! the same wasm module) is what `doc/Browser.md` explains DRT deliberately
//! does not do.

use drt_swarm::engine::{
    Engine, EngineError, Instance, LoadSpec, ProgramBytes, PushOutcome, QueueHandle, QueueStatus,
    RestoreSpec, Step, UsageReport, WaitSet,
};

use crate::bridge::{HostBridge, InstanceHandle};

/// The browser engine: instances live in JS, this holds handles to them.
#[derive(Clone)]
pub struct BrowserEngine<B: HostBridge> {
    bridge: B,
}

impl<B: HostBridge> BrowserEngine<B> {
    pub fn new(bridge: B) -> Self {
        BrowserEngine { bridge }
    }
}

/// Bytecode never reaches the bridge. The browser tier has no verifier
/// either (GUARANTEES.md), and refusing here rather than in JS keeps the
/// refusal in one place across every host.
fn source_of<'a>(program: &ProgramBytes<'a>) -> Result<&'a str, EngineError> {
    match program {
        ProgramBytes::Source(src) => Ok(src),
        ProgramBytes::Bytecode(_) => Err(EngineError::Engine(
            "the browser engine loads source only: there is no bytecode verifier \
             (GUARANTEES.md), and a precompiled chunk is refused rather than trusted"
                .into(),
        )),
    }
}

impl<B: HostBridge> Engine for BrowserEngine<B> {
    fn abi_version(&self) -> u32 {
        self.bridge.abi_version()
    }

    fn load(&self, spec: LoadSpec<'_>) -> Result<Box<dyn Instance>, EngineError> {
        let source = source_of(&spec.program)?;
        let handle = self
            .bridge
            .load(source, spec.name, spec.budget, spec.unsafe_stdlib)
            .map_err(EngineError::Program)?;
        Ok(Box::new(BrowserInstance {
            bridge: self.bridge.clone(),
            handle,
        }))
    }

    fn restore(&self, spec: RestoreSpec<'_>) -> Result<Box<dyn Instance>, EngineError> {
        let handle = self
            .bridge
            .restore(
                spec.snapshot,
                spec.host_stamp,
                spec.budget,
                spec.unsafe_stdlib,
            )
            .map_err(EngineError::Engine)?;
        Ok(Box::new(BrowserInstance {
            bridge: self.bridge.clone(),
            handle,
        }))
    }
}

/// One JS-side instance. Dropping it tells JS to release its entry — a
/// swarm that hibernates and kills would otherwise leak the JS table.
pub struct BrowserInstance<B: HostBridge> {
    bridge: B,
    handle: InstanceHandle,
}

impl<B: HostBridge> BrowserInstance<B> {
    /// The JS-side handle, for an export that needs to name this instance.
    pub fn handle(&self) -> InstanceHandle {
        self.handle
    }
}

impl<B: HostBridge> Drop for BrowserInstance<B> {
    fn drop(&mut self) {
        self.bridge.release(self.handle);
    }
}

fn engine_err(e: String) -> EngineError {
    EngineError::Engine(e)
}

impl<B: HostBridge> Instance for BrowserInstance<B> {
    fn queue(&mut self, name: &str) -> Option<QueueHandle> {
        self.bridge.queue(self.handle, name)
    }

    fn queue_info(&mut self, queue: QueueHandle) -> Result<QueueStatus, EngineError> {
        self.bridge
            .queue_info(self.handle, queue)
            .map_err(engine_err)
    }

    fn push(&mut self, queue: QueueHandle, msgpack: &[u8]) -> Result<PushOutcome, EngineError> {
        self.bridge
            .push(self.handle, queue, msgpack)
            .map_err(engine_err)
    }

    fn pop(&mut self, queue: QueueHandle) -> Result<Option<Vec<u8>>, EngineError> {
        self.bridge.pop(self.handle, queue).map_err(engine_err)
    }

    fn run(&mut self) -> Result<Step, EngineError> {
        self.bridge.run(self.handle).map_err(EngineError::Program)
    }

    fn resume(&mut self, fired: QueueHandle) -> Result<Step, EngineError> {
        self.bridge
            .resume(self.handle, fired)
            .map_err(EngineError::Program)
    }

    fn resume_timeout(&mut self) -> Result<Step, EngineError> {
        self.bridge
            .resume_timeout(self.handle)
            .map_err(EngineError::Program)
    }

    fn current_wait(&mut self) -> Option<WaitSet> {
        self.bridge.current_wait(self.handle)
    }

    fn usage(&self) -> UsageReport {
        self.bridge.usage(self.handle)
    }

    fn exceeded(&self) -> bool {
        self.bridge.exceeded(self.handle)
    }

    fn snapshot(&mut self, host_stamp: Option<&str>) -> Result<Vec<u8>, EngineError> {
        self.bridge
            .snapshot(self.handle, host_stamp)
            .map_err(engine_err)
    }

    /// The JS-side handle. This is the whole reason `host_token` exists:
    /// `JsHost` is handed `&mut dyn Instance` and must tell JS *which*
    /// instance to drive.
    fn host_token(&self) -> Option<u32> {
        Some(self.handle)
    }
}
