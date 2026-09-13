//! The `numeric` block, end to end: a config states it, attenuation holds
//! it at spawn, and the audit flag is reported beside `exceeded`.
//!
//! `doc/Plan-2026-09.md` §3.4 is the rule these check — a kernel's cost is
//! elements processed, not instructions executed, so `budget` does not
//! cover it and an instance that could raise its own element bound or its
//! own tier has no bound at all.
//!
//! ## surface block
//!
//! - [`load`]: write a config and load it, so every config assertion goes
//!   through the real loader.
//! - [`SUPERVISOR`]: a program that spawns one child and reports what the
//!   swarm said about it.
//! - The four entry points, in the order the bound travels: the loader, the
//!   attenuation, the engine seam, the report.

use drt_config::{Numeric, Tier};

fn load(dir: &std::path::Path, name: &str, text: &str) -> Result<drt_config::RootConfig, String> {
    let path = dir.join(name);
    std::fs::write(&path, text).unwrap();
    drt::config::load(Some(&path))
}

// ---------------------------------------------------------------------------
// The loader
// ---------------------------------------------------------------------------

#[test]
fn a_numeric_block_loads_with_both_bounds() {
    let dir = tempfile::tempdir().unwrap();
    let config = load(
        dir.path(),
        "app.json",
        r#"{
  "program": { "path": "sup.lua" },
  "numeric": {
    "max_elements": 1000000,
    "max_tier": "reproducible"
  }
}"#,
    )
    .expect("the numeric block loads");
    assert_eq!(config.root.numeric.max_elements, Some(1_000_000));
    assert_eq!(config.root.numeric.max_tier, Some(Tier::Reproducible));
}

/// Either bound alone is a config. Stating one does not imply the other,
/// and the unstated one inherits at spawn rather than becoming unlimited.
#[test]
fn either_bound_may_be_stated_alone() {
    let dir = tempfile::tempdir().unwrap();
    let only_tier = load(
        dir.path(),
        "tier.json",
        r#"{ "numeric": { "max_tier": "exact" } }"#,
    )
    .unwrap();
    assert_eq!(only_tier.root.numeric.max_tier, Some(Tier::Exact));
    assert_eq!(only_tier.root.numeric.max_elements, None);

    let only_elements = load(
        dir.path(),
        "elements.json",
        r#"{ "numeric": { "max_elements": 64 } }"#,
    )
    .unwrap();
    assert_eq!(only_elements.root.numeric.max_elements, Some(64));
    assert_eq!(only_elements.root.numeric.max_tier, None);

    // And an empty block is a block: a `numeric` that states nothing is not
    // the same as no `numeric` at all to a reader, even though both are
    // unbounded. This used to be the sharper case -- `{}` was both the empty
    // list and the empty map in Lua, and telling them apart was a bug class.
    let empty = load(dir.path(), "empty.json", r#"{ "numeric": {} }"#).unwrap();
    assert!(empty.root.numeric.is_unbounded());
}

/// A tier nobody defined is refused with the three that exist, rather than
/// falling back to the loosest, which would be the worst default.
#[test]
fn a_tier_that_does_not_exist_is_refused_with_the_three_that_do() {
    let dir = tempfile::tempdir().unwrap();
    let err = load(
        dir.path(),
        "tier.json",
        r#"{ "numeric": { "max_tier": "quick" } }"#,
    )
    .unwrap_err();
    assert!(err.contains("quick"), "got: {err}");
    assert!(err.contains("exact"), "it names the three: {err}");
    assert!(
        err.contains("reproducible") && err.contains("fast"),
        "got: {err}"
    );
}

/// **A misspelled key is refused by name again**, and this test asserts
/// the promise rather than recording, as it did for one release, that
/// the promise was not kept.
///
/// `max_element` was refused by name under the `.host.lua` mapper, and for
/// one release after it went, serde read these types without
/// `deny_unknown_fields` and a misspelled bound silently did not apply --
/// recorded here as a decision, and then measured by issue #31: `creat` for
/// `create` ran migrations into a database that did not exist, in a journal
/// mode Litestream cannot replicate, while `/health` answered 200.
///
/// Every block refuses a key it does not know now, and the refusal names
/// the key, the block it sits in, and the keys it would have taken.
/// `_`-prefixed keys are comments at every depth and in every kind of
/// block, a map of names as much as a struct (`drt_config::comments`), so
/// the convention that made `deny_unknown_fields` impossible is what makes
/// it possible.
#[test]
fn a_misspelled_key_is_refused_by_name_and_a_comment_key_is_not_a_typo() {
    let dir = tempfile::tempdir().unwrap();
    let e = load(
        dir.path(),
        "typo.json",
        r#"{ "numeric": { "max_element": 10 } }"#,
    )
    .expect_err("a bound that would not apply");
    assert!(
        e.contains("max_element") && e.contains("max_elements"),
        "{e}"
    );
    assert!(e.contains("numeric"), "the block is named: {e}");

    let e = load(dir.path(), "top.json", r#"{ "numerics": {} }"#).expect_err("not a block");
    assert!(e.contains("numerics") && e.contains("numeric"), "{e}");

    // A grant is a block too: `scop` for `scope` was a grant that did not
    // narrow, and the refusal says which grant.
    let e = load(
        dir.path(),
        "grant.json",
        r#"{ "caps": [{ "capability": "host:time", "scop": {} }] }"#,
    )
    .expect_err("a scope that would not narrow");
    assert!(
        e.contains("scop") && e.contains("scope") && e.contains("caps[0]"),
        "{e}"
    );

    // The shape #31 found on a listener: `max_bod` for `max_body`.
    let e = load(
        dir.path(),
        "listener.json",
        r#"{ "listeners": [{ "bind": "127.0.0.1:0", "max_bod": 1 }] }"#,
    )
    .expect_err("a bound that would not bind");
    assert!(e.contains("max_bod") && e.contains("max_body"), "{e}");

    // A struct inside a map of names, which is where #31's `_who` lives:
    // the refusal says which label.
    let e = load(
        dir.path(),
        "label.json",
        r#"{ "relay": { "bind": "127.0.0.1:8092", "labels": {
              "abc": { "park_key": "k", "caller_key": "k", "parkkey": "x" } } } }"#,
    )
    .expect_err("a key no label has");
    assert!(
        e.contains("parkkey") && e.contains("relay.labels.abc"),
        "{e}"
    );

    // Comment keys, at every depth and in every kind of block: a struct, a
    // map of names (`connectors`, `relay.labels`), a connector's scope.
    // Before this, a comment in a map of names was refused naming the
    // struct it was not.
    let config = load(
        dir.path(),
        "commented.json",
        r#"{
          "_note": "why",
          "numeric": { "_note": "why", "max_elements": 64 },
          "connectors": {
            "_note": "a map of names",
            "time": { "_note": "why" },
            "fs": { "scope": { "_note": "inside a scope", "scope": ".", "access": "read" } }
          },
          "relay": { "bind": "127.0.0.1:8092", "labels": {
            "_note": "a comment, not a label",
            "abc": { "_who": "this machine", "park_key": "k", "caller_key": "k" } } }
        }"#,
    )
    .expect("a comment key is not a typo");
    assert_eq!(config.root.numeric.max_elements, Some(64));
    assert_eq!(config.connectors.len(), 2);
    let relay = config.relay.as_ref().expect("the block loads");
    assert_eq!(relay.labels.keys().collect::<Vec<_>>(), ["abc"]);
}

/// The block is not feature-gated, unlike `relay`, `stun`, `turn` and
/// `wireguard`. Those name servers a build may not carry; this names a
/// bound, and "do not run fast kernels" is an answer every binary can give.
#[test]
fn the_block_loads_on_a_build_without_the_optional_servers() {
    let dir = tempfile::tempdir().unwrap();
    assert!(load(
        dir.path(),
        "b.json",
        r#"{ "numeric": { "max_tier": "exact" } }"#,
    )
    .is_ok());
}

// ---------------------------------------------------------------------------
// Attenuation at spawn, through the swarm's own drive loop
// ---------------------------------------------------------------------------

mod spawn {
    use super::*;
    use std::sync::Arc;

    use drt_caps::Grant;
    use drt_config::Budget;
    use drt_swarm::engine::diluvium_engine::DiluviumEngine;
    use drt_swarm::swarm::{StepHost, Swarm};
    use drt_swarm::InstanceId;

    /// A parent that spawns one child with the numeric block it is given,
    /// then parks so the test can read what the swarm said.
    ///
    /// The child parks too, and that is load-bearing rather than tidy: a
    /// child that runs to completion is reaped, its slot is released, and
    /// the roster questions below would be asking about an instance that is
    /// no longer there — which answers `None` for a reason that has nothing
    /// to do with what is being tested.
    fn supervisor(child_numeric: &str) -> String {
        format!(
            r#"
            local lc = queue.declare("system/lifecycle", {{ capacity = 4 }})
            local ev = queue.declare("system/events", {{ capacity = 16, exported = true }})
            local hold = queue.declare("hold", {{ capacity = 1 }})
            queue.push(lc, {{
                op = "spawn",
                code = [==[queue.wait({{queue.declare("hold", {{capacity = 1}})}})]==],
                {child_numeric}
            }})
            queue.wait({{hold}})
        "#
        )
    }

    fn events(numeric: Numeric, child_numeric: &str) -> Vec<(String, String)> {
        let engine = Arc::new(DiluviumEngine::new().unwrap());
        let mut sw = Swarm::new(engine, StepHost::new());
        let root = sw
            .root_with_numeric(
                supervisor(child_numeric).as_bytes(),
                vec![Grant::grant("lifecycle")],
                Budget::default(),
                numeric,
            )
            .unwrap();
        for _ in 0..10 {
            sw.step();
        }
        let inst = sw.instance_mut(root).unwrap();
        let q = inst.queue("system/events").unwrap();
        let mut out = Vec::new();
        while let Ok(Some(raw)) = inst.pop(q) {
            let v = rmpv::decode::read_value(&mut raw.as_slice()).unwrap();
            let field = |name: &str| {
                v.as_map()
                    .unwrap()
                    .iter()
                    .find(|(k, _)| k.as_str() == Some(name))
                    .map(|(_, v)| v.to_string())
                    .unwrap_or_default()
            };
            out.push((
                field("event").trim_matches('"').to_string(),
                field("detail").trim_matches('"').to_string(),
            ));
        }
        let _ = root;
        out
    }

    /// The parent's bounds are the ceiling, and a child may narrow them.
    #[test]
    fn a_child_may_state_a_stricter_bound() {
        let parent = Numeric {
            max_elements: Some(1_000_000),
            max_tier: Some(Tier::Fast),
        };
        let seen = events(
            parent,
            r#"numeric = { max_elements = 1000, max_tier = "exact" }"#,
        );
        assert!(
            seen.iter().any(|(e, _)| e == "spawned"),
            "the child was refused: {seen:?}"
        );
    }

    /// And may not raise either one. The refusal names the field, because
    /// two bounds fail for two different reasons and a supervisor fixing
    /// one should not have to guess which.
    #[test]
    fn a_child_may_not_raise_the_element_bound_or_the_tier() {
        let parent = Numeric {
            max_elements: Some(1000),
            max_tier: Some(Tier::Reproducible),
        };

        let seen = events(parent, "numeric = { max_elements = 2000 }");
        let denied = seen
            .iter()
            .find(|(e, _)| e == "denied")
            .unwrap_or_else(|| panic!("the child was not refused: {seen:?}"));
        assert!(denied.1.contains("max_elements"), "got: {}", denied.1);
        assert!(!seen.iter().any(|(e, _)| e == "spawned"));

        let seen = events(parent, r#"numeric = { max_tier = "fast" }"#);
        let denied = seen
            .iter()
            .find(|(e, _)| e == "denied")
            .unwrap_or_else(|| panic!("the child was not refused: {seen:?}"));
        assert!(denied.1.contains("max_tier"), "got: {}", denied.1);
    }

    /// Saying nothing was the cheaper escape of the two, because it needs
    /// no intent at all: an unstated bound resolves to the parent's
    /// ceiling, not to unlimited.
    #[test]
    fn a_child_that_states_nothing_inherits_rather_than_escapes() {
        let parent = Numeric {
            max_elements: Some(1000),
            max_tier: Some(Tier::Exact),
        };
        let engine = Arc::new(DiluviumEngine::new().unwrap());
        let mut sw = Swarm::new(engine, StepHost::new());
        let root = sw
            .root_with_numeric(
                supervisor("").as_bytes(),
                vec![Grant::grant("lifecycle")],
                Budget::default(),
                parent,
            )
            .unwrap();
        for _ in 0..10 {
            sw.step();
        }
        let child = InstanceId(root.0 + 1);
        assert_eq!(
            sw.numeric(child),
            Some(parent),
            "the child that stated nothing did not inherit the ceiling"
        );
    }

    /// The roster answers the audit question for every live instance, and
    /// the answer today is `false` everywhere: no fast-tier backend exists
    /// in this workspace or in the pinned core, so no fast kernel can have
    /// run. TODO(A2) makes this a real reading rather than a real `false`.
    #[test]
    fn the_roster_reports_the_audit_flag_and_it_is_false() {
        let engine = Arc::new(DiluviumEngine::new().unwrap());
        let mut sw = Swarm::new(engine, StepHost::new());
        let root = sw
            .root_with_numeric(
                supervisor("").as_bytes(),
                vec![Grant::grant("lifecycle")],
                Budget::default(),
                Numeric::default(),
            )
            .unwrap();
        for _ in 0..10 {
            sw.step();
        }
        for id in sw.ids() {
            assert_eq!(
                sw.numeric_touched_fast(id),
                Some(false),
                "instance {} claims a fast kernel ran, and none exists",
                id.0
            );
        }
        // An id that is not in the roster is not `false`, it is nothing:
        // "no fast kernel ran" and "there is no such instance" are
        // different answers and a panel should not conflate them.
        assert_eq!(sw.numeric_touched_fast(InstanceId(9999)), None);
        let _ = root;
    }
}
