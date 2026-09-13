//! Assembling the root config (SPEC.md §5): the root config is a property
//! of the OS process, merged from a file, flags and env into one object.
//!
//! The file's shape is [`drt_config::RootConfig`] — those serde types are
//! the source of truth, so this module carries no schema of its own, only
//! the reading and the startup checks. JSON is the format read today;
//! everything here is plain serde, so TOML or a `.dlua` surface is a
//! deserializer swap and not a schema change.
//!
//! **`*.host.lua` is gone.** It was `diluvium-host`'s config dialect read
//! natively — eight hundred lines mapping the C host's field names onto
//! these types, so a deployment moved from that host to DRT by swapping
//! the binary and changing no files. Every shipped config is JSON now.
//!
//! Two things went with it and both came back. An unknown key was an error
//! that named itself; serde ignored one for a while, until issue #31
//! measured what a silent default costs, and now every block refuses a key
//! it does not know while `_`-prefixed keys are comments at every depth
//! (`drt_config::comments`). And the checks that mapper made on values
//! serde cannot judge — see [`validate`], which runs on any config whatever
//! format it arrived in.
//!
//! **Grants are validated here, at startup, by name.** A capability whose
//! scope is malformed or ill-typed for the connector it names must fail
//! while the operator is still looking at the terminal — never as a
//! mystifying `denied` at first call.

use std::path::Path;

use drt_caps::ScopeRegistry;
use drt_config::RootConfig;
use drt_connector::Registry;

/// Read a root config file. An absent path is the empty root object, which
/// is a legitimate configuration: locked out of the box, granting nothing.
pub fn load(path: Option<&Path>) -> Result<RootConfig, String> {
    let Some(path) = path else {
        return Ok(RootConfig::default());
    };
    let text = drt_platform::fs::read_to_string(path)
        .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    // A `_`-prefixed key is a comment, at every depth and in every kind of
    // block (`drt_config::comments`), and every other key must be one the
    // reader knows: the refusal names the key, the block it sits in, and
    // the keys that would have been taken. Issue #31 is why: `creat` for
    // `create` ran migrations into a database that did not exist, in a
    // journal mode Litestream cannot replicate, while `/health` said 200.
    //
    // Read in stream order through `Strip`, never by way of a
    // `serde_json::Value`: a `Value`'s object sorts its keys in one build
    // and keeps them in another (`_preserve-order-test`), and a connector's
    // `scope` is the map the file spelled in the order it spelled it, which
    // the corpus snapshots hold the loader to. The path tracker sits outside
    // the filter, so a refusal names where it sits (`relay.labels.abc`) as
    // well as the line and column serde_json puts on it.
    let mut json = serde_json::Deserializer::from_str(&text);
    let mut config: RootConfig =
        serde_path_to_error::deserialize(drt_config::comments::Strip(&mut json))
            .map_err(|e| format!("{}: {}", path.display(), located(&text, e)))?;
    json.end().map_err(|e| format!("{}: {e}", path.display()))?;
    // Here rather than only at startup, so a refusal names the file it
    // refused. A deployment directory holds several of these and
    // "relay.labels.xps needs both keys" is a different message when it
    // says which file's `xps`.
    validate(&config).map_err(|e| format!("{}: {e}", path.display()))?;
    resolve_program(&mut config, path);
    Ok(config)
}

// depth: naming the block when serde cannot
//
// `RootConfig` flattens `InstanceConfig`, and serde reads a flattened field
// back out of a buffer the path tracker never sees, so a refusal under
// `program`, `caps`, `budget` or `numeric` arrives with no path at all:
// `unknown field `max_element`` with nothing saying `numeric`. Find it
// again one key at a time, as the instance block alone, where every step
// is tracked: the text parsed once more as a `serde_json::Value`, which
// a blame pass may do since order is nothing to it, stripped of its
// comments, and probed a key at a time. A path of one segment is the key
// itself being refused, which the outer error already says, and says
// without a list of the instance block's keys that would be wrong at the
// top level.
fn located(text: &str, err: serde_path_to_error::Error<serde_json::Error>) -> String {
    if err.path().iter().next().is_some() {
        return err.to_string();
    }
    let Ok(mut value) = serde_json::from_str::<serde_json::Value>(text) else {
        return err.to_string();
    };
    drt_config::comments::strip(&mut value);
    let serde_json::Value::Object(map) = value else {
        return err.to_string();
    };
    for (key, child) in map {
        let mut one = serde_json::Map::new();
        one.insert(key, child);
        let alone = serde_json::Value::Object(one);
        let Err(again) = serde_path_to_error::deserialize::<_, drt_config::InstanceConfig>(&alone)
        else {
            continue;
        };
        if again.path().iter().count() >= 2 {
            return again.to_string();
        }
    }
    err.to_string()
}

/// A relative `program` path is relative **to the config**, not to the
/// working directory.
///
/// The deployment directory is the unit that moves: `drt --config
/// ~/deploys/fp/app.json start` must find `app.dlua` next to that file, not
/// next to wherever the operator happened to be standing. The `.host.lua`
/// mapper did this for `supervisor` and said why; the JSON path did not,
/// which meant every config in `examples/` — all of which name a bare
/// filename — worked only from its own directory.
///
/// An absolute path is left alone, and so is a `Program` that is not a
/// path: source has no directory and `stdlib:` names nothing on disk.
fn resolve_program(config: &mut RootConfig, path: &Path) {
    let Some(drt_config::Program::Path(program)) = &config.root.program else {
        return;
    };
    if program.is_absolute() {
        return;
    }
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    config.root.program = Some(drt_config::Program::Path(dir.join(program)));
}

/// Check every grant against the scope-types the wired connectors declare.
///
/// The registry gates *shape*, not existence: a grant naming a capability
/// no connector declares passes here and is answered `denied` at call time,
/// which is the honest split — "this build does not carry that" is a
/// different fact from "that grant is malformed".
pub fn validate_grants(config: &RootConfig, registry: &Registry) -> Result<(), String> {
    let mut scopes = ScopeRegistry::new();
    registry.declare_scope_types(&mut scopes);
    // The peer family has no connector to declare it: a cross-peer write is
    // a queue write, not a hostcall into something wired. Declared here so a
    // peer grant is still shape-checked at load rather than at a call that
    // this build cannot make anyway.
    scopes.declare(drt_caps::PEER_FAMILY, drt_caps::PeerScope);
    scopes
        .validate(&config.root.caps)
        .map_err(|e| e.to_string())
}

/// Semantic checks serde cannot make, at load, where the file is still
/// named.
///
/// Serde catches an absent field and a wrong type. These are the two facts
/// about a *present, well-typed* value that make a server serve nothing,
/// and they carry over from the `.host.lua` mapper, which is where they
/// used to live: deleting a loader must not quietly delete its checks.
///
/// Both failure modes are the same shape, and it is the shape this
/// repository keeps naming — a process that binds, reports itself healthy,
/// and carries no traffic. `verify_key` already fails closed on an empty
/// key, so nothing is *unsafe* without this; what is lost without it is
/// ever being told, and a relay that refuses every leg looks exactly like
/// a relay nobody is using.
///
/// Called on the assembled config whatever format it came from, so JSON
/// and anything added later are checked once, here.
pub fn validate(config: &RootConfig) -> Result<(), String> {
    let Some(relay) = &config.relay else {
        return Ok(());
    };
    // A port is not optional and not defaulted: a relay is an address you
    // hand out, and `0.0.0.0` is not one. Split rather than parsed, because
    // a hostname with a port is as valid here as an address with one and
    // only the port is in question; `rsplit_once` rather than `rsplit` so a
    // v6 literal's own colons stay on the host side.
    let port = relay.bind.rsplit_once(':').map(|(_, port)| port);
    if !port.is_some_and(|p| p.parse::<u16>().is_ok_and(|n| n != 0)) {
        return Err(format!(
            "relay.bind is '{}', which names no port; write host:port",
            relay.bind
        ));
    }
    for (name, label) in &relay.labels {
        if label.park_key.is_empty() || label.caller_key.is_empty() {
            return Err(format!(
                "relay.labels.{name} needs both park_key and caller_key; \
                 an absent key refuses every leg"
            ));
        }
    }
    Ok(())
}

/// What a run should be allowed to reach.
///
/// A config that names its ceiling gets exactly that ceiling. A run with no
/// config at all is the operator running their own program locally, and
/// takes the wide grant — what is actually reachable is then whatever the
/// build wires, since an unwired family answers `denied` either way.
pub fn ceiling(config: &RootConfig) -> Vec<drt_caps::Grant> {
    if config.root.caps.is_empty() {
        vec![drt_caps::Grant::grant("host:*")]
    } else {
        config.root.caps.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(json: &str) -> RootConfig {
        serde_json::from_str(json).expect("the probe itself must parse")
    }

    fn relay(bind: &str, park: &str, caller: &str) -> RootConfig {
        cfg(&format!(
            r#"{{"relay":{{"bind":"{bind}","labels":{{"xps":{{"park_key":"{park}","caller_key":"{caller}"}}}}}}}}"#
        ))
    }

    /// The check that came out of the `.host.lua` mapper when the mapper
    /// went away. `verify_key` already fails closed on an empty key, so
    /// what this buys is not safety but *being told*: a relay refusing
    /// every leg is indistinguishable from a relay nobody is using.
    #[test]
    fn a_blank_relay_key_is_refused_by_name_at_load() {
        for (park, caller) in [("", "c"), ("p", ""), ("", "")] {
            let Err(e) = validate(&relay("0.0.0.0:8443", park, caller)) else {
                panic!("({park:?}, {caller:?}) must be refused");
            };
            assert!(e.contains("xps"), "the label is named: {e}");
            assert!(e.contains("refuses every leg"), "{e}");
        }
        assert!(validate(&relay("0.0.0.0:8443", "p", "c")).is_ok());
    }

    /// The other half: an address with no port is an address nobody can be
    /// handed. Port 0 is the same fact spelled differently -- the kernel
    /// picks one, and a relay whose address is decided after it starts
    /// cannot appear in anyone's config.
    #[test]
    fn a_relay_bind_without_a_usable_port_is_refused() {
        for bind in ["0.0.0.0", "0.0.0.0:", "0.0.0.0:0", "8443"] {
            assert!(
                validate(&relay(bind, "p", "c")).is_err(),
                "'{bind}' names no port"
            );
        }
        // A hostname's port and a v6 literal's own colons: `rsplit_once`
        // splits at the last colon, so `[::1]:8443` keeps its address
        // together and only `8443` is read as the port.
        for bind in ["0.0.0.0:8443", "relay.example:8443", "[::1]:8443"] {
            assert!(validate(&relay(bind, "p", "c")).is_ok(), "'{bind}' is fine");
        }
    }

    /// The deployment directory is the unit that moves, so a bare filename
    /// is found beside the config and not beside the operator.
    ///
    /// Through `testfs`, not a tempdir: `load` reads via `drt_platform::fs`,
    /// whose backend is installed for the **process**, so a test that goes
    /// to the real disk while another has a `MemFs` installed reads from
    /// that `MemFs` instead and fails only in the full suite. `seed` takes
    /// the one lock that makes this safe.
    #[test]
    fn a_relative_program_resolves_against_the_config() {
        let seeded = crate::testfs::seed(
            true,
            &[("/r/deploy/app.json", r#"{"program":{"path":"app.dlua"}}"#)],
        );
        let loaded = load(Some(Path::new("/r/deploy/app.json"))).unwrap();
        assert_eq!(
            loaded.root.program,
            Some(drt_config::Program::Path("/r/deploy/app.dlua".into()))
        );
        drop(seeded);
    }

    fn resolved(program: &str, config: &str) -> String {
        let mut c = RootConfig {
            root: drt_config::InstanceConfig {
                program: Some(drt_config::Program::Path(program.into())),
                ..Default::default()
            },
            ..RootConfig::default()
        };
        resolve_program(&mut c, Path::new(config));
        match c.root.program {
            Some(drt_config::Program::Path(p)) => p.to_string_lossy().into_owned(),
            other => panic!("still a path, got {other:?}"),
        }
    }

    /// The two edges of that join.
    ///
    /// `--config app.json` has no directory on it at all, and `parent()` is
    /// `""` there: joining onto it must leave the name alone rather than
    /// making `/app.dlua`. And an absolute program is already an answer, so
    /// joining would corrupt it.
    #[test]
    fn joining_handles_a_bare_config_name_and_an_absolute_program() {
        assert_eq!(resolved("a.dlua", "app.json"), "a.dlua");
        assert_eq!(resolved("a.dlua", "deploy/app.json"), "deploy/a.dlua");
        assert_eq!(
            resolved("/opt/fp/a.dlua", "/etc/drt/app.json"),
            "/opt/fp/a.dlua"
        );
    }

    /// Nothing to check is not a failure. A config with no relay at all is
    /// the common case and must cost nothing.
    #[test]
    fn a_config_with_no_relay_passes() {
        assert!(validate(&RootConfig::default()).is_ok());
        assert!(validate(&cfg(r#"{"relay":{"bind":"0.0.0.0:8443"}}"#)).is_ok());
    }
}
