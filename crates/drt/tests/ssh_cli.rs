//! `drt ssh` and the REPL's `:ssh`, as a person meets them: the real
//! binary on a pseudo-terminal, against OpenSSH's sshd on a port of the
//! test's own.
//!
//! The session underneath is held to its promises in
//! `connectors/ssh/tests/interactive.rs`. What only a terminal shows is
//! here: the known_hosts question on a tty, raw mode, the window size and
//! a resize reaching the remote `stty`, the exit status coming back as
//! the command's, the REPL's prompt returning afterwards, and the
//! config-scoped `:ssh` going through the grant.
//!
//! Needs `/usr/sbin/sshd` and `ssh-keygen`. Without them every test says
//! it was skipped, on stderr, and passes: a machine with no sshd cannot
//! answer this question either way.
//!
//! sshd runs as root, through `sudo -n` when the test does not, for the
//! reason `crates/drt-ssh-web/page/e2e.mjs` gives: an unprivileged sshd
//! cannot give a pty to the tty group, and ends every pty session it
//! opens. `UsePAM`, because without it a root sshd refuses a locked
//! account, which is what a fresh CI user's is.
//!
//! ## surface block
//!
//! - Entry points: the `#[test]`s below.
//! - Configurable: [`WITHIN`], how long any one thing may take to appear.
//! - Fan-out: [`Sshd`] is the far end; [`Pty`] is the terminal.

#![cfg(all(unix, feature = "connector-ssh"))]

use std::io::{Read, Write};
use std::os::fd::{FromRawFd, OwnedFd, RawFd};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const WITHIN: Duration = Duration::from_secs(20);

/// OpenSSH on a free port, admitting one client key, as whoever runs the
/// test.
struct Sshd {
    child: Child,
    port: u16,
    dir: tempfile::TempDir,
    user: String,
}

impl Sshd {
    fn start() -> Option<Sshd> {
        if !Path::new("/usr/sbin/sshd").exists() {
            eprintln!("skipped: no /usr/sbin/sshd on this machine");
            return None;
        }
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        for name in ["host", "client"] {
            let ok = Command::new("ssh-keygen")
                .args(["-q", "-t", "ed25519", "-N", "", "-f"])
                .arg(d.join(name))
                .status()
                .map(|s| s.success())
                .unwrap_or(false);
            if !ok {
                eprintln!("skipped: ssh-keygen did not run");
                return None;
            }
        }
        std::fs::copy(d.join("client.pub"), d.join("authorized")).unwrap();
        // A port picked by binding and releasing can be picked by another
        // test at the same moment; the sshd that loses the race exits, and
        // a connect would then reach the winner -- another test's sshd,
        // with another host key and another authorized key. So a port is
        // ours only when *our* sshd says it is listening -- by writing its
        // pid file, which it does after it has bound and never when the
        // bind failed -- and a lost race is retried on a fresh port. The
        // pid file and not the log: through sudo, the log is root's and
        // unreadable here.
        let mut child = None;
        for _ in 0..5 {
            let port = {
                let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
                l.local_addr().unwrap().port()
            };
            let _ = std::fs::remove_file(d.join("sshd.log"));
            let _ = std::fs::remove_file(d.join("pid"));
            std::fs::write(
                d.join("sshd_config"),
                format!(
                    "Port {port}\nListenAddress 127.0.0.1\nHostKey {host}\nAuthorizedKeysFile {auth}\n\
                     PasswordAuthentication no\nKbdInteractiveAuthentication no\n\
                     PermitRootLogin prohibit-password\nStrictModes no\nUsePAM yes\n\
                     PrintMotd no\nPrintLastLog no\nPidFile {pid}\n",
                    host = d.join("host").display(),
                    auth = d.join("authorized").display(),
                    pid = d.join("pid").display(),
                ),
            )
            .unwrap();
            let mut c = sshd_command()
                .args(["-D", "-E"])
                .arg(d.join("sshd.log"))
                .arg("-f")
                .arg(d.join("sshd_config"))
                .spawn()
                .unwrap();
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                let pid = std::fs::read_to_string(d.join("pid")).unwrap_or_default();
                if !pid.trim().is_empty() {
                    child = Some((c, port));
                    break;
                }
                if c.try_wait().unwrap().is_some() {
                    break;
                }
                if Instant::now() > deadline {
                    let _ = c.kill();
                    let _ = c.wait();
                    break;
                }
                std::thread::sleep(Duration::from_millis(25));
            }
            if child.is_some() {
                break;
            }
        }
        let Some((child, port)) = child else {
            eprintln!(
                "skipped: sshd did not listen (it may need privileges this run lacks): {}",
                std::fs::read_to_string(d.join("sshd.log")).unwrap_or_default()
            );
            return None;
        };
        let user = std::env::var("USER")
            .ok()
            .or_else(|| {
                let out = Command::new("id").arg("-un").output().ok()?;
                Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
            })
            .unwrap();
        Some(Sshd {
            child,
            port,
            dir,
            user,
        })
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }

    fn target(&self) -> String {
        format!("{}@127.0.0.1:{}", self.user, self.port)
    }

    /// The flags every `drt ssh` here passes: the client key and a
    /// known_hosts of the test's own.
    fn flags(&self) -> Vec<String> {
        vec![
            "-i".into(),
            self.path("client").display().to_string(),
            "--known-hosts".into(),
            self.path("known_hosts").display().to_string(),
        ]
    }

    fn host_key(&self) -> String {
        std::fs::read_to_string(self.path("host.pub")).unwrap()
    }
}

/// Whether this test runs as root.
fn root() -> bool {
    // SAFETY: getuid has no preconditions.
    unsafe { libc::getuid() == 0 }
}

/// sshd, as root: directly when this is root, else through `sudo -n`.
fn sshd_command() -> Command {
    if root() {
        Command::new("/usr/sbin/sshd")
    } else {
        let mut c = Command::new("sudo");
        c.args(["-n", "/usr/sbin/sshd"]);
        c
    }
}

impl Drop for Sshd {
    fn drop(&mut self) {
        // A failed test shows what sshd said: a refused key is otherwise
        // only a password prompt on the client's side.
        if std::thread::panicking() {
            if let Ok(log) = std::fs::read_to_string(self.path("sshd.log")) {
                eprintln!("sshd said:\n{log}");
            }
        }
        // Through sudo, the child is sudo: SIGKILL would orphan a root sshd,
        // so the pid it wrote is what is stopped.
        if !root() {
            if let Ok(pid) = std::fs::read_to_string(self.path("pid")) {
                let _ = Command::new("sudo")
                    .args(["-n", "kill", pid.trim()])
                    .status();
            }
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for Pty {
    // A test that fails part-way would otherwise leave `drt` running.
    fn drop(&mut self) {
        if let Ok(None) = self.child.try_wait() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

/// `drt` on a pseudo-terminal, everything it writes collected.
struct Pty {
    child: Child,
    master: std::fs::File,
    seen: Arc<Mutex<String>>,
    /// How much of `seen` an `expect` has consumed.
    at: usize,
}

fn winsize(cols: u16, rows: u16) -> libc::winsize {
    libc::winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    }
}

impl Pty {
    fn spawn(args: &[String], cols: u16, rows: u16) -> Pty {
        let (mut master, mut slave): (RawFd, RawFd) = (-1, -1);
        let size = winsize(cols, rows);
        // SAFETY: openpty writes two descriptors this test then owns.
        let r = unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null(),
                &size,
            )
        };
        assert_eq!(r, 0, "openpty");
        // SAFETY: fresh descriptors from openpty, owned from here.
        let (master, slave) =
            unsafe { (OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(slave)) };
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_drt"));
        cmd.args(args)
            .env("TERM", "xterm")
            .env_remove("SSH_AUTH_SOCK")
            .stdin(Stdio::from(slave.try_clone().unwrap()))
            .stdout(Stdio::from(slave.try_clone().unwrap()))
            .stderr(Stdio::from(slave));
        let child = cmd.spawn().unwrap();
        let master = std::fs::File::from(master);
        let seen = Arc::new(Mutex::new(String::new()));
        let mut reader = master.try_clone().unwrap();
        let s = seen.clone();
        std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            while let Ok(n) = reader.read(&mut buf) {
                if n == 0 {
                    break;
                }
                s.lock()
                    .unwrap()
                    .push_str(&String::from_utf8_lossy(&buf[..n]));
            }
        });
        Pty {
            child,
            master,
            seen,
            at: 0,
        }
    }

    /// Wait for `text` after what the last expect consumed.
    fn expect(&mut self, text: &str) {
        let deadline = Instant::now() + WITHIN;
        loop {
            {
                let seen = self.seen.lock().unwrap();
                if let Some(i) = seen[self.at..].find(text) {
                    self.at += i + text.len();
                    return;
                }
            }
            if Instant::now() > deadline {
                panic!(
                    "never saw {text:?}; the terminal shows:\n{}",
                    self.seen.lock().unwrap()
                );
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Wait until the terminal has shown something new and then gone
    /// quiet: a remote shell's prompt, whatever it looks like.
    fn settle(&mut self) {
        let start = self.seen.lock().unwrap().len();
        let deadline = Instant::now() + WITHIN;
        let mut last = start;
        let mut quiet_since = Instant::now();
        loop {
            let now = self.seen.lock().unwrap().len();
            if now != last {
                last = now;
                quiet_since = Instant::now();
            } else if now > start && quiet_since.elapsed() > Duration::from_millis(400) {
                self.at = now;
                return;
            }
            if Instant::now() > deadline {
                panic!(
                    "nothing new appeared; it shows:\n{}",
                    self.seen.lock().unwrap()
                );
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn send(&mut self, text: &str) {
        self.master.write_all(text.as_bytes()).unwrap();
    }

    fn resize(&self, cols: u16, rows: u16) {
        let size = winsize(cols, rows);
        // SAFETY: TIOCSWINSZ on the master this test owns.
        unsafe {
            libc::ioctl(
                std::os::fd::AsRawFd::as_raw_fd(&self.master),
                libc::TIOCSWINSZ,
                &size,
            )
        };
    }

    fn status(&mut self) -> i32 {
        let deadline = Instant::now() + WITHIN;
        loop {
            if let Some(s) = self.child.try_wait().unwrap() {
                return s.code().unwrap_or(-1);
            }
            if Instant::now() > deadline {
                let _ = self.child.kill();
                panic!("drt never exited; it shows:\n{}", self.seen.lock().unwrap());
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

#[test]
fn drt_ssh_asks_once_runs_a_shell_follows_a_resize_and_passes_back_the_status() {
    let Some(sshd) = Sshd::start() else { return };
    let mut args = vec!["ssh".to_string()];
    args.extend(sshd.flags());
    args.push(sshd.target());

    let mut t = Pty::spawn(&args, 120, 40);
    t.expect("is not in");
    t.expect("(yes/no) ");
    t.send("yes\n");
    t.expect("Added");
    t.settle();
    t.send("stty size; echo FIRST-$((6*7))\r");
    t.expect("40 120");
    t.expect("FIRST-42");
    t.resize(90, 33);
    // The size is read every quarter second; give it two.
    std::thread::sleep(Duration::from_millis(600));
    t.send("stty size; exit 3\r");
    t.expect("33 90");
    assert_eq!(t.status(), 3, "the remote shell's status is the command's");

    // Known now: no question, straight in.
    let mut t = Pty::spawn(&args, 80, 24);
    t.settle();
    t.send("echo AGAIN-$((6*7)); exit 0\r");
    t.expect("AGAIN-42");
    let shown = t.seen.lock().unwrap().clone();
    assert!(!shown.contains("(yes/no)"), "asked twice:\n{shown}");
    assert_eq!(t.status(), 0);
}

#[test]
fn drt_ssh_refuses_a_wrong_pin_before_signing_in() {
    let Some(sshd) = Sshd::start() else { return };
    let mut args = vec![
        "ssh".to_string(),
        "--hostkey".into(),
        "SHA256:not-it".into(),
    ];
    args.extend(sshd.flags());
    args.push(sshd.target());
    let mut t = Pty::spawn(&args, 80, 24);
    t.expect("refused the host key");
    t.expect("not the key this connection pins");
    assert_eq!(t.status(), 255);
}

#[test]
fn the_repl_hands_its_terminal_to_ssh_and_takes_it_back() {
    let Some(sshd) = Sshd::start() else { return };
    let mut t = Pty::spawn(&["repl".to_string()], 100, 30);
    t.expect("dv> ");
    t.send("x = 40\r");
    t.expect("dv> ");
    t.send(&format!(
        ":ssh {} {}\r",
        sshd.flags().join(" "),
        sshd.target()
    ));
    t.expect("(yes/no) ");
    t.send("yes\n");
    t.expect("Added");
    t.settle();
    t.send("echo INSIDE-$((6*7)); exit 4\r");
    t.expect("INSIDE-42");
    t.expect("closed, exit 4");
    t.expect("dv> ");
    // The instance is the one from before: its state survived the session.
    t.send("x + 2\r");
    t.expect("42");
    t.send("\x04");
    assert_eq!(t.status(), 0);
}

#[test]
fn a_bare_ssh_in_the_repl_is_the_config_s_grant_and_scope() {
    let Some(sshd) = Sshd::start() else { return };
    let host_key = sshd.host_key();
    let config = |caps: &str| {
        serde_json::json!({
            "caps": [{ "capability": caps }],
            "connectors": { "ssh": { "scope": {
                "host": format!("127.0.0.1:{}", sshd.port),
                "user": sshd.user,
                "key_path": sshd.path("client"),
                "host_key": host_key.trim(),
            }}}
        })
    };
    let granted = sshd.path("granted.json");
    std::fs::write(&granted, config("host:ssh/shell").to_string()).unwrap();
    let mut t = Pty::spawn(
        &[
            "--config".into(),
            granted.display().to_string(),
            "repl".into(),
        ],
        100,
        30,
    );
    t.expect("dv> ");
    t.send(":ssh\r");
    t.expect(":ssh");
    t.settle();
    t.send("echo SCOPED-$((6*7)); exit 5\r");
    t.expect("SCOPED-42");
    t.expect("exit = 5");
    t.expect("dv> ");
    t.send("\x04");
    assert_eq!(t.status(), 0);

    // exec granted, shell not: the grant is the call's, by name.
    let exec_only = sshd.path("exec.json");
    std::fs::write(&exec_only, config("host:ssh/exec").to_string()).unwrap();
    let mut t = Pty::spawn(
        &[
            "--config".into(),
            exec_only.display().to_string(),
            "repl".into(),
        ],
        100,
        30,
    );
    t.expect("dv> ");
    t.send(":ssh\r");
    t.expect("denied");
    t.expect("dv> ");
    t.send("\x04");
    assert_eq!(t.status(), 0);
}
