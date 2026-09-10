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

/// How much numeric work an instance may do, and how exactly it must be
/// done (`doc/Plan-2026-09.md` §3.4, the numeric spec §4).
///
/// Bounded per instance and attenuated at spawn for the same reason
/// [`Budget`] is: a kernel's cost is not the guest's instruction count, and
/// a child that could raise its own ceiling has no ceiling. `None` on
/// either field means "no bound stated", which under attenuation means
/// "inherit the parent's".
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Numeric {
    /// The most elements one kernel call may process. `None` is no bound
    /// stated, which under attenuation means "inherit the parent's".
    ///
    /// **Never `Some(0)`.** `dv_numeric_set_max_elements` reads `0` as *no
    /// limit*, so a config writing `max_elements = 0` and meaning "forbid"
    /// would get "unbounded" -- a bound failing in the one direction a
    /// bound must never fail in. Rather than carry a value whose meaning
    /// inverts at the boundary, zero is refused where it can be written:
    /// [`Numeric::check_representable`] is that check, and the loader and
    /// the spawn path both run it. An unstated bound is the only way to
    /// mean unlimited, and it is the only thing that reaches the core as
    /// `0`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_elements: Option<u64>,
    /// The loosest determinism tier a kernel may run at.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tier: Option<Tier>,
}

/// What a kernel implementation promises about its results, from the
/// numeric spec §4. The order is looseness, and it is what attenuation
/// compares: `exact` promises most, `fast` promises nothing across targets.
///
/// - `exact`: integer, NTT, decQuad. Bit-identical by construction on every
///   target.
/// - `reproducible`: bit-identical to the portable kernel on every target.
/// - `fast`: no cross-target guarantee.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Tier {
    Exact,
    Reproducible,
    Fast,
}

impl Tier {
    /// How loose this tier is. Higher permits more, so a child's rank may
    /// not exceed its parent's -- the same shape as a budget, where a
    /// child's number may not exceed its parent's.
    pub fn rank(self) -> u8 {
        match self {
            Tier::Exact => 0,
            Tier::Reproducible => 1,
            Tier::Fast => 2,
        }
    }

    /// The name this tier carries in a config and in a report.
    pub fn name(self) -> &'static str {
        match self {
            Tier::Exact => "exact",
            Tier::Reproducible => "reproducible",
            Tier::Fast => "fast",
        }
    }

    pub fn parse(name: &str) -> Option<Tier> {
        match name {
            "exact" => Some(Tier::Exact),
            "reproducible" => Some(Tier::Reproducible),
            "fast" => Some(Tier::Fast),
            _ => None,
        }
    }
}

impl Numeric {
    pub fn is_unbounded(&self) -> bool {
        *self == Numeric::default()
    }

    /// Why these bounds cannot be represented, if they cannot.
    ///
    /// One case, and see [`Numeric::max_elements`] for why it is a refusal
    /// rather than a translation: zero means "no limit" at the ABI and
    /// "none allowed" to anyone reading the config, and the two cannot both
    /// be served.
    pub fn check_representable(&self) -> Result<(), &'static str> {
        if self.max_elements == Some(0) {
            return Err(
                "numeric.max_elements may not be 0: the core reads 0 as \"no limit\", which is \
                 the opposite of what writing it here would mean. Omit the field for no limit, \
                 or state the number of elements you mean to allow",
            );
        }
        Ok(())
    }

    /// A child's numeric bounds fit when neither is looser than its
    /// parent's. Unstated inherits, and inheriting fits by being equal.
    pub fn fits_within(&self, parent: &Numeric) -> bool {
        let elements = match (self.max_elements, parent.max_elements) {
            (_, None) | (None, _) => true,
            (Some(c), Some(p)) => c <= p,
        };
        let tier = match (self.max_tier, parent.max_tier) {
            (_, None) | (None, _) => true,
            (Some(c), Some(p)) => c.rank() <= p.rank(),
        };
        elements && tier
    }

    /// Resolve unstated bounds to the parent's -- what enforcement runs on.
    pub fn resolved_against(&self, parent: &Numeric) -> Numeric {
        Numeric {
            max_elements: self.max_elements.or(parent.max_elements),
            max_tier: self.max_tier.or(parent.max_tier),
        }
    }
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
    /// How much numeric work this instance may do. Beside `budget` rather
    /// than inside it: the two bound different things -- instructions the
    /// guest executes, and elements a kernel processes on its behalf -- and
    /// a kernel charges the instruction budget too (§3.4).
    #[serde(default, skip_serializing_if = "Numeric::is_unbounded")]
    pub numeric: Numeric,
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
    /// The child states a larger `max_elements` than the parent's.
    NumericElementsExceedParent,
    /// The child states numeric bounds that cannot be represented at the
    /// ABI; the string says which and why.
    NumericUnrepresentable(&'static str),
    /// The child states a looser `max_tier` than the parent's -- asking to
    /// be allowed results its parent would not accept.
    NumericTierExceedsParent,
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConfigError::Caps(e) => e.fmt(f),
            ConfigError::BudgetExceedsParent => {
                f.write_str("the budget exceeds the parent's; a budget may only narrow")
            }
            ConfigError::NumericElementsExceedParent => f.write_str(
                "numeric.max_elements exceeds the parent's; a child may state a smaller \
                 element bound than its parent's, never a larger",
            ),
            ConfigError::NumericUnrepresentable(why) => f.write_str(why),
            ConfigError::NumericTierExceedsParent => f.write_str(
                "numeric.max_tier is looser than the parent's; a child may state a stricter \
                 tier than its parent's, never a looser one",
            ),
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
        self.numeric
            .check_representable()
            .map_err(ConfigError::NumericUnrepresentable)?;
        // Checked one bound at a time so the refusal names which one moved:
        // "your numeric block is wrong" is not an answer anyone can act on.
        let elements_only = Numeric {
            max_elements: self.numeric.max_elements,
            max_tier: None,
        };
        if !elements_only.fits_within(&parent.numeric) {
            return Err(ConfigError::NumericElementsExceedParent);
        }
        if !self.numeric.fits_within(&parent.numeric) {
            return Err(ConfigError::NumericTierExceedsParent);
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
    /// How long a request waits for `queue` to *exist* before the host
    /// answers 503 on the program's behalf.
    ///
    /// A listener accepts from the moment the process binds it, which is
    /// before the program has run a line — so a request can arrive for a
    /// queue the program has not declared yet. Answering that at once is
    /// a definitive answer to a question the deployment has not finished
    /// hearing, and it lands hardest on a caller that handshakes exactly
    /// once at startup: it takes the refusal as the answer and never asks
    /// again (issue #11).
    ///
    /// So the request waits, and the wait is bounded rather than open:
    /// past this, a program that has declared nothing is a program that
    /// declares nothing, and 503 is the truth. `0` restores the immediate
    /// refusal. A value at or past `conn_deadline_ms` cannot be reached —
    /// the connection's own deadline answers 504 first.
    #[serde(default = "default_admit_grace_ms")]
    pub admit_timeout_ms: u64,
    /// The request-header allowlist, lowercased: a header the deployment
    /// does not name never reaches the program. The bound is the C's
    /// `DH_MAX_HDRS` (16) per direction.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub headers: Vec<String>,
    /// The response-header allowlist: a name a guest reply uses that is
    /// not here is dropped whole — never truncated, never cleaned.
    /// `response_headers` is the C host's config spelling.
    #[serde(
        default,
        skip_serializing_if = "Vec::is_empty",
        alias = "response_headers"
    )]
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
/// Two seconds: long enough to cover a program's declare-before-serve
/// boot (measured in tens of milliseconds on a real deployment), short
/// enough that a deployment which never declares the queue is still told
/// so while an operator is watching. Deliberately its own number rather
/// than `RelayConfig`'s equal one: the two answer different questions and
/// should be free to move apart.
fn default_admit_grace_ms() -> u64 {
    2000
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

/// The rendezvous relay (`drt relay`): parked WSS legs paired by label and
/// spliced. Keys are **per label**, not global, because tickets later
/// replace the key *values* and not this shape — and because per-label
/// revocation is what a leaked device key needs.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RelayConfig {
    /// e.g. `127.0.0.1:8090` — behind the edge, which terminates TLS and
    /// routes `<label>--tunnel.<zone>` here.
    pub bind: String,
    /// Label → its two keys. A label absent here has no route at all, and
    /// an empty key is a closed door: the relay carries bytes to someone's
    /// sshd, so absence must fail rather than open.
    #[serde(default)]
    pub labels: BTreeMap<String, RelayLabel>,
    /// Where the relay's control events land on the root program, when the
    /// relay runs inside a deployment (`drt start`). Presence, session
    /// open/close, and the byte counts a meter needs all arrive here as
    /// ordinary queue messages — the same bridge the http listener uses,
    /// so the supervisor learns about tunnels without new mechanism.
    #[serde(default = "default_relay_queue")]
    pub queue: String,
    /// Where the relay reads *answers*, which is what turns observation
    /// into arbitration.
    ///
    /// **Empty means no arbitration**, and that is the default: the static
    /// per-label key is the only gate, exactly as it is today. Naming a
    /// reply queue is how a deployment opts in to being asked — and once
    /// it has, a question it does not answer within `admit_timeout_ms` is
    /// a refusal, because absence must fail for the same reason an empty
    /// key does. Opting in is therefore also opting into answering.
    #[serde(default)]
    pub reply_queue: String,
    /// How long the relay waits for an admit answer before refusing.
    #[serde(default = "default_admit_timeout_ms")]
    pub admit_timeout_ms: u64,
}

/// The STUN binding server, as `drt start` and `drt stun` run it.
///
/// DRT's own, like `relay`: the C host has no STUN, so this is an
/// extension rather than a dialect match. It exists for the same reason
/// the relay does — a fetchpoint behind CGNAT has to learn its own
/// reflexive address from somewhere, and depending on a third party's
/// STUN service to find out puts a stranger on the path of your own NAT
/// traversal.
///
/// Two servers are the useful deployment, not one: `stun.detect_mapping`
/// classifies a NAT by asking two or more servers about the *same* socket
/// and comparing what they saw, and refuses with `NotEnoughServers`
/// below two. A `stun1`/`stun2` pair on separate addresses is what makes
/// that classification available to anyone pointed at them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StunConfig {
    /// e.g. `0.0.0.0:3478`, or a bare host paired with `port`. Unlike the
    /// relay and the http listener, a STUN server is *meant* to face the
    /// world — its whole job is reporting the address the world sees — so
    /// there is no loopback default to inherit here. The config says where.
    pub bind: String,
    /// Where the server's counters land on the root program when it runs
    /// inside a deployment (`drt start`), as ordinary queue messages on
    /// the same bridge the relay's control plane uses.
    #[serde(default = "default_stun_queue")]
    pub queue: String,
    /// How often to report. A binding server is stateless and has no
    /// events to speak of — only counters — so it reports on a timer
    /// rather than per datagram: a busy server would otherwise spend the
    /// deployment's queue on telemetry about itself.
    #[serde(default = "default_stun_report_ms")]
    pub report_ms: u64,
}

fn default_stun_queue() -> String {
    "stun_in".into()
}
/// Ten seconds: often enough for a health panel, rare enough that the
/// report is never the busiest thing on the queue.
fn default_stun_report_ms() -> u64 {
    10_000
}

/// The TURN relay, as `drt start` and `drt turn` run it: the last rung of
/// the traversal ladder, for the peers `stun` says cannot punch.
///
/// DRT's own, like `stun`, and for the same reason: a fetchpoint whose
/// mapping changes per destination has no direct path, and the relay
/// that carries it should be one the deployment runs rather than a
/// stranger's. Credentials are coturn's `use-auth-secret` scheme byte
/// for byte -- `<expiry>:<principal>`, and a password that is the
/// HMAC-SHA1 of that under a shared secret -- which is what
/// `crypto/turn_credential` mints under `connectors.crypto.turn`. The
/// same secret in both blocks is the whole deployment, and a credential
/// minted here verifies against coturn too, so either can stand behind
/// the same `--ice` answer (issue #12).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnConfig {
    /// e.g. `0.0.0.0:3478`, or a bare host paired with `port`. Faces the
    /// world, as `stun` does, so there is no loopback default: the config
    /// says where.
    pub bind: String,
    /// The address peers are told to send relayed traffic to -- on a host
    /// behind a NAT, its public address, which is not the bound one.
    /// Defaults to `bind`'s address when that names a specific one, and
    /// must be given when `bind` is a wildcard: a relay handing out an
    /// address nobody can reach fails after authentication, which is the
    /// worst place to fail.
    #[serde(default)]
    pub relay_address: String,
    /// The local address the relay sockets themselves bind to.
    #[serde(default = "default_turn_relay_bind")]
    pub relay_bind: String,
    /// The authentication realm, echoed to clients.
    #[serde(default = "default_turn_realm")]
    pub realm: String,
    /// The shared secret, inline. The same three knobs as
    /// `connectors.crypto.turn`, resolved in the same order (file, then
    /// env, then inline), because it is the same secret; one of the three
    /// is required, and none is an open relay, which refuses to bind.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    /// The shared secret, from a file; one trailing newline is trimmed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key_file: Option<PathBuf>,
    /// The shared secret, from the named environment variable, read once
    /// at startup; unset is a refusal by name there.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key_env: Option<String>,
    /// Hard cap on simultaneous allocations. Past it a request is refused
    /// and counted, never queued: a relay is a buffering machine, and this
    /// is its bound.
    #[serde(default = "default_turn_max_allocations")]
    pub max_allocations: usize,
    /// Where the server's reports land on the root program inside
    /// `drt start`: a counter snapshot on the timer below, and one
    /// `turn_closed` per allocation as it closes, with the principal and
    /// the bytes it relayed -- the message a meter reads.
    #[serde(default = "default_turn_queue")]
    pub queue: String,
    /// How often the counters are reported. Closes are reported as they
    /// happen whatever this says, because a short allocation that opened
    /// and closed between two snapshots would otherwise take its byte
    /// count with it.
    #[serde(default = "default_turn_report_ms")]
    pub report_ms: u64,
}

fn default_turn_relay_bind() -> String {
    "0.0.0.0".into()
}
fn default_turn_realm() -> String {
    "drt".into()
}
/// ego-transport's own default, and coturn-scale for one box.
fn default_turn_max_allocations() -> usize {
    256
}
fn default_turn_queue() -> String {
    "turn_in".into()
}
fn default_turn_report_ms() -> u64 {
    10_000
}

/// A WireGuard peer, as the `wireguard` block names it.
///
/// The fields are `wg-quick`'s, deliberately: an operator who has written a
/// `[Peer]` stanza should be able to write this without learning anything,
/// and a key pasted from one should work in the other. Keys are base64, the
/// tool's own encoding, not hex.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WireguardPeer {
    /// The peer's public key, base64. Its identity: there is no other name
    /// for a peer in this protocol.
    pub public_key: String,
    /// What may be routed to and accepted from this peer, as CIDR. Both
    /// directions at once, which is WireGuard's cryptokey routing: a packet
    /// arriving from this peer with a source outside these networks is
    /// dropped, not merely unroutable.
    #[serde(default)]
    pub allowed_ips: Vec<String>,
    /// Where to send to, before the peer has been heard from. Optional
    /// because a punched peer has no endpoint until the rendezvous supplies
    /// one, which is what the `endpoint` command on the reply queue is for.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
    /// Seconds between keepalives, or none. This is the field a punched
    /// hole depends on: a NAT mapping with no traffic through it closes in
    /// tens of seconds, and 25 is the interval the tooling settled on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keepalive: Option<u16>,
    /// An optional symmetric key mixed into the handshake, base64, from an
    /// environment variable. Post-quantum belt and braces, and per peer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preshared_key_env: Option<String>,
}

/// Which stack terminates the tunnel's IP side.
///
/// `kernel`: an interface this process creates, which needs
/// `CAP_NET_ADMIN` (root on macOS, `wintun.dll` on Windows) and is reached
/// by address like any other. `userspace`: a TCP/IP stack inside this
/// process, needing no privilege at all and reached only through
/// [`WireguardConfig::forward`] and [`WireguardConfig::expose`]. Explicit
/// and never inferred from a missing privilege, by this repository's rule
/// that a config which did not ask is not steered.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WireguardMode {
    #[default]
    Kernel,
    Userspace,
}

impl WireguardMode {
    /// The name as the config spells it.
    pub fn as_str(self) -> &'static str {
        match self {
            WireguardMode::Kernel => "kernel",
            WireguardMode::Userspace => "userspace",
        }
    }

    /// The config's spelling, or `None` for anything else -- the `.host.lua`
    /// loader's half of the same two names serde reads from JSON.
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "kernel" => Some(WireguardMode::Kernel),
            "userspace" => Some(WireguardMode::Userspace),
            _ => None,
        }
    }
}

/// A local port that reaches an address inside the tunnel, in
/// `mode = "userspace"`: `ssh -p 2222 127.0.0.1` with nothing configured on
/// the client. The caller half; `tunnel`'s `bind` is the same shape.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WireguardForward {
    /// The local `ip:port` to listen on. Port `0` takes an ephemeral one,
    /// and the `wireguard_forward` report is how a program learns which.
    pub bind: String,
    /// The `ip:port` inside the tunnel that each accepted connection dials,
    /// from the stack's own address. An address, not a name: there is no
    /// resolver inside a tunnel.
    pub to: String,
}

/// An address inside the tunnel that reaches a local port, in
/// `mode = "userspace"`: what a peer dials to reach this machine's sshd.
/// The device half; without it nothing listens inside a userspace stack,
/// where in kernel mode the kernel would deliver to the box's own sshd.
/// `tunnel --park --to`'s shape: dialed lazily, on the first connection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WireguardExpose {
    /// The `ip:port` a peer dials. The address is the block's own
    /// `address`, because the stack answers on that and nothing else.
    pub tunnel: String,
    /// The local `host:port` each inbound connection is dialed to,
    /// `127.0.0.1:22` in front of an sshd.
    pub to: String,
}

/// The WireGuard peer this deployment is, as `drt start` and `drt wg` run
/// it.
///
/// DRT's own, like `relay`, `stun` and `turn`, and the rung above all
/// three: `netcheck` says whether a direct path can exist, `stun` measures
/// the mapping that decides it, `turn` carries what cannot be punched --
/// and this is what runs *over* the path once there is one. In the process,
/// on gotatun, so a deployment that has a fetchpoint's address gets an
/// encrypted link to it without a kernel module, `wg-quick`, or root on
/// anything but the interface.
///
/// The connection to hole punching is the whole point of the block's shape:
/// `listen_port` is the port `netcheck --udp-port` measures, peers may
/// start with no endpoint at all, and the endpoint is settable at runtime
/// over `reply_queue`. So the rendezvous a program already runs over the
/// relay -- trade the two measured endpoints, tell both sides -- completes
/// here without a second daemon holding the socket.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WireguardConfig {
    /// The UDP port to listen on. **Required, and never zero**: gotatun
    /// reports the port it was *configured* with rather than the one it
    /// bound, so an ephemeral port could not be read back and named to a
    /// peer; and a NAT mapping is measured per port, so a peer that wants
    /// to be punched to must own a stable one. 51820 is the convention.
    #[serde(default = "default_wireguard_port")]
    pub listen_port: u16,
    /// The address to put on the interface, as CIDR -- `10.9.0.1/24`.
    /// Without one the interface comes up with no address and nothing can
    /// use it, which is the `ip addr add` this block exists to avoid.
    ///
    /// One address, because that is what the tun layer takes; a second
    /// (an IPv6 address beside an IPv4 one, say) is still `ip addr add`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub address: Option<String>,
    /// The interface MTU. 1420 by default, not 1500: WireGuard's own
    /// overhead is 60 bytes over IPv4 and 80 over IPv6, and an interface
    /// left at the ethernet default fragments every full-size packet.
    #[serde(default = "default_wireguard_mtu")]
    pub mtu: u16,
    /// Whether this device may be asked, at run time, to send through a
    /// TURN allocation — the fallback for a NAT `stun` says cannot be
    /// punched.
    ///
    /// Off by default, and the cost of turning it on is a real one: a
    /// device that may relay gives up gotatun's batched `recvmmsg` read,
    /// because a batch parked on the direct socket would starve the relayed
    /// path. A device that will never relay should not pay for the option.
    ///
    /// No credential here: `crypto/turn_credential` mints one under a
    /// secret the guest cannot read, and the program hands it over on
    /// [`WireguardConfig::reply_queue`] when the mapping comes back
    /// `punchable: false`. So the decision is the program's, and the
    /// `wireguard` block never holds a TURN secret.
    #[serde(default)]
    pub turn_fallback: bool,
    /// STUN servers to measure this device's own mapping with, **on
    /// `listen_port`, immediately before the device binds it**.
    ///
    /// This is the one piece that makes a punch measurable rather than
    /// hoped for. `netcheck --udp-port` cannot do it once a deployment is
    /// running -- the device already holds the port, so the measurement
    /// would refuse to bind, and measuring a *different* port measures a
    /// different mapping. Measuring here, microseconds before the same
    /// local port is bound for real, is as close as a userspace program
    /// gets to asking about the socket it is going to use.
    ///
    /// Two servers minimum on separate addresses, because one server can
    /// report an address and only two can say whether it *changed* --
    /// which is the fact that decides whether a punch can work at all.
    /// Empty means do not measure.
    #[serde(default)]
    pub stun: Vec<String>,
    /// The tunnel interface to create: `drt0`, `wg0`, `utun` on macOS.
    /// Creating one needs privilege (CAP_NET_ADMIN on Linux, root on macOS,
    /// wintun.dll on Windows), and that is the whole privilege this needs:
    /// no module, no `wg` tools, no `wg-quick`. Not read in
    /// `mode = "userspace"`, which creates nothing.
    #[serde(default = "default_wireguard_interface")]
    pub interface: String,
    /// Kernel interface or in-process stack. `kernel` unless the config
    /// says otherwise; see [`WireguardMode`].
    #[serde(default)]
    pub mode: WireguardMode,
    /// `mode = "userspace"` only: local ports that reach into the tunnel.
    /// In kernel mode the interface is reached by address and a forward
    /// here is refused, since it is a config that believes it is in the
    /// other mode.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub forward: Vec<WireguardForward>,
    /// `mode = "userspace"` only: addresses inside the tunnel that reach
    /// local ports. A userspace stack with neither this nor `forward` is
    /// refused: a device nothing can reach terminates traffic nobody can
    /// hand it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub expose: Vec<WireguardExpose>,
    /// This peer's private key, base64, inline. The same three knobs as
    /// every other secret in this config, resolved file, then env, then
    /// inline; one is required, since a device without a key has no
    /// identity and can complete no handshake.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub private_key: Option<String>,
    /// This peer's private key, base64, from a file -- `wg genkey`'s own
    /// output, one trailing newline trimmed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub private_key_file: Option<PathBuf>,
    /// This peer's private key, base64, from the named environment
    /// variable, read once at startup.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub private_key_env: Option<String>,
    /// The peers. May be empty: a deployment that learns its peers at
    /// runtime adds them over `reply_queue`.
    #[serde(default)]
    pub peers: Vec<WireguardPeer>,
    /// Where the device's reports land on the root program inside
    /// `drt start`: a peer snapshot on the timer below, and an endpoint
    /// change as it happens.
    #[serde(default = "default_wireguard_queue")]
    pub queue: String,
    /// Where the device reads commands, which is what makes a punch
    /// possible from inside a program: `endpoint` to point a peer at an
    /// address the rendezvous just learned, `keepalive` to hold the hole
    /// open. **Empty means the device takes no commands**, and that is the
    /// default: a config that does not ask to be steered is not steerable.
    #[serde(default)]
    pub reply_queue: String,
    /// How often the peer snapshot is reported. An endpoint change is
    /// reported as it happens whatever this says: a peer that roamed
    /// between two snapshots is the event a punch is waiting for.
    #[serde(default = "default_wireguard_report_ms")]
    pub report_ms: u64,
}

fn default_wireguard_interface() -> String {
    "drt0".into()
}
fn default_wireguard_port() -> u16 {
    51820
}
fn default_wireguard_mtu() -> u16 {
    1420
}
fn default_wireguard_queue() -> String {
    "wg_in".into()
}
fn default_wireguard_report_ms() -> u64 {
    10_000
}

fn default_relay_queue() -> String {
    "relay_in".into()
}
fn default_admit_timeout_ms() -> u64 {
    2000
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RelayLabel {
    /// Presented by the device parking a leg (`/park/<label>?k=…`).
    #[serde(default)]
    pub park_key: String,
    /// Presented by the caller claiming one (`/s/<label>?k=…`). Distinct
    /// from `park_key` on purpose: the device's key is long-lived on a
    /// laptop, the caller's is the one handed to whoever is connecting.
    #[serde(default)]
    pub caller_key: String,
}

/// `drt tunnel`, from a file: `drt --config device.json tunnel`.
///
/// One key per flag the verb takes, and the mode is told by which keys are
/// present, exactly as the flags tell it: `park` with `to` is the device
/// side of the relay, `claim` with `bind` is a local port that claims one
/// leg per connection, `claim` alone is the stdio bridge (OpenSSH's
/// `ProxyCommand` shape), and `listen` with `to` is a WebSocket acceptor
/// in front of any sshd. Two of those in one block is a refusal by name,
/// as is a key that belongs to another mode.
///
/// Why a file at all: the `?k=` in a park or claim URL is a credential.
/// On a command line it is in `ps`, in shell history, and in every "run
/// this" someone pastes; in a 0600 file it is in none of them. The rest is
/// that a device's tunnel becomes one file a setup script writes and a
/// unit runs, which is the shape everything else on a box already has.
///
/// Flags merge over the file **per key**: a flag naming a key the file
/// also names replaces it, and a flag naming a different mode than the
/// file is the same conflict two flags would be. Nothing here changes
/// when the tunnel later takes a direct path: the same two files, and
/// whether a session went direct or through the relay is DRT's to know.
///
/// `claim`, because that is what the relay calls the act (`Parked::Claimed`,
/// "claim first, splice second") and the positional flag has no name of
/// its own. `bind`, as every other block spells the local address it
/// listens on. `park`, `to` and `listen` as the flags are.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TunnelConfig {
    /// The `ws://` or `wss://` URL the caller half dials: the relay's
    /// `/s/<label>?k=…`, or a gate straight in front of a `listen`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claim: Option<String>,
    /// With `claim`: serve this local address instead of stdio, one fresh
    /// leg per accepted connection. `127.0.0.1:2222`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bind: Option<String>,
    /// The device side of the relay: hold a parked leg at this `/park/`
    /// URL and dial `to` lazily when a caller claims it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub park: Option<String>,
    /// The other half: accept WebSocket connections here and bridge each
    /// to `to`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub listen: Option<String>,
    /// Where `park` and `listen` deliver: a `host:port` this process can
    /// dial, `127.0.0.1:22` in front of an sshd.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to: Option<String>,
    /// PEM files to trust beside the public roots, never instead of them --
    /// spelled as `connectors.rest`'s `extra_roots` is, for its reason. An
    /// internal CA in front of the gate, typically.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub extra_roots: Vec<PathBuf>,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub relay: Option<RelayConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stun: Option<StunConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn: Option<TurnConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wireguard: Option<WireguardConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tunnel: Option<TunnelConfig>,
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
            numeric: Numeric::default(),
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

    /// Numeric bounds attenuate like a budget: a child may narrow either,
    /// and may raise neither. Unstated inherits.
    #[test]
    fn numeric_bounds_attenuate_and_the_refusal_names_which_one() {
        let parent = InstanceConfig {
            numeric: Numeric {
                max_elements: Some(1_000_000),
                max_tier: Some(Tier::Reproducible),
            },
            ..cfg(vec![], Budget::default())
        };
        let child = |numeric: Numeric| InstanceConfig {
            numeric,
            ..cfg(vec![], Budget::default())
        };

        // Narrower on both: fine.
        assert_eq!(
            child(Numeric {
                max_elements: Some(1000),
                max_tier: Some(Tier::Exact),
            })
            .check_attenuation(&parent),
            Ok(())
        );

        // More elements than the parent allows.
        assert_eq!(
            child(Numeric {
                max_elements: Some(2_000_000),
                max_tier: None,
            })
            .check_attenuation(&parent),
            Err(ConfigError::NumericElementsExceedParent)
        );

        // A looser tier: `fast` promises less than `reproducible`, so
        // asking for it is asking to be allowed results the parent would
        // not accept. Named separately from the element bound, because
        // "your numeric block is wrong" is not actionable.
        assert_eq!(
            child(Numeric {
                max_elements: None,
                max_tier: Some(Tier::Fast),
            })
            .check_attenuation(&parent),
            Err(ConfigError::NumericTierExceedsParent)
        );

        // Unstated inherits, and resolution pins what enforcement runs on
        // -- the half that is easier to miss, because saying nothing needs
        // no intent at all.
        let unstated = child(Numeric::default());
        assert_eq!(unstated.check_attenuation(&parent), Ok(()));
        let resolved = unstated.numeric.resolved_against(&parent.numeric);
        assert_eq!(resolved.max_elements, Some(1_000_000));
        assert_eq!(resolved.max_tier, Some(Tier::Reproducible));

        // A parent that states nothing bounds nothing, so any child fits.
        let open = cfg(vec![], Budget::default());
        assert_eq!(
            child(Numeric {
                max_elements: Some(u64::MAX),
                max_tier: Some(Tier::Fast),
            })
            .check_attenuation(&open),
            Ok(())
        );
    }

    /// Zero is refused rather than carried.
    ///
    /// `dv_numeric_set_max_elements` reads `0` as "no limit", so a config
    /// writing `max_elements = 0` and meaning "forbid" would get the
    /// loosest bound instead of the strictest. There is no translation that
    /// serves both readings, so the value never gets in.
    #[test]
    fn a_zero_element_bound_is_refused_rather_than_inverted() {
        let zero = Numeric {
            max_elements: Some(0),
            max_tier: None,
        };
        let why = zero.check_representable().unwrap_err();
        assert!(why.contains("no limit"), "the refusal says why: {why}");
        assert!(
            why.contains("Omit the field"),
            "and what to do instead: {why}"
        );

        // And it is refused through the attenuation check, which is the
        // path a spawn request takes.
        let child = InstanceConfig {
            numeric: zero,
            ..cfg(vec![], Budget::default())
        };
        assert!(matches!(
            child.check_attenuation(&cfg(vec![], Budget::default())),
            Err(ConfigError::NumericUnrepresentable(_))
        ));

        // Any other number is fine, one included.
        assert!(Numeric {
            max_elements: Some(1),
            max_tier: None,
        }
        .check_representable()
        .is_ok());
    }

    /// The tier names are the ones the spec and the config use, and they
    /// round-trip through serde and through `parse`.
    #[test]
    fn tier_names_are_the_specs_three() {
        for (tier, name, rank) in [
            (Tier::Exact, "exact", 0u8),
            (Tier::Reproducible, "reproducible", 1),
            (Tier::Fast, "fast", 2),
        ] {
            assert_eq!(tier.name(), name);
            assert_eq!(tier.rank(), rank);
            assert_eq!(Tier::parse(name), Some(tier));
            let bytes = rmp_serde::to_vec_named(&tier).unwrap();
            assert_eq!(rmp_serde::from_slice::<Tier>(&bytes).unwrap(), tier);
        }
        assert_eq!(Tier::parse("quick"), None);
    }

    /// An instance stating no numeric block serializes without one, so a
    /// spawn request from a deployment that does no numeric work is the
    /// same bytes it was before this field existed.
    #[test]
    fn an_unstated_numeric_block_is_absent_from_the_wire() {
        let plain = cfg(vec![], Budget::default());
        let raw: rmpv::Value =
            rmp_serde::from_slice(&rmp_serde::to_vec_named(&plain).unwrap()).unwrap();
        let keys: Vec<&str> = raw
            .as_map()
            .unwrap()
            .iter()
            .filter_map(|(k, _)| k.as_str())
            .collect();
        assert!(!keys.contains(&"numeric"), "got {keys:?}");
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
