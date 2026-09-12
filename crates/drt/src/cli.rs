//! The command surface: `run | start | repl | ps` (SPEC.md §13a settles it
//! — one process per deployment, client commands reaching a running one
//! over its control endpoint, no daemon and no registry).
//!
//! In the library rather than in `main.rs` so that every host parses the
//! same command line: the binary's `main` is one call into [`main`], and a
//! page's terminal parses `drt run app.dlua` with the same [`Cli`], gets
//! the same `--help` and the same argument errors, and assembles the same
//! config and dispatcher with [`assemble`] (doc/Wasm.md D3, §5). What a
//! page cannot share is the *loop* — it may not sleep — so the verbs are
//! driven there through `drive::Solo`, `repl::Repl` and
//! `start::DeployDriver`, and [`main`] here is the native loop over the
//! same three.
//!
//! `run` is real: one program, driven to completion with the hostcall pump
//! (see `run.rs`). `start` is real: the deployment — root program plus its
//! swarm — foreground, with park timeouts honoured on the host clock (see
//! `start.rs`). `ps` is an honest stub until the control endpoint exists.
//! [`wire_connectors`] is where a profile's feature gates meet the root
//! config, once, for every subcommand.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};

use crate::{config, repl, run, start};
use drt_config::RootConfig;
use drt_connector::Registry;

/// The named profiles, as the exact feature sets `Cargo.toml` declares
/// them, so `buildinfo` names a build by what it *is*. A binary matching
/// none of these is `custom`, which is more useful than calling it
/// whichever named profile it resembles: a package declaring
/// `requires.connectors` is admitted by name, never by resemblance.
///
/// Sorted, because the comparison is against a sorted list of what this
/// binary was compiled with. Adding a feature to a profile means adding it
/// here too, and `profile_matches_its_manifest` in `tests/cli.rs` is what
/// notices when someone forgets.
const PROFILE_SLIM: &[&str] = &[
    "cli",
    "connector-crypto",
    "connector-fs",
    "connector-time",
    "listen",
];
const PROFILE_WASI: &[&str] = &[
    "connector-crypto",
    "connector-fs",
    "connector-sql",
    "connector-time",
    "listen",
];
const PROFILE_WEB: &[&str] = &["cli", "connector-crypto", "connector-fs", "connector-time"];
const PROFILE_FULL: &[&str] = &[
    "cli",
    "connector-crypto",
    "connector-data",
    "connector-exec",
    "connector-fs",
    "connector-rest",
    "connector-sql",
    "connector-ssh",
    "connector-ssmtp",
    "connector-time",
    "listen",
    "netcheck",
    "relay",
    "runtime",
    "stun",
    "tunnel",
    "turn",
    "turn-client",
    "wireguard",
];

/// What the embedded diluvium core carries, per profile: the other half of
/// the compatibility fact `dv_abi` starts. A package that needs regular
/// expressions, or later the `numeric` array library, can only be admitted
/// or refused by name if the binary will say which of them are inside.
///
/// TODO(A0): hard-coded, because the core does not yet say. Session A's A0
/// milestone adds `dv_features()` -- a newline-separated list, stable for
/// the life of the process -- and when that pin lands these four tables go
/// away and the list is read off the linked core instead. That is
/// `doc/Release.md`'s rule: the compatibility fact travels with the bytes,
/// and a fact this file states about bytes it did not compile is a fact
/// that can be wrong. It is per profile already so that the day a profile
/// carries a different core -- the web profile without `numeric`, say, if
/// C1's size ledger says it costs too much -- nothing has to be reshaped
/// to say so.
///
/// Sorted, like the profile tables above, and gated the same way by
/// `core_features_agree_with_the_changelog` in `tests/cli.rs`.
const CORE_FEATURES_FULL: &[&str] = &["regex"];
const CORE_FEATURES_SLIM: &[&str] = &["regex"];
const CORE_FEATURES_WASI: &[&str] = &["regex"];
const CORE_FEATURES_WEB: &[&str] = &["regex"];
/// A build whose feature set matches no named profile still embeds a core,
/// and `unknown` is the honest answer about which features it carries --
/// the same answer `diluvium: unknown` gives for an unpinned revision. An
/// empty list would read as "carries none", which is a different claim.
const CORE_FEATURES_CUSTOM: &[&str] = &[];

/// The N in `5.5.1_buildN`: which diluvium build is inside, as a number a
/// `requires.diluvium_build` range can be compared against. The revision
/// beside it is exact but unordered -- two revisions cannot be asked which
/// is newer -- and that is what this field adds.
///
/// TODO(A0): hard-coded for the same reason and with the same fix as
/// [`CORE_FEATURES_FULL`]; A0 adds `dv_build()`. Until then the number is
/// written down once, here, and `diluvium_build_agrees_with_the_changelog`
/// in `tests/cli.rs` is what stops it going stale when the pin moves: the
/// changelog records the pin, the pin is checked against `Cargo.lock` by
/// `script/changelog.py check`, and this is checked against the changelog.
const DILUVIUM_BUILD: u32 = 14;

/// What `drt wg` does. All three are diagnostics or key handling, and the
/// serving that used to sit beside them is `drt start` now.
#[cfg(feature = "wireguard")]
#[derive(clap::Subcommand)]
pub enum WgAction {
    /// Print a fresh key pair: the private key on the first line, its
    /// public key on the second. `wg genkey | wg pubkey` without
    /// wireguard-tools, which is the point -- a block that needed those
    /// installed to produce a key would not have removed the dependency.
    Keygen,
    /// Print the public key of a private key read from stdin -- `wg pubkey`,
    /// and the half `keygen` cannot give you for a key you already have.
    /// A key kept in a secret store has to be nameable to a peer without
    /// being pasted into a terminal, and this is how.
    Pubkey,
    /// Read the `wireguard` block, say what is wrong with it, and stop.
    ///
    /// Creating the interface needs a privilege; being told the config is
    /// wrong should not. This runs every check `drt wg` runs before it
    /// touches the interface -- keys, addresses, CIDRs, ports -- and prints
    /// the warnings a running device would print, so a config can be
    /// written and checked anywhere and only deployed where it is allowed.
    Check,
}

/// What `drt key` can do.
#[derive(clap::Subcommand)]
pub enum KeyAction {
    /// Generate a key: write its private seed to `path`, print its public key
    /// on stdout.
    ///
    /// The public half is what goes into a root's `consent.json` `signers`
    /// list, so it is the only thing on stdout -- `drt key new k > k.pub`
    /// leaves a file with one key in it.
    New {
        /// Where the private seed goes. Never overwritten.
        path: PathBuf,
    },
    /// Sign a decision about a pending grant request.
    ///
    /// This is what makes consent.md §8's "a human with a text editor" a real
    /// approval path: the runtime cannot tell a portal from a person, but only
    /// if a person can actually produce a valid signature.
    Sign {
        /// The key file, as written by `drt key new`.
        key: PathBuf,
        /// A request from `state/gsr/pending/`.
        request: PathBuf,
        /// The `key_id` this signs as. Must match a `signers` entry in the
        /// root's `consent.json`, or verification stops at step 1.
        #[arg(long = "key-id")]
        key_id: String,
        /// Deny instead of approving. A signed deny is an answer, which is why
        /// the directory is `decided/` and not `approved/`.
        #[arg(long)]
        deny: bool,
        /// When this decision stops being valid, as `YYYY-MM-DDTHH:MM:SSZ`.
        /// Defaults to an hour out, because revocation is out of scope and a
        /// forgotten approval is the failure mode.
        #[arg(long = "not-after", value_name = "INSTANT")]
        not_after: Option<String>,
    },
}

#[derive(Parser)]
#[command(name = "drt", version, about = "The Diluvium RunTime")]
pub struct Cli {
    /// Root config file. Flags and env merge over it into one root object.
    #[arg(long, global = true)]
    pub config: Option<PathBuf>,
    /// The root to act on, instead of looking for `.drt_root/` in the
    /// working directory.
    ///
    /// Discovery deliberately does not walk up, so a systemd unit should use
    /// this rather than `WorkingDirectory=`: systemd's default working
    /// directory is `/`, and a unit relying on discovery would find no root
    /// and run the no-root path without saying so.
    #[arg(long, global = true, value_name = "PATH")]
    pub root: Option<PathBuf>,
    /// Accept a **first** consent acceptance without asking.
    ///
    /// Deliberately not enough for a ceiling that has widened. A `-y` that
    /// also accepted a widening would mean every unit file and CI job carried
    /// permanent pre-consent to every future ceiling a root might declare,
    /// which defeats the approval chain's invariant in practice while leaving
    /// it true on paper. `--accept-changes` is that, separately.
    #[arg(short = 'y', long, global = true)]
    pub yes: bool,
    /// Accept a ceiling that has widened since it was consented to. The
    /// deliberate act, and the one `-y` does not cover.
    #[arg(long, global = true)]
    pub accept_changes: bool,
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand)]
pub enum Command {
    /// Run one program to completion: config + one program is a complete
    /// deployment.
    Run {
        /// A file, a declared profile, Diluvium code, `-` for stdin, or
        /// `stdlib:<name>`. Optional when the config names a program.
        ///
        /// Which it is, is decided in this order: an explicit flag below,
        /// then `-`, then a `stdlib:` prefix, then a leading `/` or `./`, then
        /// evidence of code (a newline, parens, quotes, `=` or `;` — not
        /// whitespace, which is path-legal), then a declared profile name, and
        /// a file otherwise. The last rule is what lets the error name the
        /// flags instead of guessing.
        target: Option<String>,
        /// Read the argument as a file, whatever it looks like.
        #[arg(short = 'f', long, conflicts_with_all = ["command", "profile"])]
        file: bool,
        /// Read the argument as Diluvium code.
        #[arg(short = 'c', long, conflicts_with_all = ["file", "profile"])]
        command: bool,
        /// Read the argument as a declared profile name.
        #[arg(short = 'p', long, conflicts_with_all = ["file", "command"])]
        profile: bool,
    },
    /// Run the deployment: the root program, its swarm, and whatever
    /// listeners the config names. Foreground; a process supervisor
    /// backgrounds it, and there is deliberately no --detach.
    Start {
        /// Which profile, by name (`debug`, not `debug.config.json`).
        /// Without one, `project.json`'s `default_profile` decides, and
        /// without a `project.json` the pre-recognized fallback order does.
        profile: Option<String>,
        /// Replace what is already deployed, then start.
        ///
        /// Without it, `start` on an already-deployed root says so and stops
        /// rather than overwriting `live/`. A deploy replaces rather than
        /// merges, and `live/` is what a stateful node's directory survives
        /// restarts in, so overwriting it is destructive enough to be asked for.
        #[arg(long)]
        rm: bool,
        /// Arguments for the entry, overriding the profile's declared defaults
        /// by key: `--verbose`, `--port 9000`, `--label=gate`, and a repeated
        /// `--stun a --stun b` for a key whose default is a list.
        ///
        /// **Everything after the profile name belongs to the entry.** drt's own
        /// flags therefore come *before* it — `drt start --rm debug --verbose`,
        /// not `drt start debug --rm`. That is the documented rule and it is
        /// what lets a profile declare its own command line without drt having
        /// to reserve names against it.
        ///
        /// How each is parsed comes from the declared default's type, so a
        /// profile is the whole declaration of its own command line and an
        /// undeclared key is a named failure rather than a silent addition.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Copy this profile's source into `live/`, and nothing else.
    ///
    /// From `dlua_dir` when the profile sets one, else from `init/`. What runs
    /// is always `live/`, so this is the step that makes an edit take effect.
    Deploy,
    /// Delete this deployment from `live/`.
    Rm,
    /// Capture what is running, from `live/` into `init/`, and write the
    /// envelope.
    ///
    /// From `live/` and never from an editor's buffer: nothing reaches `init/`
    /// without having been visible in what runs, so a supervisor can review a
    /// commit by looking at the deployment.
    Commit,
    /// A REPL is an instance, not a mode: a sealed guest with a generous
    /// local grant, bridged to this terminal.
    Repl {
        /// Evaluate with the unsafe stdlib: `os`, `io` and `require`
        /// present, as a language REPL needs them and as `drt run`
        /// refuses. Still an instance -- the capability grants and the
        /// budget apply exactly as they do without it -- but the seal
        /// `drt` otherwise keeps is off, which the banner says out loud.
        /// It costs replayability and makes the budget approximate; dv.h
        /// says why.
        #[arg(long = "unsafe")]
        unsafe_stdlib: bool,
    },
    /// Keys: generate one, or sign a decision about a pending grant request.
    ///
    /// drt does the cryptography and never needs dollup installed. dollup
    /// stores keys and copies public halves into a root's `consent.json`;
    /// anything it can do to a key, this does to the same key with a path.
    Key {
        #[command(subcommand)]
        action: KeyAction,
    },
    /// The introspection surface: instances, caps, budgets, usage, health.
    Ps,
    /// What this binary is and what it carries: version, the dv ABI it
    /// speaks, its feature profile, and the connectors compiled into it.
    ///
    /// A binary that cannot say what it has cannot be checked against a
    /// package that declares what it needs. `--json` is the form a
    /// package manager reads; the release workflow uses it to fill
    /// BUILDINFO.txt from the artifact itself rather than from a guess
    /// made in YAML.
    Buildinfo {
        /// Machine-readable, for a package manager or a release job.
        #[arg(long)]
        json: bool,
    },
    /// The WireGuard toolbox: make a key, read a public key off one, and
    /// say what is wrong with a `wireguard` block. None of the three needs
    /// a privilege, and none of them serves.
    ///
    /// Serving is `drt start` with a `wireguard` block, whose program is
    /// `stdlib:wg` when it only reports. This used to be bare `drt wg`,
    /// back when a block with no program had no other way to run.
    ///
    /// Creating the interface needs CAP_NET_ADMIN or root on Linux, root
    /// on macOS, and wintun.dll on Windows -- so it is `start` that needs
    /// them, and nothing here does.
    #[cfg(feature = "wireguard")]
    Wg {
        #[command(subcommand)]
        action: WgAction,
    },
    /// SSH over WSS, as a dumb pipe. With a URL: bridge this process's
    /// stdio to it — the OpenSSH ProxyCommand contract, so
    /// `ssh -o ProxyCommand="drt tunnel wss://gate/fp" user@fp` (and rsync,
    /// sftp, -L/-R through it) works like normal SSH over the WebSocket
    /// carrier. With a URL and --local: serve a local port instead, one
    /// fresh leg per accepted connection, which is how a program reaches
    /// a parked device. With --listen/--to: accept WebSocket connections
    /// and bridge each to a TCP target, in front of any sshd. With
    /// --park/--to: the device side of the relay.
    ///
    /// Every flag is also a key of the `tunnel` block in --config, under
    /// the block's name (the URL is `claim`, --local is `bind`), so the
    /// credential in a park or claim URL can live in a 0600 file. Flags
    /// win per key; a flag naming a different mode than the file is
    /// refused as the conflict it is.
    #[cfg(feature = "tunnel")]
    Tunnel {
        /// The wss:// or ws:// URL to bridge stdio to.
        url: Option<String>,
        /// With a URL: bind this local address instead of using stdio,
        /// and give each accepted connection its own fresh leg to the URL
        /// -- one claim per connection, nothing multiplexed over one.
        /// `ssh/exec` scoped to this address, `rest` dialing it, or a
        /// desktop client with no ProxyCommand support, reach a parked
        /// device this way. A claim the relay refuses closes the local
        /// connection at once rather than leaving it half-open.
        ///
        /// What each flag requires and conflicts with is judged with the
        /// file's keys in `tunnel::resolve`, not here: a `requires` on
        /// the flag alone would refuse `--local` beside a `claim` the
        /// file supplies.
        #[arg(long, value_name = "HOST:PORT")]
        local: Option<String>,
        /// Serve the other half: accept WebSockets here…
        #[arg(long)]
        listen: Option<String>,
        /// …and bridge each connection to this host:port (used by both
        /// --listen and --park).
        #[arg(long)]
        to: Option<String>,
        /// The device side of the rendezvous relay: hold a parked leg at
        /// this /park URL; when a caller claims it, dial --to lazily and
        /// splice, re-parking a fresh leg immediately. Reconnects forever.
        #[arg(long)]
        park: Option<String>,
        /// Trust this PEM certificate in addition to the public roots --
        /// an internal CA in front of the gate, typically. Repeatable.
        /// Added, never substituted: the public roots stay trusted, for
        /// the same reason `connectors.rest`'s `extra_roots` says so.
        #[arg(long = "extra-root", value_name = "PEM")]
        extra_root: Vec<std::path::PathBuf>,
    },
    /// What can this network do, and what should you do about it.
    ///
    /// Prints one of four verdicts — direct, v6-direct, punchable, relay —
    /// with the measurements that produced it. Exit 0 on a verdict,
    /// `relay` included: a network that needs a tunnel is a successful
    /// measurement, not an error. Non-zero means nothing could be
    /// measured.
    #[cfg(feature = "netcheck")]
    Netcheck {
        /// A STUN server, repeatable. Two on separate addresses are needed
        /// to classify a mapping; `detect_mapping` refuses below two rather
        /// than guessing, so one server yields "not measured" and the
        /// relay fallback, never a confident wrong answer.
        ///
        /// An override. A `--reflect` edge that names its own pair in its
        /// answer supplies them, and this flag is for a network being
        /// diagnosed by hand; when both are given, this wins.
        #[arg(long = "stun", value_name = "HOST:PORT")]
        stun: Vec<String>,
        /// A reflect edge, repeatable. Fills the `address` and `tcp map`
        /// lines from what an edge saw over TCP, keyed by the `edge` it
        /// names itself. An edge that does not answer stays "not
        /// measured", with the reason — never a guess and never a zero.
        ///
        /// This is the one flag an ordinary run needs. The first edge is
        /// asked, before anything is measured, how to measure against it
        /// -- the STUN pair and the vantage addresses -- and the run
        /// configures itself from the answer. An edge that answers with
        /// nothing of the kind leaves the run to `--stun` and
        /// `--reflect-at`, exactly as before. The evidence block's first
        /// line says which happened.
        #[arg(long = "reflect", value_name = "URL")]
        reflect: Vec<String>,
        /// Ask `--reflect` at this address rather than at the one its name
        /// resolves to. Repeatable: each is one vantage, and the `Host`
        /// stays the name, so one fetchpoint answers from several edges.
        ///
        /// One `--reflect` is one vantage whatever its name resolves to,
        /// so a name with two A records still yields one view per run;
        /// naming each address here is what gets a comparison. It is
        /// `curl --resolve` by another name.
        ///
        /// An override, like `--stun`: an edge that lists its vantages in
        /// its answer supplies them, and this flag wins when both are given.
        #[arg(long = "reflect-at", value_name = "ADDRESS")]
        reflect_at: Vec<String>,
        /// Ask a probe edge to connect back to the address it observes,
        /// and report whether it reached this port.
        /// Repeatable, asked sequentially and bounded, because the prober
        /// rate-limits per address.
        ///
        /// Probes the service already listening on the port. It binds
        /// nothing: a diagnostic that opened a socket would be a different
        /// and more surprising thing, and would want a differently named
        /// flag saying so.
        ///
        /// Needs `--probe-at`. Where a prober is deployed is the edge's
        /// fact, not this text's: an earlier version of this sentence said
        /// "not deployed yet" long after one was.
        #[arg(long = "port", value_name = "N")]
        port: Vec<u16>,
        /// The edge the probe's SYN should come from — one this run has not
        /// contacted for reflect.
        ///
        /// `NETCHECK-SPEC.md` §3: with a prober on both gates the original
        /// asymmetry becomes a client obligation, because a SYN from an
        /// address the caller just contacted can traverse the mapping the
        /// caller's own request created and answer `connected` when nothing
        /// out there can reach them.
        #[arg(long = "probe-at", value_name = "ADDRESS")]
        probe_at: Option<String>,
        /// Send every `--reflect` request from the **same local source
        /// port**, which is what turns two edges into a TCP mapping
        /// comparison rather than two unrelated observations.
        ///
        /// On by default whenever more than one fetch is planned, which is
        /// the only time it measures anything, so a bare run gets the
        /// comparison it can make. This flag forces it for a single fetch
        /// -- where it measures nothing -- and exists so a script written
        /// when it was required keeps working. Sequential either way, so a
        /// NAT may rebind between the requests; the evidence line says so.
        #[arg(long = "pin-source-port")]
        pin_source_port: bool,
        /// Take the UDP mapping from **this local port** rather than an
        /// ephemeral one. A mapping is a fact about one flow: on a NAT that
        /// is not port-preserving, the port an ephemeral probe is mapped to
        /// says nothing about what `udp/51820` will get, so a tool that
        /// wants the answer for WireGuard's socket asks from WireGuard's
        /// port. A port that cannot be bound is reported by name under
        /// `udp map`, never quietly replaced.
        #[arg(long = "udp-port", value_name = "N")]
        udp_port: Option<u16>,
        /// Trust this PEM certificate in addition to the public roots --
        /// an intercepting proxy's CA, typically. Repeatable. Added, never
        /// substituted, the same rule and wording `drt tunnel` uses.
        ///
        /// This is the flag that makes `netcheck` usable on a corporate
        /// network, which is the network whose behaviour is hardest to
        /// guess and where "run netcheck" is most often the advice.
        /// Without it an intercepted `--reflect` fetch fails
        /// `UnknownIssuer` and the TCP half reads "not measured" -- an
        /// honest answer, but not the one the operator can act on.
        #[arg(long = "extra-root", value_name = "PEM")]
        extra_root: Vec<std::path::PathBuf>,
        /// Machine-readable output. The default is human text, because the
        /// primary consumer is a person deciding what to do next.
        #[arg(long)]
        json: bool,
    },
}

/// Wire the connectors this build carries against the root config. Off by
/// default, all of them: only what the config names gets wired.
#[cfg_attr(
    not(any(
        feature = "connector-time",
        feature = "connector-ssh",
        feature = "connector-fs",
        feature = "connector-sql",
        feature = "connector-crypto",
        feature = "connector-rest",
        feature = "connector-ssmtp",
        feature = "connector-exec",
        feature = "connector-data",
        feature = "netcheck"
    )),
    allow(unused_mut, unused_variables)
)]
/// What this binary carries, computed from the features it was actually
/// built with rather than declared anywhere.
///
/// The connector list is the load-bearing part: `full` and `slim` differ
/// precisely in their connector set, so a package that declares
/// `requires.connectors` can only be admitted or refused by name if the
/// binary will say what it has. The dv ABI numbers are the other half —
/// `library` is what the linked C core reports at runtime, `expected` is
/// what these bindings were built against, and a difference between them
/// is the mismatch `DiluviumEngine::new` refuses on.
pub fn buildinfo(json: bool) -> String {
    let mut connectors: Vec<&str> = Vec::new();
    if cfg!(feature = "connector-time") {
        connectors.push("time");
    }
    if cfg!(feature = "connector-fs") {
        connectors.push("fs");
    }
    if cfg!(feature = "connector-crypto") {
        connectors.push("crypto");
    }
    if cfg!(feature = "connector-sql") {
        connectors.push("sql");
    }
    if cfg!(feature = "connector-ssh") {
        connectors.push("ssh");
    }
    if cfg!(feature = "connector-rest") {
        connectors.push("rest");
    }
    if cfg!(feature = "connector-ssmtp") {
        connectors.push("ssmtp");
    }
    if cfg!(feature = "connector-exec") {
        connectors.push("exec");
    }
    if cfg!(feature = "connector-data") {
        connectors.push("data");
    }
    if cfg!(feature = "listen") {
        connectors.push("listen");
    }

    let mut verbs: Vec<&str> = vec![
        "buildinfo",
        "commit",
        "deploy",
        "key",
        "ps",
        "repl",
        "rm",
        "run",
        "start",
    ];
    if cfg!(feature = "netcheck") {
        verbs.push("netcheck");
    }
    if cfg!(feature = "tunnel") {
        verbs.push("tunnel");
    }
    // `relay`, `stun` and `turn` were verbs here and are not any more: each
    // is a config block plus `stdlib:<name>` under `start`. `wg` stays,
    // carrying keygen/pubkey/check -- the three things that are not serving.
    if cfg!(feature = "wireguard") {
        verbs.push("wg");
    }
    verbs.sort_unstable();

    // Named by what the profile actually is, not by what was asked for: a
    // build with an unusual feature set is `custom`, and saying so is more
    // useful than calling it whichever named profile it resembles. The
    // comparison is exact -- the wasi profile is slim's connectors plus sql
    // and minus listen, and a looser match once reported it as `slim`,
    // which the examples gate then read as "skip what needs sql".
    let profile = profile_name(&enabled_features());

    // What the core inside carries, the other half of the `dv_abi` fact.
    // Keyed off the profile because that is what decides which core was
    // compiled -- see the note on `CORE_FEATURES_FULL`, and the TODO(A0)
    // that ends this indirection.
    let features = core_features(profile);

    // Asked of drt-swarm, which owns the engine feature — see the note on
    // `abi_versions` there. `null`/`unknown` is reported honestly rather
    // than as a zero a consumer would read as a real ABI number.
    let abi = drt_swarm::engine::abi_versions();

    // Which diluvium is inside, stamped at build time from `Cargo.lock`
    // (build.rs). A **revision**, deliberately, not a version: the core
    // exposes no version string at runtime, and the distinctions that have
    // actually mattered between DRT and diluvium — FM-2 affected or fixed,
    // the budget escape open or closed — are revision facts that a semver
    // range could not express even if one existed. `unknown` on a build
    // that does not pin it by revision.
    let diluvium_rev = env!("DRT_DILUVIUM_REV");
    // The release tag, when the build was one. `version` is the crate's and
    // every candidate under it prints the same `0.5.0`, so a box running
    // rc3 could not be asked which candidate it ran; discofetch pinned the
    // installed tag in a file beside the binary because the binary would
    // not say (`DRT_ASKS.md` §3). Stamped by build.rs from the workflow's
    // `DRT_RELEASE_TAG`; a local build has none and prints no line.
    let tag = option_env!("DRT_RELEASE_TAG").filter(|t| !t.is_empty());

    if json {
        format!(
            "{{\"version\":\"{}\",\"tag\":{},\"profile\":\"{}\",\"dv_abi\":{},\
             \"dv_abi_expected\":{},\"diluvium\":\"{}\",\"diluvium_build\":{},\
             \"features\":[{}],\
             \"connectors\":[{}],\"verbs\":[{}]}}\n",
            env!("CARGO_PKG_VERSION"),
            tag.map_or("null".to_string(), |t| format!("\"{t}\"")),
            profile,
            abi.map_or("null".into(), |(l, _)| l.to_string()),
            abi.map_or("null".into(), |(_, e)| e.to_string()),
            diluvium_rev,
            DILUVIUM_BUILD,
            features
                .iter()
                .map(|f| format!("\"{f}\""))
                .collect::<Vec<_>>()
                .join(","),
            connectors
                .iter()
                .map(|c| format!("\"{c}\""))
                .collect::<Vec<_>>()
                .join(","),
            verbs
                .iter()
                .map(|v| format!("\"{v}\""))
                .collect::<Vec<_>>()
                .join(","),
        )
    } else {
        format!(
            "version: {}\n{}profile: {}\ndv_abi: {}\ndv_abi_expected: {}\n\
             diluvium: {}\ndiluvium_build: {}\nfeatures: {}\n\
             connectors: {}\nverbs: {}\n",
            env!("CARGO_PKG_VERSION"),
            tag.map_or(String::new(), |t| format!("tag: {t}\n")),
            profile,
            abi.map_or("unknown".into(), |(l, _)| l.to_string()),
            abi.map_or("unknown".into(), |(_, e)| e.to_string()),
            diluvium_rev,
            DILUVIUM_BUILD,
            features.join(","),
            connectors.join(","),
            verbs.join(","),
        )
    }
}

/// Every feature this binary was compiled with, sorted, in the spelling
/// `Cargo.toml` uses. `slim` and `full` themselves are not listed: they
/// are the names of sets, and this is the set.
fn enabled_features() -> Vec<&'static str> {
    let mut on: Vec<&'static str> = Vec::new();
    macro_rules! feature {
        ($name:literal) => {
            if cfg!(feature = $name) {
                on.push($name);
            }
        };
    }
    feature!("cli");
    feature!("connector-crypto");
    feature!("connector-fs");
    feature!("connector-rest");
    feature!("connector-sql");
    feature!("connector-ssh");
    feature!("connector-ssmtp");
    feature!("connector-data");
    feature!("connector-exec");
    feature!("connector-time");
    feature!("listen");
    feature!("netcheck");
    feature!("relay");
    feature!("runtime");
    feature!("stun");
    feature!("tunnel");
    feature!("turn");
    // Probed even though no profile turns it on by itself: `wireguard` names
    // it, so a full build has it, and PROFILE_FULL lists it. A name in a
    // profile table that is not probed here makes that profile unreportable
    // -- `profile_matches_its_manifest`'s sibling below is what says so.
    feature!("turn-client");
    feature!("wireguard");
    on.sort_unstable();
    on
}

/// Which named profile `features` is, exactly, or `custom`.
fn profile_name(features: &[&str]) -> &'static str {
    if features == PROFILE_FULL {
        "full"
    } else if features == PROFILE_SLIM {
        "slim"
    } else if features == PROFILE_WASI {
        "wasi"
    } else if features == PROFILE_WEB {
        "web"
    } else {
        "custom"
    }
}

/// The core features a named profile carries.
///
/// TODO(A0): replaced by one call to `dv_features()` once that pin lands.
fn core_features(profile: &str) -> &'static [&'static str] {
    match profile {
        "full" => CORE_FEATURES_FULL,
        "slim" => CORE_FEATURES_SLIM,
        "wasi" => CORE_FEATURES_WASI,
        "web" => CORE_FEATURES_WEB,
        _ => CORE_FEATURES_CUSTOM,
    }
}

pub fn wire_connectors(config: &RootConfig) -> Result<Registry, String> {
    let mut registry = Registry::new();
    for (name, wiring) in &config.connectors {
        match name.as_str() {
            #[cfg(feature = "connector-time")]
            "time" => registry
                .wire(
                    "time",
                    std::sync::Arc::new(drt_connector_time::TimeConnector::new()),
                    wiring.scope.clone(),
                )
                .map_err(|e| e.to_string())?,
            // Wired only when the config names it, and only ever to the
            // directory the config grants: the program names files inside
            // that, and nothing wires a default place on its behalf.
            #[cfg(feature = "connector-fs")]
            "fs" => registry
                .wire(
                    "fs",
                    std::sync::Arc::new(drt_connector_fs::FsConnector::new()),
                    wiring.scope.clone(),
                )
                .map_err(|e| e.to_string())?,
            // Same scope discipline as fs: the config grants a directory,
            // the program names its databases inside it.
            #[cfg(feature = "connector-sql")]
            "sql" => registry
                .wire(
                    "sql",
                    std::sync::Arc::new(drt_connector_sql::SqlConnector::new()),
                    wiring.scope.clone(),
                )
                .map_err(|e| e.to_string())?,
            // The one scope whose contents deliberately never reach the
            // program it serves: the key stays in this process, and a
            // grant of `host:crypto/jwt_sign` is the right to ask for a
            // signature, not the key.
            #[cfg(feature = "connector-crypto")]
            "crypto" => registry
                .wire(
                    "crypto",
                    std::sync::Arc::new(drt_connector_crypto::CryptoConnector::new()),
                    wiring.scope.clone(),
                )
                .map_err(|e| e.to_string())?,
            // The same scope discipline as `fs`, and literally the same
            // jail: the config grants a directory, the program names its
            // parquet and CSV files inside it, and a path resolving out of
            // it is refused with symlinks followed.
            #[cfg(feature = "connector-data")]
            "data" => registry
                .wire(
                    "data",
                    std::sync::Arc::new(drt_connector_data::DataConnector::new()),
                    wiring.scope.clone(),
                )
                .map_err(|e| e.to_string())?,
            // Not in `run`'s local defaults: `ssh/exec` reaches off this
            // machine and wants a deliberate grant.
            //
            // The old wording here said it "needs a tokio reactor (`start`
            // brings one)". That was wrong twice: `start` does NOT bring one
            // to the hostcall path — its drive loop is on the main thread and
            // the relay/STUN runtimes are on others — and no hostcall path
            // runs a reactor: the pump polls a connector's future on the
            // loop's own cadence (drt-swarm/src/pump.rs). So `ssh/exec`
            // panicked from every guest loop, and this comment is why it
            // read as expected. The connector carries its own runtime now.
            #[cfg(feature = "connector-ssh")]
            "ssh" => registry
                .wire(
                    "ssh",
                    std::sync::Arc::new(drt_connector_ssh::SshConnector::new()),
                    wiring.scope.clone(),
                )
                .map_err(|e| e.to_string())?,
            // Like `ssh`, needs a reactor, and like `crypto`, its scope
            // carries secrets the guest must not see: an operator-injected
            // `authorization` lets a program call an authenticated API
            // without ever holding the credential.
            #[cfg(feature = "connector-rest")]
            "rest" => registry
                .wire(
                    "rest",
                    std::sync::Arc::new(drt_connector_rest::RestConnector::new()),
                    wiring.scope.clone(),
                )
                .map_err(|e| e.to_string())?,
            // The scope carries what the guest must never hold: the relay
            // credential and the envelope sender. A program sends mail
            // without the password and cannot choose who it is from.
            #[cfg(feature = "connector-ssmtp")]
            "ssmtp" => registry
                .wire(
                    "ssmtp",
                    std::sync::Arc::new(drt_connector_ssmtp::SsmtpConnector::new()),
                    wiring.scope.clone(),
                )
                .map_err(|e| e.to_string())?,
            // The one connector the instruction budget cannot reach. Wired
            // only when a config names it, and announced when it is:
            // leaving the sandbox is a conscious act, so the process says
            // so on stderr before the first step, in GUARANTEES.md's words.
            // What bounds it is the scope's -- a deadline, an output cap
            // and an allow list -- and nothing else.
            #[cfg(feature = "connector-exec")]
            "exec" => {
                let _ = std::io::Write::write_all(
                    &mut drt_platform::stdio::stderr(),
                    b"drt: exec wired: granting host:exec/run leaves the sandbox \
                      (GUARANTEES.md); only the scope's deadline, output cap and allow \
                      list bound it\n",
                );
                registry
                    .wire(
                        "exec",
                        std::sync::Arc::new(drt_connector_exec::ExecConnector::new()),
                        wiring.scope.clone(),
                    )
                    .map_err(|e| e.to_string())?
            }
            other => {
                return Err(format!(
                    "config wires connector '{other}', which this build does not carry"
                ))
            }
        }
    }
    Ok(registry)
}

/// With no config file, a local run still gets the connectors this build
/// carries that need no scope of their own — the zero-ceremony case. `fs`
/// is not among them on purpose: it has no default place, and inventing one
/// on the program's behalf is the wrong the scope model exists to fix.
///
/// Public because [`assemble`] is not the only assembly any more: a page
/// building a swarm without a config (`drt-web`'s `swarm` module) is
/// entitled to the same zero-ceremony case a config-less `drt run` gets,
/// and getting it from here is what keeps the two the same.
pub fn local_defaults(config: &mut RootConfig) {
    if cfg!(feature = "connector-time") {
        config.connectors.insert("time".into(), Default::default());
    }
}

/// The root config and the connectors wired against it, from the command
/// line: the one assembly every verb and every host goes through.
pub fn assemble(cli: &Cli) -> Result<(RootConfig, drt_connector::Dispatcher), String> {
    let mut config = config::load(cli.config.as_deref())?;
    if cli.config.is_none() {
        local_defaults(&mut config);
    }
    let registry = wire_connectors(&config)?;
    // By name, at startup: never a mystifying `denied` at first call, and
    // never a server that binds and then refuses everything in silence.
    config::validate(&config)?;
    config::validate_grants(&config, &registry)?;
    Ok((config, drt_connector::Dispatcher::new(registry)))
}

/// Print the setup report. One place, so `drt start preflight` and
/// `drt run -p preflight` say the same thing.
fn preflight_report(resolution: &drt_config::resolve::Resolution) -> ExitCode {
    let pin = resolution.pin.as_ref().map(|p| p.value.as_str());
    match crate::stdlib::preflight(
        resolution,
        pin,
        env!("CARGO_PKG_VERSION"),
        resolution.consent.as_ref(),
        &mut std::io::stdout(),
    ) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("drt: {e}");
            ExitCode::FAILURE
        }
    }
}

/// `drt key`: generate, or sign.
fn key_verb(cli: &Cli, action: &KeyAction) -> ExitCode {
    match action {
        KeyAction::New { path } => match crate::key::new(path, &mut std::io::stdout()) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("drt key new: {e}");
                ExitCode::FAILURE
            }
        },
        KeyAction::Sign {
            key,
            request,
            key_id,
            deny,
            not_after,
        } => {
            let not_after = match not_after {
                Some(text) => match drt_config::time::Timestamp::parse(text) {
                    Ok(instant) => Some(instant),
                    Err(e) => {
                        eprintln!("drt key sign: {e}");
                        return ExitCode::FAILURE;
                    }
                },
                None => None,
            };
            // Where the decision goes: this root's `decided/`, because that is
            // the directory the runtime reads. Outside a root there is nothing
            // to approve, and saying so beats writing a signature into the
            // working directory for nobody to find.
            let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
            let Some(root) = crate::drt_root::discover(&cwd, cli.root.as_deref()) else {
                eprintln!(
                    "drt key sign: there is no root here to record a decision in; \
                     run it inside one, or name one with --root"
                );
                return ExitCode::FAILURE;
            };
            let verdict = if *deny {
                drt_config::gsr::Verdict::Deny
            } else {
                drt_config::gsr::Verdict::Approve
            };
            match crate::key::sign(
                key,
                request,
                &root.gsr_decided(),
                key_id,
                verdict,
                not_after,
                crate::drt_root::now(),
            ) {
                Ok(path) => {
                    eprintln!("wrote {}", path.display());
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("drt key sign: {e}");
                    ExitCode::FAILURE
                }
            }
        }
    }
}

/// Which reading the flags forced, if any. Clap's `conflicts_with_all` has
/// already refused two at once, so this cannot lose information.
fn explicit(file: bool, command: bool, profile: bool) -> Option<drt_config::resolve::Explicit> {
    use drt_config::resolve::Explicit;
    match (file, command, profile) {
        (true, _, _) => Some(Explicit::File),
        (_, true, _) => Some(Explicit::Code),
        (_, _, true) => Some(Explicit::Profile),
        _ => None,
    }
}

/// `drt run`: one program to completion, from whichever of the five things the
/// argument turned out to be.
///
/// The classification is `drt_config::resolve::classify`'s, not this file's, so
/// the rules are stated once and are testable without a process. What is here
/// is the IO each answer needs: opening a file, reading stdin, looking a
/// profile up in a root.
fn run_verb(
    cli: &Cli,
    target: Option<&str>,
    explicit: Option<drt_config::resolve::Explicit>,
    config: &RootConfig,
    dispatcher: drt_connector::Dispatcher,
) -> ExitCode {
    use drt_config::resolve::Requested;

    let Some(target) = target else {
        // No argument inside a root: run what is deployed. `drt run` is the
        // verb that does *not* deploy -- the already-deployed message points
        // here for exactly that -- so it reads `live/` as it stands and says so
        // when there is nothing there.
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        if crate::drt_root::discover(&cwd, cli.root.as_deref()).is_some() {
            return run_profile(cli, None);
        }
        // Outside a root: the config's own program, as it always was.
        let Some(drt_config::Program::Path(path)) = config.root.program.clone() else {
            eprintln!("drt run: name a program, as an argument or as `program` in the config");
            return ExitCode::FAILURE;
        };
        return drive_run(&path, config, dispatcher);
    };

    // A profile name is only a profile if a root declares it, so the root is
    // read before the argument is classified. That ordering is what makes
    // "a listed profile wins over a file of the same name" true rather than
    // aspirational.
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let root = crate::drt_root::discover(&cwd, cli.root.as_deref());
    let declared: Vec<drt_config::project::ProfileName> = root
        .as_ref()
        .map(|root| {
            let (inputs, _) = root.read(Requested::Default, Vec::new());
            inputs
                .root
                .as_ref()
                .and_then(|r| r.project.as_ref())
                .map(|p| p.declared_profiles().0.into_keys().collect())
                .unwrap_or_default()
        })
        .unwrap_or_default();

    match drt_config::resolve::classify(target, explicit, &declared) {
        Requested::File(path) => {
            let path = PathBuf::from(&path);
            if !drt_platform::fs::exists(&path) {
                // The one place the disambiguators get named.
                eprintln!("drt run: {}", drt_config::resolve::no_such_file(target));
                return ExitCode::FAILURE;
            }
            drive_run(&path, config, dispatcher)
        }
        Requested::Code(source) => drive_source(&source, "=command", config, dispatcher),
        Requested::Stdin => match read_stdin() {
            Ok(source) => drive_source(&source, "=stdin", config, dispatcher),
            Err(e) => {
                eprintln!("drt run: cannot read stdin: {e}");
                ExitCode::FAILURE
            }
        },
        Requested::Stdlib(name) => match crate::stdlib::lookup(&name) {
            Some(crate::stdlib::Kind::Source(src)) => {
                drive_source(src, &format!("=stdlib:{name}"), config, dispatcher)
            }
            // A native program reports on a root; `drt run` has no resolution
            // to hand it, so it says which verb does rather than printing an
            // empty report.
            Some(crate::stdlib::Kind::Native(name)) => {
                eprintln!(
                    "drt run: '{name}' reports on a root, so it is `drt start {name}` -- \
                     `run` has no resolution to report on"
                );
                ExitCode::FAILURE
            }
            None => {
                eprintln!(
                    "drt run: this build carries no stdlib program called '{name}'; it carries {}",
                    crate::stdlib::names().join(", ")
                );
                ExitCode::FAILURE
            }
        },
        // `drt run -p <profile>` runs `live/` as it stands; `drt start
        // <profile>` deploys first. The already-deployed message is what teaches
        // the difference, so this arm must not quietly deploy.
        Requested::Profile(name) => {
            if root.is_none() {
                eprintln!(
                    "drt run: there is no root here, so '{name}' names no profile; \
                     `--config <path>` is the self-contained form"
                );
                return ExitCode::FAILURE;
            }
            run_profile(cli, Some(&name))
        }
        Requested::Default => unreachable!("classify never answers Default"),
    }
}

/// Run a profile from `live/` as it stands, deploying nothing.
///
/// Through the same `boot` that `start` uses, so the two agree about which
/// profile is current, where its entry is, and what consent says — and then
/// stops short of the one step that makes them different.
fn run_profile(cli: &Cli, profile: Option<&str>) -> ExitCode {
    let booted = match booted_with(cli, "run", profile) {
        Ok(booted) => booted,
        Err(code) => return code,
    };
    if let Some(deployment) = &booted.deployment {
        if !deployment.deployed {
            eprintln!(
                "drt run: '{}' is not deployed; `drt deploy` copies {} into live/, \
                 or `drt start` does both",
                deployment.name,
                deployment.source.describe()
            );
            return ExitCode::FAILURE;
        }
    }
    // A native stdlib entry reports rather than running, here as in `start`.
    if let (Some(crate::boot::Runnable::Native(name)), Some(resolution)) =
        (&booted.runnable, &booted.resolution)
    {
        if *name == crate::stdlib::PREFLIGHT {
            return preflight_report(resolution);
        }
    }
    let Some(drt_config::Program::Path(path)) = booted.config.root.program.clone() else {
        eprintln!("drt run: this profile names no program to run");
        return ExitCode::FAILURE;
    };
    drive_run(&path, &booted.config, booted.dispatcher)
}

/// Read every byte of stdin. The piped-code case, and the reason `-` is
/// implemented rather than reserved: `drt run - <<'EOF'` is how a shell hands
/// over a program without a file.
fn read_stdin() -> std::io::Result<String> {
    use std::io::Read;
    let mut source = String::new();
    std::io::stdin().read_to_string(&mut source)?;
    Ok(source)
}

fn drive_run(
    path: &std::path::Path,
    config: &RootConfig,
    dispatcher: drt_connector::Dispatcher,
) -> ExitCode {
    match run::run(
        path,
        std::sync::Arc::new(dispatcher),
        config::ceiling(config),
        config.root.budget,
        config.root.numeric,
    ) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("drt run: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Run source that never was a file. `name` is what a traceback says, which is
/// why it is the one thing that differs between a command, stdin and a stdlib
/// program: `[string "=command"]:1:` tells a reader where their code came from.
fn drive_source(
    source: &str,
    name: &str,
    config: &RootConfig,
    dispatcher: drt_connector::Dispatcher,
) -> ExitCode {
    match run::run_source(
        source,
        name,
        std::sync::Arc::new(dispatcher),
        config::ceiling(config),
        config.root.budget,
        config.root.numeric,
    ) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("drt run: {e}");
            ExitCode::FAILURE
        }
    }
}

/// `drt start`: find the root, resolve, gate on consent, then run the
/// deployment — or, for a native stdlib entry, print its report and stop.
///
/// Separate from [`main`] because it is the one verb that does not take
/// [`assemble`]'s config: `crate::boot` produces its own, and the order there
/// is load-bearing (consent before connectors).
/// Find the root for a verb that needs one, or say why there is none.
fn rooted(cli: &Cli, verb: &str) -> Result<crate::drt_root::Root, ExitCode> {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    crate::drt_root::discover(&cwd, cli.root.as_deref()).ok_or_else(|| {
        eprintln!(
            "drt {verb}: there is no root here; run it inside one, name one with --root, \
             or `dollup init` to make one"
        );
        ExitCode::FAILURE
    })
}

/// `boot`, for a verb that only needs the resolution and not a dispatcher.
///
/// Through the same function `start` uses, so a `deploy` and the `start` that
/// would follow it cannot disagree about which profile is current or where its
/// source is. `-y` is passed through because a verb that writes into a root is
/// a verb an operator ran deliberately; it does not widen anything, since
/// `--accept-changes` is still separate.
fn booted_with(
    cli: &Cli,
    verb: &str,
    profile: Option<&str>,
) -> Result<crate::boot::Booted, ExitCode> {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let mut ask = crate::consent_gate::Terminal;
    crate::boot::boot(
        &cwd,
        cli.root.as_deref(),
        cli.config.as_deref(),
        profile,
        crate::consent_gate::Flags {
            yes: cli.yes,
            accept_changes: cli.accept_changes,
        },
        &mut ask,
    )
    .map_err(|e| {
        eprintln!("drt {verb}: {e}");
        ExitCode::FAILURE
    })
}

/// `drt deploy`: source into `live/`, and nothing else.
fn deploy_verb(cli: &Cli) -> ExitCode {
    let booted = match booted_with(cli, "deploy", None) {
        Ok(booted) => booted,
        Err(code) => return code,
    };
    let (Some(root), Some(deployment)) = (&booted.root, &booted.deployment) else {
        eprintln!("drt deploy: there is no root here to deploy into");
        return ExitCode::FAILURE;
    };
    match crate::deploy::deploy(root, &deployment.name, &deployment.source) {
        Ok(report) => {
            eprintln!(
                "deployed {} from {} ({} file(s))",
                deployment.name,
                deployment.source.describe(),
                report.files
            );
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("drt deploy: {e}");
            ExitCode::FAILURE
        }
    }
}

/// `drt rm`: this deployment gone from `live/`.
fn rm_verb(cli: &Cli) -> ExitCode {
    let root = match rooted(cli, "rm") {
        Ok(root) => root,
        Err(code) => return code,
    };
    // Deliberately not through `boot`: `rm` is how an operator gets out of a
    // root that will not start, so it must not need the root to resolve. The
    // name comes from `project.json` if it is readable and from the directory
    // otherwise, which is what `deployment_name` already does.
    let (inputs, _) = root.read(drt_config::resolve::Requested::Default, Vec::new());
    let project = inputs.root.as_ref().and_then(|r| r.project.as_ref());
    let name = crate::deploy::deployment_name(&root, project);
    match crate::deploy::remove(&root, &name) {
        Ok(()) => {
            eprintln!("removed {name} from live/");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("drt rm: {e}");
            ExitCode::FAILURE
        }
    }
}

/// `drt commit`: what is running, captured into `init/`, with an envelope.
fn commit_verb(cli: &Cli) -> ExitCode {
    let booted = match booted_with(cli, "commit", None) {
        Ok(booted) => booted,
        Err(code) => return code,
    };
    let (Some(root), Some(deployment)) = (&booted.root, &booted.deployment) else {
        eprintln!("drt commit: there is no root here to commit in");
        return ExitCode::FAILURE;
    };
    let (inputs, _) = root.read(drt_config::resolve::Requested::Default, Vec::new());
    let Some(project) = inputs.root.as_ref().and_then(|r| r.project.clone()) else {
        eprintln!(
            "drt commit: this root has no project.json, so a commit has no root_id to record"
        );
        return ExitCode::FAILURE;
    };
    match crate::deploy::commit(root, &deployment.name, &project) {
        Ok(committed) => {
            eprintln!(
                "committed {} file(s) into init/\nenvelope: {}",
                committed.report.files, committed.hash
            );
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("drt commit: {e}");
            ExitCode::FAILURE
        }
    }
}

fn start_verb(cli: &Cli, profile: Option<&str>, rm: bool, args: &[String]) -> ExitCode {
    let overrides = match drt_config::resolve::parse_overrides(args) {
        Ok(overrides) => overrides,
        Err(e) => {
            eprintln!("drt start: {e}");
            return ExitCode::FAILURE;
        }
    };
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let mut ask = crate::consent_gate::Terminal;
    let booted = match crate::boot::boot_with(
        &cwd,
        cli.root.as_deref(),
        cli.config.as_deref(),
        profile,
        overrides,
        crate::consent_gate::Flags {
            yes: cli.yes,
            accept_changes: cli.accept_changes,
        },
        &mut ask,
    ) {
        Ok(booted) => booted,
        Err(e) => {
            eprintln!("drt start: {e}");
            return ExitCode::FAILURE;
        }
    };

    // A native stdlib program is the host's to run, not an instance's. `boot`
    // has already decided which it is, so this reads the answer rather than
    // re-deriving it from the entry -- one place that knows.
    if let (Some(crate::boot::Runnable::Native(name)), Some(resolution)) =
        (&booted.runnable, &booted.resolution)
    {
        if *name == crate::stdlib::PREFLIGHT {
            let pin = resolution.pin.as_ref().map(|p| p.value.as_str());
            return match crate::stdlib::preflight(
                resolution,
                pin,
                env!("CARGO_PKG_VERSION"),
                resolution.consent.as_ref(),
                &mut std::io::stdout(),
            ) {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("drt start: {e}");
                    ExitCode::FAILURE
                }
            };
        }
    }

    // Deploy, then run. `live/` is what runs, so this is the step that makes
    // `start` different from `run` -- and the already-deployed message is where
    // a reader learns that difference.
    if let (Some(root), Some(deployment)) = (&booted.root, &booted.deployment) {
        if deployment.deployed && !rm {
            let name = &deployment.name;
            eprintln!("'{name}' is already deployed");
            eprintln!();
            eprintln!("   drt run         # run {name} from live");
            eprintln!("   drt rm          # delete {name} from live");
            // Relative to the root, which is how the profile wrote it: an
            // absolute path here is four lines of noise in the message an
            // operator sees most often.
            let from = deployment
                .source
                .path()
                .strip_prefix(&root.dir)
                .unwrap_or(deployment.source.path());
            eprintln!(
                "   drt start --rm  # start {name} from {} (overwrites live/{name})",
                from.display()
            );
            return ExitCode::FAILURE;
        }
        if let Err(e) = crate::deploy::deploy(root, &deployment.name, &deployment.source) {
            eprintln!("drt start: {e}");
            return ExitCode::FAILURE;
        }
    }

    match start::start(&booted.config, booted.dispatcher) {
        // Ok means the swarm drained: every instance exited. For a
        // server-shaped deployment that never happens and foreground-forever
        // is the contract; for a batch-shaped one this is the finish line.
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("drt start: {e}");
            ExitCode::FAILURE
        }
    }
}

/// The native binary: assemble, then run the verb to completion, sleeping
/// where the driver says to.
pub fn main(cli: Cli) -> ExitCode {
    // Before the first byte goes out: on Windows this is what keeps the C
    // core's `print` and this file's `eprintln!` on the same line ending.
    // A no-op everywhere else.
    drt_platform::stdio::bytes_as_written();
    // `start` assembles its own, before anything else here runs. A rooted
    // deployment's config is the *resolved profile's* and not `--config`'s, so
    // going through `assemble` first would wire one set of connectors to throw
    // away and announce `exec` twice on a config that names it.
    match cli.command {
        Command::Start {
            ref profile,
            rm,
            ref args,
        } => return start_verb(&cli, profile.as_deref(), rm, args),
        Command::Deploy => return deploy_verb(&cli),
        Command::Rm => return rm_verb(&cli),
        Command::Commit => return commit_verb(&cli),
        _ => {}
    }
    let (config, dispatcher) = match assemble(&cli) {
        Ok(pair) => pair,
        Err(e) => {
            eprintln!("drt: {e}");
            return ExitCode::FAILURE;
        }
    };
    match cli.command {
        Command::Run {
            ref target,
            file,
            command,
            profile,
        } => run_verb(
            &cli,
            target.as_deref(),
            explicit(file, command, profile),
            &config,
            dispatcher,
        ),
        // The root verbs are handled before `assemble` (see `main`), so these
        // arms cannot be reached. Kept as named unreachables rather than
        // deleted, because clap's `Command` must still be exhaustive here and a
        // `_` arm would swallow the next verb somebody adds.
        Command::Start { .. } | Command::Deploy | Command::Rm | Command::Commit => {
            unreachable!("the root verbs are dispatched before assemble")
        }
        #[cfg(feature = "wireguard")]
        Command::Wg {
            action: WgAction::Keygen,
        } => {
            // Two lines, in the order a config wants them, and on stdout
            // so `drt wg keygen | head -1` is a private key and nothing
            // else. A key printed among prose is a key someone will paste
            // with the prose.
            let (private, public) = crate::wireguard::keygen();
            println!("{private}");
            println!("{public}");
            ExitCode::SUCCESS
        }
        #[cfg(feature = "wireguard")]
        Command::Wg {
            action: WgAction::Pubkey,
        } => {
            let mut key = String::new();
            if let Err(e) = std::io::Read::read_to_string(&mut std::io::stdin(), &mut key) {
                eprintln!("drt wg pubkey: cannot read the key from stdin: {e}");
                return ExitCode::FAILURE;
            }
            // The label is the field a reader would go and look at, not the
            // verb they just typed: `parse_key` prefixes it, and "drt wg
            // pubkey: drt wg pubkey: ..." helps nobody.
            match crate::wireguard::parse_key("the key on stdin", &key) {
                Ok(bytes) => {
                    println!(
                        "{}",
                        crate::wireguard::public_key(&gotatun::x25519::StaticSecret::from(bytes))
                    );
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("drt wg pubkey: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        #[cfg(feature = "wireguard")]
        Command::Wg {
            action: WgAction::Check,
        } => {
            let Some(wg_config) = config.wireguard.clone() else {
                eprintln!("drt wg check: the config names no `wireguard` block");
                return ExitCode::FAILURE;
            };
            match crate::wireguard::validate(&wg_config) {
                Ok(warnings) => {
                    for warning in &warnings {
                        eprintln!("drt wg check: {warning}");
                    }
                    // A warning is not a failure: the config works the
                    // moment the route exists, and an exit code that said
                    // otherwise would fail a deploy over a note.
                    let userspace = wg_config.mode == drt_config::WireguardMode::Userspace;
                    println!(
                        "ok: {} on port {}, {} peer(s){}",
                        if userspace {
                            "userspace"
                        } else {
                            &wg_config.interface
                        },
                        wg_config.listen_port,
                        wg_config.peers.len(),
                        if warnings.is_empty() {
                            String::new()
                        } else {
                            format!(", {} warning(s)", warnings.len())
                        }
                    );
                    // What the mode reaches, from the config alone. No
                    // port is bound: `check` promises not to create
                    // anything, and a bound-and-released TCP port is a
                    // side effect it should keep not having.
                    if userspace {
                        let forwards: Vec<String> = wg_config
                            .forward
                            .iter()
                            .map(|f| format!("{} -> {}", f.bind, f.to))
                            .collect();
                        let exposes: Vec<String> = wg_config
                            .expose
                            .iter()
                            .map(|e| format!("{} -> {}", e.tunnel, e.to))
                            .collect();
                        println!(
                            "    no interface; {}{}{}",
                            if forwards.is_empty() {
                                String::new()
                            } else {
                                format!("forwards {}", forwards.join(", "))
                            },
                            if forwards.is_empty() || exposes.is_empty() {
                                ""
                            } else {
                                "; "
                            },
                            if exposes.is_empty() {
                                String::new()
                            } else {
                                format!("exposes {}", exposes.join(", "))
                            }
                        );
                    }
                    // A config with no peers is the one a rendezvous
                    // writes, so `ok: ... 0 peer(s)` reads like a config
                    // that forgot something. Say what it will actually do
                    // instead of leaving the operator to guess.
                    if wg_config.peers.is_empty() {
                        println!(
                            "    no peers named: it will {}, {} measure its \
                             mapping, and wait for `add` on {}",
                            if userspace {
                                "run a stack in this process".to_string()
                            } else {
                                format!("create {}", wg_config.interface)
                            },
                            match &wg_config.address {
                                Some(cidr) => format!("give it {cidr},"),
                                None => "which needs an address before it carries \
                                         anything,"
                                    .into(),
                            },
                            if wg_config.reply_queue.is_empty() {
                                "a reply_queue it does not have"
                            } else {
                                &wg_config.reply_queue
                            }
                        );
                    }
                    // The config is one question and this machine is
                    // another, so they get different words and the exit
                    // code follows only the first. `check` exists so a
                    // config can be written on a laptop and deployed where
                    // the privilege is; a laptop with no tun node is not a
                    // bad config, and failing over one would break the
                    // workflow the verb is for. Saying nothing was the old
                    // behaviour, and it let `ok:` be read as "this will
                    // work here" twice in one evening (issue #21).
                    // On stderr, beside the config's own warnings, for the
                    // same reason they are: stdout is the verdict and
                    // stderr is what qualifies it.
                    //
                    // Nothing to establish for a userspace stack: it
                    // touches neither `/dev/net/tun` nor `CapEff`, so a
                    // `here:` line about either would be an answer to a
                    // question the config did not ask.
                    let here = if userspace {
                        Vec::new()
                    } else {
                        crate::wireguard::interface_here()
                    };
                    for finding in &here {
                        eprintln!("here: {finding}");
                    }
                    if !here.is_empty() {
                        eprintln!(
                            "      `ok` is the config; `here` is this machine, and the \
                             exit code follows the config."
                        );
                    }
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("drt wg check: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        #[cfg(feature = "tunnel")]
        Command::Tunnel {
            url,
            local,
            listen,
            to,
            park,
            extra_root,
        } => {
            // The file's block and the flags, judged together and before
            // anything is bound: which mode this is, and which keys
            // disagree, by name.
            let resolved = match crate::tunnel::resolve(
                config.tunnel.as_ref(),
                &crate::tunnel::Flags {
                    url,
                    local,
                    listen,
                    to,
                    park,
                    extra_root,
                },
            ) {
                Ok(resolved) => resolved,
                Err(e) => {
                    eprintln!("drt tunnel: {e}");
                    return ExitCode::FAILURE;
                }
            };
            // Read and parse before anything is dialed, so a wrong path is
            // a refusal by name rather than a TLS error on the first
            // connection -- named as the flag or the config key, whichever
            // the operator wrote.
            let roots = match crate::roots::load_roots_named(
                resolved.extra_roots_key,
                &resolved.extra_roots,
            ) {
                Ok(roots) => roots,
                Err(e) => {
                    eprintln!("drt tunnel: {e}");
                    return ExitCode::FAILURE;
                }
            };
            let runtime = tokio::runtime::Runtime::new().expect("a tokio runtime");
            let outcome = runtime.block_on(crate::tunnel::run(resolved.mode, &roots));
            // Leak the runtime rather than drop it. tokio 1.53.1 has a
            // use-after-free in runtime teardown — `BlockingPool::shutdown`
            // racing a worker's `park::Inner::unpark` into a freed Condvar
            // (backtrace in doc/Release.md) — and every one of these verbs
            // resolves a hostname through `lookup_host`, which is a
            // `spawn_blocking`, so there is always a parked blocking worker
            // to race. The process is exiting; the OS reclaims everything
            // drop would have. Leaking costs nothing and removes the whole
            // class from shipped code.
            std::mem::forget(runtime);
            match outcome {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("drt tunnel: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        Command::Buildinfo { json } => {
            print!("{}", buildinfo(json));
            ExitCode::SUCCESS
        }
        Command::Repl { unsafe_stdlib } => {
            match repl::repl(
                std::sync::Arc::new(dispatcher),
                config::ceiling(&config),
                config.root.budget,
                unsafe_stdlib,
            ) {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("drt repl: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        #[cfg(feature = "netcheck")]
        Command::Netcheck {
            stun,
            reflect,
            reflect_at,
            port,
            probe_at,
            pin_source_port,
            udp_port,
            extra_root,
            json,
        } => {
            // Before the runtime and before any measurement: a wrong path
            // should cost nothing and be named, not surface as a TLS error
            // partway through a diagnostic.
            let roots = match crate::roots::load_roots(&extra_root) {
                Ok(roots) => roots,
                Err(e) => {
                    eprintln!("drt netcheck: {e}");
                    return ExitCode::FAILURE;
                }
            };
            let ports: Vec<u16> = port.clone();
            let inputs = crate::netcheck::Inputs {
                stun,
                reflect,
                reflect_at,
                port: ports,
                probe_at,
                pin_source_port,
                udp_port,
            };
            let runtime = tokio::runtime::Runtime::new().expect("a tokio runtime");
            // One implementation of the measurement order, shared with the
            // `netcheck` config block. The order is not obvious -- the
            // configuration fetch before the measurements, UDP before reflect so
            // STUN's address wins, the inbound probe last because it needs the
            // reflect views -- so having it twice would mean having it wrong
            // once.
            let m = runtime.block_on(crate::netcheck::run(&inputs, &roots));
            // The same leak as `stun`/`relay`/`tunnel`, for the same reason:
            // FM-1, tokio 1.53.1's use-after-free in runtime teardown, and
            // `detect_mapping` resolves through `lookup_host`, so there is
            // always a parked blocking worker to race. See doc/Failure-Modes.md.
            std::mem::forget(runtime);

            let (verdict, why) = crate::netcheck::decide(&m);
            if json {
                print!("{}", crate::netcheck::render_json(&m, verdict, why));
            } else {
                print!("{}", crate::netcheck::render_text(&m, verdict, why));
            }
            // A verdict is a success, `relay` included. Only "nothing could
            // be measured at all" is a failure, and that is the case where
            // there was no network to ask rather than a network that
            // answered badly.
            //
            // `probed_anything` and not `udp_mapping.is_none() &&
            // routable_v6.is_none()`, which is what this was: holding a
            // routable v6 address is read off the routing table and costs no
            // packet, so a machine with v6 whose STUN probes all failed
            // exited 0 while every evidence line that involved asking the
            // network said `not measured`. The exit status is what a script
            // reads, so it has to mean what it says.
            if !m.probed_anything() {
                return ExitCode::FAILURE;
            }
            ExitCode::SUCCESS
        }
        Command::Key { ref action } => key_verb(&cli, action),
        Command::Ps => {
            // Unlike the REPL, `ps` has nothing it can do standalone: its
            // whole subject is a deployment already running in another
            // process, which is the control endpoint's to reach.
            eprintln!(
                "drt ps: not built yet — it reaches a running deployment over the \
                 control endpoint, which lands with sshd (SPEC.md §13a)"
            );
            ExitCode::FAILURE
        }
    }
}
