//! [`SwarmHost`] over a [`HostBridge`] — the JS `drive` callback.
//!
//! The Lab already implements this exact seam: `js_host_drive(ud, id, inst,
//! ctx)` is called from wasm during a step and returns without awaiting.
//! This is the same shape with DRT's swarm underneath instead of `dvs.c`'s.

use drt_caps::CapSet;
use drt_swarm::engine::Instance;
use drt_swarm::swarm::{Driven, SwarmHost};
use drt_swarm::InstanceId;

use crate::bridge::HostBridge;

/// Delegates every drive to JS.
pub struct JsHost<B: HostBridge> {
    bridge: B,
}

impl<B: HostBridge> JsHost<B> {
    pub fn new(bridge: B) -> Self {
        JsHost { bridge }
    }
}

impl<B: HostBridge + 'static> SwarmHost for JsHost<B> {
    fn drive(&mut self, id: InstanceId, _caps: &CapSet, inst: &mut dyn Instance) -> Driven {
        // Which instance, in JS's terms. Only a `BrowserEngine` answers
        // this; anything else in the slot is a wiring mistake, reported
        // rather than guessed at.
        let Some(handle) = inst.host_token() else {
            return Driven::Faulted(
                "a JsHost was given an instance no BrowserEngine created".into(),
            );
        };
        self.bridge.drive(id.0, handle).into()
    }
}
