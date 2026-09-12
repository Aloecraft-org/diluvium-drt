//! What happens before a deployment's first step: find the root, resolve what
//! would run, gate it on consent, and hand `start` a config and a dispatcher.
//!
//! The one place the three layers meet. `drt-config` decides (`resolve`,
//! `consent::check`), `drt_root` reads and writes, `consent_gate` asks — and
//! this is the order they go in, which is itself a decision: the ceiling is
//! checked before a connector is wired, so a root whose consent has lapsed
//! never reaches the point of opening a socket.
//!
//! ## surface block
//!
//! - Entry points: [`boot`], the whole of it; [`Booted`], what `start` gets.
//! - Configurable values: none. Every name and every order comes from
//!   `drt_config` or from the four rules below.
//! - Fan-out: [`entry_program`] is where an entry becomes something to run,
//!   one arm per [`Entry`] kind, and the only place `stdlib:` is resolved.
//!
//! Four rules, in the order they fire:
//!
//! 1. **No root is the no-root path**, unchanged: `--config` with its own caps
//!    as the ceiling, or the wide local default with neither. The wide default
//!    lives there and nowhere else.
//!
//! And one rule that is not about order: **what runs is `live/`**. A file entry
//! resolves to `live/<name>/<entry>`, never to `dlua_dir` or `init/`. Those are
//! where a deploy reads *from*; nothing loads out of either, which is what makes
//! `drt deploy` a step an operator can see rather than something `start` does
//! invisibly — and what makes an edit not take effect until it is deployed.
//! 2. **A root's findings are reported before anything runs**, all of them, and
//!    the first blocking one stops start. Audit prints the same list from the
//!    same function; this is the runtime's end of "they cannot drift".
//! 3. **Consent is gated before connectors are wired** — except for a native
//!    stdlib entry, which runs nothing and so holds nothing. A ceiling nobody
//!    has accepted should not get as far as binding a port; but
//!    `drt start preflight` exists to tell an operator *whether* consent
//!    matches, and a preflight that refused to run until consent was settled
//!    could never answer the question it is for. Consent bounds the grants a
//!    root's nodes can hold, and a report holds none.
//! 4. **`--config` inside a root attenuates under the root's ceiling.**
//!    `--config` is never a consent bypass, and the refusal is the same
//!    attenuation check a spawn makes.

use drt_config::resolve::{Entry, Requested, Resolution};
use drt_config::{Program, RootConfig};

use crate::consent_gate::{self, Ask};
use crate::drt_root::{self, Nesting, Root};

/// What `start` needs to begin.
pub struct Booted {
    pub config: RootConfig,
    pub dispatcher: drt_connector::Dispatcher,
    /// The root this is a deployment of, when there is one. `None` is the
    /// no-root path.
    pub root: Option<Root>,
    /// What resolved and why, for the report a preflight profile prints.
    pub resolution: Option<Resolution>,
    /// What to run. `None` on the no-root path, where the config names its own
    /// program exactly as it always has.
    pub runnable: Option<Runnable>,
    /// Which subdirectory of `live/` this deployment uses, and whether
    /// something is already there. `None` on the no-root path.
    pub deployment: Option<Deployment>,
}

/// A root's deployment: what it is called under `live/`, where a deploy would
/// read from, and whether it is there now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Deployment {
    pub name: String,
    pub source: crate::deploy::Source,
    pub deployed: bool,
}

/// Hand-written because a [`drt_connector::Dispatcher`] holds trait objects
/// and is not `Debug`. What a reader wants from a failed boot is which root,
/// which profile and which program -- the dispatcher is the same wiring in
/// every case and says nothing.
impl std::fmt::Debug for Booted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Booted")
            .field("root", &self.root.as_ref().map(|r| &r.dir))
            .field(
                "profile",
                &self
                    .resolution
                    .as_ref()
                    .and_then(|r| r.profile.as_ref())
                    .map(|p| p.value.to_string()),
            )
            .field("program", &self.config.root.program)
            .finish_non_exhaustive()
    }
}

/// Everything before the first step, with no command-line arguments for the
/// entry. The shape every verb but `start` wants.
pub fn boot(
    cwd: &std::path::Path,
    root_flag: Option<&std::path::Path>,
    config_flag: Option<&std::path::Path>,
    profile: Option<&str>,
    consent: consent_gate::Flags,
    ask: &mut dyn Ask,
) -> Result<Booted, String> {
    boot_with(
        cwd,
        root_flag,
        config_flag,
        profile,
        Vec::new(),
        consent,
        ask,
    )
}

/// [`boot`] with arguments for the entry, which only `start` has.
#[allow(clippy::too_many_arguments)]
pub fn boot_with(
    cwd: &std::path::Path,
    root_flag: Option<&std::path::Path>,
    config_flag: Option<&std::path::Path>,
    profile: Option<&str>,
    overrides: drt_config::resolve::Overrides,
    consent: consent_gate::Flags,
    ask: &mut dyn Ask,
) -> Result<Booted, String> {
    let Some(root) = drt_root::discover(cwd, root_flag) else {
        // Rule 1. Nothing about a root applies, and neither does consent.
        let mut config = crate::config::load(config_flag)?;
        if config_flag.is_none() {
            crate::cli::local_defaults(&mut config);
        }
        if let Some(name) = profile {
            return Err(format!(
                "there is no root here, so '{name}' names no profile; \
                 `--config <path>` is the self-contained form"
            ));
        }
        if !overrides.is_empty() {
            // A config's `args` block is merged by resolution, and there is no
            // resolution on this path. Saying so beats accepting flags and
            // dropping them.
            return Err(
                "arguments for the entry come from a profile's declared `args`, and there is no \
                 root here to resolve one"
                    .to_string(),
            );
        }
        let dispatcher = wire(&config)?;
        return Ok(Booted {
            config,
            dispatcher,
            root: None,
            resolution: None,
            runnable: None,
            deployment: None,
        });
    };

    let requested = match profile {
        Some(name) => Requested::Profile(name.to_string()),
        None => Requested::Default,
    };
    let (mut inputs, soft) = root.read(requested, overrides);
    for line in &soft {
        ask.say(&format!("drt start: {line}"));
    }
    inputs.config_flag = match config_flag {
        Some(path) => Some(crate::config::load(Some(path))?),
        None => None,
    };

    let resolution = drt_config::resolve::resolve(&inputs);
    // Rule 2. The blocker stops start, and the non-blocking findings do not
    // reach stderr.
    //
    // They are in the `Resolution`, and `drt start preflight` and `dollup
    // audit` print every one. That division is deliberate: "no drt version is
    // pinned" is worth reporting and is not worth saying on every single start
    // of an unpinned root, which is the nagging this repository has a standing
    // rule against. Start acts; the two report surfaces report. The cost is
    // real and is named here rather than discovered: an operator who edits a
    // config that `profiles` does not declare learns why from preflight, not
    // from the start that ignored it.
    if let Some(blocker) = resolution.blocker() {
        return Err(blocker.to_string());
    }

    let project = inputs
        .root
        .as_ref()
        .and_then(|r| r.project.as_ref())
        .ok_or_else(|| {
            "this directory has a .drt_root but no project.json; `dollup init` writes one, or \
             name a config with --config"
                .to_string()
        })?;

    nesting(&root, project.allow_nested, ask)?;

    let (mut config, runnable, deployment) = profile_config(&root, &inputs, &resolution, project)?;

    // Rule 3. Before a connector is wired, and not at all for a report.
    //
    // The resolution already carries what `consent::check` answered, so
    // preflight prints the consent situation rather than being stopped by it.
    // The first shape of this file gated first and resolved second, which made
    // `drt start preflight` on an unconsented root refuse with "no terminal to
    // ask" -- the one question preflight exists to answer, unanswerable.
    if !matches!(runnable, Runnable::Native(_)) {
        let stored = inputs.root.as_ref().and_then(|r| r.consent.as_ref());
        consent_gate::gate(&root, project, stored, consent, ask)?;
    }

    // Rule 4. A `--config` inside a root narrows, never widens.
    if let Some(flag) = &inputs.config_flag {
        let ceiling = drt_config::InstanceConfig {
            caps: project.caps.clone(),
            ..drt_config::InstanceConfig::default()
        };
        flag.root
            .check_attenuation(&ceiling)
            .map_err(|e| format!("--config is not a consent bypass: {e}"))?;
        config = flag.clone();
    }

    // The ceiling is the root's, whatever the profile asked for: the profile
    // has already been checked to attenuate under it, and running the
    // intersection rather than the declaration is what makes that check
    // load-bearing instead of advisory.
    let dispatcher =
        wire(&config)?.with_grants(std::sync::Arc::new(crate::gsr::Desk::new(root.clone())));
    Ok(Booted {
        config,
        dispatcher,
        root: Some(root),
        resolution: Some(resolution),
        runnable: Some(runnable),
        deployment: Some(deployment),
    })
}

// depth: the pieces, each one rule

fn wire(config: &RootConfig) -> Result<drt_connector::Dispatcher, String> {
    let registry = crate::cli::wire_connectors(config)?;
    crate::config::validate_grants(config, &registry)?;
    Ok(drt_connector::Dispatcher::new(registry))
}

/// Say something about being nested, or refuse, per the inner root's policy.
fn nesting(
    root: &Root,
    policy: drt_config::project::AllowNested,
    ask: &mut dyn Ask,
) -> Result<(), String> {
    match root.nesting(policy) {
        Nesting::Free | Nesting::Allowed { .. } => Ok(()),
        Nesting::Warn { outer } => {
            ask.say(&format!(
                "drt start: this root is nested beneath {} -- they are peers, not parent and \
                 child, and neither knows about the other. Set `allow_nested` to silence this.",
                outer.display()
            ));
            Ok(())
        }
        Nesting::Refused { outer } => Err(format!(
            "this root is nested beneath {} and its `allow_nested` is \"error\"",
            outer.display()
        )),
    }
}

/// The resolved profile's config, with its entry turned into a program.
fn profile_config(
    root: &Root,
    inputs: &drt_config::resolve::ResolveInputs,
    resolution: &Resolution,
    project: &drt_config::project::ProjectJson,
) -> Result<(RootConfig, Runnable, Deployment), String> {
    let rooted = inputs.root.as_ref().expect("a root was resolved");
    let name = resolution
        .profile
        .as_ref()
        .ok_or("no profile resolved, and nothing said why")?;
    let filename = name.value.filename();
    let mut config = rooted
        .profiles
        .get(&filename)
        .cloned()
        .ok_or_else(|| format!("'{filename}' is declared but not in profile/"))?;
    let entry = resolution
        .entry
        .as_ref()
        .ok_or_else(|| format!("profile '{}' declares no entry", name.value))?;
    let deployment = Deployment {
        name: crate::deploy::deployment_name(root, Some(project)),
        source: crate::deploy::source_for(root, &config),
        deployed: false,
    };
    let deployment = Deployment {
        deployed: crate::deploy::is_deployed(root, &deployment.name),
        ..deployment
    };
    // The config handed on is the *resolved* one, so `start` delivers what the
    // command line actually merged rather than the profile's declared defaults.
    config.args = resolution.args.clone();
    let runnable = entry_runnable(root, &deployment, &entry.value)?;
    // A native program is not loaded, so the config names none. `start` would
    // otherwise refuse a deployment with no program, which is the right
    // refusal for every case except this one.
    if let Runnable::Program(program) = &runnable {
        config.root.program = Some(program.clone());
    }
    Ok((config, runnable, deployment))
}

/// What an entry turns out to be.
///
/// Three kinds rather than two, because a native stdlib program is **not** a
/// program in the swarm sense: `preflight`'s content is a `Resolution` this
/// process already holds, and there is nothing to load into an instance. An
/// earlier version of this folded natives into `Program::Source` by way of a
/// `source()` lookup that returned `None` for them, which produced the
/// genuinely absurd "this build carries no stdlib program called 'preflight';
/// it carries preflight" -- two registries disagreeing about the same name.
/// One enum, one lookup.
#[derive(Debug, Clone, PartialEq)]
pub enum Runnable {
    /// Load and run it: a file under `dlua_dir`, or stdlib source.
    Program(Program),
    /// The host runs it itself and nothing is loaded.
    Native(&'static str),
}

/// An entry, as something to run.
///
/// A file entry resolves **under `live/`**, not under `dlua_dir`: those are
/// where a deploy reads from, and what runs is what was deployed. Resolution has
/// already checked that the entry exists in the source, so the failure worth
/// naming here is the stdlib half.
pub fn entry_runnable(
    root: &Root,
    deployment: &Deployment,
    entry: &Entry,
) -> Result<Runnable, String> {
    match entry {
        Entry::File(path) => Ok(Runnable::Program(Program::Path(
            crate::deploy::live_path(root, &deployment.name).join(path),
        ))),
        Entry::Stdlib(name) => match crate::stdlib::lookup(name) {
            Some(crate::stdlib::Kind::Native(native)) => Ok(Runnable::Native(native)),
            Some(crate::stdlib::Kind::Source(src)) => {
                Ok(Runnable::Program(Program::Source(src.to_string())))
            }
            None => Err(format!(
                "this build carries no stdlib program called '{name}'; it carries {}",
                crate::stdlib::names().join(", ")
            )),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testfs;
    use drt_config::resolve::Rule;
    use std::path::PathBuf;

    /// A recording operator that never answers, so every test here is about
    /// what boot does before anybody is asked.
    #[derive(Default)]
    struct Quiet {
        said: Vec<String>,
    }

    impl Ask for Quiet {
        fn interactive(&self) -> bool {
            false
        }
        fn say(&mut self, line: &str) {
            self.said.push(line.to_string());
        }
        fn confirm(&mut self, _: &str) -> std::io::Result<bool> {
            unreachable!("no test here answers a prompt")
        }
    }

    impl Quiet {
        fn transcript(&self) -> String {
            self.said.join("\n")
        }
    }

    fn yes() -> consent_gate::Flags {
        consent_gate::Flags {
            yes: true,
            accept_changes: false,
        }
    }

    #[test]
    fn a_rooted_start_resolves_its_profile_and_runs_its_entry() {
        let seeded = testfs::seed(false, &[]);
        let mut ask = Quiet::default();
        let booted = boot(
            std::path::Path::new(testfs::DIR),
            None,
            None,
            None,
            yes(),
            &mut ask,
        )
        .expect("it boots");

        assert!(booted.root.is_some());
        let resolution = booted.resolution.as_ref().unwrap();
        assert_eq!(
            resolution.profile.as_ref().unwrap().rule,
            Rule::DefaultProfile
        );
        assert_eq!(
            booted.config.root.program,
            Some(Program::Path(PathBuf::from(
                "/r/.drt_root/live/my_drt_project/app.dlua"
            ))),
            "what runs is live/, not the authoring directory"
        );
        let deployment = booted.deployment.as_ref().unwrap();
        assert_eq!(deployment.name, "my_drt_project");
        assert_eq!(deployment.source.describe(), "dlua_dir");
        assert!(!deployment.deployed, "nothing has been deployed yet");
        // Consent was accepted by `-y`, so the file is there for next time.
        assert!(seeded.consent().is_some());
    }

    /// A report is not a deployment: `drt start preflight` on a root nobody
    /// has consented to still reports, because telling an operator whether
    /// consent matches is the whole job. The first version of this gated first
    /// and could not answer its own question.
    #[test]
    fn a_native_entry_reports_rather_than_gating_on_consent() {
        let seeded = testfs::seed(
            false,
            &[(
                "/r/.drt_root/profile/preflight.config.json",
                r#"{"entry":"stdlib:preflight"}"#,
            )],
        );
        // Declare it, since `profiles` is authoritative.
        seeded.fs.add_file(
            "/r/.drt_root/project.json",
            format!(
                r#"{{"root_id":"{}","project_name":"r","project_version":"0.0.0","drt":"0.5.0",
                    "caps":[{{"effect":"grant","capability":"host:fs/*"}}],
                    "default_profile":"debug",
                    "profiles":["debug.config.json","preflight.config.json"]}}"#,
                testfs::ROOT_ID
            ),
        );

        let mut ask = Quiet::default();
        let booted = boot(
            std::path::Path::new(testfs::DIR),
            None,
            None,
            Some("preflight"),
            // No `-y`, nobody to ask: a deployment would refuse here.
            consent_gate::Flags::default(),
            &mut ask,
        )
        .expect("a report does not need consent");

        assert_eq!(booted.runnable, Some(Runnable::Native("preflight")));
        assert!(
            booted.config.root.program.is_none(),
            "nothing is loaded for a native program"
        );
        // And the report has the consent answer to print.
        assert!(matches!(
            booted.resolution.as_ref().unwrap().consent,
            Some(drt_config::consent::ConsentCheck::First { .. })
        ));
        assert!(
            seeded.consent().is_none(),
            "and it accepted nothing on the operator's behalf"
        );
    }

    /// Rule 3: a ceiling nobody accepted does not get as far as a connector.
    #[test]
    fn consent_is_gated_before_anything_is_wired() {
        let _seeded = testfs::seed(false, &[]);
        let mut ask = Quiet::default();
        let e = boot(
            std::path::Path::new(testfs::DIR),
            None,
            None,
            None,
            consent_gate::Flags::default(),
            &mut ask,
        )
        .unwrap_err();
        assert!(e.contains("no terminal"), "{e}");
        assert!(e.contains("-y"), "{e}");
    }

    /// Rule 4, and consent.md acceptance 5.
    #[test]
    fn a_config_flag_inside_a_root_may_not_widen_the_ceiling() {
        let _seeded = testfs::seed(
            false,
            &[(
                "/tmp/wide.json",
                r#"{"caps":[{"effect":"grant","capability":"host:exec/run"}],
                    "dlua_dir":"dlua/","entry":"app.dlua"}"#,
            )],
        );
        let mut ask = Quiet::default();
        let e = boot(
            std::path::Path::new(testfs::DIR),
            None,
            Some(std::path::Path::new("/tmp/wide.json")),
            None,
            yes(),
            &mut ask,
        )
        .unwrap_err();
        assert!(e.contains("consent bypass"), "{e}");
        assert!(e.contains("host:exec/run"), "it names the grant: {e}");
    }

    /// Rule 2: every finding is reported, not only the one that stops it.
    #[test]
    fn only_the_blocker_reaches_stderr() {
        let _seeded = testfs::seed(
            false,
            &[(
                "/r/.drt_root/project.json",
                r#"{"root_id":"0192f0c1-8000-7000-8000-00000000abcd",
                    "caps":[{"effect":"grant","capability":"host:fs/*"}],
                    "default_profile":"debug","profiles":["debug.config.json","gone.config.json"]}"#,
            )],
        );
        let mut ask = Quiet::default();
        let e = boot(
            std::path::Path::new(testfs::DIR),
            None,
            None,
            None,
            yes(),
            &mut ask,
        )
        .unwrap_err();

        // The blocker is what stops it, and is what is said.
        assert!(e.contains("gone.config.json"), "{e}");
        // The non-blocking ones are in the resolution for preflight and audit,
        // and are deliberately not on stderr: an unpinned root should not nag
        // on every start.
        assert!(
            !ask.transcript().contains("no project_name"),
            "start is quiet about what does not stop it: {}",
            ask.transcript()
        );
    }

    /// An unknown stdlib name says what the build does carry, rather than
    /// failing as though the entry were a missing file.
    #[test]
    fn an_unknown_stdlib_entry_names_what_this_build_carries() {
        let seeded = testfs::seed(
            false,
            &[(
                "/r/.drt_root/profile/debug.config.json",
                r#"{"entry":"stdlib:nonesuch"}"#,
            )],
        );
        let deployment = Deployment {
            name: "p".into(),
            source: crate::deploy::Source::Init(seeded.root.init()),
            deployed: false,
        };
        let e = entry_runnable(&seeded.root, &deployment, &Entry::Stdlib("nonesuch".into()))
            .unwrap_err();
        assert!(e.contains("nonesuch"), "{e}");
        assert!(e.contains("preflight"), "it lists what is here: {e}");

        // And the name this build *does* carry resolves, rather than being
        // reported absent by a second registry that disagreed with the first.
        assert_eq!(
            entry_runnable(
                &seeded.root,
                &deployment,
                &Entry::Stdlib("preflight".into())
            )
            .unwrap(),
            Runnable::Native("preflight")
        );
    }

    /// No root is the no-root path, and a profile name there is a named
    /// failure rather than a silent ignore.
    #[test]
    fn no_root_is_the_no_root_path() {
        let _seeded = testfs::seed(true, &[("/elsewhere/app.dlua", "print('hi')\n")]);
        let mut ask = Quiet::default();
        let booted = boot(
            std::path::Path::new("/elsewhere"),
            None,
            None,
            None,
            consent_gate::Flags::default(),
            &mut ask,
        )
        .expect("no root, no consent");
        assert!(booted.root.is_none());
        assert!(booted.resolution.is_none());

        let e = boot(
            std::path::Path::new("/elsewhere"),
            None,
            None,
            Some("debug"),
            consent_gate::Flags::default(),
            &mut ask,
        )
        .unwrap_err();
        assert!(e.contains("no root here"), "{e}");
    }

    /// Nesting warns by default and does not stop anything.
    #[test]
    fn nesting_warns_by_default() {
        let _seeded = testfs::seed(false, &[("/r/nested/.drt_root/project.json", "{}")]);
        let inner = Root {
            dir: PathBuf::from("/r/nested"),
        };
        let mut ask = Quiet::default();
        nesting(&inner, drt_config::project::AllowNested::Warn, &mut ask).unwrap();
        assert!(ask.transcript().contains("peers"), "{}", ask.transcript());

        let e = nesting(&inner, drt_config::project::AllowNested::Error, &mut ask).unwrap_err();
        assert!(e.contains("allow_nested"), "{e}");
    }
}
