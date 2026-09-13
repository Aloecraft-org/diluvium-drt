//! The `socketpair` transport: DRT forks, execs, and keeps fd 3.
//!
//! # Surface
//!
//! Entry points:
//! - [`ProcessChannel::spawn`] — start a plugin and hold its stream.
//! - [`ProcessChannel::pid`] — the child, for a caller that reports it.
//!
//! Configurable values:
//! - [`PLUGIN_FD`] — three. The C host's number, and changing it breaks
//!   every plugin written for either host.
//! - [`SHUTDOWN_SIGNAL`] — what a dropped channel sends the group.
//!
//! Fan-out: none. This file is one transport; the others in
//! `doc/Plugins.md` §4.1 are their own files and share only [`Channel`].
//!
//! # The four disciplines, all borrowed from `connectors/exec`
//!
//! 1. **An absolute path, never a `PATH` search.** What a deployment wired
//!    is what runs. A `PATH` lookup would make the plugin that answers
//!    depend on the environment the operator happened to start DRT in.
//! 2. **Its own process group**, set before `exec`, so the kill on drop
//!    reaches everything the plugin started and not just the plugin.
//! 3. **Nothing above the three standard descriptors is inherited** — Rust
//!    opens everything close-on-exec — except fd 3, which is put there
//!    deliberately by `dup2`, whose result is not close-on-exec.
//! 4. **stdout and stderr are the plugin's own.** They are inherited on
//!    purpose: a plugin author's `print` has to go somewhere, and the
//!    reason it can safely go to stdout is that stdout is *not* the
//!    channel. That was the call `doc/Plugins.md` §2 made — one stray log
//!    line on the channel desynchronises framing forever — and keeping the
//!    frames on their own descriptor is what makes logging harmless.

use std::os::unix::io::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};

use crate::channel::{Channel, ChannelError};

/// The descriptor a plugin finds its channel on. The C host's choice.
pub const PLUGIN_FD: RawFd = 3;

/// Sent to the plugin's process group when the channel is dropped.
pub const SHUTDOWN_SIGNAL: libc::c_int = libc::SIGKILL;

/// A plugin subprocess and the socket to it.
#[derive(Debug)]
pub struct ProcessChannel {
    /// The host's end, non-blocking. Owned, so it closes on drop.
    fd: OwnedFd,
    child: Child,
}

impl ProcessChannel {
    /// Start `exec` with its channel on fd 3.
    ///
    /// `exec` must be absolute; a relative path is refused by name rather
    /// than resolved, because resolving it would consult `PATH`.
    pub fn spawn(exec: &Path, argv: &[String]) -> Result<Self, ChannelError> {
        if !exec.is_absolute() {
            return Err(ChannelError::Broken(format!(
                "a plugin's exec is an absolute path; '{}' is not, and this \
                 host does not search PATH for one",
                exec.display()
            )));
        }

        let (host, child_end) = socketpair()?;
        set_nonblocking(host.as_raw_fd())?;

        let mut command = Command::new(exec);
        command
            .args(argv)
            .stdin(Stdio::null())
            // Inherited on purpose: see discipline 4 in the module header.
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .process_group(0);

        // `dup2` the plugin's end onto fd 3 in the child. Both calls are
        // async-signal-safe, which is the whole requirement on this
        // closure: it runs between `fork` and `exec`.
        let child_raw = child_end.as_raw_fd();
        unsafe {
            command.pre_exec(move || {
                if libc::dup2(child_raw, PLUGIN_FD) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }

        let child = command.spawn().map_err(|e| {
            ChannelError::Broken(format!(
                "the plugin '{}' would not start: {e}",
                exec.display()
            ))
        })?;
        // The parent has no use for the child's end; holding it open would
        // mean a read here never sees EOF when the plugin exits.
        drop(child_end);

        Ok(Self { fd: host, child })
    }

    /// The plugin's pid, for a caller that wants to name it in a message.
    pub fn pid(&self) -> u32 {
        self.child.id()
    }
}

impl Channel for ProcessChannel {
    fn write_some(&mut self, bytes: &[u8]) -> Result<usize, ChannelError> {
        if bytes.is_empty() {
            return Ok(0);
        }
        loop {
            let n = unsafe {
                libc::write(
                    self.fd.as_raw_fd(),
                    bytes.as_ptr() as *const libc::c_void,
                    bytes.len(),
                )
            };
            if n >= 0 {
                return Ok(n as usize);
            }
            match errno() {
                libc::EINTR => continue,
                e if e == libc::EAGAIN || e == libc::EWOULDBLOCK => return Ok(0),
                libc::EPIPE | libc::ECONNRESET => return Err(ChannelError::Closed),
                _ => return Err(ChannelError::Broken(last_error())),
            }
        }
    }

    fn read_some(&mut self, out: &mut Vec<u8>) -> Result<usize, ChannelError> {
        let mut buf = [0u8; 8192];
        loop {
            let n = unsafe {
                libc::read(
                    self.fd.as_raw_fd(),
                    buf.as_mut_ptr() as *mut libc::c_void,
                    buf.len(),
                )
            };
            if n > 0 {
                out.extend_from_slice(&buf[..n as usize]);
                return Ok(n as usize);
            }
            if n == 0 {
                // EOF: the plugin closed its end or exited.
                return Err(ChannelError::Closed);
            }
            match errno() {
                libc::EINTR => continue,
                e if e == libc::EAGAIN || e == libc::EWOULDBLOCK => return Ok(0),
                libc::ECONNRESET => return Err(ChannelError::Closed),
                _ => return Err(ChannelError::Broken(last_error())),
            }
        }
    }
}

impl Drop for ProcessChannel {
    /// Kill the group, then reap. A plugin that ignored the closing socket
    /// does not outlive the deployment, and the group means whatever it
    /// started does not either.
    fn drop(&mut self) {
        let pid = self.child.id() as libc::pid_t;
        unsafe {
            libc::killpg(pid, SHUTDOWN_SIGNAL);
        }
        let _ = self.child.wait();
    }
}

// depth: the three libc calls this file needs and the errno spelling.

fn socketpair() -> Result<(OwnedFd, OwnedFd), ChannelError> {
    let mut fds = [0 as RawFd; 2];
    // SOCK_CLOEXEC on both: the child's end reaches fd 3 through `dup2`,
    // whose result is deliberately not close-on-exec, and nothing else
    // should survive the exec at all.
    let rc = unsafe {
        libc::socketpair(
            libc::AF_UNIX,
            libc::SOCK_STREAM | libc::SOCK_CLOEXEC,
            0,
            fds.as_mut_ptr(),
        )
    };
    if rc == -1 {
        return Err(ChannelError::Broken(format!(
            "no socketpair for the plugin channel: {}",
            last_error()
        )));
    }
    unsafe { Ok((OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1]))) }
}

fn set_nonblocking(fd: RawFd) -> Result<(), ChannelError> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags == -1 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } == -1 {
        return Err(ChannelError::Broken(format!(
            "the plugin channel would not go non-blocking: {}",
            last_error()
        )));
    }
    Ok(())
}

fn errno() -> libc::c_int {
    std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
}

fn last_error() -> String {
    std::io::Error::last_os_error().to_string()
}
