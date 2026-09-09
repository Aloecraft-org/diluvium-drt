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
//! - [`load`]: write a `.host.lua` and load it, so every config assertion
//!   goes through the real loader.
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
        "app.host.lua",
        r#"return {
  supervisor = "sup.lua",
  numeric = {
    max_elements = 1000000,
    max_tier = "reproducible",
  },
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
        "tier.host.lua",
        r#"return { numeric = { max_tier = "exact" } }"#,
    )
    .unwrap();
    assert_eq!(only_tier.root.numeric.max_tier, Some(Tier::Exact));
    assert_eq!(only_tier.root.numeric.max_elements, None);

    let only_elements = load(
        dir.path(),
        "elements.host.lua",
        r#"return { numeric = { max_elements = 64 } }"#,
    )
    .unwrap();
    assert_eq!(only_elements.root.numeric.max_elements, Some(64));
    assert_eq!(only_elements.root.numeric.max_tier, None);

    // And an empty block is a block: `{}` is both the empty list and the
    // empty map in Lua, and this one is a map that states nothing.
    let empty = load(dir.path(), "empty.host.lua", r#"return { numeric = {} }"#).unwrap();
    assert!(empty.root.numeric.is_unbounded());
}

/// A typo is a typo, not a silent default — the C loader's promise, kept
/// for this block like every other.
#[test]
fn a_misspelled_numeric_key_or_tier_is_refused_by_name() {
    let dir = tempfile::tempdir().unwrap();
    let err = load(
        dir.path(),
        "typo.host.lua",
        r#"return { numeric = { max_element = 10 } }"#,
    )
    .unwrap_err();
    assert!(err.contains("max_element"), "got: {err}");
    assert!(err.contains("max_elements"), "it names the real key: {err}");

    // A tier nobody defined is refused with the three that exist, rather
    // than falling back to the loosest, which would be the worst default.
    let err = load(
        dir.path(),
        "tier.host.lua",
        r#"return { numeric = { max_tier = "quick" } }"#,
    )
    .unwrap_err();
    assert!(err.contains("quick"), "got: {err}");
    assert!(err.contains("exact"), "it names the three: {err}");
    assert!(
        err.contains("reproducible") && err.contains("fast"),
        "got: {err}"
    );

    // And the top-level list of known keys names it, so a config with a
    // block this build should have known is not told the key is unknown.
    let err = load(dir.path(), "top.host.lua", r#"return { numerics = {} }"#).unwrap_err();
    assert!(err.contains("numeric"), "got: {err}");
}

/// Zero is refused by the loader, with the reason and the alternative.
///
/// `dv_numeric_set_max_elements` reads `0` as "no limit". A config writing
/// it means the opposite, so the value never gets in rather than inverting
/// at the boundary.
#[test]
fn max_elements_zero_is_refused_with_the_reason() {
    let dir = tempfile::tempdir().unwrap();
    let err = load(
        dir.path(),
        "zero.host.lua",
        r#"return { numeric = { max_elements = 0 } }"#,
    )
    .unwrap_err();
    assert!(err.contains("no limit"), "got: {err}");
    assert!(err.contains("Omit the field"), "it says what to do: {err}");

    // One is a bound like any other; only zero is the ambiguous value.
    assert_eq!(
        load(
            dir.path(),
            "one.host.lua",
            r#"return { numeric = { max_elements = 1 } }"#,
        )
        .unwrap()
        .root
        .numeric
        .max_elements,
        Some(1)
    );
}

/// The block is not feature-gated, unlike `relay`, `stun`, `turn` and
/// `wireguard`. Those name servers a build may not carry; this names a
/// bound, and "do not run fast kernels" is an answer every binary can give.
#[test]
fn the_block_loads_on_a_build_without_the_optional_servers() {
    let dir = tempfile::tempdir().unwrap();
    assert!(load(
        dir.path(),
        "b.host.lua",
        r#"return { numeric = { max_tier = "exact" } }"#,
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

    /// And a spawn request carrying it is denied, not silently unbounded.
    #[test]
    fn a_child_asking_for_a_zero_element_bound_is_denied() {
        let seen = events(Numeric::default(), "numeric = { max_elements = 0 }");
        let denied = seen
            .iter()
            .find(|(e, _)| e == "denied")
            .unwrap_or_else(|| panic!("the child was not refused: {seen:?}"));
        assert!(denied.1.contains("no limit"), "got: {}", denied.1);
        assert!(!seen.iter().any(|(e, _)| e == "spawned"));
    }

    /// The roster answers the audit question for every live instance.
    ///
    /// Now a real reading of `dv_numeric_touched_fast` rather than a
    /// literal, and still `false`: the pinned core carries no fast-tier
    /// backend, so no fast kernel can have run. The value is the same; what
    /// changed is that the core is the one saying it.
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
