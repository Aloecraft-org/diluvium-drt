//! The config schema (SPEC.md §5). These serde types are the **source of
//! truth**: LuaCATS defs for editor support are *generated* from them (a
//! build-tool seam, not yet built), never authored by hand.
//!
//! The keystone: **one config shape at every depth**. An instance takes the
//! same configuration whether it is the root or ten generations deep; the
//! host is simply the root's parent, so host-config and spawn-request are the
//! same serde object ([`InstanceConfig`]), and attenuation is the only rule.
//!
//! The root config is a property of the OS process — file + flags + env
//! merged into one [`RootConfig`]. Merging lives with the `drt` binary; the
//! shape lives here. The on-disk format is deliberately not fixed by this
//! crate: everything is plain serde, so msgpack (the tests), JSON, TOML, or a
//! `.dlua` surface all read into the same object.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use drt_caps::{AttenuationError, Grant};

/// Quantitative limits. `None` means "no limit stated", which under
/// attenuation means "inherit the parent's" — a child may state a smaller
/// number, never a larger one.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Budget {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instructions: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_kb: Option<u64>,
}

/// Where a program's source comes from. Config never carries the
/// application's own filenames as *scopes* — this is the program itself.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Program {
    /// A `.dlua`/`.lua` file, resolved against the process working directory
    /// for the root; spawn requests carry source, not paths.
    Path(PathBuf),
    /// Inline source text.
    Source(String),
}

/// The one config shape: host-config and spawn-request are this same object.
/// A child's config must fit inside its parent's —
/// [`InstanceConfig::check_attenuation`] is that rule, checked identically
/// whether the parent is the process or another instance.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct InstanceConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub program: Option<Program>,
    /// The capability grants: `effect × capability × scope` each.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub caps: Vec<Grant>,
    #[serde(default, skip_serializing_if = "Budget::is_unlimited")]
    pub budget: Budget,
}

impl Budget {
    fn is_unlimited(&self) -> bool {
        *self == Budget::default()
    }

    /// A child budget fits when every bound it states is no looser than the
    /// parent's; an unstated bound inherits the parent's ceiling, which fits
    /// by being equal to it.
    pub fn fits_within(&self, parent: &Budget) -> bool {
        fn fits(child: Option<u64>, parent: Option<u64>) -> bool {
            match (child, parent) {
                (_, None) | (None, _) => true,
                (Some(c), Some(p)) => c <= p,
            }
        }
        fits(self.instructions, parent.instructions) && fits(self.memory_kb, parent.memory_kb)
    }

    /// Resolve unstated bounds to the parent's — what enforcement runs on.
    pub fn resolved_against(&self, parent: &Budget) -> Budget {
        Budget {
            instructions: self.instructions.or(parent.instructions),
            memory_kb: self.memory_kb.or(parent.memory_kb),
        }
    }
}

/// Why a child config does not fit inside its parent's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigError {
    Caps(AttenuationError),
    /// The child states a budget looser than the parent's ceiling.
    BudgetExceedsParent,
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConfigError::Caps(e) => e.fmt(f),
            ConfigError::BudgetExceedsParent => {
                f.write_str("the budget exceeds the parent's; a budget may only narrow")
            }
        }
    }
}

impl std::error::Error for ConfigError {}

impl InstanceConfig {
    /// Attenuation, the only rule in the system: every grant covered by the
    /// parent set, every parent deny kept, budget no looser. The caps check
    /// is [`drt_caps::CapSet::attenuate`]'s; this wrapper exists so a spawn
    /// request is validated as one object.
    pub fn check_attenuation(&self, parent: &InstanceConfig) -> Result<(), ConfigError> {
        let parent_set = drt_caps::CapSet::root(parent.caps.clone());
        parent_set
            .attenuate(
                drt_caps::Principal("attenuation-check".into()),
                self.caps.clone(),
            )
            .map_err(ConfigError::Caps)?;
        if !self.budget.fits_within(&parent.budget) {
            return Err(ConfigError::BudgetExceedsParent);
        }
        Ok(())
    }
}

/// How one connector is wired for this process: which backing this build
/// resolves the name to, and the *scope* the host grants it — a place (a
/// directory for `fs`, a directory for `sql`, a key), never the
/// application's filenames. Programs name resources within the scope.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ConnectorWiring {
    /// Names a registered backing when a build carries more than one
    /// (real vs mock, native vs browser). Default: the registry's default
    /// backing for the connector's name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backing: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<drt_caps::Scope>,
}

/// A listener: a network surface published on purpose (GUARANTEES.md). The
/// `http` scheme is `dhost_http.c`'s contract — a queue bridge, where
/// requests land on a named root queue and replies drain from another —
/// with the same field names and the same defaults, so a deployment moves
/// between the C host and DRT by moving its config.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Listener {
    /// `http` today; `ssh` lands with the control endpoint. Non-local
    /// schemes resolve through ego-transport.
    pub scheme: String,
    /// e.g. `127.0.0.1:8080`. The C defaults its bind to the loopback —
    /// the LB's side — and so should configs here: the edge terminates
    /// TLS and sets the trusted headers, and a listener facing the world
    /// directly is a deliberate act, not a default.
    pub address: String,
    /// Requests land here, on the root program.
    #[serde(default = "default_request_queue")]
    pub queue: String,
    /// Responses drain from here. Two listeners may share one.
    #[serde(default = "default_reply_queue")]
    pub reply_queue: String,
    /// Refuse bigger request bodies (413).
    #[serde(default = "default_max_body")]
    pub max_body: usize,
    /// The host-side timeout, per connection: a program that has not
    /// answered by then gets its connection a 504 and the late reply is
    /// consumed without a reader. `deadline_ms` is the C host's spelling,
    /// accepted so a `.host.lua` maps without a rename.
    #[serde(default = "default_conn_deadline_ms", alias = "deadline_ms")]
    pub conn_deadline_ms: u64,
    #[serde(default = "default_max_conns")]
    pub max_conns: usize,
    /// The request-header allowlist, lowercased: a header the deployment
    /// does not name never reaches the program. The bound is the C's
    /// `DH_MAX_HDRS` (16) per direction.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub headers: Vec<String>,
    /// The response-header allowlist: a name a guest reply uses that is
    /// not here is dropped whole — never truncated, never cleaned.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub resp_headers: Vec<String>,
}

fn default_request_queue() -> String {
    "http_in".into()
}
fn default_reply_queue() -> String {
    "http_out".into()
}
fn default_max_body() -> usize {
    65536
}
fn default_conn_deadline_ms() -> u64 {
    10_000
}
fn default_max_conns() -> usize {
    64
}

/// The host-side residency policy (`doc/Hibernate.md` §9.1.2: the policy
/// belongs to the host, never the swarm — the swarm's table bounds how many
/// instances *exist*, resident or cached alike). With a budget set, `drt
/// start` hibernates the least-recently-active instances past it; a
/// deployment that states none keeps everything resident, bounded by the
/// instance table.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Residency {
    /// How many non-root instances may be resident at once. The root is
    /// exempt: it holds the request queues, and a deployment whose front
    /// door hibernates is not saving memory, it is closed.
    pub max_resident: usize,
}

/// Process identity. The host key doubles as the node identity and the
/// snapshot stamp source (SPEC.md §§8–9).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Identity {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_key_path: Option<PathBuf>,
}

/// Authorized keys → capability grant sets: an SSH principal is an attenuated
/// node in the provenance tree.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SshPrincipal {
    /// The public key, OpenSSH one-line format.
    pub key: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub caps: Vec<Grant>,
}

/// The root object the process merges file + flags + env into. The root
/// *instance* config is embedded flat — the same shape at depth zero — and
/// the process-level rest is what only the OS process can own: connector
/// wiring, listeners, identity, principals.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct RootConfig {
    #[serde(flatten)]
    pub root: InstanceConfig,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub connectors: BTreeMap<String, ConnectorWiring>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub listeners: Vec<Listener>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub residency: Option<Residency>,
    #[serde(default, skip_serializing_if = "Identity::is_default")]
    pub identity: Identity,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub principals: Vec<SshPrincipal>,
}

impl Identity {
    fn is_default(&self) -> bool {
        *self == Identity::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use drt_caps::Grant;

    fn cfg(caps: Vec<Grant>, budget: Budget) -> InstanceConfig {
        InstanceConfig {
            program: None,
            caps,
            budget,
        }
    }

    #[test]
    fn host_config_and_spawn_request_are_one_shape() {
        // The root's instance config round-trips as msgpack and re-reads as a
        // spawn request unchanged: literally the same serde object.
        let root = RootConfig {
            root: cfg(
                vec![Grant::grant("host:fs/*")],
                Budget {
                    instructions: Some(1_000_000),
                    memory_kb: Some(4096),
                },
            ),
            ..RootConfig::default()
        };
        let bytes = rmp_serde::to_vec_named(&root.root).unwrap();
        let spawn: InstanceConfig = rmp_serde::from_slice(&bytes).unwrap();
        assert_eq!(spawn, root.root);
    }

    #[test]
    fn attenuation_checks_caps_and_budget_together() {
        let parent = cfg(
            vec![Grant::grant("host:fs/*")],
            Budget {
                instructions: Some(1000),
                memory_kb: None,
            },
        );
        let ok = cfg(
            vec![Grant::grant("host:fs/read")],
            Budget {
                instructions: Some(500),
                memory_kb: Some(64),
            },
        );
        assert_eq!(ok.check_attenuation(&parent), Ok(()));

        let wide_caps = cfg(vec![Grant::grant("host:exec")], Budget::default());
        assert!(matches!(
            wide_caps.check_attenuation(&parent),
            Err(ConfigError::Caps(_))
        ));

        let wide_budget = cfg(
            vec![Grant::grant("host:fs/read")],
            Budget {
                instructions: Some(2000),
                memory_kb: None,
            },
        );
        assert_eq!(
            wide_budget.check_attenuation(&parent),
            Err(ConfigError::BudgetExceedsParent)
        );

        // An unstated child bound inherits the parent's ceiling: it fits, and
        // resolution pins it to the number enforcement will use.
        let unstated = cfg(vec![], Budget::default());
        assert_eq!(unstated.check_attenuation(&parent), Ok(()));
        assert_eq!(
            unstated
                .budget
                .resolved_against(&parent.budget)
                .instructions,
            Some(1000)
        );
    }

    #[test]
    fn root_config_defaults_are_empty_not_permissive() {
        let root = RootConfig::default();
        assert!(root.root.caps.is_empty());
        assert!(root.connectors.is_empty());
        // Locked out of the box: an empty config grants nothing.
        let set = drt_caps::CapSet::root(root.root.caps.clone());
        assert!(!set.holds("host:time"));
    }
}
