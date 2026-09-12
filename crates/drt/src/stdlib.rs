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
//! **What is not here, stated rather than implied.** `tunnel` and `netcheck`
//! belong here by the plan and are not built: the first needs raw stdin as a
//! guest-reachable byte stream and the second needs a `netcheck` config block
//! whose verdict lands on a queue. Neither exists yet, so neither is listed —
//! a name that resolves to nothing would be worse than a name that is absent.

use drt_config::resolve::Resolution;

/// The setup report: what `start` would resolve, printed, with nothing run.
pub const PREFLIGHT: &str = "preflight";

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
        _ => None,
    }
}

/// Every name this build carries, for a refusal that can say what it does have.
/// Filtered through [`lookup`], so the list cannot advertise what does not
/// resolve.
pub fn names() -> Vec<&'static str> {
    [PREFLIGHT]
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
        assert_eq!(names(), ["preflight"]);
        assert_eq!(lookup("preflight"), Some(Kind::Native("preflight")));
        assert!(is_native("preflight"));

        // Not built, and not pretended: a name that resolved to nothing would
        // be worse than a name that is absent.
        assert!(lookup("tunnel").is_none());
        assert!(!is_native("tunnel"));
    }
}
