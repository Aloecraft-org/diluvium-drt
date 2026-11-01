//! Connectors a page hands in (SPEC.md §7's `JsConnector`, issue #80): a
//! JavaScript object behind the same registration every builtin has, so
//! the dispatcher gates it, echoes its tokens and answers every request
//! drained for it, and a guest cannot tell it from a native backing.
//!
//! ## surface block
//!
//! - [`backings`]: a page's `{ family: object }` map, as the makers
//!   `Term::with_backings` and `Swarm::new` take. One per kernel: a page
//!   with several kernels hands each its own (doc/Browser.md).
//! - Fan-out, by family name -- the one table that decides what a handed
//!   in object is:
//!   - `net`: a transport, `{ open, listen?, send, close }`, under
//!     `drt-connector-net`. The connector keeps the handles, owners and
//!     queues; the object only moves bytes. Its calls:
//!     - `open(address, link)` -> conn, or a promise of one. Bytes from
//!       the far end go to `link.receive(bytes)`, the end to
//!       `link.closed(reason?)`.
//!     - `listen(address, acceptor)` -> listener, or a promise. A
//!       connection arriving is `acceptor.connection(conn, peer)`, which
//!       answers that connection's link.
//!     - `send(conn, bytes)` -> anything, or a promise; a throw or a
//!       rejection is the guest's `error`.
//!     - `close(conn)`, once per conn or listener, never for one already
//!       reported closed.
//!   - anything else: a generic backing, `{ call(name, args, node) }`,
//!     answering every call of its family. `args` is the request's value
//!     as plain JavaScript (maps as objects, bytes as `Uint8Array`), the
//!     answer is converted back the same way, and a throw or a rejected
//!     promise is the guest's `error` with the message. `node` is the
//!     calling instance's id, or `null` for the root. Optional
//!     `release(node)` hears that a node is gone for good.
//! - [`NetLink`], [`NetAcceptor`]: the two classes a transport is handed.
//!
//! **Why a `Local`.** A connector is `Send + Sync`, and a `JsValue` is
//! neither, because a JavaScript object belongs to the thread that made
//! it. This module is compiled only for `wasm32-unknown-unknown` without
//! the `atomics` feature, where there is one thread and no way to make a
//! second, so nothing can ever reach a value from another thread; `Local`
//! says so to the compiler, and refuses to build where it would be false.

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::sync::Arc;
use std::task::{Context, Poll};

use js_sys::{Array, Function, Object, Promise, Reflect, Uint8Array};
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use wasm_bindgen_futures::JsFuture;

use drt_caps::Scope;
use drt_connector::{Asker, CallError, CallResult, Caller, Connector};
use drt_connector_net::{Acceptor, Link, NetConnector, Transport};

use crate::term::Backing;

#[cfg(target_feature = "atomics")]
compile_error!("drt-web's js module assumes one thread; a build with atomics needs another design");

/// The family a handed-in object is a transport for.
pub const NET: &str = "net";

/// A page's backings: every own enumerable key of `object`, each to the
/// maker its family's row of the fan-out names. `undefined` and `null`
/// are no backings. A value of the wrong shape is refused here, by family,
/// before any command runs.
pub fn backings(object: &JsValue) -> Result<BTreeMap<String, Backing>, String> {
    let mut out: BTreeMap<String, Backing> = BTreeMap::new();
    if object.is_undefined() || object.is_null() {
        return Ok(out);
    }
    let object: &Object = object
        .dyn_ref()
        .ok_or("the backings must be an object of family: backing")?;
    for entry in Object::entries(object).iter() {
        let pair: Array = entry.unchecked_into();
        let family = pair
            .get(0)
            .as_string()
            .ok_or("a backing's family must be a string")?;
        let value = pair.get(1);
        let maker: Backing = match family.as_str() {
            NET => {
                for method in ["open", "send", "close"] {
                    method_of(&value, method)
                        .map_err(|e| format!("the '{family}' backing: {e}"))?;
                }
                let value = Local(value);
                Rc::new(move || {
                    Arc::new(NetConnector::new(JsTransport {
                        object: value.clone(),
                    })) as Arc<dyn Connector>
                })
            }
            _ => {
                method_of(&value, "call").map_err(|e| format!("the '{family}' backing: {e}"))?;
                let value = Local(value);
                Rc::new(move || {
                    Arc::new(JsConnector {
                        object: value.clone(),
                    }) as Arc<dyn Connector>
                })
            }
        };
        out.insert(family, maker);
    }
    Ok(out)
}

/// One command's connectors, from the makers: what a `DrtSwarm`, which
/// is one deployment for its whole life, is built with.
pub fn supplied(makers: &BTreeMap<String, Backing>) -> drt::cli::Supplied {
    makers
        .iter()
        .map(|(family, make)| (family.clone(), make()))
        .collect()
}

// depth: the generic backing

/// A family answered by a JavaScript object's `call`.
struct JsConnector {
    object: Local<JsValue>,
}

#[async_trait::async_trait]
impl Connector for JsConnector {
    async fn call(
        &self,
        call: &str,
        args: Option<rmpv::Value>,
        scope: Option<&Scope>,
    ) -> CallResult {
        let asker = Asker {
            caller: Caller::Root,
            grants: &[],
        };
        self.call_as(&asker, call, args, scope).await
    }

    async fn call_as(
        &self,
        asker: &Asker<'_>,
        call: &str,
        args: Option<rmpv::Value>,
        _scope: Option<&Scope>,
    ) -> CallResult {
        let object = self.object.0.clone();
        let call = call.to_string();
        let node = node_of(asker.caller);
        Local(Box::pin(async move {
            let args = args.as_ref().map(to_js).unwrap_or(JsValue::UNDEFINED);
            let f = method_of(&object, "call").map_err(CallError::new)?;
            let answer = f
                .call3(&object, &JsValue::from_str(&call), &args, &node)
                .map_err(|e| CallError::new(message(&e)))?;
            let answer = settle(answer).await.map_err(CallError::new)?;
            from_js(&answer).map_err(|e| CallError::new(format!("the backing's answer: {e}")))
        }))
        .await
    }

    fn release(&self, caller: &Caller) -> Vec<String> {
        if let Ok(f) = method_of(&self.object.0, "release") {
            if let Err(e) = f.call1(&self.object.0, &node_of(*caller)) {
                return vec![format!("the backing's release threw: {}", message(&e))];
            }
        }
        Vec::new()
    }
}

// depth: the net transport

/// `drt-connector-net`'s transport over a page's object. A conn is
/// whatever the object answered, held as it is and handed back.
struct JsTransport {
    object: Local<JsValue>,
}

type Conn = Local<JsValue>;

#[async_trait::async_trait]
impl Transport for JsTransport {
    type Conn = Conn;

    async fn open(&self, address: &str, link: Link<Conn>) -> Result<Conn, String> {
        let object = self.object.0.clone();
        let address = address.to_string();
        Local(Box::pin(async move {
            let f = method_of(&object, "open")?;
            let link = JsValue::from(NetLink { link });
            let answer = f
                .call2(&object, &JsValue::from_str(&address), &link)
                .map_err(|e| message(&e))?;
            settle(answer).await.map(Local)
        }))
        .await
    }

    async fn listen(&self, address: &str, accept: Acceptor<Conn>) -> Result<Conn, String> {
        let object = self.object.0.clone();
        let address = address.to_string();
        Local(Box::pin(async move {
            let f = method_of(&object, "listen")
                .map_err(|_| "this page's net transport cannot listen".to_string())?;
            let accept = JsValue::from(NetAcceptor { accept });
            let answer = f
                .call2(&object, &JsValue::from_str(&address), &accept)
                .map_err(|e| message(&e))?;
            settle(answer).await.map(Local)
        }))
        .await
    }

    async fn send(&self, conn: &Conn, data: Vec<u8>) -> Result<(), String> {
        let object = self.object.0.clone();
        let conn = conn.0.clone();
        Local(Box::pin(async move {
            let f = method_of(&object, "send")?;
            let bytes = Uint8Array::from(data.as_slice());
            let answer = f.call2(&object, &conn, &bytes).map_err(|e| message(&e))?;
            settle(answer).await.map(|_| ())
        }))
        .await
    }

    fn close(&self, conn: &Conn) {
        // A throw here has nobody to tell: the handle is already gone.
        if let Ok(f) = method_of(&self.object.0, "close") {
            let _ = f.call1(&self.object.0, &conn.0);
        }
    }
}

/// One connection's report line, as the transport sees it.
#[wasm_bindgen]
pub struct NetLink {
    link: Link<Conn>,
}

#[wasm_bindgen]
impl NetLink {
    /// One message from the far end, whole.
    pub fn receive(&self, bytes: &[u8]) {
        self.link.receive(bytes.to_vec());
    }

    /// The far end is gone. `reason` reaches the owner as it is.
    pub fn closed(&self, reason: Option<String>) {
        self.link.closed(reason);
    }
}

/// One listener's report line.
#[wasm_bindgen]
pub struct NetAcceptor {
    accept: Acceptor<Conn>,
}

#[wasm_bindgen]
impl NetAcceptor {
    /// A connection arrived from `peer`; `conn` is what `send` and `close`
    /// will be handed for it. Answers the link its bytes go to.
    pub fn connection(&self, conn: JsValue, peer: String) -> NetLink {
        NetLink {
            link: self.accept.connection(Local(conn), peer),
        }
    }
}

// depth: values across the boundary

/// msgpack to plain JavaScript: maps with string keys as objects, other
/// maps as `Map`, bytes as `Uint8Array`, integers as numbers while they
/// are exact and as `BigInt` past 2^53.
pub fn to_js(value: &rmpv::Value) -> JsValue {
    use rmpv::Value as V;
    match value {
        V::Nil => JsValue::NULL,
        V::Boolean(b) => JsValue::from_bool(*b),
        V::Integer(n) => match (n.as_i64(), n.as_u64()) {
            (Some(i), _) if i.unsigned_abs() <= MAX_SAFE => JsValue::from_f64(i as f64),
            (Some(i), _) => js_sys::BigInt::from(i).into(),
            (None, Some(u)) => js_sys::BigInt::from(u).into(),
            (None, None) => JsValue::NULL,
        },
        V::F32(f) => JsValue::from_f64(f64::from(*f)),
        V::F64(f) => JsValue::from_f64(*f),
        V::String(s) => match s.as_str() {
            Some(s) => JsValue::from_str(s),
            None => Uint8Array::from(s.as_bytes()).into(),
        },
        V::Binary(b) => Uint8Array::from(b.as_slice()).into(),
        V::Array(items) => items.iter().map(to_js).collect::<Array>().into(),
        V::Map(pairs) if pairs.iter().all(|(k, _)| k.is_str()) => {
            let o = Object::new();
            for (k, v) in pairs {
                let _ = Reflect::set(
                    &o,
                    &JsValue::from_str(k.as_str().unwrap_or_default()),
                    &to_js(v),
                );
            }
            o.into()
        }
        V::Map(pairs) => {
            let m = js_sys::Map::new();
            for (k, v) in pairs {
                m.set(&to_js(k), &to_js(v));
            }
            m.into()
        }
        V::Ext(_, data) => Uint8Array::from(data.as_slice()).into(),
    }
}

const MAX_SAFE: u64 = (1 << 53) - 1;

/// Plain JavaScript to msgpack, the inverse of [`to_js`]. A function, a
/// symbol, or an object that is not plain data is refused by name.
pub fn from_js(value: &JsValue) -> Result<rmpv::Value, String> {
    use rmpv::Value as V;
    if value.is_null() || value.is_undefined() {
        return Ok(V::Nil);
    }
    if let Some(b) = value.as_bool() {
        return Ok(V::Boolean(b));
    }
    if let Some(f) = value.as_f64() {
        if f.fract() == 0.0 && f.abs() <= MAX_SAFE as f64 {
            return Ok(if f < 0.0 {
                V::from(f as i64)
            } else {
                V::from(f as u64)
            });
        }
        return Ok(V::F64(f));
    }
    if value.is_bigint() {
        let text = js_sys::BigInt::unchecked_from_js_ref(value)
            .to_string(10)
            .map_err(|_| "a BigInt that cannot be written".to_string())?;
        let text = String::from(text);
        return text
            .parse::<i64>()
            .map(V::from)
            .or_else(|_| text.parse::<u64>().map(V::from))
            .map_err(|_| format!("the BigInt {text} does not fit 64 bits"));
    }
    if let Some(s) = value.as_string() {
        return Ok(V::from(s));
    }
    if let Some(bytes) = value.dyn_ref::<Uint8Array>() {
        return Ok(V::Binary(bytes.to_vec()));
    }
    if let Some(buffer) = value.dyn_ref::<js_sys::ArrayBuffer>() {
        return Ok(V::Binary(Uint8Array::new(buffer).to_vec()));
    }
    if Array::is_array(value) {
        let items: &Array = value.unchecked_ref();
        return items
            .iter()
            .map(|v| from_js(&v))
            .collect::<Result<_, _>>()
            .map(V::Array);
    }
    if let Some(map) = value.dyn_ref::<js_sys::Map>() {
        let mut pairs = Vec::new();
        let mut failed = None;
        map.for_each(&mut |v, k| match (from_js(&k), from_js(&v)) {
            (Ok(k), Ok(v)) => pairs.push((k, v)),
            (Err(e), _) | (_, Err(e)) => failed = Some(e),
        });
        return match failed {
            Some(e) => Err(e),
            None => Ok(V::Map(pairs)),
        };
    }
    if value.is_function() || value.is_symbol() {
        return Err("a function or a symbol is not data".into());
    }
    if let Some(object) = value.dyn_ref::<Object>() {
        let mut pairs = Vec::new();
        for entry in Object::entries(object).iter() {
            let pair: Array = entry.unchecked_into();
            pairs.push((from_js(&pair.get(0))?, from_js(&pair.get(1))?));
        }
        return Ok(V::Map(pairs));
    }
    Err("a value with no msgpack form".into())
}

// depth: calling into the page

/// `object[name]` as a function, or why not.
fn method_of(object: &JsValue, name: &str) -> Result<Function, String> {
    Reflect::get(object, &JsValue::from_str(name))
        .ok()
        .and_then(|f| f.dyn_into::<Function>().ok())
        .ok_or_else(|| format!("it has no '{name}' function"))
}

/// A value, or what a promise of one settles to. Not a promise: answered
/// on this poll, so a backing that answers synchronously costs the guest
/// no extra tick.
async fn settle(value: JsValue) -> Result<JsValue, String> {
    match value.dyn_into::<Promise>() {
        Ok(promise) => JsFuture::from(promise).await.map_err(|e| message(&e)),
        Err(value) => Ok(value),
    }
}

/// What a throw says: an `Error`'s message, or the value as a string.
fn message(thrown: &JsValue) -> String {
    match thrown.dyn_ref::<js_sys::Error>() {
        Some(e) => String::from(e.message()),
        None => thrown.as_string().unwrap_or_else(|| format!("{thrown:?}")),
    }
}

fn node_of(caller: Caller) -> JsValue {
    match caller {
        Caller::Root => JsValue::NULL,
        Caller::Node(id) => JsValue::from(id),
    }
}

/// A value or a future that is only ever on the page's one thread; see
/// the module's header for why that makes it `Send` and `Sync`.
#[derive(Clone)]
pub struct Local<T>(pub T);

// SAFETY: compiled only for wasm32-unknown-unknown without atomics (the
// `compile_error!` above), which has exactly one thread. There is no
// other thread to send to or share with.
unsafe impl<T> Send for Local<T> {}
unsafe impl<T> Sync for Local<T> {}

impl<F: Future + Unpin> Future for Local<F> {
    type Output = F::Output;
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        Pin::new(&mut self.0).poll(cx)
    }
}
