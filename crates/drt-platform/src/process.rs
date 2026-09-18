//! Starting a child, and owning everything it starts.
//!
//! ## surface block
//!
//! - Entry points: [`Tree::spawn`], which starts a [`Command`] as the root
//!   of a tree this owns, and [`Tree::sweep`], which kills the tree now.
//!   [`Tree`]'s `Drop` sweeps too, so a tree cannot outlive its owner by
//!   being forgotten.
//! - Configurable values: [`SWEEP_EXIT_CODE`], what a swept tree's members
//!   report on Windows. None on unix: a signal is not a code.
//! - Fan-out: the two bodies, [`unix`] and [`windows`], each `cfg`-gated
//!   and each implementing the same three operations. A target that is
//!   neither has no `process` module at all (see `lib.rs`), because a
//!   host that cannot spawn should fail to compile a caller rather than
//!   refuse one at runtime.
//!
//! # Why this exists
//!
//! A deadline that kills only the direct child is a deadline a child
//! escapes by starting one more process. `connectors/exec` has always
//! known this: it puts the child in its own process group and aims the
//! kill at the group. That is the *unix spelling* of one idea -- a
//! process tree with an owner -- and this module is the idea itself, so
//! that the exec connector, the plugin channel's transports, and anything
//! else that starts a program state the intent once and get whatever the
//! platform calls it.
//!
//! On unix the tree is a process group and the sweep is `SIGKILL` to its
//! negative pid. On Windows it is a Job Object with
//! `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`, which is strictly *stronger*: a
//! process group on unix is advisory, and a child that calls `setsid` or
//! `setpgid` leaves it, while a process assigned to a job cannot leave
//! that job and neither can anything it starts. The weaker guarantee is
//! the one to design against, so nothing above this module may assume a
//! tree is inescapable.
//!
//! # The assignment window, stated rather than hidden
//!
//! On unix the group is set *before* `exec`, by the child itself, so
//! there is no window: the child is in its group before it runs a single
//! instruction of the program.
//!
//! On Windows the job is created before the spawn but a process can only
//! be assigned *after* it exists. Between `CreateProcess` returning and
//! `AssignProcessToJobObject`, the child is running and unowned, and a
//! grandchild started in that window is outside the job forever. The
//! window is microseconds against a `CreateProcess` the child must itself
//! perform, so it is small, but it is not zero and this comment is the
//! only honest place to say so.
//!
//! Closing it costs more than it is worth here: `CREATE_SUSPENDED` would
//! make the spawn atomic with the assignment, but resuming the child
//! needs its main thread's handle, which `std::process::Command` does not
//! surrender, so it would mean enumerating the process's threads through
//! ToolHelp to find the one to resume. If a plugin or a program is ever
//! observed leaking a grandchild through this window, that is the change
//! to make, and [`Tree::spawn`] is the one place it would go.

use std::io;
use std::process::{Child, Command};

/// What a swept tree's members report as their exit code on Windows.
///
/// One, not zero: a tree that was killed did not succeed, and a caller
/// reading the code should not mistake a sweep for a clean exit. Unix has
/// no equivalent knob -- a `SIGKILL`ed process is reported as signalled,
/// which is a different thing from any code -- so this constant is
/// Windows's alone and is named here rather than buried at its use.
pub const SWEEP_EXIT_CODE: u32 = 1;

/// A spawned child and everything it starts, owned as one.
///
/// Dropping sweeps. That is the rule on both platforms and it is what
/// makes the type worth having: every exit path from a call that started
/// a program -- the reply, the deadline, a cap tripping, an error, a
/// panic unwinding through -- ends with the tree gone, without each path
/// having to remember.
pub struct Tree {
    #[cfg(unix)]
    inner: unix::Tree,
    #[cfg(windows)]
    inner: windows::Tree,
}

impl Tree {
    /// Start `command` as the root of a tree this owns.
    ///
    /// The returned [`Child`] is the root, for the caller to feed, read
    /// and wait on as usual; the [`Tree`] is everything that root starts,
    /// and holding it is what keeps the promise. A caller that drops the
    /// tree and keeps the child has kept a process and thrown away the
    /// only handle on its descendants, which is why the two come back
    /// together and neither is derivable from the other.
    ///
    /// Anything the platform refuses here -- a job that cannot be created,
    /// a child that cannot be assigned -- is an [`io::Error`] and the
    /// child is not left running: a tree that could not be owned is not a
    /// tree this module hands back half-made.
    pub fn spawn(command: &mut Command) -> io::Result<(Child, Tree)> {
        #[cfg(unix)]
        {
            let (child, inner) = unix::spawn(command)?;
            Ok((child, Tree { inner }))
        }
        #[cfg(windows)]
        {
            let (child, inner) = windows::spawn(command)?;
            Ok((child, Tree { inner }))
        }
    }

    /// Kill every process in the tree, now.
    ///
    /// Idempotent, and quiet about a tree that is already empty: every
    /// exit path calls this, most of them after the root has already
    /// exited on its own, so "there was nothing to kill" is the ordinary
    /// case and not a failure worth returning.
    pub fn sweep(&self) {
        self.inner.sweep();
    }
}

impl Drop for Tree {
    fn drop(&mut self) {
        self.inner.sweep();
    }
}

// depth: the two bodies

#[cfg(unix)]
mod unix {
    use std::io;
    use std::os::unix::process::CommandExt;
    use std::process::{Child, Command};

    pub struct Tree {
        pid: u32,
    }

    pub fn spawn(command: &mut Command) -> io::Result<(Child, super::unix::Tree)> {
        // Its own process group, set between fork and exec by the child
        // itself, so a kill aimed at the group after `spawn` returns
        // reaches everything the program starts -- and so that there is no
        // window in which the child is running outside its group.
        command.process_group(0);
        let child = command.spawn()?;
        let pid = child.id();
        Ok((child, Tree { pid }))
    }

    impl Tree {
        pub fn sweep(&self) {
            // SAFETY: a plain syscall against a group this process created
            // by asking for it at spawn. No memory crosses the boundary.
            // `ESRCH` -- the group is already empty -- is the ordinary
            // case on a clean exit and is deliberately not reported.
            unsafe {
                libc::kill(-(self.pid as libc::pid_t), libc::SIGKILL);
            }
        }
    }
}

#[cfg(windows)]
mod windows {
    use std::io;
    use std::process::{Child, Command};

    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
        SetInformationJobObject, TerminateJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };
    use windows_sys::Win32::System::Threading::{
        OpenProcess, PROCESS_SET_QUOTA, PROCESS_TERMINATE,
    };

    /// A job handle, and the processes in it.
    ///
    /// The handle is the tree: closing it kills every member, because
    /// that is what `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` means, so `Drop`
    /// needs no second thought about whether a sweep already happened.
    pub struct Tree {
        job: HANDLE,
    }

    // SAFETY: a job handle is a kernel handle, not a thread-affine
    // resource; the Win32 calls this module makes on it are all
    // documented as callable from any thread. `Tree` is moved between
    // threads by callers that spawn a child on one and sweep it from
    // another (the exec connector's deadline does exactly this).
    unsafe impl Send for Tree {}
    unsafe impl Sync for Tree {}

    pub fn spawn(command: &mut Command) -> io::Result<(Child, Tree)> {
        // The job first, so that a failure to make one is a failure
        // before anything is running -- there is no worse outcome here
        // than a spawned child with nothing owning it.
        let job = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        if job.is_null() {
            return Err(io::Error::last_os_error());
        }
        let tree = Tree { job };

        let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        let set = unsafe {
            SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                &limits as *const _ as *const core::ffi::c_void,
                core::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        };
        if set == 0 {
            // `tree` drops here and closes the handle; nothing is running
            // yet, so kill-on-close has nothing to kill.
            return Err(io::Error::last_os_error());
        }

        let child = command.spawn()?;

        // The window this module's header describes is exactly here.
        let assigned = assign(job, child.id());
        if let Err(e) = assigned {
            // An unassignable child is not left running: it is not in the
            // job, so dropping the job would not reach it, and a caller
            // that got an error must not also inherit a process.
            let _ = kill_unowned(child.id());
            return Err(e);
        }
        Ok((child, tree))
    }

    /// Put one already-running process into the job.
    fn assign(job: HANDLE, pid: u32) -> io::Result<()> {
        // `PROCESS_SET_QUOTA | PROCESS_TERMINATE` is what
        // `AssignProcessToJobObject` documents as the required access, and
        // asking for no more is the same discipline the rest of this tree
        // applies to capabilities.
        let process = unsafe { OpenProcess(PROCESS_SET_QUOTA | PROCESS_TERMINATE, 0, pid) };
        if process.is_null() {
            return Err(io::Error::last_os_error());
        }
        let ok = unsafe { AssignProcessToJobObject(job, process) };
        let err = io::Error::last_os_error();
        unsafe {
            CloseHandle(process);
        }
        if ok == 0 {
            return Err(err);
        }
        Ok(())
    }

    /// Kill a process that could not be put in a job, by handle.
    fn kill_unowned(pid: u32) -> io::Result<()> {
        use windows_sys::Win32::System::Threading::TerminateProcess;
        let process = unsafe { OpenProcess(PROCESS_TERMINATE, 0, pid) };
        if process.is_null() {
            return Err(io::Error::last_os_error());
        }
        unsafe {
            TerminateProcess(process, super::SWEEP_EXIT_CODE);
            CloseHandle(process);
        }
        Ok(())
    }

    impl Tree {
        pub fn sweep(&self) {
            // Every member, whatever it started, whatever it is doing.
            // A job with no live members is not an error to terminate,
            // which is the ordinary case after a clean exit.
            unsafe {
                TerminateJobObject(self.job, super::SWEEP_EXIT_CODE);
            }
        }
    }

    impl Drop for Tree {
        fn drop(&mut self) {
            // The sweep above already ran (`Tree`'s own `Drop` calls it),
            // so this only releases the handle -- and kill-on-close makes
            // the release a second sweep for anything that somehow
            // survived, which costs nothing and closes a gap rather than
            // opening one.
            unsafe {
                CloseHandle(self.job);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Tree;
    use std::process::{Command, Stdio};

    /// The root runs, is waited on, and the tree is dropped after: the
    /// ordinary path, which must not report a failure just because the
    /// sweep found nothing left to kill.
    #[test]
    fn a_child_that_exits_on_its_own_leaves_a_tree_that_sweeps_quietly() {
        let mut command = sleeper(0);
        let (mut child, tree) = Tree::spawn(&mut command).expect("a tree");
        let status = child.wait().expect("the child exits");
        assert!(status.success(), "{status:?}");
        tree.sweep();
        drop(tree);
    }

    /// The deadline path: the root is still running, and the sweep ends
    /// it. Without an owner this is the case that leaks.
    #[test]
    fn a_sweep_ends_a_child_that_is_still_running() {
        let mut command = sleeper(30);
        let (mut child, tree) = Tree::spawn(&mut command).expect("a tree");
        tree.sweep();
        let status = child.wait().expect("the child is reaped");
        assert!(
            !status.success(),
            "a swept child did not succeed: {status:?}"
        );
    }

    /// Dropping is sweeping, so a caller that returns early -- an error,
    /// a cap, an unwind -- does not have to remember.
    #[test]
    fn dropping_the_tree_ends_the_child() {
        let mut command = sleeper(30);
        let (mut child, tree) = Tree::spawn(&mut command).expect("a tree");
        drop(tree);
        let status = child.wait().expect("the child is reaped");
        assert!(!status.success(), "a dropped tree left a child: {status:?}");
    }

    /// A program that sleeps for `secs`, spelled for whichever platform
    /// is running the test. Both are present on a bare install of their
    /// own system, which is what a test may assume and a shipped example
    /// may not.
    fn sleeper(secs: u32) -> Command {
        #[cfg(unix)]
        let mut command = {
            let mut c = Command::new("/bin/sh");
            c.arg("-c").arg(format!("sleep {secs}"));
            c
        };
        #[cfg(windows)]
        let mut command = {
            let mut c = Command::new("cmd.exe");
            // `timeout` needs a console; `ping` to loopback is the
            // portable Windows sleep, one second per echo.
            c.arg("/c")
                .arg(format!("ping -n {} 127.0.0.1 > NUL", secs + 1));
            c
        };
        command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        command
    }
}
