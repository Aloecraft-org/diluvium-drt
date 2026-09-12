//! Programs this binary carries: thin readers, diagnostics, preflight.
//!
//! Reached as `stdlib:<name>` — on the command line and in a profile's `entry`
//! — which is a spelling that cannot collide with a filename and so needs no
//! place in `drt run`'s bare-token rules.
//!
//! These run with **no root, no cache and no dollup**. That is the whole
//! reason they exist: a verb that used to be a subcommand has to stay
//! reachable on a box holding one static binary, and a program inside the
//! binary is reachable there by construction.
//!
//! ## surface block
//!
//! - Entry points: [`lookup`], a name to a program; [`names`], what this build
//!   carries; [`preflight`], the one program that is not Diluvium source.
//! - Configurable values: [`PREFLIGHT`], the name of that program.
//! - Fan-out: [`lookup`]'s match **is** the registry, and [`names`] is derived
//!   from nothing else. A program added to this build is added in one place.
//!   An earlier shape had `source()` and `names()` as two lists, which
//!   disagreed immediately: `names()` advertised `preflight` and `source()`
//!   answered `None` for it, so an entry naming it was refused with "this build
//!   carries no stdlib program called 'preflight'; it carries preflight".
//!
//! **What a reader is not.** `relay`, `stun`, `turn` and `wg` name blocks that
//! *arbitrate* as well as report: a `relay` block naming `reply_queue` is asking
//! a program to decide whether each leg is admitted, and silence inside
//! `admit_timeout_ms` is itself a refusal. These readers print and do not
//! answer, so a config that asks for arbitration and runs one of them refuses
//! every leg — visibly, and failing closed, which is the right way round. A
//! deployment that arbitrates wants its own program; that is what a program is
//! for.
//!
//! **What is not here, stated rather than implied.** `tunnel` belongs here by
//! the plan and is not built: the ProxyCommand shape needs raw stdin as a
//! guest-reachable byte stream, and nothing offers one. It is therefore not
//! listed — a name that resolves to nothing would be worse than a name that is
//! absent — and `drt --config <file>` with a `tunnel` block is the path that
//! does work.

use drt_config::resolve::Resolution;

/// The setup report: what `start` would resolve, printed, with nothing run.
pub const PREFLIGHT: &str = "preflight";

/// The NAT diagnostic's reader: wait for the verdict the `netcheck` block
/// measures, print it, stop.
pub const NETCHECK: &str = "netcheck";

/// The four server-shaped blocks' readers. Each is the program half of a block
/// `drt start` already serves: the block binds and measures, the program keeps
/// the deployment alive and prints what the block reports.
pub const RELAY: &str = "relay";
pub const STUN: &str = "stun";
pub const TURN: &str = "turn";
pub const WG: &str = "wg";

/// One reader, four blocks.
///
/// A deployment is config plus a program, and these four blocks used to be
/// verbs precisely because they had no program to be the other half of. This is
/// that half: declare the block's report queue, print whatever lands, and never
/// return — a server-shaped deployment is foreground-forever, and the program
/// that exits is the one that takes the ports with it.
///
/// `capacity = 16` and not 1: these blocks report on a timer and a slow
/// terminal must not be backpressure on a relay's accounting.
fn reader(queue: &str) -> String {
    format!(
        r#"
-- The `{queue}` block's reports, printed. The block does the work; this keeps
-- the deployment alive and says what it hears.
local q = queue.declare("{queue}", {{ capacity = 16 }})
while true do
    local _, report = queue.wait({{q}})
    print(report)
    local more = queue.pop(q)
    while more ~= nil do
        print(more)
        more = queue.pop(q)
    end
end
"#
    )
}

/// `stdlib:netcheck`, as Diluvium.
///
/// A **thin reader**, which is the whole point: the measurement is the host's --
/// it needs UDP sockets and a TLS stack, which no connector offers a guest --
/// and what used to be a subcommand is now a config block plus these nine lines.
/// Nothing here reimplements anything, and that is why this is a stdlib program
/// rather than a port.
///
/// It waits rather than polling: a measurement takes seconds, and a program that
/// looped would be a program burning a deployment's instruction budget on an
/// answer that is not there yet.
#[cfg(feature = "netcheck")]
const NETCHECK_SOURCE: &str = r#"
-- Wait for the verdict the `netcheck` block measures, print it, and stop.
local q = queue.declare("netcheck", { capacity = 1 })
local _, answer = queue.wait({q})

print("verdict: " .. tostring(answer.verdict))
if answer.why ~= nil then print("  why: " .. tostring(answer.why)) end
if answer.advice ~= nil then print("  " .. tostring(answer.advice)) end
"#;

/// What a stdlib name resolves to.
///
/// Two kinds, because not every program here is a guest. `preflight`'s whole
/// content is a `Resolution` — Rust data this process already holds — and
/// serializing that through an args queue so a guest could format it would buy
/// nothing but a second place for the wording to drift.
///
/// Named `Kind` and not `Program`: `drt_config::Program` is what an instance
/// loads, and two types called `Program` in one call chain is the collision
/// this repository keeps finding the hard way.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Diluvium source, loaded into an instance like any other program.
    Source(&'static str),
    /// The host runs it; nothing is loaded.
    Native(&'static str),
}

/// The registry. One match, so there is one answer to "what does this build
/// carry" and it cannot disagree with itself.
pub fn lookup(name: &str) -> Option<Kind> {
    match name {
        PREFLIGHT => Some(Kind::Native(PREFLIGHT)),
        // Only where the block exists. A name that resolved to a reader with
        // nothing to read would be worse than a name that is absent, so each of
        // these is gated on the feature that carries the block it reads.
        #[cfg(feature = "netcheck")]
        NETCHECK => Some(Kind::Source(NETCHECK_SOURCE)),
        #[cfg(feature = "relay")]
        RELAY => Some(Kind::Source(relay_source())),
        #[cfg(feature = "stun")]
        STUN => Some(Kind::Source(stun_source())),
        #[cfg(feature = "turn")]
        TURN => Some(Kind::Source(turn_source())),
        #[cfg(feature = "wireguard")]
        WG => Some(Kind::Source(wg_source())),
        _ => None,
    }
}

// depth: the four readers, each built once and leaked into a `&'static str`
//
// `reader` formats, and a `Kind::Source` is `&'static str` because a program's
// source outlives the process that loads it. Four `OnceLock`s rather than four
// hand-written constants so the shape is written once: a fifth block is a
// default queue name and one line here.
macro_rules! reader_source {
    ($name:ident, $queue:expr) => {
        fn $name() -> &'static str {
            static SOURCE: std::sync::OnceLock<String> = std::sync::OnceLock::new();
            SOURCE.get_or_init(|| reader($queue)).as_str()
        }
    };
}

#[cfg(feature = "relay")]
reader_source!(relay_source, "relay_in");
#[cfg(feature = "stun")]
reader_source!(stun_source, "stun_in");
#[cfg(feature = "turn")]
reader_source!(turn_source, "turn_in");
#[cfg(feature = "wireguard")]
reader_source!(wg_source, "wg_in");

/// Every name this build carries, for a refusal that can say what it does have.
/// Filtered through [`lookup`], so the list cannot advertise what does not
/// resolve.
pub fn names() -> Vec<&'static str> {
    [PREFLIGHT, NETCHECK, RELAY, STUN, TURN, WG]
        .into_iter()
        .filter(|n| lookup(n).is_some())
        .collect()
}

/// Is this a program the host runs itself rather than loading into an instance?
pub fn is_native(name: &str) -> bool {
    matches!(lookup(name), Some(Kind::Native(_)))
}

/// `stdlib:preflight`: resolve exactly what `start` would resolve, print it,
/// and run nothing.
///
/// The answer to "is this root set up right" for somebody who has drt and not
/// dollup. It is the same [`Resolution`] `start` acts on and `dollup audit`
/// reports, so the three cannot disagree about what would happen — which is
/// the property the whole shape of `resolve` exists to give.
pub fn preflight(
    resolution: &Resolution,
    pinned: Option<&str>,
    present: &str,
    consent: Option<&drt_config::consent::ConsentCheck>,
    out: &mut dyn std::io::Write,
) -> std::io::Result<()> {
    if let Some(profile) = &resolution.profile {
        writeln!(out, "profile: {profile}")?;
    }
    match pinned {
        Some(pin) => writeln!(out, "drt: {pin} pinned, {present} present")?,
        None => writeln!(out, "drt: not pinned, {present} present")?,
    }
    if let Some(ceiling) = &resolution.ceiling {
        write!(out, "ceiling: {} caps", ceiling.value.len())?;
        match consent {
            Some(drt_config::consent::ConsentCheck::Unchanged) => {
                writeln!(out, ", consent: listed, matches")?
            }
            Some(drt_config::consent::ConsentCheck::First { .. }) => {
                writeln!(out, ", consent: not yet accepted")?
            }
            Some(drt_config::consent::ConsentCheck::Narrowed { .. }) => {
                writeln!(out, ", consent: listed, narrowed since")?
            }
            Some(drt_config::consent::ConsentCheck::Widened { .. }) => writeln!(
                out,
                ", consent: listed, WIDENED since -- start would prompt"
            )?,
            Some(drt_config::consent::ConsentCheck::Blanket { .. }) => {
                writeln!(out, ", consent: blanket operator consent")?
            }
            None => writeln!(out, ", consent: does not apply (no root)")?,
        }
    }
    if let Some(entry) = &resolution.entry {
        writeln!(out, "entry: {}, present", entry.value)?;
    }
    if !resolution.args.is_empty() {
        let rendered: Vec<String> = resolution
            .args
            .iter()
            .map(|(k, v)| format!("{k}: {}", render(v)))
            .collect();
        writeln!(out, "args: {{ {} }}", rendered.join(", "))?;
    }
    for finding in &resolution.findings {
        writeln!(out, "- {finding}")?;
    }
    match resolution.blocker() {
        Some(blocker) => writeln!(out, "start would refuse: {blocker}"),
        None => writeln!(out, "start would run. nothing started."),
    }
}

fn render(value: &drt_config::resolve::ArgValue) -> String {
    use drt_config::resolve::ArgValue;
    match value {
        ArgValue::Bool(b) => b.to_string(),
        ArgValue::Int(n) => n.to_string(),
        ArgValue::Str(s) => s.clone(),
        ArgValue::List(items) => format!("[{}]", items.join(", ")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use drt_config::resolve::{ArgValue, Decided, Entry, Rule};

    fn resolution() -> Resolution {
        Resolution {
            profile: Some(Decided::new(
                drt_config::project::ProfileName::new("debug").unwrap(),
                Rule::DefaultProfile,
            )),
            entry: Some(Decided::new(
                Entry::File("app.dlua".into()),
                Rule::FromProfile,
            )),
            ceiling: Some(Decided::new(
                vec![drt_caps::Grant::grant("host:fs/*")],
                Rule::ProjectCeiling,
            )),
            args: [("verbose".to_string(), ArgValue::Bool(false))]
                .into_iter()
                .collect(),
            ..Resolution::default()
        }
    }

    fn rendered(
        resolution: &Resolution,
        consent: Option<&drt_config::consent::ConsentCheck>,
    ) -> String {
        let mut out = Vec::new();
        preflight(resolution, Some("0.5.0"), "0.5.0", consent, &mut out).unwrap();
        String::from_utf8(out).unwrap()
    }

    /// The report in the design doc, line for line, including the parenthetical
    /// -- which is the whole reason every resolved field carries its rule.
    #[test]
    fn preflight_says_what_resolved_and_why_and_starts_nothing() {
        let report = rendered(
            &resolution(),
            Some(&drt_config::consent::ConsentCheck::Unchanged),
        );
        assert!(
            report.contains("profile: debug (default_profile)"),
            "{report}"
        );
        assert!(
            report.contains("drt: 0.5.0 pinned, 0.5.0 present"),
            "{report}"
        );
        assert!(
            report.contains("ceiling: 1 caps, consent: listed, matches"),
            "{report}"
        );
        assert!(report.contains("entry: app.dlua, present"), "{report}");
        assert!(report.contains("args: { verbose: false }"), "{report}");
        assert!(
            report.ends_with("start would run. nothing started.\n"),
            "{report}"
        );
    }

    /// A widened ceiling is said out loud, because the point of running this is
    /// to find out *before* the restart that would prompt.
    #[test]
    fn a_widened_ceiling_is_reported_as_something_start_would_prompt_on() {
        let report = rendered(
            &resolution(),
            Some(&drt_config::consent::ConsentCheck::Widened {
                ceiling_hash: drt_config::project::caps_hash(&[]).unwrap(),
                objection: drt_config::consent::Objection(
                    drt_caps::AttenuationError::NotHeldByParent {
                        capability: "host:exec/run".into(),
                    },
                ),
                change: drt_config::consent::Change::default(),
            }),
        );
        assert!(report.contains("WIDENED"), "{report}");
        assert!(report.contains("start would prompt"), "{report}");
    }

    #[test]
    fn a_blocking_finding_is_what_it_says_start_would_do() {
        let mut resolution = resolution();
        resolution
            .findings
            .push(drt_config::resolve::Finding::NoCeiling);
        let report = rendered(&resolution, None);
        assert!(
            report.contains("- this root declares no ceiling"),
            "{report}"
        );
        assert!(report.contains("start would refuse:"), "{report}");
    }

    /// The registry is honest about what is absent: a name that resolved to
    /// nothing would be worse than a name that is not listed.
    /// The reader is nine lines and reimplements nothing: the measurement needs
    /// UDP sockets and a TLS stack, which no connector offers a guest, so it
    /// stays the host's and this reads its answer.
    #[cfg(feature = "netcheck")]
    #[test]
    fn the_netcheck_reader_only_reads() {
        let source = match lookup(NETCHECK) {
            Some(Kind::Source(source)) => source,
            other => panic!("netcheck is Diluvium source, got {other:?}"),
        };
        assert!(
            source.contains("queue.declare(\"netcheck\""),
            "it reads the block's queue"
        );
        assert!(
            source.contains("queue.wait"),
            "it waits rather than polling"
        );
        assert!(
            source.contains("answer.verdict"),
            "and indexes a table, not text"
        );
        assert!(
            !source.contains("host.call"),
            "a reader makes no hostcalls: the measurement is the host's"
        );
        assert!(
            source.lines().filter(|l| !l.trim().is_empty()).count() < 12,
            "thin enough to read at a glance"
        );
    }

    /// The four server-block readers, each against the queue its block
    /// actually reports on. A reader wired to the wrong queue name is a
    /// deployment that starts, serves, and prints nothing forever -- which
    /// looks exactly like a relay with no traffic, so nothing would say so.
    #[test]
    fn each_reader_declares_the_queue_its_block_reports_on() {
        // The defaults in `drt_config`. Written out rather than imported so
        // that a rename over there fails here, loudly, instead of the two
        // moving together into agreement about the wrong name.
        for (name, queue, built) in [
            (RELAY, "relay_in", cfg!(feature = "relay")),
            (STUN, "stun_in", cfg!(feature = "stun")),
            (TURN, "turn_in", cfg!(feature = "turn")),
            (WG, "wg_in", cfg!(feature = "wireguard")),
        ] {
            let Some(kind) = lookup(name) else {
                assert!(!built, "'{name}' is built and must resolve");
                continue;
            };
            assert!(built, "'{name}' resolved in a build without its block");
            let Kind::Source(source) = kind else {
                panic!("'{name}' is a reader, so Diluvium source, got {kind:?}");
            };
            assert!(
                source.contains(&format!("queue.declare(\"{queue}\"")),
                "'{name}' must read {queue}, got: {source}"
            );
            assert!(
                source.contains("queue.wait"),
                "'{name}' waits rather than polling"
            );
            // The distinction the module header draws. A reader that wrote
            // back would be arbitrating, and arbitration that is nine lines
            // long is the kind nobody reviews.
            assert!(
                !source.contains("queue.push"),
                "'{name}' reports and does not answer; arbitration wants its own program"
            );
            assert!(
                !source.contains("host.call"),
                "'{name}' makes no hostcalls: the serving is the block's"
            );
        }
    }

    /// One registry, so `names` and `lookup` cannot disagree -- which they did,
    /// and the symptom was a refusal listing the very name it had just refused.
    #[test]
    fn the_registry_lists_exactly_what_it_resolves() {
        for name in names() {
            assert!(
                lookup(name).is_some(),
                "'{name}' is advertised and must resolve"
            );
        }
        assert_eq!(lookup("preflight"), Some(Kind::Native("preflight")));
        assert!(is_native("preflight"));
        assert!(names().contains(&"preflight"));

        // Each reader is there exactly when its block is, because a reader
        // with nothing to read is worse than an absent name.
        assert_eq!(cfg!(feature = "netcheck"), names().contains(&"netcheck"));
        assert_eq!(cfg!(feature = "relay"), names().contains(&"relay"));
        assert_eq!(cfg!(feature = "stun"), names().contains(&"stun"));
        assert_eq!(cfg!(feature = "turn"), names().contains(&"turn"));
        assert_eq!(cfg!(feature = "wireguard"), names().contains(&"wg"));

        // Not built, and not pretended.
        assert!(lookup("tunnel").is_none());
        assert!(!is_native("tunnel"));
    }
}
