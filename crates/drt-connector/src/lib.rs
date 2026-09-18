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

pub mod handles;
pub use handles::{Caller, HandleId, Handles, NoSuchHandle};

/// Who is asking, and under which grants.
///
/// `grants` is the scope on every grant that permits this call — one entry
/// per permitting grant that carries a scope, none for a grant that does
/// not. A connector that scopes per node (`crypto/derive`'s label
/// patterns, `doc/Plan-0.7.0.md` §4.3a) reads them; most connectors ignore
/// them, since a wiring scope is a place and a grant scope is a permission
/// and only a few calls have anything to say about the second.
///
/// A struct rather than two parameters, so the next thing a connector
/// needs to know about its caller is a field and not a signature change.
pub struct Asker<'a> {
    pub caller: Caller,
    pub grants: &'a [Scope],
}

/// Something a connector has to say to an owner unprompted: a message for
/// one of the owner's queues (`doc/Plan-0.7.0.md` §3.4). Readiness is the
/// first kind — a handle became readable — and a timer firing would be
/// another; the shape does not care. The host delivers it as any push
/// into that queue, which is what wakes a hibernated owner that asked to
/// be woken on a message.
#[derive(Debug, Clone, PartialEq)]
pub struct Notice {
    pub owner: Caller,
    pub queue: String,
    pub message: rmpv::Value,
}

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

/// What backs a wired family, for `capabilities/list`'s `kind` and `owner`.
///
/// A guest cannot tell a plugin from a builtin at a call -- that is the
/// point of a plugin being an ordinary [`Connector`] -- so this is the one
/// place the difference is visible at all, and it is visible only to a
/// program that asked for the menu. `doc/Plugins.md` §4.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum Backing {
    /// Compiled into this binary. There is nobody to name as its owner:
    /// the family is the host's own.
    #[default]
    Builtin,
    /// A plugin, named as the deployment's `plugins` block names it -- the
    /// `<name>` in `<name>.plugin.json`, and deliberately not the family,
    /// because a family that named itself as its own owner would say
    /// nothing a reader did not already have.
    Plugin { name: String },
}

impl Backing {
    /// The C host's `kind` spelling (vera `DRT_ASKS.md` §14).
    pub fn kind(&self) -> &'static str {
        match self {
            Backing::Builtin => "builtin",
            Backing::Plugin { .. } => "plugin",
        }
    }

    /// The C host's `owner`: nil for a builtin, the plugin's name otherwise.
    pub fn owner(&self) -> rmpv::Value {
        match self {
            Backing::Builtin => rmpv::Value::Nil,
            Backing::Plugin { name } => name.as_str().into(),
        }
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

    /// What backs this family, reported by `capabilities/list` and nothing
    /// else. Default: [`Backing::Builtin`], which every connector in
    /// `connectors/` is and none of them has to say.
    fn backing(&self) -> Backing {
        Backing::Builtin
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

    /// Answer one call, knowing who asked and under what. The default
    /// forgets the asker and answers [`Connector::call`], so a connector
    /// that holds nothing per node implements only that one and is
    /// unchanged by this method existing. A connector that keeps resources
    /// on a guest's behalf implements this instead, and keys what it keeps
    /// by `asker.caller` (`doc/Plan-0.7.0.md` §2.1–2.2).
    ///
    /// Everything in `asker` is a value the dispatcher took from the
    /// routing step, never from the request: a request whose subject is a
    /// string the asking node chose is not an identity.
    async fn call_as(
        &self,
        asker: &Asker<'_>,
        call: &str,
        args: Option<rmpv::Value>,
        scope: Option<&Scope>,
    ) -> CallResult {
        let _ = asker;
        self.call(call, args, scope).await
    }

    /// One node is gone for good — killed, out of budget, trapped — and
    /// whatever this connector held for it goes with it. Each string
    /// returned is one thing that did not end well, said so a reader can
    /// tell whose it was (§2.4–2.5).
    ///
    /// Never called for hibernation. A parked node keeps what it holds;
    /// that is the whole point of it being able to park.
    ///
    /// The default holds nothing per node and so loses nothing. As with
    /// [`Connector::finish`], an empty answer is a claim and not a shrug.
    fn release(&self, caller: &Caller) -> Vec<String> {
        let _ = caller;
        Vec::new()
    }

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

    /// Owners whose **vital** resource has ended since the last ask, each
    /// with a sentence saying which (`doc/Plan-0.7.0.md` §3.3). A vital
    /// resource is one its owner declared it exists to serve; when it
    /// ends, the runtime ends the owner, and this is how the runtime
    /// hears. Asked every step, so an answer must be a look and not a
    /// wait, and each ending is reported exactly once.
    ///
    /// The default holds nothing vital and reports nothing.
    fn ended(&self) -> Vec<(Caller, String)> {
        Vec::new()
    }

    /// What this connector has to tell owners unprompted since the last
    /// ask (§3.4): each a [`Notice`] for one of the owner's queues. Asked
    /// every step, so an answer is a look and not a wait. A notice the
    /// host cannot deliver — the queue full, the owner gone or parked
    /// without asking to be woken — is dropped like any such push; a
    /// connector says a thing again only when it becomes true again.
    ///
    /// The default has nothing to say.
    fn notices(&self) -> Vec<Notice> {
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
    grants: Option<Arc<dyn GrantDesk>>,
}

impl Dispatcher {
    pub fn new(registry: Registry) -> Self {
        Dispatcher {
            registry,
            grants: None,
        }
    }

    /// Give this dispatcher somewhere to put grant requests.
    ///
    /// Answered here rather than by a connector for the reason
    /// `capabilities/list` is: only the dispatcher holds both halves. A
    /// connector's `call` receives its wiring's scope and nothing about the
    /// *instance* — deliberately, since a scope is a place and not an
    /// identity — so a connector cannot know which node is asking. The
    /// dispatcher has the caller's [`CapSet`], and a set knows whose it is.
    ///
    /// Optional because most processes have no root to write into: `drt run`
    /// over a single file has no `state/gsr/`, and a call arriving with no desk
    /// is answered `denied` by the same rule as a call arriving with no
    /// connector — "this build does not carry that" is an honest answer.
    pub fn with_grants(mut self, desk: Arc<dyn GrantDesk>) -> Self {
        self.grants = Some(desk);
        self
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

    /// Tell every wired connector one node is gone for good, and collect
    /// what each says it lost for that node. The per-node counterpart of
    /// [`Dispatcher::finish`], and like it visited in name order so the
    /// report is stable.
    ///
    /// Called from the host's death hook and never from its hibernation
    /// hook — `SwarmHost::released`, not `detached` (§2.4).
    pub fn release(&self, caller: &Caller) -> Vec<String> {
        self.registry
            .wired
            .values()
            .flat_map(|w| w.connector.release(caller))
            .collect()
    }

    /// Every owner some connector says has lost the resource it exists to
    /// serve (§3.3), with the connector's sentence. Name order, like the
    /// other two sweeps.
    pub fn ended(&self) -> Vec<(Caller, String)> {
        self.registry
            .wired
            .values()
            .flat_map(|w| w.connector.ended())
            .collect()
    }

    /// Everything every connector has to tell an owner unprompted (§3.4),
    /// name order.
    pub fn notices(&self) -> Vec<Notice> {
        self.registry
            .wired
            .values()
            .flat_map(|w| w.connector.notices())
            .collect()
    }

    /// Answer one drained request against one guest's capability set, with
    /// no instance behind it. The root is the caller: a harness, a test, or
    /// the process's own bookkeeping, and what such a call creates lives to
    /// teardown.
    pub async fn dispatch(&self, caps: &CapSet, raw: &[u8]) -> Reply {
        self.dispatch_as(Caller::Root, caps, raw).await
    }

    /// [`Dispatcher::dispatch`], knowing which instance asked.
    pub async fn dispatch_as(&self, caller: Caller, caps: &CapSet, raw: &[u8]) -> Reply {
        match self.route_as(caller, caps, raw) {
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
        self.route_as(Caller::Root, caps, raw)
    }

    /// [`Dispatcher::route`], knowing which instance asked. The pump uses
    /// this one; the caller rides on the [`PendingCall`] so the connector
    /// learns it however late the call is answered.
    pub fn route_as(&self, caller: Caller, caps: &CapSet, raw: &[u8]) -> Routed {
        let req: Request = match drt_hostcall::from_bytes(raw) {
            Ok(req) => req,
            Err(e) => {
                return Routed::Answered(Reply::malformed(
                    salvage_token(raw),
                    format!("unreadable request: {e}"),
                ))
            }
        };
        let cap = call_capability(&req.call);
        if !caps.holds(&cap) {
            return Routed::Answered(Reply::denied(
                req.tok,
                format!("'{}' is outside this instance's grants", req.call),
            ));
        }
        // The scope on each grant that lets this call through, for a
        // connector that scopes per node. Collected here because only the
        // dispatcher holds the capability set, and collected as values so
        // the call can be parked without borrowing the set that routed it.
        let grants: Vec<Scope> = caps
            .grants()
            .iter()
            .filter(|g| {
                matches!(g.effect, drt_caps::Effect::Grant)
                    && drt_caps::implies(&g.capability, &cap)
            })
            .filter_map(|g| g.scope.clone())
            .collect();
        // The menu, answered here because only the dispatcher holds both
        // halves of the answer: what is wired, and what this instance may
        // reach. The C host wires it as a connector (`dhost.c`,
        // `conn_capabilities`) and gates it the same way -- a program needs
        // `host:capabilities/list`, and an auditor granted that and nothing
        // else can report what a swarm reaches without reaching any of it.
        if req.call == CAPABILITIES_LIST {
            return Routed::Answered(Reply::ok(req.tok, self.capabilities(caps)));
        }
        // The other half of consent.md §10, and here for the same reason.
        if req.call == REQUEST_GRANT {
            return Routed::Answered(self.request_grant(caps, req.tok, req.args.as_ref()));
        }
        let Some(wired) = self.registry.resolve(&req.call) else {
            return Routed::Answered(Reply::denied(
                req.tok,
                format!("no connector is wired for '{}' in this process", req.call),
            ));
        };
        Routed::Call(PendingCall {
            caller,
            grants,
            tok: req.tok,
            call: req.call,
            args: req.args,
            connector: Arc::clone(&wired.connector),
            scope: wired.scope.clone(),
        })
    }
}

/// The calls the dispatcher answers with a value of its own.
pub const CAPABILITIES_LIST: &str = "capabilities/list";
/// `request_grant` (consent.md §10): a node asking for something beyond its
/// current grant. Under the `capabilities` family, so `host:capabilities/list`
/// alone reports without being able to ask — an auditor and a petitioner are
/// different grants.
pub const REQUEST_GRANT: &str = "capabilities/request_grant";

/// Where a grant request goes, and what the root's ceiling is.
///
/// Deliberately untyped at this boundary: `drt-connector` knows nothing of
/// realms, `project.json` or signatures, and a trait naming those types would
/// drag `drt-config` in here for no gain. The implementation in `drt` does all
/// the decoding and all the verifying; this is the seam, so that a dispatcher
/// with no root answers honestly instead of pretending.
pub trait GrantDesk: Send + Sync {
    /// The capability names the root's declared ceiling allows. Reported by
    /// `capabilities/list` so a node can tell "not granted, ask" from "not
    /// granted, and this root will never allow it, reconfigure" — which is the
    /// difference between a request worth making and a message worth printing.
    fn ceiling(&self) -> Vec<String>;

    /// Ask. `node` is the calling node's path, taken from the capability set
    /// rather than from the request, because a request whose subject is a
    /// string the asking node chose is not an audit record.
    fn request(&self, node: &str, args: Option<&rmpv::Value>) -> Result<rmpv::Value, String>;
}

impl Dispatcher {
    /// `capabilities/list`: one entry per wired family, in the C host's
    /// shape -- `{name, kind, owner, granted, visibility}` -- plus the
    /// menu itself. `granted` is asked the way a call would ask it, so the
    /// listing cannot drift from what a call would do; a family is
    /// `granted` when the instance holds it or anything under it, since a
    /// program holding only `host:fs/read` reaches `fs` and should be told
    /// so. `kind` and `owner` come from the wired connector's
    /// [`Backing`], so a plugin-backed family reads `plugin` and names the
    /// plugin -- the one place a guest can tell the two apart at all.
    /// `visibility` is still always `public`: DRT has no visibility policy
    /// to narrow a family with, and the field is here so a program written
    /// against the C host reads the same map (vera `DRT_ASKS.md` §14).
    fn capabilities(&self, caps: &CapSet) -> rmpv::Value {
        use drt_caps::Effect;
        let under_family = |family: &str| {
            let cap = call_capability(family);
            let under = format!("{cap}/");
            move |name: &str| name == cap || name.starts_with(&under)
        };
        let granted = |family: &str| {
            let matches = under_family(family);
            let cap = call_capability(family);
            caps.holds(&cap)
                || caps
                    .grants()
                    .iter()
                    .any(|g| matches!(g.effect, Effect::Grant) && matches(&g.capability))
        };
        // What this instance *holds* under a family, not merely whether it
        // holds something. A program told `granted = false` cannot tell a
        // missing grant from a denied one, and a program told `granted = true`
        // for `fs` still does not know whether it may write.
        let held = |family: &str| {
            let matches = under_family(family);
            rmpv::Value::Array(
                caps.grants()
                    .iter()
                    .filter(|g| matches(&g.capability))
                    .map(|g| {
                        rmpv::Value::Map(vec![
                            ("capability".into(), g.capability.as_str().into()),
                            (
                                "effect".into(),
                                match g.effect {
                                    Effect::Grant => "grant",
                                    Effect::Deny => "deny",
                                }
                                .into(),
                            ),
                        ])
                    })
                    .collect(),
            )
        };
        // Whether the root's ceiling could *ever* permit this family.
        // consent.md §10's point: "not granted, ask" and "not granted, and
        // this root's ceiling will never allow it, reconfigure" are different
        // messages, and only the second is worth failing early on. `true` with
        // no desk, because a process with no root has no ceiling to be outside
        // of -- and claiming otherwise would make every family look closed.
        let ceiling = self.grants.as_ref().map(|desk| desk.ceiling());
        let within_ceiling = |family: &str| match &ceiling {
            None => true,
            Some(names) => {
                let matches = under_family(family);
                names
                    .iter()
                    .any(|name| matches(name) || drt_caps::implies(name, &call_capability(family)))
            }
        };
        // `{name, kind, owner, granted, visibility}` is the C host's shape
        // (vera `DRT_ASKS.md` §14) and stays exactly that. The new facts are
        // *added* keys: a program written against the C host reads the same
        // five and ignores these, which is why they are fields here rather
        // than a reshaped reply.
        let entry = |name: &str, family: &str, is_granted: bool, backing: Backing| {
            rmpv::Value::Map(vec![
                ("name".into(), name.into()),
                ("kind".into(), backing.kind().into()),
                ("owner".into(), backing.owner()),
                ("granted".into(), rmpv::Value::Boolean(is_granted)),
                ("visibility".into(), "public".into()),
                ("held".into(), held(family)),
                (
                    "within_ceiling".into(),
                    rmpv::Value::Boolean(within_ceiling(family)),
                ),
            ])
        };
        let mut out: Vec<rmpv::Value> = self
            .registry
            .wired
            .iter()
            .map(|(family, wired)| {
                entry(family, family, granted(family), wired.connector.backing())
            })
            .collect();
        // The menu itself is the dispatcher's own and cannot be wired, so
        // it is a builtin by construction rather than by asking anyone.
        out.push(entry(
            "capabilities",
            "capabilities",
            caps.holds(&call_capability(CAPABILITIES_LIST)),
            Backing::Builtin,
        ));
        rmpv::Value::Array(out)
    }

    /// `request_grant`: hand the ask to the desk, or say there is none.
    ///
    /// The answer is always `Status::Ok` with a value; `pending`, `granted`
    /// and `denied` are *values* and not statuses, because `doc/Hostcall.md`
    /// has no pending status on purpose -- under the queue shape "the answer
    /// has not arrived" is an empty queue. A realm outside the ceiling comes
    /// back `ok` plus `denied`, never `Status::Denied`, which means "you do
    /// not hold `host:...`" and would send a node looking for the wrong fix.
    fn request_grant(
        &self,
        caps: &CapSet,
        tok: drt_hostcall::Token,
        args: Option<&rmpv::Value>,
    ) -> Reply {
        let Some(desk) = &self.grants else {
            return Reply::denied(
                tok,
                "this process has no root to record a grant request in; \
                 `request_grant` needs a deployment, not a single program",
            );
        };
        let Some(node) = caps.holder() else {
            // A desk exists, so there is a root, so the asking instance is in
            // a tree and has a path. Named rather than unwrapped: the day that
            // stops being true, this says so instead of panicking in a pump.
            return Reply::error(
                tok,
                "this instance has no node path, so a grant request would have no subject",
            );
        };
        match desk.request(&node.0, args) {
            Ok(value) => Reply::ok(tok, value),
            Err(why) => Reply::error(tok, why),
        }
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
    caller: Caller,
    grants: Vec<Scope>,
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

    /// Who asked.
    pub fn caller(&self) -> Caller {
        self.caller
    }

    /// The scopes on the grants that let this call through.
    pub fn grants(&self) -> &[Scope] {
        &self.grants
    }

    /// Run the connector and shape its answer into the reply: `ok` with
    /// the value, or `error` with the connector's own sentence. `denied`
    /// was decided at routing and never comes from here.
    ///
    /// The blob lane is closed here and nowhere else: a connector answering
    /// a column wraps it with `drt_hostcall::column`, and this is what
    /// moves those bytes into the reply's side channel and leaves the
    /// `{dtype, len, blob}` descriptor behind (`doc/Plan-2026-09.md` §3.2).
    /// Doing it here rather than in each connector is what lets the trait
    /// keep one method: a connector answers one value, and whether that
    /// value holds a column is a property of the value.
    pub async fn answer(self) -> Reply {
        let PendingCall {
            caller,
            grants,
            tok,
            call,
            args,
            connector,
            scope,
        } = self;
        let asker = Asker {
            caller,
            grants: &grants,
        };
        match connector.call_as(&asker, &call, args, scope.as_ref()).await {
            Ok(value) => {
                let mut blobs = Vec::new();
                let value = drt_hostcall::lift_columns(value, &mut blobs);
                let mut reply = Reply::ok(tok, value);
                reply.blobs = blobs;
                reply
            }
            Err(CallError(detail)) => Reply::error(tok, detail),
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
        backing: Backing,
    }

    impl MockConnector {
        pub fn new() -> Self {
            Self::default()
        }

        pub fn answer(mut self, call: impl Into<String>, value: rmpv::Value) -> Self {
            self.answers.insert(call.into(), value);
            self
        }

        /// Claim a backing other than builtin. This exists so the capability
        /// menu's `kind` and `owner` can be tested here: `drt-plugin`
        /// depends on this crate, so a real `PluginConnector` cannot.
        pub fn backed_by(mut self, backing: Backing) -> Self {
            self.backing = backing;
            self
        }
    }

    #[async_trait::async_trait]
    impl Connector for MockConnector {
        fn backing(&self) -> Backing {
            self.backing.clone()
        }

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

    /// A plugin-backed family says so, and names the plugin. The rest of
    /// the row is a builtin's row: a guest that calls it cannot tell, and
    /// only the menu can (`doc/Plugins.md` §4).
    #[test]
    fn the_menu_tells_a_plugin_backed_family_from_a_builtin() {
        fn row(reply: &Reply, name: &str) -> Vec<(String, rmpv::Value)> {
            let entries = match reply.value.as_ref().unwrap() {
                rmpv::Value::Array(v) => v.clone(),
                other => panic!("the menu is an array, got {other:?}"),
            };
            let found = entries
                .iter()
                .find(|e| match e {
                    rmpv::Value::Map(m) => m
                        .iter()
                        .any(|(k, v)| k.as_str() == Some("name") && v.as_str() == Some(name)),
                    _ => false,
                })
                .unwrap_or_else(|| panic!("no '{name}' row in the menu"));
            match found {
                rmpv::Value::Map(m) => m
                    .iter()
                    .map(|(k, v)| (k.as_str().unwrap_or_default().to_string(), v.clone()))
                    .collect(),
                _ => unreachable!(),
            }
        }
        fn get<'a>(row: &'a [(String, rmpv::Value)], key: &str) -> &'a rmpv::Value {
            &row.iter().find(|(k, _)| k == key).expect(key).1
        }

        let mut reg = Registry::new();
        reg.wire("time", Arc::new(MockConnector::new()), None)
            .unwrap();
        reg.wire(
            "browser",
            Arc::new(MockConnector::new().backed_by(Backing::Plugin {
                name: "chromium-driver".into(),
            })),
            None,
        )
        .unwrap();
        let d = Dispatcher::new(reg);

        let raw = to_bytes(&drt_hostcall::Request {
            tok: 9,
            call: "capabilities/list".into(),
            args: None,
        })
        .unwrap();
        let reply = pollster::block_on(
            d.dispatch(&caps(&["host:capabilities/list", "host:browser"]), &raw),
        );

        let plugin = row(&reply, "browser");
        assert_eq!(get(&plugin, "kind").as_str(), Some("plugin"));
        assert_eq!(get(&plugin, "owner").as_str(), Some("chromium-driver"));
        // The plugin row is otherwise an ordinary row: a family a guest
        // holds reads `granted`, whoever answers it.
        assert_eq!(get(&plugin, "granted"), &rmpv::Value::Boolean(true));
        assert_eq!(get(&plugin, "visibility").as_str(), Some("public"));

        // A builtin still has no owner to name, and the menu itself is one.
        for family in ["time", "capabilities"] {
            let builtin = row(&reply, family);
            assert_eq!(get(&builtin, "kind").as_str(), Some("builtin"), "{family}");
            assert_eq!(get(&builtin, "owner"), &rmpv::Value::Nil, "{family}");
        }
    }

    /// The menu: what is wired, and whether this instance may reach it,
    /// in the C host's shape. Gated like any call, so an ungranted program
    /// learns nothing (vera `DRT_ASKS.md` §14).
    #[test]
    fn capabilities_list_names_every_wired_family_and_whether_it_is_held() {
        fn entries(reply: &Reply) -> Vec<rmpv::Value> {
            reply.value.as_ref().unwrap().as_array().unwrap().to_vec()
        }
        fn field(e: &rmpv::Value, key: &str) -> rmpv::Value {
            let map: &[(rmpv::Value, rmpv::Value)] = e.as_map().unwrap();
            map.iter()
                .find(|(k, _)| k.as_str() == Some(key))
                .unwrap()
                .1
                .clone()
        }
        fn find(entries: &[rmpv::Value], name: &str) -> rmpv::Value {
            entries
                .iter()
                .find(|e| field(e, "name") == rmpv::Value::from(name))
                .unwrap_or_else(|| panic!("no entry for {name}: {entries:?}"))
                .clone()
        }
        let d = dispatcher_with_time();
        let raw = to_bytes(&Request {
            tok: 7,
            call: "capabilities/list".into(),
            args: None,
        })
        .unwrap();

        let reply =
            pollster::block_on(d.dispatch(&caps(&["host:capabilities/list", "host:time"]), &raw));
        assert_eq!(reply.status, Status::Ok, "{reply:?}");
        let all = entries(&reply);
        let time = find(&all, "time");
        assert_eq!(field(&time, "granted"), rmpv::Value::Boolean(true));
        assert_eq!(field(&time, "kind"), rmpv::Value::from("builtin"));
        assert_eq!(field(&time, "owner"), rmpv::Value::Nil);
        assert_eq!(field(&time, "visibility"), rmpv::Value::from("public"));
        assert_eq!(
            field(&find(&all, "capabilities"), "granted"),
            rmpv::Value::Boolean(true)
        );

        // Held narrowly is still reached: `host:time/x` is under `time`.
        let reply =
            pollster::block_on(d.dispatch(&caps(&["host:capabilities/list", "host:time/x"]), &raw));
        assert_eq!(
            field(&find(&entries(&reply), "time"), "granted"),
            rmpv::Value::Boolean(true)
        );

        // Not held at all is said so, and a program without the menu is told nothing.
        let reply = pollster::block_on(d.dispatch(&caps(&["host:capabilities/list"]), &raw));
        assert_eq!(
            field(&find(&entries(&reply), "time"), "granted"),
            rmpv::Value::Boolean(false)
        );
        let reply = pollster::block_on(d.dispatch(&caps(&["host:time"]), &raw));
        assert_eq!(reply.status, Status::Denied, "{reply:?}");
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

    /// Acceptance 1, the runtime half: a connector written before the
    /// caller existed — implementing `call` and nothing else — is reached
    /// through the caller-aware path unchanged, because `call_as` defaults
    /// to it. (The compile half is the nine crates in `connectors/`
    /// building with no edits.)
    #[test]
    fn a_connector_that_never_heard_of_callers_still_answers() {
        let d = dispatcher_with_time();
        let caps = caps(&["host:time"]);
        let raw = to_bytes(&Request {
            tok: 5,
            call: "time".into(),
            args: None,
        })
        .unwrap();
        let reply = pollster::block_on(d.dispatch_as(Caller::Node(3), &caps, &raw));
        assert_eq!(reply, Reply::ok(5, rmpv::Value::from(1_700_000_000_000u64)));
    }

    /// A connector that does care sees exactly who asked, and the plain
    /// `dispatch` path presents as the root.
    #[test]
    fn a_caller_aware_connector_is_told_who_asked() {
        struct Echo;
        #[async_trait::async_trait]
        impl Connector for Echo {
            async fn call(&self, _: &str, _: Option<rmpv::Value>, _: Option<&Scope>) -> CallResult {
                unreachable!("call_as is implemented, so this is never the path")
            }
            async fn call_as(
                &self,
                asker: &Asker<'_>,
                _: &str,
                _: Option<rmpv::Value>,
                _: Option<&Scope>,
            ) -> CallResult {
                Ok(rmpv::Value::from(asker.caller.to_string()))
            }
        }
        let mut reg = Registry::new();
        reg.wire("who", Arc::new(Echo), None).unwrap();
        let d = Dispatcher::new(reg);
        let caps = caps(&["host:who"]);
        let raw = to_bytes(&Request {
            tok: 1,
            call: "who".into(),
            args: None,
        })
        .unwrap();

        let reply = pollster::block_on(d.dispatch_as(Caller::Node(42), &caps, &raw));
        assert_eq!(reply.value, Some(rmpv::Value::from("instance 42")));

        let reply = pollster::block_on(d.dispatch(&caps, &raw));
        assert_eq!(reply.value, Some(rmpv::Value::from("the root")));
    }

    /// §4.3a's control: a connector that scopes per node is handed the
    /// scope on every grant that permitted the call — and only those. A
    /// grant with no scope contributes nothing, so a broad unscoped grant
    /// beside a narrow scoped one yields exactly the narrow one.
    #[test]
    fn a_connector_is_handed_the_scopes_on_the_grants_that_let_it_through() {
        struct Sees;
        #[async_trait::async_trait]
        impl Connector for Sees {
            async fn call(&self, _: &str, _: Option<rmpv::Value>, _: Option<&Scope>) -> CallResult {
                unreachable!()
            }
            async fn call_as(
                &self,
                asker: &Asker<'_>,
                _: &str,
                _: Option<rmpv::Value>,
                _: Option<&Scope>,
            ) -> CallResult {
                Ok(rmpv::Value::Array(
                    asker.grants.iter().map(|Scope(v)| v.clone()).collect(),
                ))
            }
        }
        let mut reg = Registry::new();
        reg.wire("scoped", Arc::new(Sees), None).unwrap();
        let d = Dispatcher::new(reg);
        let caps = CapSet::root(vec![
            Grant::grant("host:*"),
            Grant {
                effect: drt_caps::Effect::Grant,
                capability: "host:scoped/*".into(),
                scope: Some(Scope(rmpv::Value::from("room:*"))),
            },
            Grant {
                effect: drt_caps::Effect::Grant,
                capability: "host:elsewhere".into(),
                scope: Some(Scope(rmpv::Value::from("never"))),
            },
        ]);
        let raw = to_bytes(&Request {
            tok: 1,
            call: "scoped/derive".into(),
            args: None,
        })
        .unwrap();
        let reply = pollster::block_on(d.dispatch_as(Caller::Node(1), &caps, &raw));
        assert_eq!(
            reply.value,
            Some(rmpv::Value::Array(vec![rmpv::Value::from("room:*")])),
            "{reply:?}"
        );
    }

    /// Acceptance 5: a release that loses work reports it, attributed to
    /// the node, and a release for a node that held nothing reports
    /// nothing. Two connectors, so the report is seen to be collected
    /// across the registry in name order.
    #[test]
    fn release_collects_each_connectors_loss_attributed_to_the_node() {
        struct Holds {
            table: Handles<&'static str>,
        }
        #[async_trait::async_trait]
        impl Connector for Holds {
            async fn call(&self, _: &str, _: Option<rmpv::Value>, _: Option<&Scope>) -> CallResult {
                Ok(rmpv::Value::Nil)
            }
            fn release(&self, caller: &Caller) -> Vec<String> {
                self.table
                    .release(*caller)
                    .into_iter()
                    .map(|(h, what)| {
                        format!(
                            "{} {} #{} owned by {caller} ended with {what}",
                            self.table.kind(),
                            self.table.kind(),
                            h.0
                        )
                    })
                    .collect()
            }
        }
        let a = Arc::new(Holds {
            table: Handles::new("alpha"),
        });
        let b = Arc::new(Holds {
            table: Handles::new("beta"),
        });
        a.table.insert(Caller::Node(7), "work unflushed");
        b.table.insert(Caller::Node(7), "a transaction open");
        b.table.insert(Caller::Node(8), "nothing of 7's");

        let mut reg = Registry::new();
        reg.wire("beta", b.clone(), None).unwrap();
        reg.wire("alpha", a.clone(), None).unwrap();
        let d = Dispatcher::new(reg);

        let lost = d.release(&Caller::Node(7));
        assert_eq!(lost.len(), 2, "{lost:?}");
        assert!(lost[0].starts_with("alpha"), "name order: {lost:?}");
        assert!(lost[1].starts_with("beta"), "name order: {lost:?}");
        assert!(
            lost.iter().all(|l| l.contains("owned by instance 7")),
            "attributed: {lost:?}"
        );

        assert!(
            d.release(&Caller::Node(9)).is_empty(),
            "held nothing, lost nothing"
        );
        assert_eq!(
            b.table.count(Caller::Node(8)),
            1,
            "a sibling's is untouched"
        );
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
