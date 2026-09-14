//! Ownership at the guest boundary (`doc/Plan-0.7.0.md` §2–§4): what a
//! real instance can and cannot see of what the host holds for it.
//!
//! # Surface
//!
//! Entry points: the tests. Each builds a real swarm — `DiluviumEngine`
//! under a `PumpHost` over `DeployHost`, exactly the shape `drt start`
//! runs (`crates/drt/src/start.rs`, `deployment`) — so the instance being
//! asked about has a heap and a snapshot.
//!
//! Configurable values:
//! - `DEV_KEY` — the master the crypto connector is wired with.
//! - `STEPS` — how far to drive before the program is expected parked.
//!
//! Fan-out: none.
//!
//! These tests derive what the host holds *independently* of the
//! connector, from the public KDF labels, and search the snapshot for it.
//! A test that asked the connector for the bytes would be asking the thing
//! under test to describe its own leak.
//!
//! **What a snapshot carries, learned the hard way:** the parked
//! coroutine's stack and everything reachable from it, plus the queue
//! subsystem — verbatim, as msgpack. A *global* is not in it: `_G` is a
//! permanent, referenced by fingerprint and never serialised. So a value
//! the test needs to find must be a `local` that is still live across the
//! park. That is also why the control assertion below exists: without it,
//! the negative assertions would pass against an empty snapshot.

#![cfg(feature = "connector-crypto")]

use std::sync::Arc;

use drt::start::DeployHost;
use drt_caps::{Effect, Grant, Scope};
use drt_config::Budget;
use drt_connector::{Dispatcher, Registry};
use drt_connector_crypto::{CryptoConnector, KDF_LABEL_DERIVE, KDF_LABEL_HMAC, KDF_LABEL_JWT};
use drt_swarm::engine::diluvium_engine::DiluviumEngine;
use drt_swarm::pump::PumpHost;
use drt_swarm::swarm::Swarm;
use hmac::{Mac, SimpleHmac};
use sha2::Sha256;

const DEV_KEY: &str = "capability-testing-dev-key-0123456789";
const STEPS: usize = 64;

fn hmac_sha256(key: &[u8], msg: &[u8]) -> [u8; 32] {
    let mut mac = <SimpleHmac<Sha256> as Mac>::new_from_slice(key).expect("any key length");
    mac.update(msg);
    mac.finalize().into_bytes().into()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty() && haystack.windows(needle.len()).any(|w| w == needle)
}

fn crypto_dispatcher() -> Dispatcher {
    let mut reg = Registry::new();
    reg.wire(
        "crypto",
        Arc::new(CryptoConnector::new()),
        Some(Scope(rmpv::Value::Map(vec![(
            "key".into(),
            DEV_KEY.into(),
        )]))),
    )
    .unwrap();
    Dispatcher::new(reg)
}

fn scoped(capability: &str, scope: &str) -> Grant {
    Grant {
        effect: Effect::Grant,
        capability: capability.into(),
        scope: Some(Scope(rmpv::Value::from(scope))),
    }
}

/// Acceptance 15: `crypto/derive` registers a label, `crypto/hmac` signs
/// with it, and **the key is nowhere in the guest** — the snapshot taken
/// after signing contains none of the bytes the host derived, raw or hex.
#[test]
fn a_derived_key_is_in_neither_the_guests_heap_nor_its_snapshot() {
    let engine = Arc::new(DiluviumEngine::new().unwrap());
    let mut sw = Swarm::new(
        engine,
        PumpHost::new(DeployHost::new(), crypto_dispatcher()),
    );

    // Derive, sign, keep the answer live, park. Each call asserts its own
    // status in the guest, so a refusal faults the instance with the
    // reason on stderr rather than parking with nothing to show. `mac` is
    // a local that is still needed after the wait, so it is on the parked
    // stack and in the snapshot: a MAC is not a key, and its presence is
    // the control that says the search below is reading real guest state.
    const PROGRAM: &str = "\
        local inbox = queue.lookup('inbox')\n\
        local _, s, d = host.try('crypto/derive', {label = 'room:1'})\n\
        assert(s == 'ok', 'derive: ' .. tostring(s) .. ' ' .. tostring(d))\n\
        local mac, s2, d2 = host.try('crypto/hmac', {data = 'the message', key = 'room:1'})\n\
        assert(s2 == 'ok', 'hmac: ' .. tostring(s2) .. ' ' .. tostring(d2))\n\
        queue.wait({inbox})\n\
        return mac\n";
    let caps = vec![
        scoped("host:crypto/derive", "room:*"),
        Grant::grant("host:crypto/hmac"),
    ];
    let root = sw
        .root(PROGRAM.as_bytes(), caps, Budget::default())
        .unwrap();
    for _ in 0..STEPS {
        sw.step();
    }
    assert_eq!(
        sw.alive(),
        1,
        "the program faulted rather than parking; its reason is on stderr"
    );
    sw.hibernate(root)
        .expect("parked on its inbox, so it hibernates");
    let snap = sw.snapshot(root).expect("hibernated").to_vec();

    // What the host holds, from the public labels and nothing else.
    let k_label = hmac_sha256(DEV_KEY.as_bytes(), KDF_LABEL_DERIVE);
    let label_master = hmac_sha256(&k_label, b"room:1");
    let l_hmac = hmac_sha256(&label_master, KDF_LABEL_HMAC);
    let l_jwt = hmac_sha256(&label_master, KDF_LABEL_JWT);
    let k_hmac = hmac_sha256(DEV_KEY.as_bytes(), KDF_LABEL_HMAC);

    // The control: the guest's own MAC is there, so the search is real.
    let mac = hmac_sha256(&l_hmac, b"the message");
    assert!(
        contains(&snap, hex(&mac).as_bytes()),
        "the guest kept its MAC in a live local; it should be in the snapshot"
    );

    for (name, secret) in [
        ("the master", DEV_KEY.as_bytes()),
        ("k_label", &k_label[..]),
        ("the label master", &label_master[..]),
        ("the label's hmac subkey", &l_hmac[..]),
        ("the label's jwt subkey", &l_jwt[..]),
        ("the default hmac subkey", &k_hmac[..]),
    ] {
        assert!(!contains(&snap, secret), "{name} is in the snapshot, raw");
        assert!(
            !contains(&snap, hex(secret).as_bytes()),
            "{name} is in the snapshot, as hex"
        );
    }
}
