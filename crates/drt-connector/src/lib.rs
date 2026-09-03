//! Connectors (SPEC.md §7): one trait, several backings, zero distinctions at
//! the call site.
//!
//! A connector is an ordinary Rust impl of [`Connector`]. The [`Dispatcher`]
//! does capability gating, token echo, and the answered-always guarantee
//! *once* — a connector never sees a request it was not granted, never
//! touches a token, and cannot cause a request to go unanswered.
//!
//! Mocks implement the same trait, and guests cannot tell. That
//! indistinguishability is load-bearing (prototype against mocks, deploy
//! against real, guest unchanged) and is the acceptance test.

use std::collections::BTreeMap;
use std::sync::Arc;

use drt_caps::{call_capability, CapSet, Scope, ScopeError, ScopeRegistry, ScopeType};
use drt_hostcall::{salvage_token, Reply, Request, Token};

/// What a connector answers with. `Err` becomes `status = "error"` with the
/// detail worded for the program to read; `denied` is never a connector's to
/// say — the dispatcher decides it from the capability set, so a mock cannot
/// diverge from a real backing on refusals.
pub type CallResult = Result<rmpv::Value, CallError>;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct CallError(pub String);

impl CallError {
    pub fn new(detail: impl Into<String>) -> Self {
        CallError(detail.into())
    }
}

/// A connector: answers the calls of one family (`time`, `fs/…`, `sql/…`).
///
/// Typed via serde by convention: an impl deserializes `args` into its own
/// struct and serializes its answer into the reply value — the dispatcher
/// stays untyped so the boundary stays bytes.
#[async_trait::async_trait]
pub trait Connector: Send + Sync {
    /// The scope-type this connector declares (SPEC.md §5), used to validate
    /// its wiring at startup, by name. Default: no scope.
    fn scope_type(&self) -> Box<dyn ScopeType> {
        Box::new(drt_caps::NoScope)
    }

    /// Answer one call. `call` is the full name (`"fs/read"`), already gated:
    /// the guest holds `host:<call>`. `scope` is the wiring this process
    /// granted the connector — a place to resolve names within, never the
    /// application's own resource names.
    async fn call(
        &self,
        call: &str,
        args: Option<rmpv::Value>,
        scope: Option<&Scope>,
    ) -> CallResult;

    /// Last call before the process goes away. A connector holding state
    /// that outlives a hostcall says here whether that state ended well,
    /// and each string it returns is one thing that did not.
    ///
    /// It exists because of `sql`. Its handles are cached across calls, so a
    /// guest can open a transaction in one hostcall and never close it, and
    /// SQLite's own behaviour on a dropped connection is to roll back --
    /// correctly, silently, with the writes gone and the exit status still
    /// zero. Every layer above believed the `ok` it was given. A connector
    /// that can lose work at teardown has to be able to say so.
    ///
    /// Most connectors hold nothing across calls and take the default.
    /// Answering `Vec::new()` means "nothing was lost", which is a claim,
    /// not a shrug: do not implement this to report success you did not
    /// check.
    fn finish(&self) -> Vec<String> {
        Vec::new()
    }
}

/// One wired connector: a backing plus the scope it was granted.
struct Wired {
    connector: Arc<dyn Connector>,
    scope: Option<Scope>,
}

/// The registry: connector-family name → wired backing. Connectors are **off
/// by default, all of them** — construction wires each one explicitly per
/// environment, and wiring validates the scope against the connector's
/// declared scope-type immediately, so an ill-scoped wiring fails at startup
/// rather than as a mystifying refusal at first call.
#[derive(Default)]
pub struct Registry {
    wired: BTreeMap<String, Wired>,
}

impl Registry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Wire `connector` to answer the `family` call namespace (`"time"`
    /// answers `time` and `time/…`; `"fs"` answers `fs/…`).
    pub fn wire(
        &mut self,
        family: impl Into<String>,
        connector: Arc<dyn Connector>,
        scope: Option<Scope>,
    ) -> Result<(), ScopeError> {
        let family = family.into();
        let ty = connector.scope_type();
        ty.validate(scope.as_ref()).map_err(|detail| ScopeError {
            capability: call_capability(&family),
            expected: ty.describe().to_string(),
            detail,
        })?;
        self.wired.insert(family, Wired { connector, scope });
        Ok(())
    }

    /// Declare every wired connector's scope-type into a [`ScopeRegistry`],
    /// so grant validation covers `host:<family>` and `host:<family>/*`.
    pub fn declare_scope_types(&self, scopes: &mut ScopeRegistry) {
        for (family, wired) in &self.wired {
            scopes.declare(call_capability(family), wired.connector.scope_type());
        }
    }

    fn resolve(&self, call: &str) -> Option<&Wired> {
        let family = call.split('/').next().unwrap_or(call);
        self.wired.get(family)
    }
}

/// The dispatcher. **Every drained request is answered**: whatever bytes come
/// off a request queue, [`Dispatcher::dispatch`] returns exactly one reply —
/// `malformed` for bytes it could not read (echoing whatever token was
/// readable), `denied` for a call outside the guest's grants or a family this
/// process does not wire, `error`/`ok` from the connector. A host that drops
/// requests on the floor has made backpressure invisible; this type is where
/// that cannot happen.
pub struct Dispatcher {
    registry: Registry,
}

impl Dispatcher {
    pub fn new(registry: Registry) -> Self {
        Dispatcher { registry }
    }

    /// Tell every wired connector the process is ending, and collect what
    /// each says went wrong. Empty means every connector ended cleanly.
    ///
    /// The caller decides what to do with a non-empty answer; `drt run`
    /// refuses to exit zero. Connectors are visited in registry order, which
    /// is name order, so the report is stable between runs.
    pub fn finish(&self) -> Vec<String> {
        self.registry
            .wired
            .values()
            .flat_map(|w| w.connector.finish())
            .collect()
    }

    /// Answer one drained request against one guest's capability set.
    pub async fn dispatch(&self, caps: &CapSet, raw: &[u8]) -> Reply {
        match self.route(caps, raw) {
            Routed::Answered(reply) => reply,
            Routed::Call(call) => call.answer().await,
        }
    }

    /// Route one drained request: answer it here when no connector is
    /// involved — unreadable, ungranted, unwired — or hand back the
    /// connector call to be awaited.
    ///
    /// The split is what lets a pump defer (doc/Wasm.md D6): the
    /// capability decision is synchronous and cheap, and the
    /// [`PendingCall`] owns everything the connector needs, so a call whose
    /// answer is not ready can be parked and polled later, with nothing
    /// borrowed from the dispatcher or the capability set that routed it.
    pub fn route(&self, caps: &CapSet, raw: &[u8]) -> Routed {
        let req: Request = match drt_hostcall::from_bytes(raw) {
            Ok(req) => req,
            Err(e) => {
                return Routed::Answered(Reply::malformed(
                    salvage_token(raw),
                    format!("unreadable request: {e}"),
                ))
            }
        };
        if !caps.holds(&call_capability(&req.call)) {
            return Routed::Answered(Reply::denied(
                req.tok,
                format!("'{}' is outside this instance's grants", req.call),
            ));
        }
        let Some(wired) = self.registry.resolve(&req.call) else {
            return Routed::Answered(Reply::denied(
                req.tok,
                format!("no connector is wired for '{}' in this process", req.call),
            ));
        };
        Routed::Call(PendingCall {
            tok: req.tok,
            call: req.call,
            args: req.args,
            connector: Arc::clone(&wired.connector),
            scope: wired.scope.clone(),
        })
    }
}

/// One request, routed.
pub enum Routed {
    /// Answered without a connector: malformed, denied, or unwired.
    Answered(Reply),
    /// A connector's to answer; [`PendingCall::answer`] does it.
    Call(PendingCall),
}

/// A connector call with everything it needs, owned. It can be awaited
/// where it was routed or much later and somewhere else — a pump's
/// in-flight table, while the rest of the swarm keeps stepping.
pub struct PendingCall {
    tok: Token,
    call: String,
    args: Option<rmpv::Value>,
    connector: Arc<dyn Connector>,
    scope: Option<Scope>,
}

impl PendingCall {
    /// The token the reply will echo.
    pub fn tok(&self) -> Token {
        self.tok
    }

    /// The full call name, `fs/read`.
    pub fn call(&self) -> &str {
        &self.call
    }

    /// Run the connector and shape its answer into the reply: `ok` with
    /// the value, or `error` with the connector's own sentence. `denied`
    /// was decided at routing and never comes from here.
    pub async fn answer(self) -> Reply {
        match self
            .connector
            .call(&self.call, self.args, self.scope.as_ref())
            .await
        {
            Ok(value) => Reply::ok(self.tok, value),
            Err(CallError(detail)) => Reply::error(self.tok, detail),
        }
    }
}

pub mod mock {
    //! A mock is not a test double bolted on later; it is a first-class
    //! backing. Guests cannot tell — see the indistinguishability test.

    use super::*;

    /// Answers from a fixed table: call name → value. Anything else is an
    /// error, the same shape a real connector produces for a call outside
    /// its family's surface.
    #[derive(Default)]
    pub struct MockConnector {
        answers: BTreeMap<String, rmpv::Value>,
    }

    impl MockConnector {
        pub fn new() -> Self {
            Self::default()
        }

        pub fn answer(mut self, call: impl Into<String>, value: rmpv::Value) -> Self {
            self.answers.insert(call.into(), value);
            self
        }
    }

    #[async_trait::async_trait]
    impl Connector for MockConnector {
        async fn call(
            &self,
            call: &str,
            _args: Option<rmpv::Value>,
            _scope: Option<&Scope>,
        ) -> CallResult {
            self.answers
                .get(call)
                .cloned()
                .ok_or_else(|| CallError::new(format!("the mock has no answer for '{call}'")))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::mock::MockConnector;
    use super::*;
    use drt_caps::Grant;
    use drt_hostcall::{to_bytes, Status};

    fn dispatcher_with_time() -> Dispatcher {
        let mut reg = Registry::new();
        reg.wire(
            "time",
            Arc::new(MockConnector::new().answer("time", rmpv::Value::from(1_700_000_000_000u64))),
            None,
        )
        .unwrap();
        Dispatcher::new(reg)
    }

    fn caps(names: &[&str]) -> Arc<CapSet> {
        CapSet::root(names.iter().map(|n| Grant::grant(*n)).collect())
    }

    #[test]
    fn granted_and_wired_answers_ok() {
        let d = dispatcher_with_time();
        let caps = caps(&["host:time"]);
        let raw = to_bytes(&Request {
            tok: 42,
            call: "time".into(),
            args: None,
        })
        .unwrap();
        let reply = pollster::block_on(d.dispatch(&caps, &raw));
        assert_eq!(
            reply,
            Reply::ok(42, rmpv::Value::from(1_700_000_000_000u64))
        );
    }

    #[test]
    fn ungranted_is_denied_not_dropped() {
        let d = dispatcher_with_time();
        let caps = caps(&[]);
        let raw = to_bytes(&Request {
            tok: 1,
            call: "time".into(),
            args: None,
        })
        .unwrap();
        let reply = pollster::block_on(d.dispatch(&caps, &raw));
        assert_eq!(reply.status, Status::Denied);
        assert_eq!(reply.tok, Some(1));
        assert!(reply.detail.is_some());
    }

    #[test]
    fn unwired_is_denied_even_when_granted() {
        let d = dispatcher_with_time();
        let caps = caps(&["host:*"]);
        let raw = to_bytes(&Request {
            tok: 2,
            call: "fs/read".into(),
            args: None,
        })
        .unwrap();
        let reply = pollster::block_on(d.dispatch(&caps, &raw));
        assert_eq!(reply.status, Status::Denied);
    }

    #[test]
    fn unreadable_bytes_are_answered_malformed() {
        let d = dispatcher_with_time();
        let caps = caps(&["host:time"]);
        // Not a map at all.
        let reply = pollster::block_on(d.dispatch(&caps, &to_bytes(&"junk").unwrap()));
        assert_eq!(reply.status, Status::Malformed);
        assert_eq!(reply.tok, None);
        // A map with a readable tok but a missing field: the tok is echoed.
        let partial = rmpv::Value::Map(vec![("tok".into(), rmpv::Value::from(9u64))]);
        let raw = rmp_serde::to_vec(&partial).unwrap();
        let reply = pollster::block_on(d.dispatch(&caps, &raw));
        assert_eq!(reply.status, Status::Malformed);
        assert_eq!(reply.tok, Some(9));
    }

    #[test]
    fn connector_error_carries_detail() {
        let d = dispatcher_with_time();
        let caps = caps(&["host:time*"]);
        let raw = to_bytes(&Request {
            tok: 3,
            call: "time/monotonic".into(),
            args: None,
        })
        .unwrap();
        let reply = pollster::block_on(d.dispatch(&caps, &raw));
        assert_eq!(reply.status, Status::Error);
        assert!(reply.detail.unwrap().contains("time/monotonic"));
    }

    #[test]
    fn ill_scoped_wiring_fails_at_startup_by_name() {
        struct NeedsPath;
        #[async_trait::async_trait]
        impl Connector for NeedsPath {
            fn scope_type(&self) -> Box<dyn ScopeType> {
                struct PathScope;
                impl ScopeType for PathScope {
                    fn describe(&self) -> &str {
                        "a directory path"
                    }
                    fn validate(&self, scope: Option<&Scope>) -> Result<(), String> {
                        match scope {
                            Some(Scope(v)) if v.is_str() => Ok(()),
                            _ => Err("scope is required".into()),
                        }
                    }
                }
                Box::new(PathScope)
            }
            async fn call(&self, _: &str, _: Option<rmpv::Value>, _: Option<&Scope>) -> CallResult {
                Ok(rmpv::Value::Nil)
            }
        }
        let mut reg = Registry::new();
        let err = reg.wire("fs", Arc::new(NeedsPath), None).unwrap_err();
        assert_eq!(err.capability, "host:fs");
        assert!(err.to_string().contains("a directory path"));
    }
}
