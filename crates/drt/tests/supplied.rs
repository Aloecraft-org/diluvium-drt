//! Backings a host hands in (`cli::Supplied`): how a page's JavaScript
//! connectors reach the same wiring every builtin goes through.
//!
//! # Surface
//!
//! Entry points: the tests. Each builds a `RootConfig` and a `Supplied`
//! map and hands both to `wire_connectors_with` or `assemble_with`.
//!
//! Configurable values: `BUILTINS`, every family a build may carry.
//!
//! Fan-out: none.

use std::sync::Arc;

use drt::cli::{wire_connectors, wire_connectors_with, Supplied};
use drt_config::RootConfig;
use drt_connector::mock::MockConnector;

/// Every builtin family, whether or not this build carries it. The list
/// `wire_connectors_with`'s match answers to.
const BUILTINS: &[&str] = &[
    "time", "fs", "sql", "crypto", "data", "ssh", "rest", "ssmtp", "ldap", "exec", "socket", "ws",
    "kv",
];

fn naming(families: &[&str]) -> RootConfig {
    let mut config = RootConfig::default();
    for f in families {
        config.connectors.insert(f.to_string(), Default::default());
    }
    config
}

fn supplying(families: &[&str]) -> Supplied {
    families
        .iter()
        .map(|f| {
            (
                f.to_string(),
                Arc::new(MockConnector::new()) as Arc<dyn drt_connector::Connector>,
            )
        })
        .collect()
}

/// A supplied family the config names is wired; one it does not name is
/// not, because handing a backing in makes it available, not granted.
#[test]
fn a_supplied_family_is_wired_only_when_the_config_names_it() {
    let supplied = supplying(&["net", "other"]);
    let registry = wire_connectors_with(&naming(&["net"]), &supplied).expect("net is wired");
    let wired = menu(drt_connector::Dispatcher::new(registry));
    assert!(wired.contains(&"net".to_string()), "{wired:?}");
    assert!(!wired.contains(&"other".to_string()), "{wired:?}");
}

/// The families a dispatcher wires, read off `capabilities/list`.
fn menu(d: drt_connector::Dispatcher) -> Vec<String> {
    let caps = drt_caps::CapSet::root(vec![drt_caps::Grant::grant("host:capabilities/list")]);
    let raw = drt_hostcall::to_bytes(&drt_hostcall::Request {
        tok: 1,
        call: "capabilities/list".into(),
        args: None,
    })
    .unwrap();
    let reply = pollster::block_on(d.dispatch(&caps, &raw));
    reply
        .value
        .unwrap()
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|e| {
            e.as_map()?
                .iter()
                .find(|(k, _)| k.as_str() == Some("name"))
                .and_then(|(_, v)| v.as_str().map(String::from))
        })
        .collect()
}

/// A config naming a family nobody answers is told which ones were
/// handed in, so a page that misspelled a key can see it.
#[test]
fn a_named_family_nobody_answers_says_what_was_handed_in() {
    let err = wire_connectors_with(&naming(&["nett"]), &supplying(&["net"]))
        .err()
        .expect("refused");
    assert_eq!(
        err,
        "config wires connector 'nett', which this build does not carry and no backing \
         was handed in for (handed in: net)"
    );
    let err = wire_connectors(&naming(&["nett"])).err().expect("refused");
    assert_eq!(
        err,
        "config wires connector 'nett', which this build does not carry"
    );
}

/// A backing may not shadow a builtin this build carries, and may stand in
/// for one it does not: `sql` in the web profile is sql.js (#81). Which
/// families this build carries is read off the wiring itself, so the
/// refusal and the match cannot drift apart.
#[test]
fn a_supplied_backing_may_not_shadow_a_builtin() {
    for family in BUILTINS {
        let carried = match wire_connectors(&naming(&[family])) {
            Ok(_) => true,
            Err(e) => !e.contains("which this build does not carry"),
        };
        let shadow = wire_connectors_with(&RootConfig::default(), &supplying(&[family]));
        match (carried, shadow) {
            (true, Err(e)) => assert_eq!(
                e,
                format!(
                    "a backing was handed in for '{family}', which this build carries itself; \
                     one family is answered by one connector"
                )
            ),
            (false, Ok(_)) => {}
            (true, Ok(_)) => panic!("'{family}' is carried and a backing shadowed it"),
            (false, Err(e)) => panic!("'{family}' is not carried and was refused: {e}"),
        }
    }
}
