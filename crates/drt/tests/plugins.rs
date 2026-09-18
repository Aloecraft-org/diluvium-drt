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

/// The whole point of the name: `capabilities/list` says a family is a
/// plugin and says which one. This is the only place the difference is
/// visible -- a guest that *calls* `greet/hello` cannot tell it left the
/// process, which is what makes a plugin a connector backing rather than
/// a second protocol (`doc/Plugins.md` §4).
#[test]
fn the_capability_menu_names_the_plugin_behind_a_family() {
    let dir = tempfile::tempdir().unwrap();
    let exec = env!("CARGO_BIN_EXE_drt");
    let file = manifest_file(
        &dir,
        "greeter.plugin.json",
        &format!(r#"{{"family":"greet","transport":"spawn","scope":"root","exec":"{exec}"}}"#),
    );
    let registry = drt::cli::wire_connectors(&config_with("greet", &file)).expect("a wired plugin");
    let dispatcher = drt_connector::Dispatcher::new(registry);

    let raw = drt_hostcall::to_bytes(&drt_hostcall::Request {
        tok: 1,
        call: "capabilities/list".into(),
        args: None,
    })
    .unwrap();
    let caps = drt_caps::CapSet::root(vec![drt_caps::Grant::grant("host:capabilities/list")]);
    let reply = pollster::block_on(dispatcher.dispatch(&caps, &raw));

    let rows = match reply.value.as_ref().expect("the menu answers") {
        rmpv::Value::Array(v) => v.clone(),
        other => panic!("the menu is an array, got {other:?}"),
    };
    let greet = rows
        .iter()
        .find_map(|row| match row {
            rmpv::Value::Map(m) => {
                let field = |k: &str| m.iter().find(|(key, _)| key.as_str() == Some(k));
                match field("name") {
                    Some((_, v)) if v.as_str() == Some("greet") => Some(m.clone()),
                    _ => None,
                }
            }
            _ => None,
        })
        .expect("a 'greet' row in the menu");
    let field = |k: &str| {
        greet
            .iter()
            .find(|(key, _)| key.as_str() == Some(k))
            .map(|(_, v)| v.clone())
            .unwrap_or(rmpv::Value::Nil)
    };

    assert_eq!(field("kind").as_str(), Some("plugin"));
    // The manifest file is `greeter.plugin.json` and the family is
    // `greet`: the owner is the plugin's name, which is why it is not
    // simply the family echoed back.
    assert_eq!(field("owner").as_str(), Some("greeter"));
}

/// A transport this build cannot serve is refused at load, like every
/// other thing the `plugins` block refuses -- not on the first call.
///
/// `tcp` is the case that is unserviceable on every platform: the address
/// is the deployment's to name and no deployment names one yet. The
/// `process`-on-Windows case is the same refusal on one platform, spelled
/// by the same function.
#[test]
fn a_transport_this_build_cannot_serve_is_refused_at_load() {
    let dir = tempfile::tempdir().unwrap();
    let file = manifest_file(
        &dir,
        "far.plugin.json",
        r#"{"family":"far","transport":"tcp","scope":"root"}"#,
    );
    let err = wire(&config_with("far", &file)).unwrap_err();
    assert!(
        err.contains("tcp"),
        "the refusal names the transport: {err}"
    );
    assert!(
        err.contains("address"),
        "and says what is missing about it: {err}"
    );
}

/// Every way the block can be refused, in one table, asserting the one
/// property each individual test above does not: the message names the
/// **family** it is about.
///
/// A deployment wires several plugins. "the manifest cannot be read" is a
/// different message when it says which of the six it means, and a
/// refusal added later that forgets to say so fails here rather than in
/// an operator's terminal.
#[test]
fn every_refusal_names_the_family_it_is_about() {
    let dir = tempfile::tempdir().unwrap();
    let exec = env!("CARGO_BIN_EXE_drt");
    let cases: Vec<(&str, String)> = vec![
        ("gone", "/nonexistent/gone.plugin.json".to_string()),
        (
            "junk",
            manifest_file(&dir, "junk.plugin.json", "{not json at all"),
        ),
        (
            "mismatch",
            manifest_file(
                &dir,
                "mismatch.plugin.json",
                &format!(
                    r#"{{"family":"other","transport":"spawn","scope":"root","exec":"{exec}"}}"#
                ),
            ),
        ),
        (
            "far",
            manifest_file(
                &dir,
                "far.plugin.json",
                r#"{"family":"far","transport":"tcp","scope":"root"}"#,
            ),
        ),
        (
            "rel",
            manifest_file(
                &dir,
                "rel.plugin.json",
                r#"{"family":"rel","transport":"spawn","scope":"root","exec":"plugin-echo"}"#,
            ),
        ),
        (
            "oddscope",
            manifest_file(
                &dir,
                "oddscope.plugin.json",
                &format!(
                    r#"{{"family":"oddscope","transport":"spawn","scope":"per-call","exec":"{exec}"}}"#
                ),
            ),
        ),
    ];

    for (family, manifest) in cases {
        let err = match wire(&config_with(family, &manifest)) {
            Err(e) => e,
            Ok(()) => panic!("'{family}' should be refused and was wired"),
        };
        assert!(
            err.contains(family),
            "the refusal for '{family}' does not name it: {err}"
        );
    }
}
