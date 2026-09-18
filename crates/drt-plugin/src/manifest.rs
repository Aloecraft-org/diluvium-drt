//! What a plugin declares about itself, read before anything runs.
//!
//! # Surface
//!
//! Entry points:
//! - [`Manifest`] — the parsed file, every value resolved.
//! - [`Manifest::parse`] — bytes to a manifest, or a named refusal.
//! - [`Scope`] — which instance a plugin process belongs to.
//! - [`Transport`] — how its byte stream is obtained.
//! - [`Wiring`] — what scope a deployment may hand it.
//! - [`Overrides`] — the three limits a deployment may set over it.
//! - [`ManifestError`] — every way a manifest is refused.
//!
//! Configurable values:
//! - [`DEFAULT_MAX_INFLIGHT`] — calls one session may have outstanding.
//! - [`DEFAULT_CALL_TIMEOUT_MS`] — how long a call may take before the
//!   host stops waiting for it.
//! - [`DEFAULT_DIAL_BACK_TIMEOUT_MS`] — how long `spawn` waits to be
//!   greeted.
//!
//! Fan-out: [`Scope`] and [`Transport`] are closed sets, and each one's
//! refusal names every spelling it accepts.
//!
//! # Why the manifest is not the deployment's word
//!
//! A manifest ships with the plugin and is written by its publisher. A
//! `plugins` block is written by the operator. Everything here is
//! therefore a *declaration*: what this plugin is, what it can accept,
//! what it defaults to. Nothing here is a grant. The operator's block
//! supplies the scope, may narrow these limits, and decides whether an
//! inherited scope is handed over at all — because a grant asserted by
//! the thing being granted is not a grant, which is the same rule
//! `consent.json`'s ceiling already follows.
//!
//! So a manifest can say "I understand a rest-shaped scope" and it can
//! never say "I have one".

use serde::Deserialize;

/// Calls one session may have outstanding at once. One node may have
/// several, which is why this is per session and not per caller.
pub const DEFAULT_MAX_INFLIGHT: usize = 4;

/// How long a call may take before the host stops waiting. Thirty seconds
/// because the first real plugin loads web pages; a plugin that answers in
/// microseconds should say so in its own manifest rather than inherit a
/// ceiling written for the slow case.
pub const DEFAULT_CALL_TIMEOUT_MS: u64 = 30_000;

/// How long the `spawn` transport waits for a plugin to dial back. Ten
/// seconds, which was this budget's value when it was a constant in
/// `spawn.rs` with no way to change it -- generous for an interpreter
/// starting cold, and nowhere near enough for a plugin that loads a model
/// before saying hello. It is a field now so that plugin has a way to say
/// so, but dialing back first remains the better answer.
pub const DEFAULT_DIAL_BACK_TIMEOUT_MS: u64 = 10_000;

/// Which instance a plugin process belongs to (`doc/Plan-0.7.0.md` §7.1).
///
/// **The default is `node`, which inverts the C host's assumption.** There,
/// one plugin process served every caller, because that host had no nodes
/// to own anything. Here an implicitly shared process is an ambient
/// singleton, which is the thing the node and capability model exists to
/// avoid, so a plugin that says nothing gets an instance of its own per
/// calling node.
///
/// `root` stays available because some things genuinely are singletons —
/// it binds one port, it owns one device — and forcing those authors to
/// write a fronting node would be make-work. The cost is stated rather
/// than hidden: a `root` plugin serves several nodes and **cannot tell
/// them apart**, since caller identity stops at the host. Any per-caller
/// policy is the host's to enforce before the call, never the plugin's.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Scope {
    /// One process per calling node, dying with its owner.
    #[default]
    Node,
    /// One process for the whole root, outliving any one node.
    Root,
}

impl Scope {
    /// Every spelling this host accepts, for a refusal to quote.
    pub const SPELLINGS: &'static [&'static str] = &["node", "root"];
}

/// How this plugin's byte stream is obtained.
///
/// The kind lives here and the *destination* does not: where a service
/// listens is the deployment's fact, not the plugin's, so a `tcp` manifest
/// carries no address and cannot silently dial one
/// (`doc/Plugins.md` §4.3). `exec` is the exception and belongs here,
/// because it is the C host's manifest shape and the path is the
/// publisher's claim about its own program.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Transport {
    /// The host forks, execs and hands over one end of a socketpair.
    Process,
    /// The host starts the program and it dials back over loopback,
    /// proving itself with a secret the host minted. The native default:
    /// it owns the plugin's lifetime the way `process` does, and inherits
    /// nothing, which is what lets it run where there is no fd 3.
    Spawn,
    /// The host dials an address the deployment names.
    Tcp,
}

impl Transport {
    /// Every spelling this host accepts, for a refusal to quote.
    pub const SPELLINGS: &'static [&'static str] = &["process", "spawn", "tcp"];

    /// Whether this transport starts the program itself, and therefore
    /// needs `exec` to name one.
    pub fn starts_the_program(self) -> bool {
        matches!(self, Transport::Process | Transport::Spawn)
    }
}

/// What wiring scope a deployment may hand this plugin.
///
/// A wiring scope is a *place*: which domains, which paths, which
/// addresses. It is not the guest's capability, which the dispatcher
/// settles before a plugin is reached.
///
/// **A scope handed to a plugin is a promise, not an enforcement.** For a
/// builtin the host makes the request, so an allowlist binds the party
/// doing the work. A plugin makes its own requests, so the same allowlist
/// binds only a publisher who chose to honour it. That is what treating a
/// plugin as its own trust boundary means, and it is why the refusals here
/// say a plugin *was given* a scope and never that it cannot exceed one.
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
#[serde(tag = "takes", rename_all = "lowercase")]
pub enum Wiring {
    /// No scope at all, as the C host's plugins are.
    #[default]
    None,
    /// A scope of its own, whatever shape the deployment writes.
    Own,
    /// A scope shaped like a named family's, so a deployment that already
    /// bounded that family can hand the same bounds over instead of
    /// restating them. The declaration is the plugin saying it understands
    /// that shape; the handover is the operator's to write, and may narrow
    /// what it inherits and never widen it.
    Inherit {
        /// The family whose wiring scope this plugin understands.
        from: String,
    },
}

/// Every way a manifest is refused, each naming what was wrong.
#[derive(Debug, thiserror::Error, PartialEq)]
pub enum ManifestError {
    #[error("the plugin manifest is not JSON this host can read: {0}")]
    Malformed(String),

    #[error(
        "the plugin manifest declares scope '{found}', which this host does \
         not know; a plugin is scoped {spellings}"
    )]
    UnknownScope { found: String, spellings: String },

    #[error(
        "the plugin manifest declares transport '{found}', which this host \
         does not know; a plugin channel is obtained by {spellings}"
    )]
    UnknownTransport { found: String, spellings: String },

    #[error("the plugin manifest names no {0}")]
    Missing(&'static str),

    #[error(
        "the plugin manifest declares transport 'process' and no 'exec', so \
         this host has nothing to start"
    )]
    ProcessWithoutExec,

    #[error(
        "the plugin manifest's 'exec' is '{0}', which is not an absolute \
         path; a plugin is started by path and never by search"
    )]
    ExecNotAbsolute(String),

    #[error(
        "the plugin manifest sets {field} to 0, and a plugin that can {consequence} is not wired"
    )]
    ZeroLimit {
        field: &'static str,
        consequence: &'static str,
    },
}

// depth: the wire shape, kept separate from the resolved one so a
// defaulted value and a written one parse the same way and every unknown
// key is refused before anything reads the document.

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Wire {
    family: Option<String>,
    transport: Option<String>,
    exec: Option<String>,
    scope: Option<String>,
    max_inflight: Option<usize>,
    call_timeout_ms: Option<u64>,
    dial_back_timeout_ms: Option<u64>,
    #[serde(default)]
    wiring: Wiring,
}

/// A plugin's own declaration: the family it answers, how to reach it, and
/// the bounds it asks for.
#[derive(Debug, Clone, PartialEq)]
pub struct Manifest {
    /// The call namespace this plugin answers, `browser` in
    /// `browser/open`. One family per plugin.
    pub family: String,
    /// How its byte stream is obtained.
    pub transport: Transport,
    /// The absolute path to exec, for [`Transport::Process`] only.
    pub exec: Option<String>,
    /// Which instance its process belongs to.
    pub scope: Scope,
    /// Calls one session may have outstanding.
    pub max_inflight: usize,
    /// How long one call may take.
    pub call_timeout_ms: u64,
    /// How long [`Transport::Spawn`] waits for the plugin to dial back.
    ///
    /// Separate from `call_timeout_ms`, and much smaller, because it is a
    /// different wait: this one ends when the plugin says hello, and the
    /// plugin has done no work yet. A plugin whose real startup cost is
    /// loading a model should dial back *first* and load after, so the
    /// cost lands in the call budget where an operator can see it, rather
    /// than here where it looks like a plugin that failed to start.
    pub dial_back_timeout_ms: u64,
    /// What wiring scope a deployment may hand it.
    pub wiring: Wiring,
}

/// What a deployment may raise or lower over the manifest's own numbers.
///
/// Three limits and nothing else. They are the values whose right setting
/// depends on the machine the plugin runs on rather than on the plugin: a
/// model that loads in two seconds on a workstation takes a minute on a
/// small instance, and the publisher cannot know which one an operator
/// has. Everything else in a manifest -- the family, the transport, the
/// scope -- is the publisher describing their own program, and a
/// deployment that disagreed about those would be describing a different
/// program.
///
/// `None` keeps the manifest's value, so a deployment that overrides
/// nothing behaves exactly as it did before this existed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Overrides {
    pub max_inflight: Option<usize>,
    pub call_timeout_ms: Option<u64>,
    pub dial_back_timeout_ms: Option<u64>,
}

impl Manifest {
    /// Apply a deployment's overrides, refusing a zero exactly as
    /// [`Manifest::parse`] refuses one in the file.
    ///
    /// A zero is refused from either source because it means the same
    /// thing from either: a plugin that can answer no calls, finish none
    /// in time, or never be given time to dial back. An operator typing it
    /// hears the same sentence a publisher would.
    pub fn with_overrides(mut self, over: Overrides) -> Result<Self, ManifestError> {
        if let Some(n) = over.max_inflight {
            if n == 0 {
                return Err(ManifestError::ZeroLimit {
                    field: "max_inflight",
                    consequence: "answer no calls",
                });
            }
            self.max_inflight = n;
        }
        if let Some(ms) = over.call_timeout_ms {
            if ms == 0 {
                return Err(ManifestError::ZeroLimit {
                    field: "call_timeout_ms",
                    consequence: "never finish a call in time",
                });
            }
            self.call_timeout_ms = ms;
        }
        if let Some(ms) = over.dial_back_timeout_ms {
            if ms == 0 {
                return Err(ManifestError::ZeroLimit {
                    field: "dial_back_timeout_ms",
                    consequence: "never be given time to dial back",
                });
            }
            self.dial_back_timeout_ms = ms;
        }
        Ok(self)
    }

    /// Read a `<name>.plugin.json`, refusing by name.
    ///
    /// Every refusal happens here, before a process is started or an
    /// address is dialed, because a manifest is the one thing about a
    /// plugin that can be checked without running it.
    pub fn parse(bytes: &[u8]) -> Result<Self, ManifestError> {
        let wire: Wire =
            serde_json::from_slice(bytes).map_err(|e| ManifestError::Malformed(e.to_string()))?;

        let family = wire.family.ok_or(ManifestError::Missing("family"))?;
        if family.is_empty() {
            return Err(ManifestError::Missing("family"));
        }

        let transport = match wire.transport.as_deref() {
            None => return Err(ManifestError::Missing("transport")),
            Some("process") => Transport::Process,
            Some("spawn") => Transport::Spawn,
            Some("tcp") => Transport::Tcp,
            Some(found) => {
                return Err(ManifestError::UnknownTransport {
                    found: found.to_string(),
                    spellings: quoted(Transport::SPELLINGS),
                })
            }
        };

        // Absent is `node`, which is the inversion §7.1 argues for: a
        // plugin that says nothing about sharing does not get shared.
        let scope = match wire.scope.as_deref() {
            None | Some("node") => Scope::Node,
            Some("root") => Scope::Root,
            Some(found) => {
                return Err(ManifestError::UnknownScope {
                    found: found.to_string(),
                    spellings: quoted(Scope::SPELLINGS),
                })
            }
        };

        // Both transports that start the program need one to start, and
        // need it named absolutely: what a deployment wired is what runs,
        // and a `PATH` lookup would make that depend on the environment
        // DRT happened to start in.
        if transport.starts_the_program() {
            let exec = wire
                .exec
                .as_deref()
                .ok_or(ManifestError::ProcessWithoutExec)?;
            if !exec.starts_with('/') {
                return Err(ManifestError::ExecNotAbsolute(exec.to_string()));
            }
        }

        let max_inflight = wire.max_inflight.unwrap_or(DEFAULT_MAX_INFLIGHT);
        if max_inflight == 0 {
            return Err(ManifestError::ZeroLimit {
                field: "max_inflight",
                consequence: "answer no calls",
            });
        }
        let call_timeout_ms = wire.call_timeout_ms.unwrap_or(DEFAULT_CALL_TIMEOUT_MS);
        if call_timeout_ms == 0 {
            return Err(ManifestError::ZeroLimit {
                field: "call_timeout_ms",
                consequence: "never finish a call in time",
            });
        }

        let dial_back_timeout_ms = wire
            .dial_back_timeout_ms
            .unwrap_or(DEFAULT_DIAL_BACK_TIMEOUT_MS);
        if dial_back_timeout_ms == 0 {
            return Err(ManifestError::ZeroLimit {
                field: "dial_back_timeout_ms",
                consequence: "never be given time to dial back",
            });
        }

        Ok(Manifest {
            family,
            transport,
            exec: wire.exec,
            scope,
            max_inflight,
            call_timeout_ms,
            dial_back_timeout_ms,
            wiring: wire.wiring,
        })
    }
}

fn quoted(spellings: &[&str]) -> String {
    spellings
        .iter()
        .map(|s| format!("'{s}'"))
        .collect::<Vec<_>>()
        .join(" or ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(json: &str) -> Result<Manifest, ManifestError> {
        Manifest::parse(json.as_bytes())
    }

    /// The smallest manifest a plugin can ship, and what it means without
    /// saying it: an instance of its own per calling node.
    #[test]
    fn the_least_a_manifest_can_say_is_not_shared() {
        let m = parse(r#"{"family":"echo","transport":"tcp"}"#).unwrap();
        assert_eq!(m.family, "echo");
        assert_eq!(m.scope, Scope::Node);
        assert_eq!(m.transport, Transport::Tcp);
        assert_eq!(m.max_inflight, DEFAULT_MAX_INFLIGHT);
        assert_eq!(m.call_timeout_ms, DEFAULT_CALL_TIMEOUT_MS);
        assert_eq!(m.wiring, Wiring::None);
    }

    /// Sharing is something a plugin has to ask for in writing.
    #[test]
    fn root_scope_is_declared_and_never_inferred() {
        let m = parse(r#"{"family":"e","transport":"tcp","scope":"root"}"#).unwrap();
        assert_eq!(m.scope, Scope::Root);
    }

    /// Acceptance 10: a scope this host does not know is refused at load,
    /// naming what was written and every spelling that would have worked.
    #[test]
    fn an_unknown_scope_is_refused_with_both_spellings() {
        let err = parse(r#"{"family":"e","transport":"tcp","scope":"deployment"}"#).unwrap_err();
        let said = err.to_string();
        assert!(said.contains("deployment"), "{said}");
        assert!(said.contains("'node'"), "{said}");
        assert!(said.contains("'root'"), "{said}");
    }

    /// The same rule one field over, because a transport this host cannot
    /// obtain is a plugin that would hang rather than refuse.
    #[test]
    fn an_unknown_transport_is_refused_with_its_spellings() {
        let err = parse(r#"{"family":"e","transport":"carrier-pigeon"}"#).unwrap_err();
        let said = err.to_string();
        assert!(said.contains("carrier-pigeon"), "{said}");
        assert!(said.contains("'process'"), "{said}");
        assert!(said.contains("'tcp'"), "{said}");
    }

    /// A key this host does not know is a refusal, as it is in every
    /// config block since 0.6.1-rc.2. A misspelled limit that silently
    /// took its default is the failure that rule exists for.
    #[test]
    fn an_unknown_key_is_refused() {
        let err = parse(r#"{"family":"e","transport":"tcp","max_inflght":2}"#).unwrap_err();
        assert!(err.to_string().contains("max_inflght"), "{err}");
    }

    /// A process plugin is started by path, so a manifest with no path and
    /// one with a searchable name are both refused before anything forks.
    #[test]
    fn a_process_plugin_needs_an_absolute_path() {
        assert_eq!(
            parse(r#"{"family":"e","transport":"process"}"#).unwrap_err(),
            ManifestError::ProcessWithoutExec
        );
        let err = parse(r#"{"family":"e","transport":"process","exec":"plugin"}"#).unwrap_err();
        assert!(err.to_string().contains("plugin"), "{err}");
        assert!(err.to_string().contains("absolute"), "{err}");
    }

    /// A tcp manifest carries no address: where a service listens is the
    /// deployment's fact, so an address here would be a key nobody reads.
    #[test]
    fn a_tcp_manifest_cannot_name_an_address() {
        let err = parse(r#"{"family":"e","transport":"tcp","address":"127.0.0.1:9"}"#).unwrap_err();
        assert!(err.to_string().contains("address"), "{err}");
    }

    /// The plugin declares that it understands a family's scope shape. It
    /// does not thereby have one: the deployment decides that.
    #[test]
    fn a_plugin_can_declare_that_it_understands_an_inherited_scope() {
        let m = parse(
            r#"{"family":"fetch","transport":"tcp",
                "wiring":{"takes":"inherit","from":"rest"}}"#,
        )
        .unwrap();
        assert_eq!(
            m.wiring,
            Wiring::Inherit {
                from: "rest".into()
            }
        );
    }

    /// A limit of zero is a plugin that is wired and cannot work. Better
    /// refused at load than discovered on the first call.
    #[test]
    fn a_zero_limit_is_refused_rather_than_treated_as_absent() {
        for json in [
            r#"{"family":"e","transport":"tcp","max_inflight":0}"#,
            r#"{"family":"e","transport":"tcp","call_timeout_ms":0}"#,
        ] {
            assert!(parse(json).is_err(), "{json}");
        }
    }

    /// A manifest that names no family answers nothing, and the refusal
    /// says which word is missing rather than that parsing failed.
    #[test]
    fn a_manifest_without_a_family_is_refused_by_the_missing_word() {
        assert_eq!(
            parse(r#"{"transport":"tcp"}"#).unwrap_err(),
            ManifestError::Missing("family")
        );
    }
}
