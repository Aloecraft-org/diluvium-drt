//! What `start` would do, as a pure function over inputs.
//!
//! Three callers, one function: drt's `start`, drt's `stdlib:preflight`, and
//! dollup's `audit` — the last from a different binary, on a root it does
//! not trust, with nothing executing. "Audit reports what the runtime would
//! do" is only true if that is literally the same code, so there is no
//! filesystem in this file and no path is opened: the caller has already
//! read what [`ResolveInputs`] holds. drt reads it through `drt-platform` so
//! a page works at all; dollup reads the host's directly.
//!
//! Two properties make this usable by all three, and they are the only
//! things in here that are not obvious:
//!
//! - **Every decision carries why.** `profile: debug (default_profile)` —
//!   the parenthetical is provenance, so [`Decided`] pairs a value with the
//!   [`Rule`] that chose it. A bare value would leave preflight and audit
//!   unable to say which rule fired, and "which rule fired" is the whole
//!   content of a setup report.
//! - **Failures accumulate.** `start` stops at the first named failure;
//!   audit collects every problem and keeps going. So resolution returns a
//!   partial [`Resolution`] *with* a list of [`Finding`]s rather than a
//!   single error: `start` renders the first blocking one, audit renders
//!   them all, and both get the same wording because the wording is on the
//!   enum.
//!
//! ## surface block
//!
//! - Entry points: [`resolve`]; [`classify`], one typed argument to what it
//!   means; [`merge_args`], the command-line override rule; [`Entry::parse`].
//! - Configurable values: none here. The fallback order and the profile
//!   filename rule live in [`crate::project`], beside the reserved names.
//! - Fan-out: [`Requested`] is what the command line asked for, one arm per
//!   spelling; [`Rule`] is every reason a field was decided the way it was;
//!   [`Finding`] is every problem resolution can report.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use drt_caps::Grant;

// Consent rides the `consent` feature, so resolution compiles for the browser
// tier -- which is always on the no-root path, where consent does not apply.
// Everything else here (the profile, the entry, the args, the pin) answers the
// same on either build.
#[cfg(feature = "consent")]
use crate::consent::{self, ConsentCheck, ConsentJson};
use crate::project::{self, ProfileName, ProjectJson};
use crate::RootConfig;

/// What the root program is: a file under `dlua_dir`, or a program shipped
/// inside the binary.
///
/// One string on the wire, because `entry` is a field a human types.
/// `stdlib:` is the prefix that decides, and it fires before the bare-token
/// rule in `drt run` for the same reason it decides here: a prefix cannot be
/// ambiguous with a filename, and a bare name can.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub enum Entry {
    /// A path relative to the profile's `dlua_dir`.
    File(String),
    /// A program in drt's standard library: thin readers, diagnostics,
    /// preflight. Runs with no root, no cache and no dollup, which is what
    /// makes it the answer for a verb that used to be a subcommand.
    Stdlib(String),
}

/// The spelling that makes an entry a stdlib program rather than a file.
pub const STDLIB_PREFIX: &str = "stdlib:";

impl Entry {
    pub fn parse(text: &str) -> Result<Entry, BadEntry> {
        if let Some(name) = text.strip_prefix(STDLIB_PREFIX) {
            if name.is_empty() {
                return Err(BadEntry::EmptyStdlibName);
            }
            return Ok(Entry::Stdlib(name.to_string()));
        }
        if text.is_empty() {
            return Err(BadEntry::Empty);
        }
        Ok(Entry::File(text.to_string()))
    }

    pub fn as_written(&self) -> String {
        match self {
            Entry::File(path) => path.clone(),
            Entry::Stdlib(name) => format!("{STDLIB_PREFIX}{name}"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BadEntry {
    #[error("an entry cannot be empty")]
    Empty,
    #[error("'{STDLIB_PREFIX}' names no program; write '{STDLIB_PREFIX}<name>'")]
    EmptyStdlibName,
}

impl std::fmt::Display for Entry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.as_written())
    }
}

impl From<Entry> for String {
    fn from(e: Entry) -> String {
        e.as_written()
    }
}

impl TryFrom<String> for Entry {
    type Error = BadEntry;
    fn try_from(s: String) -> Result<Entry, BadEntry> {
        Entry::parse(&s)
    }
}

// depth: args, where the declared default's type is the parser

/// One argument value. The set of shapes a profile's `args` can declare,
/// which is also the set of shapes a command line can override.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ArgValue {
    Bool(bool),
    Int(i64),
    Str(String),
    /// A list, so repeated `--stun a --stun b` accumulates. An empty list in
    /// the profile is how a key declares itself repeatable.
    List(Vec<String>),
}

impl ArgValue {
    pub fn type_name(&self) -> &'static str {
        match self {
            ArgValue::Bool(_) => "a boolean",
            ArgValue::Int(_) => "an integer",
            ArgValue::Str(_) => "a string",
            ArgValue::List(_) => "a list",
        }
    }
}

/// Command-line overrides, as `(key, occurrences)` in the order they were
/// typed. The caller has already stripped `--`.
pub type Overrides = Vec<(String, Option<String>)>;

/// Merge command-line overrides onto a profile's declared defaults.
///
/// **The declared default's type drives parsing.** `"verbose": false` makes
/// `--verbose` a flag, `"port": 8092` makes `--port` take an integer,
/// `"stun": []` makes `--stun` repeatable and accumulating. That is not
/// cleverness for its own sake: it means a profile is the whole declaration
/// of its own command line, so a downloaded pre-populated profile is a
/// one-command setup with no second schema to ship beside it.
///
/// **A key with no default is a named failure.** A typo cannot silently add
/// a field, which is the same posture the `.host.lua` loader took toward an
/// unknown config key and for the same reason.
pub fn merge_args(
    declared: &BTreeMap<String, ArgValue>,
    overrides: &Overrides,
) -> Result<BTreeMap<String, ArgValue>, ArgError> {
    let mut merged = declared.clone();
    for (key, raw) in overrides {
        let Some(default) = declared.get(key) else {
            return Err(ArgError::UndeclaredKey {
                key: key.clone(),
                known: declared.keys().cloned().collect(),
            });
        };
        let want = |raw: &Option<String>| -> Result<String, ArgError> {
            raw.clone().ok_or_else(|| ArgError::MissingValue {
                key: key.clone(),
                wanted: default.type_name(),
            })
        };
        let value = match default {
            // A flag: present means true. `--verbose=false` is still
            // allowed, because a profile that ships `true` needs a way off.
            ArgValue::Bool(_) => match raw.as_deref() {
                None | Some("true") => ArgValue::Bool(true),
                Some("false") => ArgValue::Bool(false),
                Some(other) => {
                    return Err(ArgError::NotTheDeclaredType {
                        key: key.clone(),
                        wanted: "a boolean",
                        got: other.to_string(),
                    })
                }
            },
            ArgValue::Int(_) => {
                let text = want(raw)?;
                ArgValue::Int(text.parse().map_err(|_| ArgError::NotTheDeclaredType {
                    key: key.clone(),
                    wanted: "an integer",
                    got: text.clone(),
                })?)
            }
            ArgValue::Str(_) => ArgValue::Str(want(raw)?),
            ArgValue::List(_) => {
                // Accumulate onto whatever this key already holds, so the
                // first occurrence does not clear the declared default and
                // the second does not replace the first.
                let mut list = match merged.get(key) {
                    Some(ArgValue::List(existing)) => existing.clone(),
                    _ => Vec::new(),
                };
                list.push(want(raw)?);
                ArgValue::List(list)
            }
        };
        merged.insert(key.clone(), value);
    }
    Ok(merged)
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ArgError {
    #[error("'--{key}' is not an argument this profile declares; it declares {}", if known.is_empty() { "none".to_string() } else { known.join(", ") })]
    UndeclaredKey { key: String, known: Vec<String> },
    #[error("'--{key}' wants {wanted}")]
    MissingValue { key: String, wanted: &'static str },
    #[error("'--{key}' wants {wanted}, not '{got}'")]
    NotTheDeclaredType {
        key: String,
        wanted: &'static str,
        got: String,
    },
}

// depth: the inputs, which are everything resolution is allowed to look at

/// Which reading the command line forced, when it forced one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Explicit {
    /// `-f`
    File,
    /// `-c`
    Code,
    /// `-p`
    Profile,
}

/// Characters that are **evidence of code**, as a closed set.
///
/// Not "cannot be a path": parens are legal in paths on every system this runs
/// on, and whitespace is legal on Windows — which is why whitespace is
/// deliberately *not* in here, and why the name is evidence rather than
/// impossibility. A newline is what makes a bash heredoc work without a flag,
/// which is the case this set mostly exists for.
pub const CODE_EVIDENCE: &[char] = &['\n', '(', ')', '\'', '"', '=', ';'];

/// The token that means standard input.
pub const STDIN: &str = "-";

/// One typed argument to what it means.
///
/// The order is the whole rule and it is fixed: an explicit flag, then stdin,
/// then the `stdlib:` prefix, then a leading `/` or `./`, then evidence of
/// code, then a bare token against the declared profiles, and a file otherwise.
/// Each step is cheap to state and cheap to remember, which is the point —
/// anyone reaching past this ruleset should expect to look it up.
///
/// **A bare token is tried as a profile first and as a file second**, and a
/// listed profile wins even when a file of the same name sits in the working
/// directory. `drt run app.dlua` is a file because `app.dlua` is not a declared
/// profile name, not because it has an extension: extensions are not consulted
/// at all, so `drt run dlua-code.txt` works.
pub fn classify(token: &str, explicit: Option<Explicit>, profiles: &[ProfileName]) -> Requested {
    match explicit {
        Some(Explicit::File) => return Requested::File(token.to_string()),
        Some(Explicit::Code) => return Requested::Code(token.to_string()),
        Some(Explicit::Profile) => return Requested::Profile(token.to_string()),
        None => {}
    }
    if token == STDIN {
        return Requested::Stdin;
    }
    if let Some(name) = token.strip_prefix(STDLIB_PREFIX) {
        return Requested::Stdlib(name.to_string());
    }
    if token.starts_with('/') || token.starts_with("./") {
        return Requested::File(token.to_string());
    }
    if token.contains(CODE_EVIDENCE) {
        return Requested::Code(token.to_string());
    }
    if profiles.iter().any(|p| p.as_str() == token) {
        return Requested::Profile(token.to_string());
    }
    Requested::File(token.to_string())
}

/// What to say when a bare token turned out to be a file that is not there.
///
/// The one place the disambiguators get named. A reader who typed
/// `drt run foo` meaning code, or meaning a profile, learns here rather than
/// from the manual — which is the whole reason the fallthrough is a file and
/// not a refusal.
pub fn no_such_file(token: &str) -> String {
    format!(
        "no file '{token}'; use -c to run it as code, -p for a profile, \
         or -f to insist it is a file"
    )
}

/// What the command line asked to run.
#[derive(Debug, Clone, Default, PartialEq)]
pub enum Requested {
    /// `drt start` / `drt run` with no argument: `default_profile`, or the
    /// fallback order when there is no `project.json`.
    #[default]
    Default,
    /// `drt start debug`, `drt run -p debug`.
    Profile(String),
    /// `drt run -f <path>`, or an argument that began `./` or `/`.
    File(String),
    /// `drt run -c '<code>'`, or an argument carrying evidence of code.
    Code(String),
    /// `drt run stdlib:<name>`.
    Stdlib(String),
    /// `drt run -` : the piped-code case.
    Stdin,
}

/// A root's files, already read. `None` anywhere means "not present", which
/// is a fact resolution reports rather than an error it raises.
#[derive(Debug, Clone, Default)]
pub struct RootInputs {
    pub project: Option<ProjectJson>,
    #[cfg(feature = "consent")]
    pub consent: Option<ConsentJson>,
    /// Parsed profiles by filename — what is actually *in* `profile/`.
    /// `project.json`'s `profiles` list is the authority over which of these
    /// count, and comparing the two is how a stray config is reported.
    pub profiles: BTreeMap<String, RootConfig>,
    /// Filenames present in `profile/`, including any that failed to parse,
    /// so "the files match the declared list" can be answered even for a
    /// file resolution could not read.
    pub profile_dir: Vec<String>,
    /// Filenames under the resolved `dlua_dir`, for the entry-exists check.
    pub dlua_dir: Vec<String>,
    /// Which drt the caller has established is present, for the pin comparison.
    ///
    /// **`None` means "could not establish it", and no [`Finding::PinMismatch`]
    /// is reported** — a mismatch named against a guessed version would be
    /// worse than no line at all. The caller says what it could not do in its
    /// own words.
    ///
    /// The two callers establish it differently and neither is wrong. `drt`
    /// fills in its own `CARGO_PKG_VERSION`, because the pin is a fact about
    /// the binary that is running and that binary is this one. `dollup audit`
    /// cannot run `.drt_root/drt` to ask — audit promises no execution, on a
    /// root it does not trust — so it fills this in only when the binary's hash
    /// matches the pinned version's, and prints its own line otherwise.
    pub binary_version: Option<String>,
}

/// Everything resolution may look at.
#[derive(Debug, Clone, Default)]
pub struct ResolveInputs {
    /// `None` is the no-root path: `.drt_root/` was not in the cwd and
    /// `--root` named nothing. Discovery does not walk up, deliberately, so
    /// an unclaimed subdirectory of a root is not in that root.
    pub root: Option<RootInputs>,
    /// `--config <path>`, already read.
    pub config_flag: Option<RootConfig>,
    pub requested: Requested,
    pub overrides: Overrides,
    /// Filenames in the working directory, for the fallback order.
    pub cwd: Vec<String>,
}

/// Why a field resolved the way it did. The parenthetical in preflight's
/// output, and what lets audit say which rule fired instead of only what
/// the answer was.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Rule {
    /// Named on the command line.
    Explicit,
    /// `project.json`'s `default_profile`.
    DefaultProfile,
    /// The pre-recognized fallback order, consulted only when there is no
    /// `project.json`.
    FallbackOrder { file: String },
    /// `--config <path>`.
    ConfigFlag,
    /// The profile that resolved.
    FromProfile,
    /// `project.json`'s `caps`.
    ProjectCeiling,
    /// No root and no config: the wide local default. This is the **only**
    /// place it applies.
    NoRootDefault,
    /// A built-in default with nothing declaring otherwise.
    Default,
}

impl std::fmt::Display for Rule {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Rule::Explicit => f.write_str("named on the command line"),
            Rule::DefaultProfile => f.write_str("default_profile"),
            Rule::FallbackOrder { file } => write!(f, "fallback order, {file}"),
            Rule::ConfigFlag => f.write_str("--config"),
            Rule::FromProfile => f.write_str("from the profile"),
            Rule::ProjectCeiling => f.write_str("project.json caps"),
            Rule::NoRootDefault => f.write_str("no root, no config"),
            Rule::Default => f.write_str("default"),
        }
    }
}

/// A value and the rule that chose it.
#[derive(Debug, Clone, PartialEq)]
pub struct Decided<T> {
    pub value: T,
    pub rule: Rule,
}

impl<T> Decided<T> {
    pub fn new(value: T, rule: Rule) -> Decided<T> {
        Decided { value, rule }
    }
}

impl<T: std::fmt::Display> std::fmt::Display for Decided<T> {
    /// `debug (default_profile)` — the shape preflight prints.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} ({})", self.value, self.rule)
    }
}

/// A problem resolution found. `start` acts on the first [`Finding::blocks`]
/// one; audit renders every one.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum Finding {
    #[error("no project.json and none of {} in this directory; name a config with --config, or run dollup init", project::FALLBACK_ORDER.join(", "))]
    NothingToRun,
    #[error("this root declares no profiles")]
    NoProfilesDeclared,
    #[error("no default_profile is set, and this root declares {declared} profiles; name one")]
    NoDefaultProfile { declared: usize },
    #[error("'{name}' is not a profile this root declares; it declares {}", declared.join(", "))]
    UndeclaredProfile { name: String, declared: Vec<String> },
    #[error("'{filename}' is declared in project.json but is not in profile/")]
    DeclaredProfileMissing { filename: String },
    #[error("'{filename}' is in profile/ but not declared in project.json, so it is ignored")]
    UndeclaredProfilePresent { filename: String },
    #[error("profile '{profile}' declares no entry; a deployment is config plus a program")]
    NoEntry { profile: String },
    #[error("profile '{profile}' names entry '{entry}', which is not under dlua_dir '{dlua_dir}'")]
    EntryMissing {
        profile: String,
        entry: String,
        dlua_dir: String,
    },
    #[error("profile '{profile}' names a dlua_dir but no entry inside it")]
    DluaDirWithoutEntry { profile: String },
    #[error("the pinned drt is {pinned} and this binary is {present}")]
    PinMismatch { pinned: String, present: String },
    #[error("no drt version is pinned in project.json")]
    NoPin,
    #[error("profile '{profile}': {source}")]
    ProfileExceedsCeiling {
        profile: String,
        #[source]
        source: crate::ConfigError,
    },
    #[error("no project_name is set")]
    NoProjectName,
    #[error("no project_version is set")]
    NoProjectVersion,
    /// A root with a `project.json` states its caps or does not start. The
    /// wide default belongs to the no-root path alone, so an empty ceiling
    /// here is a refusal rather than a permission.
    #[error("this root declares no ceiling; a root with a project.json states its caps or does not start (the wide default belongs to the no-root path)")]
    NoCeiling,
    #[cfg(feature = "consent")]
    #[error(transparent)]
    Consent(#[from] consent::ConsentFailure),
    #[error(transparent)]
    Args(#[from] ArgError),
    #[error("project.json: {0}")]
    BadProfileFilename(#[from] project::BadProfileName),
}

impl Finding {
    /// Does this stop `start`?
    ///
    /// The split is the whole reason findings are values rather than
    /// strings: audit wants to print "no project_version" and keep going,
    /// and `start` has no business refusing to run over it.
    pub fn blocks(&self) -> bool {
        !matches!(
            self,
            Finding::NoProjectName
                | Finding::NoProjectVersion
                | Finding::NoPin
                | Finding::UndeclaredProfilePresent { .. }
        )
    }
}

/// What would run, why, and what is wrong.
#[derive(Debug, Clone, Default)]
pub struct Resolution {
    pub profile: Option<Decided<ProfileName>>,
    pub entry: Option<Decided<Entry>>,
    pub dlua_dir: Option<Decided<String>>,
    /// The ceiling this run is bounded by. Absent only when resolution could
    /// not get far enough to know one.
    pub ceiling: Option<Decided<Vec<Grant>>>,
    pub args: BTreeMap<String, ArgValue>,
    /// The consent situation, when there is a root to have one. `None` on
    /// the no-root path, where consent does not apply.
    #[cfg(feature = "consent")]
    pub consent: Option<ConsentCheck>,
    pub pin: Option<Decided<String>>,
    pub findings: Vec<Finding>,
}

impl Resolution {
    /// The first finding that stops `start`, if any.
    pub fn blocker(&self) -> Option<&Finding> {
        self.findings.iter().find(|f| f.blocks())
    }
}

/// What `start` would do. No IO; see the module header.
pub fn resolve(inputs: &ResolveInputs) -> Resolution {
    let mut out = Resolution::default();

    // The no-root path: `--config <path>` with no root is the self-contained
    // form, and its own caps are the ceiling. The wide default -- empty caps
    // meaning `host:*` -- belongs here and nowhere else.
    let Some(root) = &inputs.root else {
        match &inputs.config_flag {
            Some(config) => {
                take_profile_fields(&mut out, config, "--config", Rule::ConfigFlag);
                out.ceiling = Some(Decided::new(
                    if config.root.caps.is_empty() {
                        vec![Grant::grant("host:*")]
                    } else {
                        config.root.caps.clone()
                    },
                    if config.root.caps.is_empty() {
                        Rule::NoRootDefault
                    } else {
                        Rule::ConfigFlag
                    },
                ));
                merge_into(&mut out, config, &inputs.overrides);
            }
            None => match fallback(&inputs.cwd) {
                Some(_) => out.findings.push(Finding::NothingToRun),
                None => out.findings.push(Finding::NothingToRun),
            },
        }
        return out;
    };

    // Inside a root, the ceiling is project.json's caps and `--config` still
    // attenuates under it: --config is never a consent bypass.
    let Some(project) = &root.project else {
        // No project.json: the pre-recognized fallback order, which is the
        // only path where a config not named in a profiles list can run.
        match fallback(&inputs.cwd) {
            Some(file) => {
                if let Some(config) = root.profiles.get(&file).or(inputs.config_flag.as_ref()) {
                    take_profile_fields(
                        &mut out,
                        config,
                        &file,
                        Rule::FallbackOrder { file: file.clone() },
                    );
                    merge_into(&mut out, config, &inputs.overrides);
                }
            }
            None => out.findings.push(Finding::NothingToRun),
        }
        return out;
    };

    if project.project_name.is_none() {
        out.findings.push(Finding::NoProjectName);
    }
    if project.project_version.is_none() {
        out.findings.push(Finding::NoProjectVersion);
    }
    check_pin(&mut out, project, root);

    out.ceiling = Some(Decided::new(project.caps.clone(), Rule::ProjectCeiling));
    if project.caps.is_empty() {
        // Checked here rather than left to `consent::check` so the refusal is
        // the same on a build without the consent feature, and so there is one
        // spelling of it in the output rather than two.
        out.findings.push(Finding::NoCeiling);
    } else {
        #[cfg(feature = "consent")]
        match consent::check(project, root.consent.as_ref()) {
            Ok(check) => out.consent = Some(check),
            Err(e) => out.findings.push(Finding::Consent(e)),
        }
    }

    let (declared, bad) = project.declared_profiles();
    out.findings
        .extend(bad.into_iter().map(Finding::BadProfileFilename));
    check_profile_dir(&mut out, &declared, root);
    check_ceiling(&mut out, project, &declared, root);

    // Which profile runs, and by which rule.
    let picked = match &inputs.requested {
        Requested::Profile(name) => Some((name.clone(), Rule::Explicit)),
        Requested::Default => match &project.default_profile {
            Some(name) => Some((name.clone(), Rule::DefaultProfile)),
            None => {
                out.findings.push(if declared.is_empty() {
                    Finding::NoProfilesDeclared
                } else {
                    Finding::NoDefaultProfile {
                        declared: declared.len(),
                    }
                });
                None
            }
        },
        // A file, code, stdin or a stdlib program still runs under this
        // root's ceiling, via the default profile's wiring where there is
        // one. The entry is the command line's, not the profile's.
        Requested::File(path) => {
            out.entry = Some(Decided::new(Entry::File(path.clone()), Rule::Explicit));
            project
                .default_profile
                .clone()
                .map(|n| (n, Rule::DefaultProfile))
        }
        Requested::Stdlib(name) => {
            out.entry = Some(Decided::new(Entry::Stdlib(name.clone()), Rule::Explicit));
            project
                .default_profile
                .clone()
                .map(|n| (n, Rule::DefaultProfile))
        }
        Requested::Code(_) | Requested::Stdin => project
            .default_profile
            .clone()
            .map(|n| (n, Rule::DefaultProfile)),
    };

    let Some((name, rule)) = picked else {
        return out;
    };
    let name = match ProfileName::new(&name) {
        Ok(name) => name,
        Err(e) => {
            out.findings.push(Finding::BadProfileFilename(e));
            return out;
        }
    };
    let Some(filename) = declared.get(&name) else {
        out.findings.push(Finding::UndeclaredProfile {
            name: name.to_string(),
            declared: declared.keys().map(|n| n.to_string()).collect(),
        });
        return out;
    };
    let Some(config) = root.profiles.get(filename) else {
        out.findings.push(Finding::DeclaredProfileMissing {
            filename: filename.clone(),
        });
        return out;
    };
    out.profile = Some(Decided::new(name.clone(), rule));

    let explicit_entry = out.entry.is_some();
    take_profile_fields(&mut out, config, name.as_str(), Rule::FromProfile);
    if explicit_entry {
        // The command line named the program; the profile supplies
        // everything else. Restoring it here rather than branching above
        // keeps one path through the field extraction.
        out.entry = match &inputs.requested {
            Requested::File(path) => Some(Decided::new(Entry::File(path.clone()), Rule::Explicit)),
            Requested::Stdlib(n) => Some(Decided::new(Entry::Stdlib(n.clone()), Rule::Explicit)),
            _ => out.entry.take(),
        };
    }
    check_entry(&mut out, name.as_str(), root);
    merge_into(&mut out, config, &inputs.overrides);
    out
}

// depth: the individual checks, each the answer to one audit line

fn take_profile_fields(out: &mut Resolution, config: &RootConfig, profile: &str, rule: Rule) {
    if let Some(dir) = &config.dlua_dir {
        out.dlua_dir = Some(Decided::new(dir.clone(), rule.clone()));
    }
    match &config.entry {
        Some(entry) => out.entry = Some(Decided::new(entry.clone(), rule)),
        None if out.entry.is_none() => {
            // A profile with a `dlua_dir` and no `entry` is a different
            // mistake from a profile with neither, and saying which saves
            // the reader a guess.
            out.findings.push(if config.dlua_dir.is_some() {
                Finding::DluaDirWithoutEntry {
                    profile: profile.to_string(),
                }
            } else {
                Finding::NoEntry {
                    profile: profile.to_string(),
                }
            });
        }
        None => {}
    }
}

fn merge_into(out: &mut Resolution, config: &RootConfig, overrides: &Overrides) {
    match merge_args(&config.args, overrides) {
        Ok(args) => out.args = args,
        Err(e) => {
            out.args = config.args.clone();
            out.findings.push(Finding::Args(e));
        }
    }
}

/// The pin against what is present.
///
/// Three states, not two. A pin with a version established and different is a
/// mismatch; a pin with nothing established is just the pin, recorded and not
/// complained about, because the caller that could not establish it is the one
/// that should say so; and no pin at all is a finding audit reports and start
/// does not stop for.
fn check_pin(out: &mut Resolution, project: &ProjectJson, root: &RootInputs) {
    match (&project.drt, &root.binary_version) {
        (Some(pinned), Some(present)) if pinned != present => {
            out.pin = Some(Decided::new(pinned.clone(), Rule::ProjectCeiling));
            out.findings.push(Finding::PinMismatch {
                pinned: pinned.clone(),
                present: present.clone(),
            });
        }
        (Some(pinned), _) => out.pin = Some(Decided::new(pinned.clone(), Rule::ProjectCeiling)),
        (None, _) => out.findings.push(Finding::NoPin),
    }
}

/// `profiles` is authoritative, so this reports both directions: a declared
/// file that is missing blocks, and a present file that is undeclared is
/// ignored and said so. The second is the stray-`debug.config.json` case the
/// authority rule exists for, and a reader who cannot see why their edit did
/// nothing needs to be told.
fn check_profile_dir(
    out: &mut Resolution,
    declared: &BTreeMap<ProfileName, String>,
    root: &RootInputs,
) {
    for filename in declared.values() {
        if !root.profile_dir.iter().any(|f| f == filename) {
            out.findings.push(Finding::DeclaredProfileMissing {
                filename: filename.clone(),
            });
        }
    }
    for filename in &root.profile_dir {
        if !declared.values().any(|f| f == filename) {
            out.findings.push(Finding::UndeclaredProfilePresent {
                filename: filename.clone(),
            });
        }
    }
}

/// Every listed profile's caps must attenuate under the ceiling, with the
/// ceiling as parent and the profile as child. Checked for **all** of them
/// rather than only the one that runs: a profile that cannot run is a
/// problem an operator wants to hear about before the day they select it.
fn check_ceiling(
    out: &mut Resolution,
    project: &ProjectJson,
    declared: &BTreeMap<ProfileName, String>,
    root: &RootInputs,
) {
    let ceiling = crate::InstanceConfig {
        caps: project.caps.clone(),
        ..crate::InstanceConfig::default()
    };
    for (name, filename) in declared {
        let Some(config) = root.profiles.get(filename) else {
            continue;
        };
        if let Err(source) = config.root.check_attenuation(&ceiling) {
            out.findings.push(Finding::ProfileExceedsCeiling {
                profile: name.to_string(),
                source,
            });
        }
    }
}

/// Does the entry exist? A `stdlib:` entry resolves against the binary, so
/// the question does not apply to it — which is the one thing audit's
/// "does entry exist under dlua_dir" line has to understand.
fn check_entry(out: &mut Resolution, profile: &str, root: &RootInputs) {
    let Some(Decided { value: entry, .. }) = &out.entry else {
        return;
    };
    let Entry::File(path) = entry else {
        return;
    };
    if !root.dlua_dir.iter().any(|f| f == path) {
        out.findings.push(Finding::EntryMissing {
            profile: profile.to_string(),
            entry: path.clone(),
            dlua_dir: out
                .dlua_dir
                .as_ref()
                .map(|d| d.value.clone())
                .unwrap_or_default(),
        });
    }
}

/// The first pre-recognized config present, in order. Consulted **only**
/// when there is no `project.json`; with one present, `default_profile` and
/// the `profiles` list decide and this is not called.
pub fn fallback(cwd: &[String]) -> Option<String> {
    project::FALLBACK_ORDER
        .iter()
        .find(|candidate| cwd.iter().any(|f| f == *candidate))
        .map(|f| f.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::id::Uuid7;

    fn profile(entry: &str, caps: Vec<Grant>) -> RootConfig {
        RootConfig {
            dlua_dir: Some("dlua/".into()),
            entry: Some(Entry::parse(entry).unwrap()),
            root: crate::InstanceConfig {
                caps,
                ..crate::InstanceConfig::default()
            },
            ..RootConfig::default()
        }
    }

    fn root(project: ProjectJson, profiles: Vec<(&str, RootConfig)>) -> RootInputs {
        RootInputs {
            #[cfg(feature = "consent")]
            consent: None,
            profile_dir: profiles.iter().map(|(f, _)| f.to_string()).collect(),
            profiles: profiles
                .into_iter()
                .map(|(f, c)| (f.to_string(), c))
                .collect(),
            dlua_dir: vec!["app.dlua".into()],
            binary_version: Some("0.5.0".into()),
            project: Some(project),
        }
    }

    fn project(caps: Vec<Grant>) -> ProjectJson {
        ProjectJson {
            project_name: Some("my_drt_project".into()),
            project_version: Some("0.0.0".into()),
            drt: Some("0.5.0".into()),
            caps,
            default_profile: Some("debug".into()),
            profiles: vec!["debug.config.json".into()],
            ..ProjectJson::new(Uuid7::mint(1_757_707_440_000, [0x11; 10]))
        }
    }

    /// Preflight's own output, which is the reason every field carries its
    /// rule rather than only its value.
    #[test]
    fn the_default_profile_resolves_and_says_why() {
        let caps = vec![Grant::grant("host:fs/*")];
        let inputs = ResolveInputs {
            root: Some(root(
                project(caps.clone()),
                vec![("debug.config.json", profile("app.dlua", caps))],
            )),
            ..ResolveInputs::default()
        };
        let out = resolve(&inputs);

        let picked = out.profile.as_ref().unwrap();
        assert_eq!(picked.to_string(), "debug (default_profile)");
        assert_eq!(
            out.entry.as_ref().unwrap().value,
            Entry::File("app.dlua".into())
        );
        assert_eq!(out.pin.as_ref().unwrap().value, "0.5.0");
        assert!(out.blocker().is_none(), "{:?}", out.findings);
    }

    #[test]
    fn an_explicit_profile_beats_the_default_and_says_so() {
        let caps = vec![Grant::grant("host:fs/*")];
        let mut p = project(caps.clone());
        p.profiles.push("preflight.config.json".into());
        let inputs = ResolveInputs {
            root: Some(root(
                p,
                vec![
                    ("debug.config.json", profile("app.dlua", caps.clone())),
                    ("preflight.config.json", profile("stdlib:preflight", caps)),
                ],
            )),
            requested: Requested::Profile("preflight".into()),
            ..ResolveInputs::default()
        };
        let out = resolve(&inputs);
        assert_eq!(out.profile.as_ref().unwrap().rule, Rule::Explicit);
        assert_eq!(
            out.entry.as_ref().unwrap().value,
            Entry::Stdlib("preflight".into()),
            "a stdlib entry resolves against the binary"
        );
        // And the existence check does not fire for it, which is the one
        // thing audit's entry line has to understand.
        assert!(out.blocker().is_none(), "{:?}", out.findings);
    }

    /// The wide default is the no-root path's alone. A root with a
    /// project.json and no ceiling does not start.
    #[test]
    fn the_wide_default_belongs_to_the_no_root_path_only() {
        let no_root = ResolveInputs {
            config_flag: Some(profile("app.dlua", vec![])),
            ..ResolveInputs::default()
        };
        let out = resolve(&no_root);
        let ceiling = out.ceiling.unwrap();
        assert_eq!(ceiling.value, vec![Grant::grant("host:*")]);
        assert_eq!(ceiling.rule, Rule::NoRootDefault);

        let rooted = ResolveInputs {
            root: Some(root(
                project(vec![]),
                vec![("debug.config.json", profile("app.dlua", vec![]))],
            )),
            ..ResolveInputs::default()
        };
        let out = resolve(&rooted);
        assert!(
            out.findings.iter().any(|f| matches!(f, Finding::NoCeiling)),
            "{:?}",
            out.findings
        );
    }

    /// A profile asking for more than the ceiling is reported even when it
    /// is not the profile being run.
    #[test]
    fn a_profile_exceeding_the_ceiling_is_reported() {
        let mut p = project(vec![Grant::grant("host:fs/read")]);
        p.profiles.push("wide.config.json".into());
        let inputs = ResolveInputs {
            root: Some(root(
                p,
                vec![
                    (
                        "debug.config.json",
                        profile("app.dlua", vec![Grant::grant("host:fs/read")]),
                    ),
                    (
                        "wide.config.json",
                        profile("app.dlua", vec![Grant::grant("host:exec/run")]),
                    ),
                ],
            )),
            ..ResolveInputs::default()
        };
        let out = resolve(&inputs);
        assert!(
            out.findings.iter().any(|f| matches!(
                f,
                Finding::ProfileExceedsCeiling { profile, .. } if profile == "wide"
            )),
            "{:?}",
            out.findings
        );
    }

    /// `profiles` is authoritative in both directions, and the ignored file
    /// is said out loud rather than silently skipped.
    #[test]
    fn a_stray_profile_is_reported_as_ignored_and_does_not_block() {
        let caps = vec![Grant::grant("host:fs/*")];
        let mut r = root(
            project(caps.clone()),
            vec![("debug.config.json", profile("app.dlua", caps))],
        );
        r.profile_dir.push("stray.config.json".into());
        let out = resolve(&ResolveInputs {
            root: Some(r),
            ..ResolveInputs::default()
        });
        let stray = out
            .findings
            .iter()
            .find(|f| matches!(f, Finding::UndeclaredProfilePresent { .. }))
            .expect("reported");
        assert!(!stray.blocks(), "ignoring it is correct; hiding it is not");
        assert!(out.blocker().is_none());
    }

    #[test]
    fn a_pin_disagreeing_with_the_binary_names_both() {
        let caps = vec![Grant::grant("host:fs/*")];
        let mut r = root(
            project(caps.clone()),
            vec![("debug.config.json", profile("app.dlua", caps))],
        );
        r.binary_version = Some("0.9.0".into());
        let out = resolve(&ResolveInputs {
            root: Some(r),
            ..ResolveInputs::default()
        });
        let e = out.blocker().expect("blocks").to_string();
        assert!(e.contains("0.5.0") && e.contains("0.9.0"), "{e}");
    }

    /// `None` is "could not establish it", and must not produce a mismatch
    /// named against a version nobody verified. dollup's audit relies on this:
    /// it cannot execute `.drt_root/drt` to ask, so it leaves this unset and
    /// prints its own line.
    #[test]
    fn an_unestablished_binary_version_reports_no_mismatch() {
        let caps = vec![Grant::grant("host:fs/*")];
        let mut r = root(
            project(caps.clone()),
            vec![("debug.config.json", profile("app.dlua", caps))],
        );
        r.binary_version = None;
        let out = resolve(&ResolveInputs {
            root: Some(r),
            ..ResolveInputs::default()
        });

        assert_eq!(
            out.pin.as_ref().map(|p| p.value.as_str()),
            Some("0.5.0"),
            "the pin is still reported"
        );
        assert!(
            !out.findings
                .iter()
                .any(|f| matches!(f, Finding::PinMismatch { .. })),
            "and nothing is claimed about a binary nobody checked: {:?}",
            out.findings
        );
        assert!(out.blocker().is_none(), "{:?}", out.findings);
    }

    #[test]
    fn a_missing_entry_file_is_reported_with_its_directory() {
        let caps = vec![Grant::grant("host:fs/*")];
        let mut r = root(
            project(caps.clone()),
            vec![("debug.config.json", profile("missing.dlua", caps))],
        );
        r.dlua_dir = vec!["app.dlua".into()];
        let out = resolve(&ResolveInputs {
            root: Some(r),
            ..ResolveInputs::default()
        });
        let e = out.blocker().expect("blocks").to_string();
        assert!(e.contains("missing.dlua") && e.contains("dlua/"), "{e}");
    }

    /// Audit accumulates, start stops at the first blocker. Both read the
    /// same list.
    #[test]
    fn findings_accumulate_and_only_some_block() {
        let caps = vec![Grant::grant("host:fs/*")];
        let mut p = project(caps.clone());
        p.project_name = None;
        p.project_version = None;
        p.drt = None;
        let out = resolve(&ResolveInputs {
            root: Some(root(
                p,
                vec![("debug.config.json", profile("app.dlua", caps))],
            )),
            ..ResolveInputs::default()
        });
        assert_eq!(out.findings.len(), 3, "{:?}", out.findings);
        assert!(
            out.blocker().is_none(),
            "an unnamed, unversioned, unpinned root still starts; audit still says all three"
        );
    }

    #[test]
    fn the_fallback_order_is_first_match_wins() {
        assert_eq!(
            fallback(&[
                "release.config.json".into(),
                "default.config.json".into(),
                "debug.config.json".into()
            ])
            .as_deref(),
            Some("debug.config.json")
        );
        assert_eq!(
            fallback(&["release.config.json".into()]).as_deref(),
            Some("release.config.json")
        );
        assert_eq!(fallback(&["app.dlua".into()]), None);
    }

    // depth: the args typing rule

    #[test]
    fn the_declared_default_decides_how_an_override_parses() {
        let declared = BTreeMap::from([
            ("verbose".to_string(), ArgValue::Bool(false)),
            ("port".to_string(), ArgValue::Int(8092)),
            ("stun".to_string(), ArgValue::List(vec![])),
            ("label".to_string(), ArgValue::Str("fp".into())),
        ]);
        let merged = merge_args(
            &declared,
            &vec![
                ("verbose".into(), None),
                ("port".into(), Some("9000".into())),
                ("stun".into(), Some("a".into())),
                ("stun".into(), Some("b".into())),
            ],
        )
        .unwrap();

        assert_eq!(merged["verbose"], ArgValue::Bool(true), "a bool is a flag");
        assert_eq!(merged["port"], ArgValue::Int(9000));
        assert_eq!(
            merged["stun"],
            ArgValue::List(vec!["a".into(), "b".into()]),
            "repeated occurrences accumulate"
        );
        assert_eq!(
            merged["label"],
            ArgValue::Str("fp".into()),
            "an untouched key keeps its default"
        );
    }

    /// A typo cannot silently add a field. Same posture as the `.host.lua`
    /// loader's unknown-key refusal.
    #[test]
    fn an_undeclared_key_is_a_named_failure() {
        let declared = BTreeMap::from([("verbose".to_string(), ArgValue::Bool(false))]);
        let e = merge_args(&declared, &vec![("verbsoe".into(), None)]).unwrap_err();
        assert!(matches!(e, ArgError::UndeclaredKey { .. }), "{e}");
        assert!(
            e.to_string().contains("verbose"),
            "it names what is known: {e}"
        );
    }

    #[test]
    fn a_wrong_typed_override_is_a_named_failure() {
        let declared = BTreeMap::from([("port".to_string(), ArgValue::Int(1))]);
        let e = merge_args(&declared, &vec![("port".into(), Some("http".into()))]).unwrap_err();
        assert!(matches!(e, ArgError::NotTheDeclaredType { .. }), "{e}");
        let e = merge_args(&declared, &vec![("port".into(), None)]).unwrap_err();
        assert!(matches!(e, ArgError::MissingValue { .. }), "{e}");
    }

    /// A profile that ships `true` needs a way off it.
    #[test]
    fn a_bool_can_be_turned_off_explicitly() {
        let declared = BTreeMap::from([("verbose".to_string(), ArgValue::Bool(true))]);
        let merged =
            merge_args(&declared, &vec![("verbose".into(), Some("false".into()))]).unwrap();
        assert_eq!(merged["verbose"], ArgValue::Bool(false));
    }

    // depth: classifying one typed argument

    fn declared(names: &[&str]) -> Vec<ProfileName> {
        names.iter().map(|n| ProfileName::new(n).unwrap()).collect()
    }

    /// Every rule in the order it fires, on one list, so the order is readable
    /// as a table rather than as seven tests that each hide the precedence.
    #[test]
    fn the_classifier_rules_fire_in_order() {
        let profiles = declared(&["debug", "preflight"]);
        let c = |token: &str| classify(token, None, &profiles);

        // stdin, then the stdlib prefix, then a path-looking start.
        assert_eq!(c("-"), Requested::Stdin);
        assert_eq!(c("stdlib:preflight"), Requested::Stdlib("preflight".into()));
        assert_eq!(c("./app.dlua"), Requested::File("./app.dlua".into()));
        assert_eq!(c("/opt/app.dlua"), Requested::File("/opt/app.dlua".into()));

        // Evidence of code.
        assert_eq!(
            c("print('hello!')"),
            Requested::Code("print('hello!')".into())
        );
        assert_eq!(c("x = 1"), Requested::Code("x = 1".into()));
        assert_eq!(c("a();b()"), Requested::Code("a();b()".into()));
        assert_eq!(
            c("local q = 1\nprint(q)"),
            Requested::Code("local q = 1\nprint(q)".into()),
            "a newline is what makes a bash heredoc work with no flag"
        );

        // A bare token: a declared profile, else a file.
        assert_eq!(c("debug"), Requested::Profile("debug".into()));
        assert_eq!(c("app.dlua"), Requested::File("app.dlua".into()));
        assert_eq!(
            c("dlua-code.txt"),
            Requested::File("dlua-code.txt".into()),
            "extensions are not consulted at all"
        );
        assert_eq!(
            c("foo"),
            Requested::File("foo".into()),
            "the fallthrough is a file, so the error can name the disambiguators"
        );
    }

    /// A listed profile wins over a file of the same name. Surprising once,
    /// stated in the doc, and `-f` is the way out.
    #[test]
    fn a_declared_profile_beats_a_file_of_the_same_name() {
        let profiles = declared(&["debug"]);
        assert_eq!(
            classify("debug", None, &profiles),
            Requested::Profile("debug".into())
        );
        assert_eq!(
            classify("debug", Some(Explicit::File), &profiles),
            Requested::File("debug".into()),
            "-f insists"
        );
    }

    /// An explicit flag wins over every other rule, including the ones that
    /// would otherwise be unambiguous.
    #[test]
    fn an_explicit_flag_beats_every_other_rule() {
        let profiles = declared(&["debug"]);
        // A path-looking token as code, because the operator said so.
        assert_eq!(
            classify("./app.dlua", Some(Explicit::Code), &profiles),
            Requested::Code("./app.dlua".into())
        );
        // And a file literally named `-`, which is otherwise stdin.
        assert_eq!(
            classify("-", Some(Explicit::File), &profiles),
            Requested::File("-".into())
        );
        // `stdlib:` is a prefix, not a reservation: -f reaches a file by that
        // name if somebody really has one.
        assert_eq!(
            classify("stdlib:x", Some(Explicit::File), &profiles),
            Requested::File("stdlib:x".into())
        );
    }

    /// Whitespace is path-legal and is deliberately not evidence of code: a
    /// Windows path has spaces in it, and treating those as code would make
    /// `drt run "C:/my programs/app.dlua"` unreachable without a flag.
    #[test]
    fn whitespace_alone_is_not_evidence_of_code() {
        let profiles = declared(&[]);
        assert_eq!(
            classify("my programs/app.dlua", None, &profiles),
            Requested::File("my programs/app.dlua".into())
        );
        assert!(!CODE_EVIDENCE.contains(&' '));
        assert!(!CODE_EVIDENCE.contains(&'\t'));
    }

    #[test]
    fn the_not_found_message_names_the_way_out() {
        let message = no_such_file("foo");
        assert!(message.contains("no file 'foo'"), "{message}");
        for flag in ["-c", "-p", "-f"] {
            assert!(message.contains(flag), "{flag} is named: {message}");
        }
    }

    #[test]
    fn an_entry_is_a_file_or_a_stdlib_program() {
        assert_eq!(
            Entry::parse("app.dlua").unwrap(),
            Entry::File("app.dlua".into())
        );
        assert_eq!(
            Entry::parse("stdlib:tunnel").unwrap(),
            Entry::Stdlib("tunnel".into())
        );
        assert_eq!(
            Entry::parse("stdlib:tunnel").unwrap().as_written(),
            "stdlib:tunnel"
        );
        assert!(Entry::parse("stdlib:").is_err());
        assert!(Entry::parse("").is_err());
    }
}
