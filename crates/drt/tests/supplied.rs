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

/// Every builtin family, whether or not this build carries it, that a
/// backing may not shadow. The list `wire_connectors_with`'s match answers
/// to, less `net`, whose transport is the host's (the last test).
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
/// `bus` stands for a family no build carries: `net` was the example
/// until `full` carried it (issue #108).
#[test]
fn a_supplied_family_is_wired_only_when_the_config_names_it() {
    let supplied = supplying(&["bus", "other"]);
    let registry = wire_connectors_with(&naming(&["bus"]), &supplied).expect("bus is wired");
    let wired = menu(drt_connector::Dispatcher::new(registry));
    assert!(wired.contains(&"bus".to_string()), "{wired:?}");
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
    let err = wire_connectors_with(&naming(&["buss"]), &supplying(&["bus"]))
        .err()
        .expect("refused");
    assert_eq!(
        err,
        "config wires connector 'buss', which this build does not carry and no backing \
         was handed in for (handed in: bus)"
    );
    let err = wire_connectors(&naming(&["buss"])).err().expect("refused");
    assert_eq!(
        err,
        "config wires connector 'buss', which this build does not carry"
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

/// `net`'s transport is the host's: a backing handed in for it is wired
/// and answers, whether or not this build carries a native transport, and
/// without one handed in a build that carries one answers with its own.
#[test]
fn a_handed_in_net_transport_is_the_one_that_answers() {
    let registry = wire_connectors_with(&naming(&["net"]), &supplying(&["net"]))
        .expect("a handed-in net is wired");
    let wired = menu(drt_connector::Dispatcher::new(registry));
    assert!(wired.contains(&"net".to_string()), "{wired:?}");

    let native = wire_connectors(&naming(&["net"])).err().unwrap_or_default();
    if cfg!(feature = "connector-net") {
        // Its own transport, which refuses a wiring with no scope by name.
        assert!(
            native.contains("the net connector takes a scope"),
            "{native}"
        );
    } else {
        assert_eq!(
            native,
            "config wires connector 'net', which this build does not carry"
        );
    }
}
