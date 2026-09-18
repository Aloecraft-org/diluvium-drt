//! Whether this host will let us do a privileged thing, asked once and
//! answered per target.
//!
//! # Surface
//!
//! - [`Privilege`] — what is being asked about. One variant today.
//! - [`Held`] — the answer, four states. Not a `bool`, and §2 below is why.
//! - [`held`] — ask.
//!
//! Configurable values: none. This module measures; it decides nothing.
//!
//! Fan-out: one `cfg` arm per target family, each a private `net_admin`,
//! with a catch-all so a target nobody listed still compiles.
//!
//! # 1. Why this is not a `bool`
//!
//! The question "does this process hold CAP_NET_ADMIN" has three honest
//! answers on Linux and a fourth everywhere else, and collapsing them
//! into yes/no is what cost an operator an hour in issue #21: a container
//! already running as uid 0 was told to go and acquire a capability it
//! held, and the conclusion drawn was "the capability isn't taking
//! effect". A probe that cannot tell must say so rather than guess `false`.
//!
//! # 2. Why `Likely` exists
//!
//! Only Linux can read the privilege itself. macOS and Windows can read a
//! *proxy* for it -- being root, holding an elevated token -- and a proxy
//! that says yes has not established the thing asked about. `Likely` is
//! that distinction kept rather than rounded, because rounding it up is
//! how a probe starts lying and rounding it down is how it becomes
//! useless.
//!
//! # 3. What this is not
//!
//! **Not `drt_caps::Capability`.** That word is taken in this tree and
//! means the guest authorization model -- `host:fs/*`, attenuation,
//! provenance. This is an OS privilege on the machine DRT is running on,
//! and the two must not share a name.
//!
//! **Not a decision.** A `NetAdmin` answer is strictly smaller than "can
//! this host bring up a tunnel": `doc/WireGuard.md` lists four errnos and
//! three of them happen *with* the privilege held. So this is an input to
//! `wireguard::interface_here`, which composes it with what it learned by
//! opening the device node, and never a gate on its own. `mode` stays
//! explicit: a config that did not ask for userspace is not steered into
//! it by a missing privilege.

/// What is being asked about.
///
/// One variant, and named for the privilege rather than for the feature
/// that wants it: the question "may this process configure network
/// interfaces" outlives WireGuard being the only caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Privilege {
    /// Configure network interfaces: `CAP_NET_ADMIN` on Linux, and
    /// whatever each other host calls the same authority.
    NetAdmin,
}

/// What a host can say about a privilege.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Held {
    /// Held. Established by reading the privilege itself.
    Yes,
    /// Not held. Established the same way.
    No,
    /// Probably held: a proxy said so, and the proxy is not the thing
    /// asked about. Root on macOS, an elevated token on Windows.
    Likely,
    /// This host cannot be asked -- `/proc` is not mounted, an API
    /// refused, or the target has no such notion. Says nothing either
    /// way, on purpose.
    Unknown,
}

impl Held {
    /// Whether this is a definite no. The one thing a caller may act on
    /// without further qualification: every other state leaves the
    /// question at least partly open, and `Unknown` especially must not
    /// be read as "fine".
    pub fn is_denied(self) -> bool {
        self == Held::No
    }
}

/// Ask this host about `which`.
pub fn held(which: Privilege) -> Held {
    match which {
        Privilege::NetAdmin => net_admin(),
    }
}

// depth: one arm per target family

/// `CapEff` in `/proc/self/status` is the effective set as a hex mask,
/// and reading it covers root without a separate uid check, since root's
/// effective set is full. Eight lines of std: the `caps` crate buys
/// nothing over this and would add a dependency to a build whose release
/// target is static musl.
#[cfg(any(target_os = "linux", target_os = "android"))]
fn net_admin() -> Held {
    /// CAP_NET_ADMIN's bit in the mask.
    const CAP_NET_ADMIN: u32 = 12;

    let Ok(status) = std::fs::read_to_string("/proc/self/status") else {
        return Held::Unknown;
    };
    let Some(mask) = status
        .lines()
        .find_map(|line| line.strip_prefix("CapEff:"))
        .map(str::trim)
    else {
        return Held::Unknown;
    };
    match u64::from_str_radix(mask, 16) {
        Ok(bits) if bits & (1 << CAP_NET_ADMIN) != 0 => Held::Yes,
        Ok(_) => Held::No,
        Err(_) => Held::Unknown,
    }
}

/// macOS has no capability to read, so root is the proxy -- hence
/// [`Held::Likely`] rather than [`Held::Yes`] when it says so. Not root
/// is a real no: nothing short of it allocates a `utun`.
#[cfg(target_os = "macos")]
fn net_admin() -> Held {
    // SAFETY: `geteuid` takes nothing, returns a `uid_t`, and cannot fail.
    if unsafe { libc::geteuid() } == 0 {
        Held::Likely
    } else {
        Held::No
    }
}

/// Windows membership of the built-in Administrators group, which is the
/// proxy here for the same reason root is on macOS -- and a weaker one:
/// `doc/Platforms.md` has kernel mode needing `wintun.dll` beside the
/// binary, which is file presence and not token elevation. So an elevated
/// token is necessary and not sufficient, and the caller composes this
/// with that file check rather than treating it as the answer.
///
/// The two RIDs are defined here rather than imported: they live behind
/// `Win32_System_SystemServices`, and enabling a whole feature for two
/// integers that are fixed by the platform ABI is a worse trade than
/// writing them down.
#[cfg(windows)]
fn net_admin() -> Held {
    use windows_sys::Win32::Security::{
        AllocateAndInitializeSid, CheckTokenMembership, FreeSid, PSID, SECURITY_NT_AUTHORITY,
    };

    const SECURITY_BUILTIN_DOMAIN_RID: u32 = 32;
    const DOMAIN_ALIAS_RID_ADMINS: u32 = 544;

    // SAFETY: the SID is allocated by the call below and freed on every
    // path out of this block; `CheckTokenMembership` is given a null
    // handle, which is documented as "the calling thread's effective
    // token", and a `BOOL` it may write.
    unsafe {
        let authority = SECURITY_NT_AUTHORITY;
        let mut admins: PSID = std::ptr::null_mut();
        if AllocateAndInitializeSid(
            &authority,
            2,
            SECURITY_BUILTIN_DOMAIN_RID,
            DOMAIN_ALIAS_RID_ADMINS,
            0,
            0,
            0,
            0,
            0,
            0,
            &mut admins,
        ) == 0
        {
            return Held::Unknown;
        }
        let mut member = 0;
        let asked = CheckTokenMembership(std::ptr::null_mut(), admins, &mut member);
        FreeSid(admins);
        if asked == 0 {
            Held::Unknown
        } else if member != 0 {
            Held::Likely
        } else {
            Held::No
        }
    }
}

/// Everything else, as one catch-all rather than a list of named targets.
///
/// A list is how `wasm32-unknown-unknown` gets missed: its `target_os` is
/// `"unknown"`, so a `cfg(target_os = "wasi")` arm does not cover it and
/// the browser build -- which this repo ships -- would fail to compile at
/// the first call site. A catch-all cannot have that hole.
#[cfg(not(any(
    target_os = "linux",
    target_os = "android",
    target_os = "macos",
    windows
)))]
fn net_admin() -> Held {
    Held::Unknown
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Whatever this host is, it answers, and the answer is one of the
    /// four. The point is that no target is missing an arm -- the hole
    /// the catch-all exists to close.
    #[test]
    fn every_host_answers() {
        let h = held(Privilege::NetAdmin);
        assert!(matches!(
            h,
            Held::Yes | Held::No | Held::Likely | Held::Unknown
        ));
    }

    /// Only a definite `No` is actionable. `Unknown` especially must not
    /// read as denied, because that is the collapse issue #21 was about.
    #[test]
    fn only_a_definite_no_is_denied() {
        assert!(Held::No.is_denied());
        assert!(!Held::Yes.is_denied());
        assert!(!Held::Likely.is_denied());
        assert!(!Held::Unknown.is_denied());
    }

    /// On Linux the mask is readable in any ordinary environment, so the
    /// answer is definite rather than `Unknown`. This is the arm that
    /// reads the privilege itself, and `Likely` would be wrong from it.
    #[cfg(target_os = "linux")]
    #[test]
    fn linux_reads_the_privilege_itself() {
        let h = held(Privilege::NetAdmin);
        assert!(
            matches!(h, Held::Yes | Held::No),
            "the mask is readable here, so the answer should be definite, got {h:?}"
        );
    }
}
