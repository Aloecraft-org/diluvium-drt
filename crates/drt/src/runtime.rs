//! The ambient tokio runtime under the native drive loops.
//!
//! The hostcall pump (`drt_swarm::pump`, doc/Wasm.md M3) polls a connector's
//! future once per pass with a no-op waker and parks it when it answers
//! `Pending`, so a slow connector no longer stalls every instance — that was
//! the whole point of it. But a future only answers `Pending` if it can
//! *await* something, and every tokio-backed connector (`rest`, `ssh`,
//! `ssmtp`) begins its call with:
//!
//! ```text
//! match tokio::runtime::Handle::try_current() {
//!     Ok(_)  => work.await,                    // parks: the pump's shape
//!     Err(_) => own_runtime().block_on(work),  // blocks the drive thread
//! }
//! ```
//!
//! and until 0.5.0rc4 nothing entered a runtime around `run`, `repl` or
//! `start`, so `try_current` always failed and every one of them took the
//! second arm. Measured (`crates/drt/tests/start.rs`, `parking`): a child's
//! three-second `rest/get` held its parent's next hostcall for 3004 ms in
//! rc3, and for 1 ms with this entered. The pump was right; the reactor was
//! missing.
//!
//! So each native drive loop enters this runtime before its first tick. The
//! loop never `block_on`s it — the pump keeps polling on its own cadence —
//! and one worker thread drives the I/O and timer drivers underneath, which
//! is what turns a poll into progress. It is entered, not run: the drive
//! thread stays the drive thread.
//!
//! Leaked, never dropped: FM-1 (`doc/Failure-Modes.md`) is a use-after-free
//! in tokio 1.53.1's runtime teardown, racing a parked blocking worker, and
//! `lookup_host` parks one on every hostname resolved. A `OnceLock` holding
//! the runtime for the life of the process is the mitigation `relay`,
//! `stun` and `tunnel` spell as `mem::forget`, spelled as ownership.
//!
//! Absent from a build that carries no tokio-backed connector: the `runtime`
//! feature is implied by `connector-rest`, `connector-ssh` and
//! `connector-ssmtp`, and [`enter`] is a no-op without it. `slim` has no
//! runtime and needs none; `wasi` and `web` have no threads to drive one
//! and no connector that would ask.
//!
//! `exec` is not helped by this and is not meant to be: its body is
//! `std::process`, which has no future to park. It stalls the loop for the
//! child's lifetime, as its own doc says, until it is moved onto
//! `spawn_blocking` — a separate change.

/// Enter the ambient runtime on this thread for as long as the guard lives.
/// The drive loops hold it across their whole run.
#[cfg(feature = "runtime")]
pub fn enter() -> Option<tokio::runtime::EnterGuard<'static>> {
    static RT: std::sync::OnceLock<tokio::runtime::Runtime> = std::sync::OnceLock::new();
    let rt = RT.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .thread_name("drt-runtime")
            .enable_all()
            .build()
            .expect("a tokio runtime for the drive loop")
    });
    Some(rt.enter())
}

/// No tokio in this build, so nothing to enter: every connector here
/// answers on the spot.
#[cfg(not(feature = "runtime"))]
pub fn enter() -> Option<()> {
    None
}
