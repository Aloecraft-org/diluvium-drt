//! The swarm (SPEC.md §8): a faithful port of `dvs.c` semantics, reaching
//! instances through the [`engine::Engine`] seam.
//!
//! The six owned things, exhaustive by design — anything proposed for this
//! layer that does not reduce to one of them belongs in a program:
//!
//! 1. An instance table, because endpoints resolve against it.
//! 2. One parent field per instance, for subtree kill and attenuation.
//! 3. The capability set per instance, for enforcement.
//! 4. Draining `system/lifecycle` and calling back into the runtime.
//! 5. Enforcing per-instance budgets — the numbers, never the policy.
//! 6. The snapshot cache and `wake_on_message` delivery.
//!
//! There is still no supervisor type: restart, backoff, and topology for
//! guest agents are programs holding the lifecycle capability, and
//! orchestration strategies never touch guest agents.
//!
//! Semantics to preserve exactly, differential-tested against `dvs.c` via the
//! ported capability suite: attenuation-only grants, subtree kill,
//! self-initiated hibernation (a program parks after pushing
//! `{op="hibernate"}`; nothing swaps an instance out behind its back), the
//! four-row `dvs_push` delivery table, the bounded wake buffer (a `LIMIT`
//! refusal, never growth), the spawn rate limit, and the 32KB request cap.
//!
//! **Status:** the port itself is blocked on the completed `diluvium-sys`
//! transcription upstream (SPEC.md §4 — snapshot/budget/endpoints are
//! missing there today). What ships now are the seams the port will fill:
//! the [`engine`] trait pair, the [`snapshot`] store, and the [`refs`]
//! encoding — the last is mandatory *now* because refs are captured inside
//! snapshots, and a process-local index would make every stored snapshot
//! untranslatable.

pub mod engine;
pub mod refs;
pub mod snapshot;

/// An instance handle. 0 is never valid, and a handle is never reused —
/// `dvs_id`'s contract, kept.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct InstanceId(pub u32);

/// The request-size cap `dvs.c` enforces (32KB), kept as a named constant so
/// the differential tests can assert against one number.
pub const REQUEST_CAP_BYTES: usize = 32 * 1024;
