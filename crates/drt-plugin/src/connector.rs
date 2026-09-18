//! A plugin behind the `Connector` trait, so a guest cannot tell.
//!
//! ## surface block
//!
//! - Entry points: [`PluginConnector::new`], which takes a manifest and
//!   starts nothing; and the [`Connector`] impl, whose `call` is the
//!   whole of the runtime behaviour. [`PluginConnector::family`] is what
//!   the registry wires it under.
//! - Configurable values: none of this file's own. Every bound -- the call
//!   deadline, the number of calls that may be outstanding -- is the
//!   manifest's, read once at construction and enforced here.
//! - Fan-out: [`start`], the match from `manifest::Transport` to a channel.
//!   Three arms today, and the only place this file knows transports apart.
//!
//! # What this is
//!
//! `doc/Plugins.md` §1: the plugin channel is a connector *backing* behind
//! the existing trait, not a second protocol. Everything a builtin
//! connector gets from the dispatcher -- capability gating, token echo, the
//! answered-always rule, replay -- this inherits by being an ordinary
//! `Connector`. A plugin cannot say `denied`; that word is the
//! dispatcher's, and a plugin that tries gets its answer relayed as an
//! ordinary error.
//!
//! # Nothing here may block
//!
//! A call is answered by a future the drive loop polls with a no-op waker
//! (`drt_swarm::pump`), so this file may never wait on anything: a read
//! that blocks holds every guest in the deployment. [`Session`] is
//! non-blocking by construction and the future below is a loop of "poll
//! once, look, yield", where yielding means returning `Pending` and being
//! re-polled on the drive loop's next tick. [`Yield`] is that, and it is
//! the only unusual thing in the file.
//!
//! The one place this does wait is starting the plugin, which happens once
//! on the first call and is the same blocking-at-startup the transports
//! already do (`tcp`'s `connect`, `spawn`'s dial-back). A deployment feels
//! it as a slow first call, which is honest, and never as a stalled loop
//! afterwards.
//!
//! # Scope is a key, and that is the whole of it
//!
//! `doc/Plan-0.7.0.md` §7.3: the dispatcher routed **family → connector**,
//! and this makes it **(family, scope key) → instance**. The family half
//! is the registry's, above this file. The scope key is here, and it is
//! one function: [`PluginConnector::key_for`], which maps a caller to the
//! instance that serves it.
//!
//! - `root` — every caller maps to one key, so there is one process for
//!   the whole root. It serves several nodes and **cannot tell them
//!   apart**, which is the cost the manifest states in those words. Any
//!   per-caller policy is the host's to enforce before the call.
//! - `node` — each caller maps to itself, so there is a process per
//!   calling node, and it dies when that node does. This is the default,
//!   because an implicitly shared process is an ambient singleton and that
//!   is the thing the node model exists to avoid.
//!
//! Instances start on first call, per key, and [`Connector::release`]
//! takes one away when its owner dies. A `root` instance is not released
//! by any one node dying, because it is not that node's; it goes at
//! [`Connector::finish`] with everything else.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use drt_caps::Scope;
use drt_connector::{Asker, CallError, CallResult, Caller, Connector};

use crate::channel::{Channel, ChannelError};
use crate::frame::{ErrorClass, Reply, ReplyBody};
use crate::manifest::{Manifest, Scope as PluginScope, Transport};
use crate::session::{Session, SessionError};

/// A plugin family, served by one process this host starts on first use.
pub struct PluginConnector {
    manifest: Manifest,
    /// Scope key to the process serving it. Empty until the first call:
    /// one entry for a `root` plugin, one per calling node for a `node`
    /// one, and a `BTreeMap` rather than a hash so that `finish`'s report
    /// comes out in a stable order.
    instances: Mutex<BTreeMap<Caller, Session<Box<dyn Channel + Send>>>>,
}

impl PluginConnector {
    /// Take a manifest and start nothing.
    ///
    /// Starting is deferred to the first call on purpose: a deployment
    /// that wires six plugins and uses two should pay for two. What is
    /// *not* deferred is the refusal -- a manifest this host cannot serve
    /// is rejected here, at load, rather than at 3am on the first call.
    pub fn new(manifest: Manifest) -> Result<Self, String> {
        if manifest.transport.starts_the_program() && manifest.exec.is_none() {
            return Err(format!(
                "the plugin '{}' names no `exec` to start",
                manifest.family
            ));
        }
        Ok(Self {
            manifest,
            instances: Mutex::new(BTreeMap::new()),
        })
    }

    /// Which instance serves `caller`.
    ///
    /// The one place scope means anything at runtime. A `root` plugin
    /// folds every caller onto [`Caller::Root`], which is what "one
    /// process for the whole root" is when it is written down; a `node`
    /// plugin keeps the caller it was given.
    fn key_for(&self, caller: &Caller) -> Caller {
        match self.manifest.scope {
            PluginScope::Root => Caller::Root,
            PluginScope::Node => *caller,
        }
    }

    /// The family this connector answers, which is what it is wired under.
    pub fn family(&self) -> &str {
        &self.manifest.family
    }

    /// The manifest, for a caller that reports what is wired.
    pub fn manifest(&self) -> &Manifest {
        &self.manifest
    }

    /// How many processes are running, for the `Debug` impl.
    fn running(&self) -> usize {
        self.instances.lock().map(|t| t.len()).unwrap_or(0)
    }
}

/// Written by hand rather than derived: a derived one would need `Debug`
/// on the session and through it on every transport, and would print the
/// plugin's buffered bytes. What a reader wants here is which family, how
/// it is reached, and whether it is running.
impl std::fmt::Debug for PluginConnector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PluginConnector")
            .field("family", &self.manifest.family)
            .field("transport", &self.manifest.transport)
            .field("scope", &self.manifest.scope)
            .field("running", &self.running())
            .finish()
    }
}

#[async_trait::async_trait]
impl Connector for PluginConnector {
    /// Answer one call from an unnamed caller.
    ///
    /// The trait's required method, and what a harness or the process's
    /// own bookkeeping reaches. It is [`Caller::Root`]'s call, which for a
    /// `root` plugin is the only instance there is and for a `node` one is
    /// the root's own.
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

    /// Answer one call, routed to the instance that serves its caller.
    ///
    /// The asker decides which process answers and nothing else: a plugin
    /// is not told who called it, because `root` scope could not honour
    /// that and a `node` plugin does not need it -- it has a process to
    /// itself, which is the same fact expressed where it cannot be got
    /// wrong.
    async fn call_as(
        &self,
        asker: &Asker<'_>,
        call: &str,
        args: Option<rmpv::Value>,
        _scope: Option<&Scope>,
    ) -> CallResult {
        let key = self.key_for(&asker.caller);

        // Start on first use for this key, and queue the call. Both happen
        // under one lock and neither yields while holding it.
        let id = {
            let mut table = self.instances.lock().map_err(|_| poisoned(self.family()))?;
            let session = match table.entry(key) {
                std::collections::btree_map::Entry::Occupied(e) => e.into_mut(),
                // One lookup, and the start happens inside it: a second
                // caller for the same key cannot race in and start a
                // second process, because the table is locked throughout.
                std::collections::btree_map::Entry::Vacant(e) => e.insert(self.start()?),
            };
            session.begin(call, args).map_err(|e| self.said(e))?
        };

        let deadline = Instant::now() + Duration::from_millis(self.manifest.call_timeout_ms);
        loop {
            {
                let mut table = self.instances.lock().map_err(|_| poisoned(self.family()))?;
                // The instance can go while a call is outstanding: its
                // owner died and `release` took it. That is not a hang and
                // not a plugin failure, so it is said as what it is.
                let Some(session) = table.get_mut(&key) else {
                    return Err(CallError::new(format!(
                        "the plugin '{}' serving {key} was released while '{call}' was \
                         outstanding",
                        self.family()
                    )));
                };
                // A failed session is terminal, and the failure is the
                // answer: the plugin is gone, and saying so beats waiting
                // out a deadline for a process that will never reply.
                session.poll().map_err(|e| self.said(e))?;
                if let Some(reply) = session.take(id).map_err(|e| self.said(e))? {
                    return relay(reply);
                }
                if Instant::now() >= deadline {
                    // Stop waiting, and make sure a late reply is not read
                    // as an answer to the next question.
                    session.abandon(id);
                    return Err(CallError::new(format!(
                        "the plugin '{}' did not answer '{call}' within {} ms, its \
                         manifest's `call_timeout_ms`",
                        self.family(),
                        self.manifest.call_timeout_ms
                    )));
                }
            }
            Yield::once().await;
        }
    }

    /// One node is gone; its plugin goes with it.
    ///
    /// Only a `node` plugin has anything here. A `root` instance is not
    /// this node's to end -- it serves every node, and ending it because
    /// one caller died would take the others' plugin away -- so it is left
    /// for [`Connector::finish`].
    ///
    /// Dropping the session drops its channel, which sweeps the process
    /// tree. Calls that were still outstanding are reported, one line
    /// each: the answer they were waiting for is not coming, and a
    /// connector that can lose work at teardown has to say so.
    fn release(&self, caller: &Caller) -> Vec<String> {
        if self.manifest.scope != PluginScope::Node {
            return Vec::new();
        }
        let Ok(mut table) = self.instances.lock() else {
            return vec![format!(
                "the plugin '{}' could not be released for {caller}: a panic left its \
                 table in an unknown state",
                self.family()
            )];
        };
        match table.remove(caller) {
            Some(session) => lost(self.family(), caller, session),
            None => Vec::new(),
        }
    }

    /// The process is going. Every plugin goes with it.
    fn finish(&self) -> Vec<String> {
        let Ok(mut table) = self.instances.lock() else {
            return vec![format!(
                "the plugin '{}' could not be shut down: a panic left its table in an \
                 unknown state",
                self.family()
            )];
        };
        std::mem::take(&mut *table)
            .into_iter()
            .flat_map(|(key, session)| lost(self.family(), &key, session))
            .collect()
    }
}

/// What one ending instance did not finish.
///
/// An empty answer is a claim and not a shrug, which is what the trait
/// asks for: a session with nothing outstanding really did lose nothing,
/// because a plugin holds no state this host promised to keep.
fn lost(family: &str, key: &Caller, session: Session<Box<dyn Channel + Send>>) -> Vec<String> {
    let outstanding = session.in_flight();
    drop(session);
    if outstanding == 0 {
        return Vec::new();
    }
    vec![format!(
        "the plugin '{family}' serving {key} ended with {outstanding} call(s) \
         outstanding; their answers are not coming"
    )]
}

// depth: starting one, and relaying what it says

impl PluginConnector {
    /// The transport match: the one place this file knows them apart.
    fn start(&self) -> Result<Session<Box<dyn Channel + Send>>, CallError> {
        let channel: Box<dyn Channel + Send> = match self.manifest.transport {
            #[cfg(unix)]
            Transport::Process => Box::new(
                crate::process::ProcessChannel::spawn(&self.exec()?, &[])
                    .map_err(|e| self.would_not_start(e))?,
            ),
            #[cfg(not(unix))]
            Transport::Process => {
                return Err(CallError::new(format!(
                    "the plugin '{}' asks for the `process` transport, which hands the \
                     plugin its channel on fd 3 and exists on unix only; `spawn` is the \
                     same thing on this platform, and a manifest may name it instead",
                    self.family()
                )))
            }
            Transport::Spawn => Box::new(
                crate::spawn::SpawnChannel::start(&self.exec()?, &[])
                    .map_err(|e| self.would_not_start(e))?,
            ),
            Transport::Tcp => {
                return Err(CallError::new(format!(
                    "the plugin '{}' asks for the `tcp` transport, whose address is the \
                     deployment's to name and which nothing in a deployment names yet",
                    self.family()
                )))
            }
        };
        Ok(Session::new(channel, self.manifest.max_inflight))
    }

    /// The manifest's `exec`, which `new` has already established is there
    /// for a transport that starts the program.
    fn exec(&self) -> Result<PathBuf, CallError> {
        self.manifest
            .exec
            .as_ref()
            .map(PathBuf::from)
            .ok_or_else(|| {
                CallError::new(format!("the plugin '{}' names no `exec`", self.family()))
            })
    }

    fn would_not_start(&self, e: ChannelError) -> CallError {
        CallError::new(format!(
            "the plugin '{}' would not start: {e}",
            self.family()
        ))
    }

    /// A session failure, worded so the operator knows which plugin and
    /// what kind of wrong. Each of these is terminal for the session, and
    /// the connector's instance is left in place holding the failure so
    /// every subsequent call gets the same sentence rather than starting a
    /// second process to fail the same way.
    fn said(&self, e: SessionError) -> CallError {
        let family = self.family();
        match e {
            SessionError::Channel(ChannelError::Closed) => {
                CallError::new(format!("the plugin '{family}' is gone"))
            }
            SessionError::Saturated { in_flight } => CallError::new(format!(
                "the plugin '{family}' already has {in_flight} calls outstanding, its \
                 manifest's `max_inflight`; the call was not sent"
            )),
            other => CallError::new(format!("the plugin '{family}' failed: {other}")),
        }
    }
}

fn poisoned(family: &str) -> CallError {
    CallError::new(format!(
        "the plugin '{family}' was left in an unknown state by a panic and will not be \
         used again"
    ))
}

/// One plugin reply as a connector answer.
///
/// A plugin's `capability`-class error is still an ordinary error here.
/// `denied` is the dispatcher's word and a plugin does not get to borrow
/// it: a guest that was granted this call was granted it, and a plugin
/// disagreeing is the plugin's answer and not a permission decision.
fn relay(reply: Reply) -> CallResult {
    match reply.body {
        ReplyBody::Ok { value } => Ok(value),
        ReplyBody::Err { error } => {
            let kind = match error.class {
                ErrorClass::Capability => "does not serve",
                _ => "refused",
            };
            let _ = kind;
            Err(CallError::new(format!("{}: {}", error.code, error.message)))
        }
    }
}

// depth: yielding to the drive loop

/// Return `Pending` exactly once, so the drive loop takes the future back
/// and polls it again on its next tick.
///
/// The waker is a no-op in that loop, so nothing here may rely on being
/// woken; what makes this work is that the loop re-polls unconditionally.
/// A future that awaited something needing a real wake would simply never
/// be polled again, which is why this file waits on nothing else.
struct Yield(bool);

impl Yield {
    fn once() -> Self {
        Yield(false)
    }
}

impl std::future::Future for Yield {
    type Output = ();

    fn poll(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<()> {
        if self.0 {
            std::task::Poll::Ready(())
        } else {
            self.0 = true;
            // Asked for, so that a loop which *does* wake properly still
            // behaves; harmless where the waker does nothing.
            cx.waker().wake_by_ref();
            std::task::Poll::Pending
        }
    }
}
