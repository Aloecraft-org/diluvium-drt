//! The wasm-bindgen export layer: `doc/Browser.md`'s table, as JS classes.
//!
//! wasm32 only. Everything here is a thin shim over
//! [`drt_swarm::swarm::Swarm`], which is the `dvs.c` port and is tested
//! natively; nothing in this file makes a decision.
//!
//! ## Panic safety, and the part `doc/Browser.md` gets wrong
//!
//! That doc says every export "wraps its body and converts a panic into a
//! thrown JS error, never an abort", and credits the wrapper for it.
//! **The wrapper has nothing to do with it.** `wasm32-unknown-unknown` is
//! `panic="abort"` — the target's default on stable, not a profile choice —
//! and `catch_unwind` cannot catch an abort, so [`guard`] never runs on a
//! panic.
//!
//! What actually happens: the panic becomes a **wasm trap**, which throws
//! `RuntimeError: unreachable` into JS, and **the module keeps answering
//! afterwards**. Both halves were established by running it in a browser,
//! each after an assertion in the opposite direction failed.
//!
//! The second half is the dangerous one. Nothing ran on the way out — no
//! unwinding, no `Drop`, no cleanup — so a caller who catches the trap and
//! carries on is using a `Swarm` whose invariants nothing repaired, and it
//! looks fine from the outside. A clean death would be safer.
//!
//! So the discipline is **exports must not panic**, not "panics are
//! caught". [`guard`] stays because it is free, it is what turns an `Err`
//! into a thrown error, and it would start catching if wasm exception
//! handling ever made unwinding the default here — but nothing below may
//! rely on it, and a page that catches a trap should discard the module.
//!
//! ## What is deliberately not here
//!
//! The connector/pump layer — the third piece of task #31. A guest's
//! hostcalls reach queues, and pumping those out to JS-side connectors is
//! its own surface. Without it a program can run, park and be driven, but
//! it cannot reach `host.fs` or `host.time`. `doc/HostBaseline.md` says what
//! a browser host owes when that lands: three families answered from
//! `Date.now`, `performance.now` and `crypto.getRandomValues`, and every
//! other family denied **by name** — a stub refuses, it never fakes.

use drt_config::Budget;
use drt_swarm::swarm::Swarm as CoreSwarm;
use drt_swarm::InstanceId;
use wasm_bindgen::prelude::*;

use crate::engine::BrowserEngine;
use crate::host::JsHost;
use crate::js_bridge::JsBridge;

/// Route an export's body, converting an `Err` into a thrown JS error.
///
/// **It does not catch panics on `wasm32-unknown-unknown`**, whatever the
/// `catch_unwind` below suggests: that target is `panic="abort"`, an abort
/// is not an unwind, and a panic becomes a wasm trap that JS sees as
/// `RuntimeError: unreachable`. The call is kept for the targets where it
/// does work and because it costs nothing, but the rule that actually keeps
/// a page sound is that the bodies below do not panic.
fn guard<T>(what: &str, f: impl FnOnce() -> Result<T, JsValue>) -> Result<T, JsValue> {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)) {
        Ok(r) => r,
        Err(_) => Err(JsValue::from_str(&format!(
            "drt panicked in '{what}'; this module's state is no longer trustworthy and it \
             should be discarded rather than reused"
        ))),
    }
}

fn err(e: impl std::fmt::Display) -> JsValue {
    JsValue::from_str(&e.to_string())
}

/// Install a panic hook that prints to the console before the panic is
/// caught, so a developer sees the Rust backtrace as well as the thrown
/// message. Idempotent; call it once from page setup.
#[wasm_bindgen(js_name = setPanicHook)]
pub fn set_panic_hook() {
    console_error_panic_hook::set_once();
}

/// The dv ABI these bindings were built against.
///
/// **Must not throw** (`doc/Browser.md`), and is the wasm equivalent of
/// `drt --version`: the one call a smoke test can make against a freshly
/// instantiated module to prove it is alive and speaks the ABI expected.
#[wasm_bindgen(js_name = abiVersion)]
pub fn abi_version() -> u32 {
    // The browser tier links no engine of its own -- the interpreter is on
    // the JS side -- so this is the ABI these bindings speak, which is what
    // a page checks its diluvium build against.
    drt_swarm::engine::abi_versions()
        .map(|(_, expected)| expected)
        .unwrap_or(1)
}

/// What this wasm carries, for the same reason `drt buildinfo` exists: a
/// release artifact should say what it is rather than be guessed at from
/// its filename. `BUILDINFO.txt` gains `profile.web.exports` from this.
#[wasm_bindgen(js_name = buildInfo)]
pub fn build_info() -> JsValue {
    let o = js_sys::Object::new();
    let set = |k: &str, v: &str| {
        let _ = js_sys::Reflect::set(&o, &JsValue::from_str(k), &JsValue::from_str(v));
    };
    set("version", env!("CARGO_PKG_VERSION"));
    set("profile", "web");
    set("dvAbi", &abi_version().to_string());
    // Named, not counted: a consumer checks for the export it needs.
    set(
        "exports",
        "abiVersion,buildInfo,setPanicHook,Swarm.new,free,root,step,alive,ids,parent,kill,\
         push,budget,caps,holds,resident,cachedSize",
    );
    o.into()
}

/// DRT's swarm, in a page.
#[wasm_bindgen]
pub struct Swarm {
    inner: CoreSwarm<JsHost<JsBridge>>,
}

#[wasm_bindgen]
impl Swarm {
    /// `host` is the JS object supplying `doc/Browser.md`'s fifteen
    /// functions — the diluvium instance lives on that side, because two
    /// wasm modules cannot call each other in a browser and JS is the host
    /// in the middle.
    #[wasm_bindgen(constructor)]
    pub fn new(host: JsValue, max_instances: u32, spawns_per_step: u32) -> Result<Swarm, JsValue> {
        guard("Swarm.new", || {
            if host.is_null() || host.is_undefined() {
                return Err(JsValue::from_str(
                    "Swarm needs a host object: the diluvium instance lives on the JS side",
                ));
            }
            let bridge = JsBridge::new(host);
            let engine = BrowserEngine::new(bridge.clone());
            Ok(Swarm {
                inner: CoreSwarm::with_limits(
                    std::sync::Arc::new(engine),
                    JsHost::new(bridge),
                    max_instances,
                    spawns_per_step,
                ),
            })
        })
    }

    /// Explicit, because wasm has no GC hook: nothing tells Rust when JS
    /// dropped its last reference.
    pub fn free(self) {}

    /// Start the root instance. Returns its id.
    pub fn root(&mut self, code: &str, caps: JsValue, budget: JsValue) -> Result<u32, JsValue> {
        guard("root", || {
            let caps: Vec<drt_caps::Grant> = if caps.is_null() || caps.is_undefined() {
                Vec::new()
            } else {
                serde_wasm_bindgen::from_value(caps).map_err(err)?
            };
            let budget: Budget = if budget.is_null() || budget.is_undefined() {
                Budget::default()
            } else {
                serde_wasm_bindgen::from_value(budget).map_err(err)?
            };
            self.inner
                .root(code.as_bytes(), caps, budget)
                .map(|id| id.0)
                .map_err(err)
        })
    }

    /// One step of the drive loop. Returns the number still alive, which is
    /// the loop's own termination condition.
    pub fn step(&mut self) -> Result<usize, JsValue> {
        guard("step", || Ok(self.inner.step()))
    }

    pub fn alive(&self) -> Result<usize, JsValue> {
        guard("alive", || Ok(self.inner.alive()))
    }

    /// The roster, as ids — not a pointer, unlike `dvs_instance`.
    pub fn ids(&self) -> Result<Vec<u32>, JsValue> {
        guard("ids", || {
            Ok(self.inner.ids().into_iter().map(|i| i.0).collect())
        })
    }

    pub fn parent(&self, id: u32) -> Result<Option<u32>, JsValue> {
        guard("parent", || {
            Ok(self.inner.parent(InstanceId(id)).map(|p| p.0))
        })
    }

    pub fn kill(&mut self, id: u32) -> Result<(), JsValue> {
        guard("kill", || self.inner.kill(InstanceId(id)).map_err(err))
    }

    pub fn push(&mut self, id: u32, queue: &str, msgpack: &[u8]) -> Result<bool, JsValue> {
        guard("push", || {
            self.inner
                .push(InstanceId(id), queue, msgpack)
                .map(|()| true)
                .map_err(err)
        })
    }

    pub fn budget(&self, id: u32) -> Result<JsValue, JsValue> {
        guard("budget", || match self.inner.budget(InstanceId(id)) {
            None => Ok(JsValue::NULL),
            Some(b) => serde_wasm_bindgen::to_value(&b).map_err(err),
        })
    }

    pub fn caps(&self, id: u32) -> Result<JsValue, JsValue> {
        guard("caps", || match self.inner.caps(InstanceId(id)) {
            None => Ok(JsValue::NULL),
            Some(c) => serde_wasm_bindgen::to_value(c.grants()).map_err(err),
        })
    }

    /// Capability gating stays reachable from JS, which is the point of the
    /// row `dvs_holds` occupies in `doc/Browser.md`'s table: a page can ask
    /// what an instance may do without holding a grant itself.
    pub fn holds(&self, id: u32, cap: &str) -> Result<bool, JsValue> {
        guard("holds", || Ok(self.inner.holds(InstanceId(id), cap)))
    }

    pub fn resident(&self, id: u32) -> Result<bool, JsValue> {
        guard("resident", || Ok(self.inner.resident(InstanceId(id))))
    }

    #[wasm_bindgen(js_name = cachedSize)]
    pub fn cached_size(&self, id: u32) -> Result<usize, JsValue> {
        guard("cachedSize", || Ok(self.inner.cached_size(InstanceId(id))))
    }

    /// Panics on purpose, so the browser suite can pin what a panic
    /// actually does here — which is kill the module, not throw. Not in
    /// `doc/Browser.md`'s table: it is a test surface, and it is named so
    /// that nobody mistakes it for one.
    #[wasm_bindgen(js_name = __panicForTests)]
    pub fn panic_for_tests(&self) -> Result<(), JsValue> {
        guard("__panicForTests", || {
            panic!("deliberate panic, to prove the boundary holds");
        })
    }
}
