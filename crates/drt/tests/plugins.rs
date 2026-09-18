//! The `plugins` config key, and every way it is refused.
//!
//! # Surface
//!
//! Entry points: the tests. Each builds a `RootConfig` with a `plugins`
//! block and hands it to `wire_connectors`, which is the one path a
//! deployment reaches a plugin through.
//!
//! Configurable values: none.
//!
//! Fan-out: none.
//!
//! Every refusal here happens **at load**, while an operator is watching,
//! rather than on the first call at 3am. Nothing below starts a process:
//! `PluginConnector` starts its own on first use, so wiring is reading a
//! manifest and deciding whether this host can serve it.

#![cfg(all(feature = "plugins", any(unix, windows)))]

use drt_config::{PluginWiring, RootConfig};

/// A config with one plugin wired under `family`, from `manifest`.
fn config_with(family: &str, manifest: &str) -> RootConfig {
    let mut config = RootConfig::default();
    config.plugins.insert(
        family.to_string(),
        PluginWiring {
            manifest: manifest.to_string(),
            scope: None,
        },
    );
    config
}

/// Write a manifest to a temp dir and give back its path.
fn manifest_file(dir: &tempfile::TempDir, name: &str, body: &str) -> String {
    let path = dir.path().join(name);
    std::fs::write(&path, body).expect("the manifest is written");
    path.to_string_lossy().into_owned()
}

fn wire(config: &RootConfig) -> Result<(), String> {
    drt::cli::wire_connectors(config).map(|_| ())
}

/// The ordinary case: a manifest this host can serve is wired, and
/// nothing is started doing it.
#[test]
fn a_plugin_is_wired_from_its_manifest() {
    let dir = tempfile::tempdir().unwrap();
    let exec = env!("CARGO_BIN_EXE_drt");
    let file = manifest_file(
        &dir,
        "greet.plugin.json",
        &format!(r#"{{"family":"greet","transport":"spawn","scope":"root","exec":"{exec}"}}"#),
    );
    wire(&config_with("greet", &file)).expect("a wired plugin");
}

/// A plugin may not take a builtin's family. A guest calling `fs/read`
/// could not tell that its call now left the process, and a reader of the
/// config could not either.
#[test]
fn a_plugin_may_not_shadow_a_builtin_family() {
    let dir = tempfile::tempdir().unwrap();
    let exec = env!("CARGO_BIN_EXE_drt");
    let file = manifest_file(
        &dir,
        "fs.plugin.json",
        &format!(r#"{{"family":"fs","transport":"spawn","scope":"root","exec":"{exec}"}}"#),
    );
    let err = wire(&config_with("fs", &file)).unwrap_err();
    assert!(err.contains("builtin connector's family"), "{err}");
    assert!(err.contains("fs"), "{err}");
}

/// The rule holds whatever this build carries. `sql` is a builtin family
/// even on a build compiled without the sql connector, because a config
/// that worked on `slim` must not start shadowing a builtin the day it
/// runs on `full`.
#[test]
fn the_builtin_rule_does_not_depend_on_the_features_compiled_in() {
    let dir = tempfile::tempdir().unwrap();
    let exec = env!("CARGO_BIN_EXE_drt");
    for family in ["time", "sql", "exec", "socket", "crypto"] {
        let file = manifest_file(
            &dir,
            &format!("{family}.plugin.json"),
            &format!(
                r#"{{"family":"{family}","transport":"spawn","scope":"root","exec":"{exec}"}}"#
            ),
        );
        let err = wire(&config_with(family, &file)).unwrap_err();
        assert!(
            err.contains("builtin connector's family"),
            "'{family}' was not refused: {err}"
        );
    }
}

/// One family is served by one thing. A config naming both is a mistake
/// worth saying rather than a precedence rule worth learning.
#[test]
fn a_family_wired_as_both_a_connector_and_a_plugin_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let exec = env!("CARGO_BIN_EXE_drt");
    let file = manifest_file(
        &dir,
        "greet.plugin.json",
        &format!(r#"{{"family":"greet","transport":"spawn","scope":"root","exec":"{exec}"}}"#),
    );
    let mut config = config_with("greet", &file);
    config.connectors.insert("greet".into(), Default::default());
    let err = wire(&config).unwrap_err();
    assert!(err.contains("one family is served by one"), "{err}");
}

/// A manifest that is not there is named, with the path the operator
/// wrote, because that is the one thing they can fix.
#[test]
fn a_manifest_that_is_not_there_is_refused_by_its_path() {
    let err = wire(&config_with("greet", "/no/such/greet.plugin.json")).unwrap_err();
    assert!(err.contains("/no/such/greet.plugin.json"), "{err}");
    assert!(err.contains("cannot be read"), "{err}");
}

/// A manifest that is not a manifest is refused where it is read, not
/// where it is first used.
#[test]
fn a_manifest_this_host_cannot_read_is_refused_at_load() {
    let dir = tempfile::tempdir().unwrap();
    let file = manifest_file(&dir, "greet.plugin.json", "this is not json");
    let err = wire(&config_with("greet", &file)).unwrap_err();
    assert!(err.contains("cannot read"), "{err}");
}

/// The config key is the operator's name for the family and the manifest
/// carries the publisher's. A guest calls the first and the plugin answers
/// the second, so the two disagreeing is a typo and not a rename.
#[test]
fn a_manifest_whose_family_disagrees_with_the_key_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let exec = env!("CARGO_BIN_EXE_drt");
    let file = manifest_file(
        &dir,
        "greet.plugin.json",
        &format!(r#"{{"family":"hello","transport":"spawn","scope":"root","exec":"{exec}"}}"#),
    );
    let err = wire(&config_with("greet", &file)).unwrap_err();
    assert!(err.contains("typo"), "{err}");
    assert!(err.contains("hello"), "{err}");
}

/// A `process` manifest names an absolute path, and the manifest reader
/// says so. Checked here too because this is the path a deployment takes,
/// and a refusal that only the unit tests see is a refusal an operator
/// never gets.
#[test]
fn a_manifest_with_a_relative_exec_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let file = manifest_file(
        &dir,
        "greet.plugin.json",
        r#"{"family":"greet","transport":"spawn","scope":"root","exec":"plugin-echo"}"#,
    );
    let err = wire(&config_with("greet", &file)).unwrap_err();
    assert!(err.contains("absolute"), "{err}");
}
