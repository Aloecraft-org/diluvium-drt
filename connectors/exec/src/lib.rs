//! The exec connector: `host:exec/run`, the honest escape hatch (SPEC.md §7).
//!
//! ```text
//!   exec/run {argv = {...}, stdin?, timeout_ms?, cwd?}
//!            -> {status = n, stdout = "...", stderr = "..."}
//! ```
//!
//! The contract is `host/dhost_exec.c`'s, kept to the sentence, so a program
//! written against `diluvium-host` runs here unchanged. Granting exec is
//! leaving the sandbox -- GUARANTEES.md says it in those words -- so the
//! surface here is the *bounding*, because the instruction budget cannot
//! reach a subprocess:
//!
//! - **No shell, by construction.** `argv` is a vector handed to exec, so
//!   there is no string a quote can escape from. A program that wants a
//!   shell asks for one by name, visibly: `{"sh", "-c", ...}`.
//! - **A wall-clock deadline.** The scope caps it, a call may ask for less,
//!   and a child still running at the deadline is killed -- SIGKILL, because
//!   the polite signal would be a negotiation with a runaway -- and the call
//!   answers `error`. The kill sweeps the child's whole process *group*, and
//!   so does every exit from the call: `exec/run` is a bounded request and
//!   reply, and nothing it starts outlives it. A program that wants a daemon
//!   wants a supervisor, not an escape hatch.
//! - **An output cap on each stream**, and on stdin. Past it the child is
//!   killed and the call answers `error`: refused rather than truncated, like
//!   every other cap in this tree.
//!
//! A nonzero exit is **not** an error: `{status = 1}` is the child's answer,
//! read the way a shell script reads `$?`, and a program that does not exist
//! is `{status = 127}`, the shell's own convention. `error` is reserved for
//! the call itself failing: the deadline, a cap, a malformed request.
//!
//! **One DRT addition, and it is the scope doing its job:** `allow`, the
//! programs a call may start. The C host's config has nowhere to put one, so
//! there `host:exec/run` was every program on the box. Here a scope may name
//! them, and a call naming anything else is refused by name. Absent, the
//! behaviour is the C host's: any program `PATH` finds.
//!
//! **Honesty note, in the config's face rather than buried:** this connector
//! answers synchronously, so a running child stalls every guest in the
//! deployment until it exits or hits its deadline. Bound it tight. The
//! deferred pump (`doc/Wasm.md` M3) is what lifts that, for every connector
//! at once.
//!
//! What the child inherits: the environment, and nothing above the three
//! standard descriptors. Rust opens everything close-on-exec -- files,
//! sockets, the listener's and SQLite's alike -- so the C host's
//! close-everything sweep has no work to do here; `SIGPIPE` is reset to its
//! default in the child by `std`, as `dhost_exec.c` resets it by hand.
//!
//! Replay: the reply is a message like any other, logged and replayed. A
//! replay does **not** re-run the subprocess.

#[cfg(not(unix))]
compile_error!(
    "the exec connector is unix-only: it needs process groups and pipes. \
     Build `drt` without `connector-exec` on this target"
);

use std::ffi::{OsStr, OsString};
use std::io::{ErrorKind, Read, Write};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use serde::Deserialize;

use drt_caps::{Scope, ScopeType};
use drt_connector::{CallError, CallResult, Connector};

// ---------------------------------------------------------------------------
// Surface. [`ExecConnector`] answers exactly one call, `exec/run`, under the
// scope [`ExecScopeType`] describes; there is no dispatch beyond that call.
// The values below are the bounds a deployment tunes.
// ---------------------------------------------------------------------------

/// The ceiling on a call's wall-clock deadline when the scope states none:
/// `dhost.c`'s default. A call may ask for less, never more.
const DEFAULT_MAX_TIMEOUT_MS: u64 = 10_000;
/// The cap on each output stream, and on stdin, when the scope states none:
/// 1 MiB, `dhost.c`'s default.
const DEFAULT_MAX_OUTPUT_BYTES: u64 = 1024 * 1024;
/// `EXEC_MAX_ARGS` in `dhost_exec.c`.
const MAX_ARGS: usize = 64;
/// The shell's convention for a program that could not be started.
const EXIT_NOT_FOUND: i64 = 127;
/// How often the wait loop looks for the child's exit; `dhost_exec.c`'s
/// `nanosleep` between `waitpid` calls.
const REAP_POLL: Duration = Duration::from_millis(2);
/// How long, after the child has exited and its group has been swept, the
/// readers get to reach end-of-file before the call answers with what they
/// hold. A descendant that escaped the group (`setsid`) can keep the pipes
/// open forever; the C host answers with what is buffered at exit, and this
/// is that, with one scheduler's worth of slack.
const EXIT_DRAIN_GRACE: Duration = Duration::from_millis(50);
/// Where a program is looked up when the environment names no `PATH`, the
/// way `execvp` falls back.
const DEFAULT_PATH: &str = "/usr/bin:/bin";

/// The connector. Stateless: every bound comes from the scope on each call.
#[derive(Default)]
pub struct ExecConnector;

impl ExecConnector {
    pub fn new() -> Self {
        ExecConnector
    }
}

// depth: the scope, and its startup validation

/// What the deployment bounds: the deadline ceiling, the byte cap, and --
/// DRT's own -- the programs a call may start. Field names are `dhost.c`'s,
/// so `connectors.exec = { max_timeout_ms = ..., max_output_bytes = ... }`
/// in a `.host.lua` loads unchanged, and `exec = true` is the same as no
/// scope at all: every bound at its default.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct ExecScope {
    #[serde(default)]
    max_timeout_ms: Option<u64>,
    #[serde(default)]
    max_output_bytes: Option<u64>,
    /// Absolute paths. Compared after symlinks are resolved on both sides,
    /// so `/bin/sh` still matches on a box where `/bin` is `/usr/bin` --
    /// and, the same coin, a busybox that links every name to one binary
    /// allows every name that binary answers to.
    #[serde(default)]
    allow: Option<Vec<PathBuf>>,
}

impl ExecScope {
    fn parse(scope: Option<&Scope>) -> Result<Self, String> {
        let Some(Scope(value)) = scope else {
            return Ok(ExecScope::default());
        };
        if !value.is_map() {
            return Err("scope must be a table of bounds".into());
        }
        let parsed: ExecScope = rmpv::ext::from_value(value.clone())
            .map_err(|e| format!("scope does not parse: {e}"))?;
        if parsed.max_timeout_ms == Some(0) {
            return Err("max_timeout_ms of 0 would refuse every call".into());
        }
        if parsed.max_output_bytes == Some(0) {
            return Err("max_output_bytes of 0 would refuse every call".into());
        }
        // Checked at startup, by name: a program the allow list names must
        // be there, so a typo is a refusal at boot and not a 127 at 3am.
        for entry in parsed.allow.iter().flatten() {
            if !entry.is_absolute() {
                return Err(format!(
                    "allow entries are absolute paths; '{}' is not",
                    entry.display()
                ));
            }
            let resolved = std::fs::canonicalize(entry).map_err(|e| {
                format!(
                    "allow names '{}', which cannot be resolved: {e}",
                    entry.display()
                )
            })?;
            if !is_executable_file(&resolved) {
                return Err(format!(
                    "allow names '{}', which is not an executable file",
                    entry.display()
                ));
            }
        }
        Ok(parsed)
    }

    fn max_timeout_ms(&self) -> u64 {
        self.max_timeout_ms.unwrap_or(DEFAULT_MAX_TIMEOUT_MS)
    }

    fn max_output_bytes(&self) -> u64 {
        self.max_output_bytes.unwrap_or(DEFAULT_MAX_OUTPUT_BYTES)
    }
}

struct ExecScopeType;

impl ScopeType for ExecScopeType {
    fn describe(&self) -> &str {
        "{max_timeout_ms?, max_output_bytes?, allow?}, or no scope for the defaults"
    }

    fn validate(&self, scope: Option<&Scope>) -> Result<(), String> {
        ExecScope::parse(scope).map(|_| ())
    }
}

// depth: the request, read against the scope's bounds

struct RunArgs {
    /// The vector, bytes each: a path is not text, and exec takes bytes.
    argv: Vec<Vec<u8>>,
    stdin: Vec<u8>,
    /// Resolved: the call's ask, or the scope's ceiling.
    timeout_ms: u64,
    cwd: Option<PathBuf>,
}

const VECTOR_NOT_SHELL: &str =
    "args.argv must be a non-empty array of strings (a vector, not a shell string)";

fn parse_args(args: Option<rmpv::Value>, scope: &ExecScope) -> Result<RunArgs, String> {
    let Some(args) = args else {
        return Err("args.argv must be the command vector".into());
    };
    let Some(rmpv::Value::Array(items)) = field(&args, "argv") else {
        return Err(VECTOR_NOT_SHELL.into());
    };
    if items.is_empty() {
        return Err(VECTOR_NOT_SHELL.into());
    }
    if items.len() > MAX_ARGS {
        return Err(format!("argv is longer than the host's bound ({MAX_ARGS})"));
    }
    let mut argv = Vec::with_capacity(items.len());
    for (i, item) in items.iter().enumerate() {
        match string_bytes(item) {
            Some(bytes) if !bytes.contains(&0) => argv.push(bytes.to_vec()),
            _ => return Err(format!("argv[{}] is not a plain string", i + 1)),
        }
    }

    let cap = scope.max_output_bytes();
    let stdin = match field(&args, "stdin") {
        None | Some(rmpv::Value::Nil) => Vec::new(),
        Some(value) => string_bytes(value)
            .ok_or_else(|| "stdin must be a string".to_string())?
            .to_vec(),
    };
    if stdin.len() as u64 > cap {
        return Err(format!(
            "stdin is bigger than this deployment's byte cap ({cap})"
        ));
    }

    let ceiling = scope.max_timeout_ms();
    let timeout_ms = match field(&args, "timeout_ms") {
        None | Some(rmpv::Value::Nil) => ceiling,
        Some(value) => {
            let Some(asked) = value.as_i64().filter(|n| *n > 0) else {
                return Err("timeout_ms must be a positive integer".into());
            };
            if asked as u64 > ceiling {
                return Err(format!(
                    "timeout_ms passed the host's ceiling ({ceiling} ms); a call may ask \
                     for less, never more"
                ));
            }
            asked as u64
        }
    };

    let cwd = match field(&args, "cwd") {
        None | Some(rmpv::Value::Nil) => None,
        Some(value) => match string_bytes(value) {
            // An empty cwd is no cwd, as `dhost_exec.c` reads it.
            Some([]) => None,
            Some(bytes) if !bytes.contains(&0) => {
                Some(PathBuf::from(OsString::from_vec(bytes.to_vec())))
            }
            _ => return Err("cwd must be a plain string".into()),
        },
    };

    Ok(RunArgs {
        argv,
        stdin,
        timeout_ms,
        cwd,
    })
}

fn field<'a>(map: &'a rmpv::Value, name: &str) -> Option<&'a rmpv::Value> {
    map.as_map()?
        .iter()
        .find(|(k, _)| k.as_str() == Some(name))
        .map(|(_, v)| v)
}

/// The bytes of a msgpack `str` or `bin`. The guest's codec reads both into
/// one token, so both are accepted here; `dhost_exec.c` sees them as one.
fn string_bytes(value: &rmpv::Value) -> Option<&[u8]> {
    match value {
        rmpv::Value::String(s) => Some(s.as_bytes()),
        rmpv::Value::Binary(b) => Some(b),
        _ => None,
    }
}

// depth: finding the program, and the allow list

enum Resolved {
    Program(PathBuf),
    /// What `execvp` would have failed on: answered as `{status = 127}`.
    NotFound,
}

fn resolve_program(args: &RunArgs, scope: &ExecScope) -> Result<Resolved, String> {
    let argv0 = Path::new(OsStr::from_bytes(&args.argv[0]));
    let program = if args.argv[0].contains(&b'/') {
        // A path. `dhost_exec.c` chdirs and then execs, so a relative one
        // is relative to `cwd`; joining here is that, spelled out.
        match (&args.cwd, argv0.is_relative()) {
            (Some(cwd), true) => cwd.join(argv0),
            _ => argv0.to_path_buf(),
        }
    } else if scope.allow.is_some() {
        // A name. The allow list needs the file `PATH` would have found in
        // order to compare it, so the lookup happens here rather than in
        // the child.
        match find_on_path(argv0) {
            Some(found) => found,
            None => return Ok(Resolved::NotFound),
        }
    } else {
        // A name and no allow list: the C host's behaviour exactly, the
        // `PATH` search left to exec itself.
        return Ok(Resolved::Program(argv0.to_path_buf()));
    };
    if let Some(allow) = &scope.allow {
        let Ok(canonical) = std::fs::canonicalize(&program) else {
            return Ok(Resolved::NotFound);
        };
        let permitted = allow
            .iter()
            .any(|entry| std::fs::canonicalize(entry).is_ok_and(|e| e == canonical));
        if !permitted {
            return Err(format!(
                "'{}' is outside this scope's allow list; a program may start only what \
                 the deployment allows",
                String::from_utf8_lossy(&args.argv[0])
            ));
        }
    }
    Ok(Resolved::Program(program))
}

fn find_on_path(name: &Path) -> Option<PathBuf> {
    let path = std::env::var_os("PATH").unwrap_or_else(|| OsString::from(DEFAULT_PATH));
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|candidate| is_executable_file(candidate))
}

fn is_executable_file(path: &Path) -> bool {
    std::fs::metadata(path)
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

/// The errors `execvp` (or the `chdir` before it) fails with, which
/// `dhost_exec.c` answers as `_exit(127)`. Anything else -- no descriptors,
/// no memory, no process -- is the host failing, and is an `error`.
fn could_not_exec(e: &std::io::Error) -> bool {
    matches!(
        e.raw_os_error(),
        Some(
            libc::ENOENT
                | libc::ENOTDIR
                | libc::EACCES
                | libc::EPERM
                | libc::ENOEXEC
                | libc::ELOOP
                | libc::ENAMETOOLONG
                | libc::E2BIG
                | libc::EISDIR
                | libc::ETXTBSY
        )
    )
}

// depth: the run, under one deadline

/// One stream, read on its own thread into a capped buffer, so neither a
/// full pipe nor a silent one can hold the deadline hostage.
struct Capped {
    buf: Mutex<Vec<u8>>,
    cap: usize,
    tripped: AtomicBool,
    done: AtomicBool,
}

impl Capped {
    fn pump(mut source: impl Read + Send + 'static, cap: usize) -> Arc<Capped> {
        let sink = Arc::new(Capped {
            buf: Mutex::new(Vec::new()),
            cap,
            tripped: AtomicBool::new(false),
            done: AtomicBool::new(false),
        });
        let inner = Arc::clone(&sink);
        thread::spawn(move || {
            let mut chunk = [0u8; 4096];
            loop {
                match source.read(&mut chunk) {
                    Ok(0) => break,
                    Ok(n) => {
                        let mut buf = inner.buf.lock().unwrap_or_else(|p| p.into_inner());
                        if buf.len() + n > inner.cap {
                            inner.tripped.store(true, Ordering::SeqCst);
                            break;
                        }
                        buf.extend_from_slice(&chunk[..n]);
                    }
                    Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                    Err(_) => break,
                }
            }
            inner.done.store(true, Ordering::SeqCst);
        });
        sink
    }

    fn tripped(&self) -> bool {
        self.tripped.load(Ordering::SeqCst)
    }

    fn done(&self) -> bool {
        self.done.load(Ordering::SeqCst)
    }

    fn take(&self) -> Vec<u8> {
        std::mem::take(&mut *self.buf.lock().unwrap_or_else(|p| p.into_inner()))
    }
}

/// Which stream passed its cap, stdout first, as `dhost_exec.c` checks them.
fn tripped(out: &Capped, err: &Capped) -> Option<&'static str> {
    if out.tripped() {
        Some("stdout")
    } else if err.tripped() {
        Some("stderr")
    } else {
        None
    }
}

/// SIGKILL to the child's whole process group. `ESRCH` means it is already
/// empty, which is the state every exit path leaves it in.
fn sweep(pid: u32) {
    // SAFETY: a plain syscall on a group this call created; no memory
    // crosses.
    unsafe {
        libc::kill(-(pid as libc::pid_t), libc::SIGKILL);
    }
}

fn reap(child: &mut Child) {
    let _ = child.wait();
}

fn run(scope: &ExecScope, args: RunArgs) -> Result<rmpv::Value, String> {
    let cap = scope.max_output_bytes() as usize;
    let program = match resolve_program(&args, scope)? {
        Resolved::Program(program) => program,
        Resolved::NotFound => return Ok(reply(EXIT_NOT_FOUND, Vec::new(), Vec::new())),
    };

    let mut command = Command::new(&program);
    command
        // The program sees the name the guest gave it, as under `execvp`.
        .arg0(OsStr::from_bytes(&args.argv[0]))
        .args(args.argv[1..].iter().map(|a| OsStr::from_bytes(a)))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // Its own process group, so the deadline can kill the whole tree
        // and not just the direct child. Set before exec, so a kill aimed
        // at the group after `spawn` returns reaches everything it starts.
        .process_group(0);
    if let Some(cwd) = &args.cwd {
        command.current_dir(cwd);
    }
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(e) if could_not_exec(&e) => return Ok(reply(EXIT_NOT_FOUND, Vec::new(), Vec::new())),
        Err(e) => return Err(format!("the child would not start: {e}")),
    };
    let pid = child.id();
    let deadline = Instant::now() + Duration::from_millis(args.timeout_ms);

    // stdin on its own thread: a child that never reads must not hold the
    // deadline, and a write failing is the child closing its side, which
    // ends the feeding the way `dhost_exec.c`'s `stdin_off = stdin_len` does.
    let stdin_pipe = child.stdin.take();
    if args.stdin.is_empty() {
        drop(stdin_pipe);
    } else {
        let bytes = args.stdin;
        thread::spawn(move || {
            if let Some(mut pipe) = stdin_pipe {
                let _ = pipe.write_all(&bytes);
            }
        });
    }
    let out = Capped::pump(child.stdout.take().expect("stdout was piped"), cap);
    let err = Capped::pump(child.stderr.take().expect("stderr was piped"), cap);

    let exit = loop {
        if let Some(stream) = tripped(&out, &err) {
            sweep(pid);
            reap(&mut child);
            return Err(format!(
                "{stream} passed this deployment's byte cap ({cap}); the child was killed, \
                 the output refused"
            ));
        }
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {}
            Err(e) => {
                sweep(pid);
                reap(&mut child);
                return Err(format!("waiting on the child failed: {e}"));
            }
        }
        if Instant::now() >= deadline {
            sweep(pid);
            reap(&mut child);
            return Err(format!(
                "the child was killed at the {} ms deadline",
                args.timeout_ms
            ));
        }
        thread::sleep(REAP_POLL);
    };

    // The child is gone. Its group may still hold descendants writing to
    // its pipes -- `sh -c "server &"` -- and nothing exec/run starts
    // outlives it, so the group is swept on this path as on every other.
    sweep(pid);
    // Then what the pipes hold. End-of-file arrives at once when nothing
    // else has them open; a descendant that escaped the group keeps them,
    // and then the answer is what was buffered at exit, as the C host's is.
    let grace = Instant::now() + EXIT_DRAIN_GRACE;
    while !(out.done() && err.done()) && Instant::now() < grace {
        thread::sleep(REAP_POLL);
    }
    if let Some(stream) = tripped(&out, &err) {
        return Err(format!(
            "{stream} passed this deployment's byte cap ({cap}); the output is refused"
        ));
    }
    let status = exit
        .code()
        .map(i64::from)
        .or_else(|| exit.signal().map(|sig| 128 + i64::from(sig)))
        .unwrap_or(-1);
    Ok(reply(status, out.take(), err.take()))
}

/// `{status, stdout, stderr}`. The streams travel as msgpack `str` when
/// they are text and `bin` when they are not; the guest reads both as one
/// string, and `dhost_exec.c`'s `str` is byte-identical for text.
fn reply(status: i64, stdout: Vec<u8>, stderr: Vec<u8>) -> rmpv::Value {
    rmpv::Value::Map(vec![
        ("status".into(), rmpv::Value::from(status)),
        ("stdout".into(), text_or_bytes(stdout)),
        ("stderr".into(), text_or_bytes(stderr)),
    ])
}

fn text_or_bytes(bytes: Vec<u8>) -> rmpv::Value {
    match String::from_utf8(bytes) {
        Ok(text) => rmpv::Value::from(text),
        Err(e) => rmpv::Value::Binary(e.into_bytes()),
    }
}

#[async_trait::async_trait]
impl Connector for ExecConnector {
    fn scope_type(&self) -> Box<dyn ScopeType> {
        Box::new(ExecScopeType)
    }

    async fn call(
        &self,
        call: &str,
        args: Option<rmpv::Value>,
        scope: Option<&Scope>,
    ) -> CallResult {
        // `dhost_exec.c` answers a call outside its surface with `denied`;
        // here that word is the dispatcher's alone, so this is an `error`,
        // as every other DRT connector answers it.
        if call != "exec/run" {
            return Err(CallError::new(format!(
                "the exec connector answers 'exec/run'; '{call}' is not it"
            )));
        }
        let scope = ExecScope::parse(scope).map_err(CallError::new)?;
        let args = parse_args(args, &scope).map_err(CallError::new)?;
        run(&scope, args).map_err(CallError::new)
    }
}
