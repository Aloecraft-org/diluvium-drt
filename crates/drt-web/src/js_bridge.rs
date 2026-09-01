//! [`HostBridge`] over a real JS object. wasm32 only.
//!
//! The crate doc calls this "one small file whose only job is
//! marshalling, with no logic to get wrong that tests would not see", and
//! that is the whole design: everything above it — the engine, the host,
//! the swarm — is exercised natively against the mock bridge in
//! `tests/bridge.rs`, so what remains here is argument conversion and
//! nothing else.
//!
//! ## Why methods are looked up by name rather than typed
//!
//! `js_sys::Reflect::get` + `Function::call` rather than a
//! `#[wasm_bindgen]` `extern "C"` block over a declared interface. The
//! extern approach is prettier and binds to one module shape at compile
//! time; this binds to any object carrying the right method names, which is
//! what lets a page pass a plain object literal, a class instance, or a
//! test double without a build step. `doc/Browser.md`'s contract is
//! fifteen named functions, not a class, so name lookup is the honest
//! encoding of it.
//!
//! ## Errors
//!
//! A JS method that throws becomes `Err(String)`. A method that is absent
//! becomes `Err` naming the method, because "your host object is missing
//! `queueInfo`" is a sentence someone can act on and `undefined is not a
//! function` is not.

use std::rc::Rc;

use drt_config::Budget;
use drt_swarm::engine::{PushOutcome, QueueHandle, QueueStatus, Step, UsageReport, WaitSet};
use wasm_bindgen::prelude::*;

use crate::bridge::{Driven, HostBridge, InstanceHandle};

/// A JS host object, held by `Rc` so cloning a bridge is cheap — every
/// `BrowserInstance` holds one.
#[derive(Clone)]
pub struct JsBridge {
    host: Rc<JsValue>,
}

impl JsBridge {
    pub fn new(host: JsValue) -> Self {
        JsBridge {
            host: Rc::new(host),
        }
    }

    /// Call `name(args...)` on the host object.
    fn call(&self, name: &str, args: &[JsValue]) -> Result<JsValue, String> {
        let f = js_sys::Reflect::get(&self.host, &JsValue::from_str(name))
            .map_err(|_| format!("the host object has no '{name}'"))?;
        let f = f
            .dyn_ref::<js_sys::Function>()
            .ok_or_else(|| format!("the host object's '{name}' is not a function"))?;
        let arr = js_sys::Array::new();
        for a in args {
            arr.push(a);
        }
        js_sys::Reflect::apply(f, &self.host, &arr).map_err(describe)
    }

    /// A call whose result is discarded, and whose failure is not the
    /// caller's to handle. Only `release` is in this shape.
    fn call_void(&self, name: &str, args: &[JsValue]) {
        let _ = self.call(name, args);
    }
}

/// A thrown JS value as a sentence. An `Error` gives up its message; any
/// other value is stringified, because JS permits throwing anything and a
/// bare "Object" would tell the reader nothing.
fn describe(v: JsValue) -> String {
    if let Some(e) = v.dyn_ref::<js_sys::Error>() {
        return String::from(e.message());
    }
    v.as_string()
        .unwrap_or_else(|| String::from(js_sys::JSON::stringify(&v).unwrap_or_default()))
}

fn num(v: &JsValue, field: &str) -> Option<f64> {
    js_sys::Reflect::get(v, &JsValue::from_str(field))
        .ok()
        .and_then(|x| x.as_f64())
}

fn flag(v: &JsValue, field: &str) -> bool {
    js_sys::Reflect::get(v, &JsValue::from_str(field))
        .ok()
        .and_then(|x| x.as_bool())
        .unwrap_or(false)
}

/// `{instructions, memory_kb}` as JS sees it. `None` becomes `null` rather
/// than `0`: zero is the ABI's "no limit" and an unstated bound is not the
/// same fact as a stated unlimited one.
fn budget_to_js(b: Budget) -> JsValue {
    let o = js_sys::Object::new();
    let set = |k: &str, v: Option<u64>| {
        let _ = js_sys::Reflect::set(
            &o,
            &JsValue::from_str(k),
            &match v {
                Some(n) => JsValue::from_f64(n as f64),
                None => JsValue::NULL,
            },
        );
    };
    set("instructions", b.instructions);
    set("memoryKb", b.memory_kb);
    o.into()
}

/// A `Step` from `{parked: [...]}`-shaped JS, or `{done: true}`.
fn step_from_js(v: JsValue) -> Result<Step, String> {
    if flag(&v, "done") {
        return Ok(Step::Done);
    }
    let waits = js_sys::Reflect::get(&v, &JsValue::from_str("parked"))
        .map_err(|_| "a step must be {done: true} or {parked: [...]}".to_string())?;
    let arr = js_sys::Array::from(&waits);
    let queues: Vec<QueueHandle> = arr
        .iter()
        .filter_map(|item| item.as_f64().map(|q| QueueHandle(q as u32)))
        .collect();
    let timeout = num(&v, "timeoutMs").map(|ms| std::time::Duration::from_millis(ms as u64));
    // Handles past WAIT_MAX are dropped by `new`, which is what the ABI
    // does with them too -- the array is the bound on both sides.
    Ok(Step::Parked(WaitSet::new(
        queues,
        timeout,
        flag(&v, "forSpace"),
    )))
}

impl HostBridge for JsBridge {
    fn abi_version(&self) -> u32 {
        self.call("abiVersion", &[])
            .ok()
            .and_then(|v| v.as_f64())
            .unwrap_or(0.0) as u32
    }

    fn load(
        &self,
        program: &str,
        name: &str,
        budget: Budget,
        unsafe_stdlib: bool,
    ) -> Result<InstanceHandle, String> {
        let v = self.call(
            "load",
            &[
                JsValue::from_str(program),
                JsValue::from_str(name),
                budget_to_js(budget),
                JsValue::from_bool(unsafe_stdlib),
            ],
        )?;
        v.as_f64()
            .map(|n| n as InstanceHandle)
            .ok_or_else(|| "load must return an instance handle (a number)".into())
    }

    fn restore(
        &self,
        snapshot: &[u8],
        host_stamp: Option<&str>,
        budget: Budget,
        unsafe_stdlib: bool,
    ) -> Result<InstanceHandle, String> {
        let v = self.call(
            "restore",
            &[
                js_sys::Uint8Array::from(snapshot).into(),
                host_stamp.map(JsValue::from_str).unwrap_or(JsValue::NULL),
                budget_to_js(budget),
                JsValue::from_bool(unsafe_stdlib),
            ],
        )?;
        v.as_f64()
            .map(|n| n as InstanceHandle)
            .ok_or_else(|| "restore must return an instance handle (a number)".into())
    }

    fn release(&self, instance: InstanceHandle) {
        // Must not throw, per doc/Browser.md: this runs from a Drop, and a
        // throw out of one would unwind across the boundary.
        self.call_void("release", &[JsValue::from_f64(instance as f64)]);
    }

    fn queue(&self, instance: InstanceHandle, name: &str) -> Option<QueueHandle> {
        self.call(
            "queue",
            &[JsValue::from_f64(instance as f64), JsValue::from_str(name)],
        )
        .ok()
        .and_then(|v| v.as_f64())
        .map(|n| QueueHandle(n as u32))
    }

    fn queue_info(
        &self,
        instance: InstanceHandle,
        queue: QueueHandle,
    ) -> Result<QueueStatus, String> {
        let v = self.call(
            "queueInfo",
            &[
                JsValue::from_f64(instance as f64),
                JsValue::from_f64(queue.0 as f64),
            ],
        )?;
        Ok(QueueStatus {
            len: num(&v, "len").unwrap_or(0.0) as u32,
            capacity: num(&v, "capacity").unwrap_or(0.0) as u32,
            enabled: flag(&v, "enabled"),
            exported: flag(&v, "exported"),
        })
    }

    fn push(
        &self,
        instance: InstanceHandle,
        queue: QueueHandle,
        msgpack: &[u8],
    ) -> Result<PushOutcome, String> {
        let v = self.call(
            "push",
            &[
                JsValue::from_f64(instance as f64),
                JsValue::from_f64(queue.0 as f64),
                js_sys::Uint8Array::from(msgpack).into(),
            ],
        )?;
        // Four outcomes, not two: `Full` and `Disabled` are different
        // facts and a sender acts differently on them, so a bare boolean
        // is accepted for the common case but collapses to `Full` rather
        // than inventing a reason. A string names the outcome exactly.
        Ok(match v.as_string().as_deref() {
            Some("accepted") => PushOutcome::Accepted,
            Some("droppedOldest") => PushOutcome::DroppedOldest,
            Some("full") => PushOutcome::Full,
            Some("disabled") => PushOutcome::Disabled,
            _ => match v.as_bool() {
                Some(true) => PushOutcome::Accepted,
                _ => PushOutcome::Full,
            },
        })
    }

    fn pop(&self, instance: InstanceHandle, queue: QueueHandle) -> Result<Option<Vec<u8>>, String> {
        let v = self.call(
            "pop",
            &[
                JsValue::from_f64(instance as f64),
                JsValue::from_f64(queue.0 as f64),
            ],
        )?;
        if v.is_null() || v.is_undefined() {
            return Ok(None);
        }
        Ok(Some(js_sys::Uint8Array::new(&v).to_vec()))
    }

    fn run(&self, instance: InstanceHandle) -> Result<Step, String> {
        step_from_js(self.call("run", &[JsValue::from_f64(instance as f64)])?)
    }

    fn resume(&self, instance: InstanceHandle, fired: QueueHandle) -> Result<Step, String> {
        step_from_js(self.call(
            "resume",
            &[
                JsValue::from_f64(instance as f64),
                JsValue::from_f64(fired.0 as f64),
            ],
        )?)
    }

    fn resume_timeout(&self, instance: InstanceHandle) -> Result<Step, String> {
        step_from_js(self.call("resumeTimeout", &[JsValue::from_f64(instance as f64)])?)
    }

    fn current_wait(&self, instance: InstanceHandle) -> Option<WaitSet> {
        let v = self
            .call("currentWait", &[JsValue::from_f64(instance as f64)])
            .ok()?;
        if v.is_null() || v.is_undefined() {
            return None;
        }
        match step_from_js(v) {
            Ok(Step::Parked(w)) => Some(w),
            _ => None,
        }
    }

    fn usage(&self, instance: InstanceHandle) -> UsageReport {
        let v = self
            .call("usage", &[JsValue::from_f64(instance as f64)])
            .unwrap_or(JsValue::NULL);
        UsageReport {
            instructions: num(&v, "instructions").unwrap_or(0.0) as u64,
            memory_kb_peak: num(&v, "memoryKbPeak").unwrap_or(0.0) as u64,
            bytes_now: num(&v, "bytesNow").unwrap_or(0.0) as u64,
        }
    }

    fn exceeded(&self, instance: InstanceHandle) -> bool {
        self.call("exceeded", &[JsValue::from_f64(instance as f64)])
            .ok()
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
    }

    fn snapshot(
        &self,
        instance: InstanceHandle,
        host_stamp: Option<&str>,
    ) -> Result<Vec<u8>, String> {
        let v = self.call(
            "snapshot",
            &[
                JsValue::from_f64(instance as f64),
                host_stamp.map(JsValue::from_str).unwrap_or(JsValue::NULL),
            ],
        )?;
        Ok(js_sys::Uint8Array::new(&v).to_vec())
    }

    fn drive(&self, id: u32, instance: InstanceHandle) -> Driven {
        match self.call(
            "drive",
            &[
                JsValue::from_f64(id as f64),
                JsValue::from_f64(instance as f64),
            ],
        ) {
            // A drive that throws is the instance's fault, not the
            // engine's, and it must not propagate: the swarm is mid-step
            // over a slot table and an unwind here would leave it torn.
            Err(why) => Driven::Faulted(why),
            Ok(v) => match v.as_string().as_deref() {
                Some("alive") | None => Driven::Alive,
                Some("exited") => Driven::Exited,
                Some(other) => Driven::Faulted(other.to_string()),
            },
        }
    }
}
