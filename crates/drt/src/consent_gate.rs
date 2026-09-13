//! The start-time consent gate (consent.md §4).
//!
//! `drt-config` decides *what the situation is* — `consent::check` returns
//! one of five answers and `widen_check` is the one relation behind the two
//! that matter. This file does what a terminal can do about it: print the
//! ceiling, ask, honour `-y` and `--accept-changes`, write the entry, and
//! refuse by name when there is nobody to ask.
//!
//! Named `consent_gate` because `drt_config::consent` is the format and this
//! is the gate; one of them has to say which.
//!
//! ## surface block
//!
//! - Entry points: [`gate`], the whole of start-time consent; [`Flags`], the
//!   two command-line answers; [`Ask`], the seam a terminal fills and a test
//!   replaces.
//! - Configurable values: none. Every decision here comes from
//!   `drt_config::consent::check`.
//! - Fan-out: [`gate`]'s match over `ConsentCheck` is the whole policy, one
//!   arm per answer, and it is the only place the five are distinguished.
//!
//! **`-y` accepts a first acceptance and nothing else.** That asymmetry is
//! the point of the file. A `-y` that also accepted a *widening* would mean
//! every systemd unit and CI job in existence carried permanent pre-consent
//! to every future ceiling the root might declare — which defeats the
//! approval chain's invariant in practice while leaving it true on paper.
//! Widening takes `--accept-changes`, typed by somebody who read the delta.

use std::io::Write;

use drt_caps::{Effect, Grant};
use drt_config::consent::{Accepted, ConsentCheck, ConsentJson};
use drt_config::project::ProjectJson;
use drt_config::realm::Realm;

use crate::drt_root::{self, Root};

/// What the command line said about consent.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Flags {
    /// `-y`: accept a **first** acceptance without asking. Deliberately not
    /// enough for a widening.
    pub yes: bool,
    /// `--accept-changes`: accept a widened ceiling without asking. The
    /// deliberate act, separate from `-y` so that a unit file cannot carry it
    /// by accident.
    pub accept_changes: bool,
}

/// Where a question goes and where the answer comes from.
///
/// A seam rather than direct stdin because "is anyone there to ask" has
/// three different answers — a terminal, a pipe, and a page — and the rule
/// for all three is the same: never hang, never assume yes.
pub trait Ask {
    /// Is there an interactive terminal? `false` for a pipe, a unit file, and
    /// a page.
    fn interactive(&self) -> bool;
    /// Print to the operator. stderr, so a redirected stdout stays clean.
    fn say(&mut self, line: &str);
    /// Ask, and read the answer. Only called when [`Ask::interactive`].
    fn confirm(&mut self, question: &str) -> std::io::Result<bool>;
}

/// Run the gate. `Ok(())` means start may proceed.
pub fn gate(
    root: &Root,
    project: &ProjectJson,
    consent: Option<&ConsentJson>,
    flags: Flags,
    ask: &mut dyn Ask,
) -> Result<(), String> {
    let check = drt_config::consent::check(project, consent).map_err(|e| e.to_string())?;
    match check {
        // Blanket consent. Nothing fires, now or ever -- that is what `--all`
        // is. The ceiling having moved since is worth one line, because a
        // reader who took the blanket entry a year ago should be able to find
        // that out without running audit.
        ConsentCheck::Blanket {
            ceiling_changed, ..
        } => {
            if ceiling_changed {
                ask.say(
                    "note: blanket operator consent is in force, and this root's ceiling has \
                     changed since it was accepted. `dollup audit` prints the date.",
                );
            }
            Ok(())
        }

        // The hash matches. Silent is the whole requirement.
        ConsentCheck::Unchanged => Ok(()),

        ConsentCheck::First { ceiling_hash } => {
            if !flags.yes {
                ask.say(&format!(
                    "{} declares a ceiling this operator has not accepted:",
                    project.project_name.as_deref().unwrap_or("this root")
                ));
                for line in describe(&project.caps) {
                    ask.say(&format!("    {line}"));
                }
                ask.say("");
                ask.say(
                    "A ceiling bounds every grant any node in this root can ever hold. \
                     Accepting it does not grant anything.",
                );
                if !confirm(ask, "accept this ceiling?", "-y", "")? {
                    return Err("the ceiling was not accepted; nothing started".to_string());
                }
            }
            write_entry(root, project, consent, ceiling_hash)
        }

        // Removals only. Silent, and the record catches up so the next start
        // compares against what is actually declared.
        ConsentCheck::Narrowed { ceiling_hash, .. } => {
            write_entry(root, project, consent, ceiling_hash)
        }

        ConsentCheck::Widened {
            ceiling_hash,
            objection,
            change,
        } => {
            if !flags.accept_changes {
                ask.say(&format!(
                    "the ceiling of {} has widened since it was accepted:",
                    project.project_name.as_deref().unwrap_or("this root")
                ));
                for line in change.lines() {
                    ask.say(&format!("    {line}"));
                }
                ask.say("");
                if !confirm(
                    ask,
                    "accept the widened ceiling?",
                    "--accept-changes",
                    &objection.to_string(),
                )? {
                    return Err(format!(
                        "the widened ceiling was not accepted; nothing started ({objection})"
                    ));
                }
            }
            write_entry(root, project, consent, ceiling_hash)
        }
    }
}

// depth: asking, and the three ways there is nobody to ask

/// Ask, or fail by name. Never hang, and never assume yes.
///
/// `detail` rides on the *error* and not on the prompt, and it is the **only**
/// place the objection appears.
///
/// An earlier version printed it here and again in the block above, on the
/// argument that a supervisor reads the two separately. That argument was
/// wrong: both go to stderr, so it was plain duplication three lines apart.
/// The split that is real is delta versus objection -- the `+`/`-` lines say
/// what an operator is being asked to accept, and the objection says why it
/// needs accepting, which belongs on the line that actually stops the process
/// and survives a truncated `systemctl status`.
fn confirm(ask: &mut dyn Ask, question: &str, flag: &str, detail: &str) -> Result<bool, String> {
    if !ask.interactive() {
        let because = if detail.is_empty() {
            String::new()
        } else {
            format!(" {detail}.")
        };
        return Err(format!(
            "{question} -- but there is no terminal to ask.{because} Pass {flag} to answer in \
             advance, or run `dollup consent` where somebody can read it."
        ));
    }
    ask.confirm(question)
        .map_err(|e| format!("cannot read an answer from the terminal: {e}"))
}

/// Write or replace this root's acceptance at the root realm.
///
/// Replaces rather than appends: an `accepted` list accumulating one entry
/// per ceiling edit would make "which one governs" a question, and
/// `root_entry` answers it by realm. The ceiling is stored verbatim beside
/// its hash because a hash cannot be inverted and §4's delta needs the
/// contents.
fn write_entry(
    root: &Root,
    project: &ProjectJson,
    existing: Option<&ConsentJson>,
    ceiling_hash: drt_config::canon::Hash,
) -> Result<(), String> {
    let mut consent = match existing {
        Some(consent) => consent.clone(),
        None => ConsentJson::new(project.root_id),
    };
    consent.accepted.retain(|e| e.realm() != &Realm::root());
    consent.accepted.push(Accepted::Listed {
        realm: Realm::root(),
        ceiling_hash,
        ceiling: drt_config::project::declared_ceiling(project),
        accepted_at: drt_root::now(),
    });
    root.write_consent(&consent)
}

/// A ceiling, one grant per line, for somebody deciding whether to accept it.
///
/// Denies first and labelled. A reader scanning for what this root may do
/// needs to see what it may *not* do beside it, and a list where the two are
/// only distinguishable by a field name is a list that gets misread.
pub fn describe(caps: &[Grant]) -> Vec<String> {
    let mut lines: Vec<String> = caps
        .iter()
        .filter(|g| g.effect == Effect::Deny)
        .map(|g| format!("deny  {}", g.capability))
        .collect();
    lines.extend(
        caps.iter()
            .filter(|g| g.effect == Effect::Grant)
            .map(|g| format!("grant {}", g.capability)),
    );
    lines
}

/// The real terminal: stderr for everything it says, stdin for the answer.
pub struct Terminal;

impl Ask for Terminal {
    fn interactive(&self) -> bool {
        std::io::IsTerminal::is_terminal(&std::io::stdin())
    }

    fn say(&mut self, line: &str) {
        let _ = writeln!(std::io::stderr(), "{line}");
    }

    fn confirm(&mut self, question: &str) -> std::io::Result<bool> {
        let mut err = std::io::stderr();
        write!(err, "{question} [y/N] ")?;
        err.flush()?;
        let mut answer = String::new();
        std::io::stdin().read_line(&mut answer)?;
        Ok(matches!(answer.trim(), "y" | "Y" | "yes" | "Yes"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testfs::{self, Seeded};

    /// A scripted operator: what it was told, and what it answers.
    #[derive(Default)]
    struct Scripted {
        interactive: bool,
        answer: bool,
        said: Vec<String>,
        asked: Vec<String>,
    }

    impl Ask for Scripted {
        fn interactive(&self) -> bool {
            self.interactive
        }
        fn say(&mut self, line: &str) {
            self.said.push(line.to_string());
        }
        fn confirm(&mut self, question: &str) -> std::io::Result<bool> {
            self.asked.push(question.to_string());
            Ok(self.answer)
        }
    }

    impl Scripted {
        fn saying_yes() -> Scripted {
            Scripted {
                interactive: true,
                answer: true,
                ..Scripted::default()
            }
        }
        fn saying_no() -> Scripted {
            Scripted {
                interactive: true,
                answer: false,
                ..Scripted::default()
            }
        }
        fn transcript(&self) -> String {
            self.said.join("\n")
        }
    }

    fn project(caps: Vec<Grant>) -> ProjectJson {
        ProjectJson {
            project_name: Some("my_drt_project".into()),
            caps,
            ..ProjectJson::new(Seeded::root_id())
        }
    }

    /// consent.md acceptance 1, the first half: see the ceiling, accept, and
    /// start again silently.
    #[test]
    fn a_first_start_prints_the_ceiling_accepts_and_is_then_silent() {
        let seeded = testfs::seed(true, &[]);
        let project = project(vec![
            Grant::grant("host:fs/*"),
            Grant::deny("host:fs/remove"),
        ]);

        let mut ask = Scripted::saying_yes();
        gate(&seeded.root, &project, None, Flags::default(), &mut ask).unwrap();

        let transcript = ask.transcript();
        assert!(transcript.contains("host:fs/*"), "{transcript}");
        assert!(transcript.contains("deny  host:fs/remove"), "{transcript}");
        assert_eq!(ask.asked.len(), 1, "asked exactly once");

        let written = seeded.consent().expect("an entry was written");
        assert_eq!(written.root_id, project.root_id);
        assert!(matches!(
            written.root_entry(),
            Some(Accepted::Listed { .. })
        ));

        // Again: silent, nothing asked.
        let mut again = Scripted::saying_no();
        gate(
            &seeded.root,
            &project,
            Some(&written),
            Flags::default(),
            &mut again,
        )
        .unwrap();
        assert!(again.asked.is_empty(), "a matching hash asks nothing");
        assert!(again.said.is_empty(), "and says nothing");
    }

    #[test]
    fn declining_a_first_ceiling_starts_nothing() {
        let seeded = testfs::seed(true, &[]);
        let project = project(vec![Grant::grant("host:fs/*")]);
        let mut ask = Scripted::saying_no();
        let e = gate(&seeded.root, &project, None, Flags::default(), &mut ask).unwrap_err();
        assert!(e.contains("not accepted"), "{e}");
        assert!(seeded.consent().is_none(), "and nothing was written");
    }

    /// `-y` covers a first acceptance.
    #[test]
    fn minus_y_accepts_a_first_ceiling_without_asking() {
        let seeded = testfs::seed(true, &[]);
        let project = project(vec![Grant::grant("host:fs/*")]);
        let mut ask = Scripted::default(); // not interactive: a unit file
        gate(
            &seeded.root,
            &project,
            None,
            Flags {
                yes: true,
                ..Flags::default()
            },
            &mut ask,
        )
        .unwrap();
        assert!(ask.asked.is_empty());
        assert!(seeded.consent().is_some());
    }

    /// consent.md acceptance 1, the part `-y` must not satisfy. This is the
    /// whole asymmetry of the file.
    #[test]
    fn minus_y_does_not_accept_a_widening_and_accept_changes_does() {
        let seeded = testfs::seed(true, &[]);
        let narrow = project(vec![Grant::grant("host:fs/read")]);
        gate(
            &seeded.root,
            &narrow,
            None,
            Flags {
                yes: true,
                ..Flags::default()
            },
            &mut Scripted::default(),
        )
        .unwrap();
        let accepted = seeded.consent().unwrap();

        let wider = project(vec![
            Grant::grant("host:fs/read"),
            Grant::grant("host:exec/run"),
        ]);

        // `-y` on a unit file: refused, by name, and nothing starts.
        let mut unit = Scripted::default();
        let e = gate(
            &seeded.root,
            &wider,
            Some(&accepted),
            Flags {
                yes: true,
                ..Flags::default()
            },
            &mut unit,
        )
        .unwrap_err();
        assert!(e.contains("--accept-changes"), "{e}");
        assert!(e.contains("no terminal"), "it says why it cannot ask: {e}");
        // The delta is on stderr and the objection is on the refusal, each in
        // one place: an earlier version printed the objection twice, three
        // lines apart, on an argument about channels that did not hold.
        assert!(
            unit.transcript().contains("+ host:exec/run"),
            "{}",
            unit.transcript()
        );
        assert_eq!(
            unit.transcript().matches("may only narrow").count(),
            0,
            "the objection is not also on stderr: {}",
            unit.transcript()
        );
        assert!(e.contains("host:exec/run"), "{e}");

        // An operator at a terminal sees the delta and the objection.
        let mut operator = Scripted::saying_yes();
        gate(
            &seeded.root,
            &wider,
            Some(&accepted),
            Flags::default(),
            &mut operator,
        )
        .unwrap();
        let transcript = operator.transcript();
        assert!(transcript.contains("+ host:exec/run"), "{transcript}");
        assert!(transcript.contains("widened"), "{transcript}");

        // Or `--accept-changes`, with nobody there.
        let accepted = seeded.consent().unwrap();
        let mut ci = Scripted::default();
        gate(
            &seeded.root,
            &wider,
            Some(&accepted),
            Flags {
                accept_changes: true,
                ..Flags::default()
            },
            &mut ci,
        )
        .unwrap();
        assert!(ci.asked.is_empty());
    }

    /// Narrowing is silent, and the record catches up so the next start
    /// compares against what is declared now.
    #[test]
    fn narrowing_is_silent_and_the_record_catches_up() {
        let seeded = testfs::seed(true, &[]);
        let wide = project(vec![Grant::grant("host:fs/*"), Grant::grant("host:time")]);
        gate(
            &seeded.root,
            &wide,
            None,
            Flags {
                yes: true,
                ..Flags::default()
            },
            &mut Scripted::default(),
        )
        .unwrap();

        let narrowed = project(vec![Grant::grant("host:fs/*")]);
        let mut ask = Scripted::default();
        gate(
            &seeded.root,
            &narrowed,
            seeded.consent().as_ref(),
            Flags::default(),
            &mut ask,
        )
        .unwrap();
        assert!(ask.asked.is_empty(), "narrowing asks nothing");
        assert!(ask.said.is_empty(), "and says nothing");

        // The stored ceiling is the narrowed one, so starting again is
        // Unchanged rather than a second silent rewrite.
        let stored = seeded.consent().unwrap();
        assert!(matches!(
            drt_config::consent::check(&narrowed, Some(&stored)).unwrap(),
            ConsentCheck::Unchanged
        ));
        assert_eq!(stored.accepted.len(), 1, "replaced, not appended");
    }

    /// Dropping a deny adds nothing to the allow set, and must still prompt.
    /// The hole an additive diff would have left open, at the gate this time.
    #[test]
    fn dropping_a_deny_prompts_at_the_gate() {
        let seeded = testfs::seed(true, &[]);
        let fenced = project(vec![
            Grant::grant("host:fs/*"),
            Grant::deny("host:fs/remove"),
        ]);
        gate(
            &seeded.root,
            &fenced,
            None,
            Flags {
                yes: true,
                ..Flags::default()
            },
            &mut Scripted::default(),
        )
        .unwrap();

        let unfenced = project(vec![Grant::grant("host:fs/*")]);
        let mut ask = Scripted::default();
        let e = gate(
            &seeded.root,
            &unfenced,
            seeded.consent().as_ref(),
            Flags {
                yes: true,
                ..Flags::default()
            },
            &mut ask,
        )
        .unwrap_err();
        assert!(
            e.contains("host:fs/remove"),
            "the refusal names the deny that was dropped: {e}"
        );
        assert!(e.contains("--accept-changes"), "{e}");
        assert!(
            ask.transcript().contains("- deny host:fs/remove"),
            "and the delta on stderr shows the deny that went away: {}",
            ask.transcript()
        );
    }

    /// consent.md acceptance 4.
    #[test]
    fn no_tty_and_no_flag_fails_by_name_rather_than_hanging() {
        let seeded = testfs::seed(true, &[]);
        let project = project(vec![Grant::grant("host:fs/*")]);
        let mut ask = Scripted::default();
        let e = gate(&seeded.root, &project, None, Flags::default(), &mut ask).unwrap_err();
        assert!(e.contains("no terminal"), "{e}");
        assert!(e.contains("-y"), "it names the flag that answers: {e}");
        assert!(ask.asked.is_empty(), "nothing was asked, so nothing hung");
        assert!(seeded.consent().is_none());
    }

    /// Blanket consent never prompts, and says once when the ceiling moved.
    #[test]
    fn blanket_consent_never_prompts() {
        let seeded = testfs::seed(true, &[]);
        let project = project(vec![
            Grant::grant("host:fs/*"),
            Grant::grant("host:exec/run"),
        ]);
        let consent = ConsentJson {
            root_id: project.root_id,
            accepted: vec![Accepted::All {
                realm: Realm::root(),
                accepted_against: drt_config::project::caps_hash(&[Grant::grant("host:fs/*")])
                    .unwrap(),
                accepted_at: drt_root::now(),
            }],
            signers: Vec::new(),
        };

        let mut ask = Scripted::default();
        gate(
            &seeded.root,
            &project,
            Some(&consent),
            Flags::default(),
            &mut ask,
        )
        .unwrap();
        assert!(ask.asked.is_empty());
        assert!(
            ask.transcript().contains("blanket"),
            "the moved ceiling is said once: {}",
            ask.transcript()
        );
    }

    #[test]
    fn a_root_id_mismatch_stops_start_by_name() {
        let seeded = testfs::seed(true, &[]);
        let project = project(vec![Grant::grant("host:fs/*")]);
        let other = ConsentJson::new(
            drt_config::id::Uuid7::parse("0192f0c1-8000-7000-8000-0000000012ef").unwrap(),
        );
        assert_ne!(other.root_id, project.root_id, "a different root entirely");
        let e = gate(
            &seeded.root,
            &project,
            Some(&other),
            Flags {
                yes: true,
                accept_changes: true,
            },
            &mut Scripted::saying_yes(),
        )
        .unwrap_err();
        assert!(e.contains("never travels"), "{e}");
        // And no flag gets past it: a mismatch is not a question.
        assert!(seeded
            .fs
            .files()
            .iter()
            .all(|(p, _)| !p.ends_with("consent.json")));
    }
}
