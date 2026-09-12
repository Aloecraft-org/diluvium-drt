//! `drt run` end to end, through the real binary: a config file names the
//! ceiling and wires a connector to a place, a program names a file inside
//! that place, and the answer comes back over the hostcall pump.

use std::path::Path;
use std::process::Command;

fn drt() -> Command {
    Command::new(env!("CARGO_BIN_EXE_drt"))
}

fn write(path: &Path, text: &str) {
    std::fs::write(path, text).unwrap();
}

/// A guest that makes one hostcall and prints the reply's status, so the
/// test reads the capability decision rather than inferring it.
const CALLER: &str = r#"
local calls = queue.declare("host/calls", { capacity = 4, exported = true, on_full = "reject" })
local replies = queue.declare("host/replies", { capacity = 4 })
queue.push(calls, { tok = 1, call = "fs/read", args = { path = "note.txt" } })
local _, reply = queue.wait({replies})
print(reply.status .. "|" .. tostring(reply.value or reply.detail))
"#;

#[test]
fn a_config_wires_a_connector_to_a_place_and_the_program_names_a_file_in_it() {
    let dir = tempfile::tempdir().unwrap();
    let work = dir.path().join("work");
    std::fs::create_dir(&work).unwrap();
    write(&work.join("note.txt"), "from the granted directory");
    write(&dir.path().join("prog.dlua"), CALLER);
    write(
        &dir.path().join("drt.json"),
        &format!(
            r#"{{
              "caps": [{{"capability": "host:fs/*"}}],
              "connectors": {{
                "fs": {{"scope": {{"scope": "{}", "access": "read"}}}}
              }}
            }}"#,
            work.to_str().unwrap()
        ),
    );

    let out = drt()
        .arg("run")
        .arg(dir.path().join("prog.dlua"))
        .arg("--config")
        .arg(dir.path().join("drt.json"))
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        stdout.contains("ok|from the granted directory"),
        "the program read its file through the granted scope: {stdout}"
    );
}

#[test]
fn a_ceiling_that_does_not_cover_the_call_denies_it() {
    let dir = tempfile::tempdir().unwrap();
    let work = dir.path().join("work");
    std::fs::create_dir(&work).unwrap();
    write(&work.join("note.txt"), "unreachable");
    write(&dir.path().join("prog.dlua"), CALLER);
    // fs is wired, but the ceiling grants only time: the connector is there
    // and the call still does not happen.
    write(
        &dir.path().join("drt.json"),
        &format!(
            r#"{{
              "caps": [{{"capability": "host:time"}}],
              "connectors": {{
                "fs": {{"scope": {{"scope": "{}"}}}}
              }}
            }}"#,
            work.to_str().unwrap()
        ),
    );

    let out = drt()
        .arg("run")
        .arg(dir.path().join("prog.dlua"))
        .arg("--config")
        .arg(dir.path().join("drt.json"))
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("denied|"),
        "outside the ceiling is denied, not read: {stdout}"
    );
}

#[test]
fn an_ill_scoped_config_fails_at_startup_by_name() {
    let dir = tempfile::tempdir().unwrap();
    write(&dir.path().join("prog.dlua"), "return 1");
    write(
        &dir.path().join("drt.json"),
        r#"{"connectors": {"fs": {"scope": {"scope": "/nonexistent/place"}}}}"#,
    );

    let out = drt()
        .arg("run")
        .arg(dir.path().join("prog.dlua"))
        .arg("--config")
        .arg(dir.path().join("drt.json"))
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success());
    assert!(
        stderr.contains("cannot be resolved"),
        "the refusal names the fix: {stderr}"
    );
}

#[test]
fn a_config_may_name_the_program_and_the_argument_wins() {
    let dir = tempfile::tempdir().unwrap();
    write(&dir.path().join("from_config.dlua"), "print('config')");
    write(&dir.path().join("from_argv.dlua"), "print('argv')");
    let cfg = dir.path().join("drt.json");
    write(
        &cfg,
        &format!(
            r#"{{"program": {{"path": "{}"}}}}"#,
            dir.path().join("from_config.dlua").to_str().unwrap()
        ),
    );

    // No argument: the config's program runs.
    let out = drt().arg("run").arg("--config").arg(&cfg).output().unwrap();
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("config"),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // An argument overrides it: it is the more specific thing just typed.
    let out = drt()
        .arg("run")
        .arg(dir.path().join("from_argv.dlua"))
        .arg("--config")
        .arg(&cfg)
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&out.stdout).contains("argv"));
}

#[test]
fn no_program_anywhere_says_so() {
    let out = drt().arg("run").output().unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("name a program"));
}

/// The budget escape, and the one thing `drt run` can still say about it.
///
/// A guest catches instruction exhaustion with `pcall` and keeps running --
/// the hook clears itself before raising at this pin, so nothing re-arms
/// it. DRT cannot stop that from here (the enforcement fix is upstream,
/// doc/Ask-0.5.0-Reply.md §1.2), but it must not report success for it:
/// exit 0 would make `drt run` the only place in DRT that hides a budget
/// that stopped being enforced.
#[test]
fn a_program_that_caught_its_budget_and_kept_running_does_not_exit_zero() {
    let dir = tempfile::tempdir().unwrap();
    write(
        &dir.path().join("prog.dlua"),
        "pcall(function() local n = 0 while true do n = n + 1 end end)\nprint('kept going')\n",
    );
    write(
        &dir.path().join("drt.json"),
        r#"{"budget": {"instructions": 1000000}}"#,
    );

    let out = drt()
        .arg("run")
        .arg(dir.path().join("prog.dlua"))
        .arg("--config")
        .arg(dir.path().join("drt.json"))
        .output()
        .unwrap();

    // The program really did run past the budget -- this is the escape
    // itself, asserted so the test fails loudly if it is ever closed
    // upstream and this whole case becomes unreachable.
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("kept going"),
        "the guest did not get past the budget; if the upstream hook now \
         re-arms, this test and the branch it covers are both obsolete"
    );
    assert!(!out.status.success(), "exit 0 would hide the escape");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("exhausted its instruction budget"),
        "stderr: {stderr}"
    );
}

/// The other half: a program that stays inside its budget is untouched by
/// the check above.
#[test]
fn a_program_inside_its_budget_still_exits_zero() {
    let dir = tempfile::tempdir().unwrap();
    write(&dir.path().join("prog.dlua"), "print('fine')\n");
    write(
        &dir.path().join("drt.json"),
        r#"{"budget": {"instructions": 1000000}}"#,
    );

    let out = drt()
        .arg("run")
        .arg(dir.path().join("prog.dlua"))
        .arg("--config")
        .arg(dir.path().join("drt.json"))
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// `drt buildinfo` reports which diluvium is inside it.
///
/// The fact used to live only in `BUILDINFO.txt`, which the release
/// workflow writes by grepping `Cargo.lock` — so a binary someone copied
/// off a machine carried no answer at all, and a package's
/// `requires.diluvium` had nothing in the artifact to check against.
/// `doc/Release.md`'s rule is that the compatibility fact travels with the
/// bytes; a fact in a file beside the bytes does not travel with them.
///
/// A **revision**, deliberately, not a version. The core exposes no version
/// string at runtime, and the distinctions that have mattered between the
/// two projects are revision facts.
#[test]
fn buildinfo_reports_the_embedded_diluvium_revision() {
    let out = drt().arg("buildinfo").output().unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    let line = text
        .lines()
        .find_map(|l| l.strip_prefix("diluvium: "))
        .expect("buildinfo names the embedded diluvium");
    assert!(
        line.len() >= 7 && line.chars().all(|c| c.is_ascii_hexdigit()),
        "a git revision, or nothing — never a version-shaped string that \
         cannot be checked: {line:?}"
    );

    // And the same fact in the machine-readable form, since that is what a
    // package manager reads.
    let out = drt().arg("buildinfo").arg("--json").output().unwrap();
    let json = String::from_utf8_lossy(&out.stdout);
    assert!(
        json.contains(&format!("\"diluvium\":\"{line}\"")),
        "the two forms must agree: {json}"
    );
}

/// `drt buildinfo` names the release tag it was built as, when it was one.
///
/// Every candidate under a version prints that version, so `version:
/// 0.5.0` could not tell rc1 from rc3 and a box's operator recorded the
/// installed tag in a sidecar file because the binary would not say
/// (discofetch `DRT_ASKS.md` §3). The tag is a build-time fact from the
/// release workflow's environment; this test reads the same environment it
/// was compiled under, so it holds for a tagged build and an untagged one.
#[test]
fn buildinfo_names_the_release_tag_when_built_as_one() {
    let expected = option_env!("DRT_RELEASE_TAG").filter(|t| !t.is_empty());
    let text =
        String::from_utf8_lossy(&drt().arg("buildinfo").output().unwrap().stdout).to_string();
    let json = String::from_utf8_lossy(
        &drt()
            .arg("buildinfo")
            .arg("--json")
            .output()
            .unwrap()
            .stdout,
    )
    .to_string();
    match expected {
        Some(tag) => {
            assert!(text.lines().any(|l| l == format!("tag: {tag}")), "{text}");
            assert!(json.contains(&format!("\"tag\":\"{tag}\"")), "{json}");
        }
        None => {
            assert!(
                !text.lines().any(|l| l.starts_with("tag:")),
                "an untagged build names no tag: {text}"
            );
            assert!(json.contains("\"tag\":null"), "{json}");
        }
    }
}

/// `drt buildinfo` names its profile by exact feature set, and the sets
/// it knows are the ones `Cargo.toml` declares.
///
/// Two tables have to agree — the `[features]` profiles in the manifest
/// and the `PROFILE_*` constants in `main.rs` — and nothing but this test
/// makes them. It reads the manifest, closes over what each profile turns
/// on, works out which of those features *this* test binary was compiled
/// with (an integration test shares its package's features), and checks
/// the binary reports the profile that set is. A feature added to `full`
/// in the manifest and forgotten in `main.rs` fails here as `custom`; a
/// feature added to the manifest and unknown to this test fails by name.
#[test]
fn profile_matches_its_manifest() {
    const PROFILES: [&str; 4] = ["full", "slim", "wasi", "web"];
    const LEAVES: [&str; 19] = [
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
        // A test dependency expressed as a feature: dev-dependencies cannot be
        // optional, and a shipping feature must not carry crates only its tests
        // use. `wireguard` names it for a real runtime reason -- its TURN
        // fallback -- so it is a leaf both callers reach by name.
        "turn-client",
        "relay",
        "runtime",
        "stun",
        "tunnel",
        "turn",
        "wireguard",
    ];

    // The manifest's `[features]` table, as `name -> entries`.
    let manifest =
        std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml")).unwrap();
    let mut table: Vec<(String, Vec<String>)> = Vec::new();
    let mut in_features = false;
    for line in manifest.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_features = line == "[features]";
            continue;
        }
        if !in_features || line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (name, rest) = line
            .split_once('=')
            .expect("a feature line is `name = [...]`");
        let entries = rest
            .trim()
            .trim_start_matches('[')
            .trim_end_matches(']')
            .split(',')
            .map(|e| e.trim().trim_matches('"').to_string())
            .filter(|e| !e.is_empty())
            .collect();
        table.push((name.trim().to_string(), entries));
    }
    for (name, _) in &table {
        let known = name == "default"
            || PROFILES.contains(&name.as_str())
            || LEAVES.contains(&name.as_str());
        assert!(
            known,
            "Cargo.toml declares feature '{name}'; teach this test and main.rs's PROFILE_* \
             tables about it"
        );
    }

    // Everything a profile turns on, transitively, by leaf name: `full`
    // names `slim`, `netcheck` names `stun`, `relay` names `listen`.
    fn close(name: &str, table: &[(String, Vec<String>)], into: &mut Vec<String>) {
        let Some((_, entries)) = table.iter().find(|(n, _)| n == name) else {
            return;
        };
        for e in entries {
            if e.starts_with("dep:") || into.contains(e) {
                continue;
            }
            into.push(e.clone());
            close(e, table, into);
        }
    }
    let leaves_of = |profile: &str| -> Vec<String> {
        let mut all = Vec::new();
        close(profile, &table, &mut all);
        all.retain(|f| LEAVES.contains(&f.as_str()));
        all.sort();
        all
    };

    // Which leaves this binary was compiled with, by the same names.
    let mut enabled: Vec<String> = Vec::new();
    macro_rules! feature {
        ($name:literal) => {
            if cfg!(feature = $name) {
                enabled.push($name.to_string());
            }
        };
    }
    feature!("cli");
    feature!("connector-crypto");
    feature!("connector-data");
    feature!("connector-exec");
    feature!("connector-fs");
    feature!("connector-rest");
    feature!("connector-sql");
    feature!("connector-ssh");
    feature!("connector-ssmtp");
    feature!("connector-time");
    feature!("listen");
    feature!("netcheck");
    feature!("relay");
    feature!("runtime");
    feature!("stun");
    feature!("tunnel");
    feature!("turn");
    feature!("wireguard");
    enabled.sort();

    let expected = PROFILES
        .into_iter()
        .find(|p| leaves_of(p) == enabled)
        .unwrap_or("custom");

    let out = drt().arg("buildinfo").output().unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    let reported = text
        .lines()
        .find_map(|l| l.strip_prefix("profile: "))
        .expect("buildinfo names a profile");
    assert_eq!(
        reported, expected,
        "this binary was built with {enabled:?}, which Cargo.toml says is '{expected}'; \
         buildinfo says '{reported}', so main.rs's PROFILE_* tables and the manifest disagree"
    );
}

/// The two hard-coded compatibility facts agree with the changelog, which
/// agrees with `Cargo.lock`.
///
/// `features` and `diluvium_build` are stated in `cli.rs` rather than read
/// off the core, because the core does not yet answer either question —
/// `dv_features()` and `dv_build()` arrive with session A's A0 milestone
/// (`doc/Plan-2026-09.md` §3.1), and `TODO(A0)` marks both tables. A fact a
/// binary states about bytes it did not compile is a fact that can be
/// wrong, and the way this one goes wrong is quiet: someone moves the pin,
/// `buildinfo` keeps saying `build13`, and a package's
/// `requires.diluvium_build` is checked against a number from two pins ago.
///
/// So the chain is closed instead: `script/changelog.py check` ties the
/// changelog's `diluvium` revision to `Cargo.lock`, and this ties the
/// binary's numbers to the changelog. Moving the pin without saying so
/// fails one of the two. When A0 lands, this test and both tables go.
#[test]
fn the_hard_coded_core_facts_agree_with_the_changelog() {
    let changelog =
        std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../../CHANGELOG.yaml"))
            .expect("the changelog reads");

    // The newest entry, which is the one the tree is building towards. Its
    // fields are the first of each name after `releases:`.
    let newest = changelog
        .split_once("\nreleases:\n")
        .expect("the changelog has releases")
        .1;
    let field = |name: &str| -> String {
        newest
            .lines()
            .find_map(|l| l.trim().strip_prefix(&format!("{name}: ")))
            .unwrap_or_else(|| panic!("the newest release records {name}"))
            .trim()
            .to_string()
    };

    let text =
        String::from_utf8_lossy(&drt().arg("buildinfo").output().unwrap().stdout).to_string();
    let says = |name: &str| -> String {
        text.lines()
            .find_map(|l| l.strip_prefix(&format!("{name}: ")))
            .unwrap_or_else(|| panic!("buildinfo says {name}\n{text}"))
            .to_string()
    };

    assert_eq!(
        says("diluvium_build"),
        field("diluvium_build"),
        "DILUVIUM_BUILD in cli.rs and diluvium_build in CHANGELOG.yaml \
         disagree. If the pin moved, both move; the changelog's revision is \
         already checked against Cargo.lock by `script/changelog.py check`."
    );

    // The revision is stamped from `Cargo.lock` by build.rs, so this is the
    // link that catches a pin moved without the changelog following.
    assert_eq!(
        says("diluvium"),
        field("diluvium"),
        "the pin in Cargo.lock and the revision in CHANGELOG.yaml disagree"
    );

    // `features` is per profile in the changelog, so compare against the
    // profile this binary reports itself as. A `custom` build states none,
    // and has no changelog line to be checked against.
    let profile = says("profile");
    if profile == "custom" {
        assert_eq!(
            says("features"),
            "",
            "a custom build cannot claim a named profile's features"
        );
        return;
    }
    let recorded = newest
        .split_once("\n    features:\n")
        .expect("the newest release records features")
        .1
        .lines()
        .find_map(|l| l.trim().strip_prefix(&format!("{profile}: ")))
        .unwrap_or_else(|| panic!("the newest release records features for `{profile}`"))
        .trim()
        .trim_start_matches('[')
        .trim_end_matches(']')
        .split(',')
        .map(|f| f.trim().to_string())
        .filter(|f| !f.is_empty())
        .collect::<Vec<_>>()
        .join(",");
    assert_eq!(
        says("features"),
        recorded,
        "CORE_FEATURES_* in cli.rs and features.{profile} in CHANGELOG.yaml \
         disagree"
    );
}
