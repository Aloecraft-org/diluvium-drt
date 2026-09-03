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
    const LEAVES: [&str; 12] = [
        "connector-crypto",
        "connector-fs",
        "connector-rest",
        "connector-sql",
        "connector-ssh",
        "connector-ssmtp",
        "connector-time",
        "listen",
        "netcheck",
        "relay",
        "stun",
        "tunnel",
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
    feature!("connector-crypto");
    feature!("connector-fs");
    feature!("connector-rest");
    feature!("connector-sql");
    feature!("connector-ssh");
    feature!("connector-ssmtp");
    feature!("connector-time");
    feature!("listen");
    feature!("netcheck");
    feature!("relay");
    feature!("stun");
    feature!("tunnel");
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
