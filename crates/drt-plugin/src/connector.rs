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
//! # Root scope only, so far
//!
//! `manifest::Scope` has two values and this file serves one. A `root`
//! plugin is one process for the whole root, which is the case that needs
//! no instance table: there is one key, so there is one instance.
//!
//! A `node` manifest is **refused by name** rather than served as if it
//! were root. That is the whole reason the refusal exists: `node` is the
//! default, it means an instance per calling node, and quietly sharing one
//! process between callers would hand a deployment the ambient singleton
//! the node model exists to prevent -- silently, and exactly when the
//! manifest asked for the opposite. `doc/Plan-0.7.0.md` §7.3 is the
//! segment that adds the table, and until it lands the honest answer is a
//! sentence saying so.

use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use drt_caps::Scope;
use drt_connector::{CallError, CallResult, Connector};

use crate::channel::{Channel, ChannelError};
use crate::frame::{ErrorClass, Reply, ReplyBody};
use crate::manifest::{Manifest, Scope as PluginScope, Transport};
use crate::session::{Session, SessionError};

/// A plugin family, served by one process this host starts on first use.
pub struct PluginConnector {
    manifest: Manifest,
    /// The running plugin, or nothing yet. One, because this serves `root`
    /// scope only; the table that makes it many is its own segment.
    instance: Mutex<Option<Session<Box<dyn Channel + Send>>>>,
}

impl PluginConnector {
    /// Take a manifest and start nothing.
    ///
    /// Starting is deferred to the first call on purpose: a deployment
    /// that wires six plugins and uses two should pay for two. What is
    /// *not* deferred is the refusal -- a manifest this host cannot serve
    /// is rejected here, at load, rather than at 3am on the first call.
    pub fn new(manifest: Manifest) -> Result<Self, String> {
        if manifest.scope != PluginScope::Root {
            return Err(format!(
                "the plugin '{}' declares `node` scope, an instance per calling node, \
                 and this host serves `root`-scope plugins only so far; it is refused \
                 rather than shared between callers, which is what serving it as `root` \
                 would silently do",
                manifest.family
            ));
        }
        if manifest.transport.starts_the_program() && manifest.exec.is_none() {
            return Err(format!(
                "the plugin '{}' names no `exec` to start",
                manifest.family
            ));
        }
        Ok(Self {
            manifest,
            instance: Mutex::new(None),
        })
    }

    /// The family this connector answers, which is what it is wired under.
    pub fn family(&self) -> &str {
        &self.manifest.family
    }

    /// The manifest, for a caller that reports what is wired.
    pub fn manifest(&self) -> &Manifest {
        &self.manifest
    }

    /// Whether the plugin has been started yet.
    fn started(&self) -> bool {
        self.instance.lock().map(|h| h.is_some()).unwrap_or(false)
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
            .field("started", &self.started())
            .finish()
    }
}

#[async_trait::async_trait]
impl Connector for PluginConnector {
    /// Answer one call.
    ///
    /// This and not `call_as`, deliberately: a `root`-scope plugin is one
    /// process for every caller and *cannot tell them apart*, which the
    /// manifest says in those words. Implementing the method that is not
    /// given the asker is that fact in the type system. The segment that
    /// adds `node` scope overrides `call_as` instead, and keys the
    /// instance table by `asker.caller`.
    async fn call(
        &self,
        call: &str,
        args: Option<rmpv::Value>,
        _scope: Option<&Scope>,
    ) -> CallResult {
        // Start on first use, and queue the call. Both happen under one
        // lock and neither yields while holding it.
        let id = {
            let mut held = self.instance.lock().map_err(|_| poisoned(self.family()))?;
            if held.is_none() {
                *held = Some(self.start()?);
            }
            let session = held.as_mut().expect("just started");
            session.begin(call, args).map_err(|e| self.said(e))?
        };

        let deadline = Instant::now() + Duration::from_millis(self.manifest.call_timeout_ms);
        loop {
            {
                let mut held = self.instance.lock().map_err(|_| poisoned(self.family()))?;
                let session = held.as_mut().expect("started above");
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
