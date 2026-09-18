//! The leaf adapters (doc/Wasm.md D5): the four places DRT touches the
//! platform, `cfg`-gated per target here so that nothing above them is.
//!
//! Three targets, one crate: native, `wasm32-wasip2` under wasmtime, and
//! `wasm32-unknown-unknown` in a page. The first two are `std` all the way
//! down and differ from each other in nothing this crate can see; the
//! browser is where every module below has a second body.
//!
//! ## surface block
//!
//! - [`clock`]: `Instant`, `SystemTime`, `wall_ms`, `wall_secs`.
//! - [`entropy`]: `fill`, the CSPRNG.
//! - [`fs`]: the [`fs::Backend`] trait, [`fs::StdFs`], [`fs::MemFs`], and
//!   the process-wide [`fs::host`] the fs connector and the program loader
//!   read through.
//! - [`stdio`]: `write`, `stdout`, `stderr`, the sink a page installs, and
//!   `bytes_as_written`, the once-at-startup call that keeps Windows's C
//!   runtime from rewriting the C core's line endings.
//! - [`process`]: [`process::Tree`], a spawned child and everything it
//!   starts, owned as one and swept as one. Native only -- neither wasm
//!   target has a process API -- so a caller that spawns is gated on the
//!   platform rather than told no at runtime.
//! - [`detect`]: which of the three this build is.

pub mod clock;
pub mod entropy;
pub mod fs;
/// Starting a child, and owning everything it starts.
///
/// Absent on both wasm targets: WASI has no process API and none is on
/// the standardization track (`doc/Platforms.md`), and a page has no
/// processes at all. A caller that spawns therefore fails to compile off
/// native, which is the honest failure -- a runtime refusal would imply
/// the capability exists and was withheld.
#[cfg(any(unix, windows))]
pub mod privilege;
pub mod process;
pub mod stdio;

/// The three platforms this crate knows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Platform {
    Native,
    /// `wasm32-wasip2`: wasmtime, or anything else speaking WASI 0.2.
    Wasi,
    /// `wasm32-unknown-unknown`: a page, driven by JavaScript.
    Browser,
}

/// Which platform this build is, decided at compile time.
pub fn detect() -> Platform {
    #[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
    {
        Platform::Browser
    }
    #[cfg(all(target_arch = "wasm32", not(target_os = "unknown")))]
    {
        Platform::Wasi
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        Platform::Native
    }
}
