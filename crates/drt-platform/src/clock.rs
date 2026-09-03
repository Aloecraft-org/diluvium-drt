//! The clock. `Instant` for intervals, `SystemTime` for the wall, and the
//! two readings the `time` and `crypto` connectors actually take.
//!
//! On the browser `std::time::Instant::now()` and `SystemTime::now()`
//! compile and then panic ("time not implemented on this platform"), so
//! the types are re-exported from `web-time` there, which is
//! `performance.now()` and `Date.now()` behind the same API. Everywhere
//! else they are `std`'s.

use std::fmt;

#[cfg(not(all(target_arch = "wasm32", target_os = "unknown")))]
pub use std::time::{Instant, SystemTime, UNIX_EPOCH};
#[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
pub use web_time::{Instant, SystemTime, UNIX_EPOCH};

/// The wall clock reads before 1970, which is the only way it can fail.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BeforeEpoch;

impl fmt::Display for BeforeEpoch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("the host clock is before the epoch")
    }
}

impl std::error::Error for BeforeEpoch {}

/// Wall-clock milliseconds since the Unix epoch: what `host.time()` answers.
pub fn wall_ms() -> Result<u64, BeforeEpoch> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .map_err(|_| BeforeEpoch)
}

/// Wall-clock seconds since the Unix epoch: what a JWT's `iat` wants.
pub fn wall_secs() -> Result<u64, BeforeEpoch> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .map_err(|_| BeforeEpoch)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_wall_is_after_2020_and_monotonic_does_not_run_backwards() {
        assert!(wall_ms().unwrap() > 1_577_836_800_000);
        assert!(wall_secs().unwrap() > 1_577_836_800);
        let a = Instant::now();
        let b = Instant::now();
        assert!(b >= a);
    }
}
