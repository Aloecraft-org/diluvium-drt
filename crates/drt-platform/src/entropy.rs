//! Entropy: the platform CSPRNG and nothing else.
//!
//! `doc/HostBaseline.md`'s third rule is the reason this module has one
//! function and no fallback: a host that cannot supply entropy refuses,
//! it never returns zeros, a counter, or a PRNG seeded from the clock.
//! `getrandom` reaches `getrandom(2)`, wasi's `random_get`, or
//! `crypto.getRandomValues`, and when none is there the error says so.

use std::fmt;

/// No entropy source, with the platform's own reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NoEntropy(pub String);

impl fmt::Display for NoEntropy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "no entropy source: {}", self.0)
    }
}

impl std::error::Error for NoEntropy {}

/// Fill `buf` from the CSPRNG.
pub fn fill(buf: &mut [u8]) -> Result<(), NoEntropy> {
    getrandom::fill(buf).map_err(|e| NoEntropy(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn two_reads_differ_and_neither_is_zero() {
        let mut a = [0u8; 32];
        let mut b = [0u8; 32];
        fill(&mut a).unwrap();
        fill(&mut b).unwrap();
        assert_ne!(a, [0u8; 32]);
        assert_ne!(a, b);
    }
}
