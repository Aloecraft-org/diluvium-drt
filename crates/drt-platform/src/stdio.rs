//! Where bytes for a terminal go.
//!
//! Natively and under wasmtime that is fd 1 and fd 2. In a page there are
//! no fds: the terminal is whatever the page installed with
//! [`install_sink`], and until it installs one, output is dropped rather
//! than sent somewhere invented. The REPL's answers and a deployment's
//! own notices go through here so the same code prints in a shell and in
//! an xterm.js; the C core's `print` does not -- it reaches the page
//! through wasi-libc's `fd_write` (doc/Wasm.md D4), and the two meet at
//! the page's sink.
//!
//! Thread-local rather than global because a page's sink holds JS values,
//! which are pinned to the one thread there is; natively the sink is only
//! ever installed by a test capturing output.

use std::cell::RefCell;
use std::io::{self, Write};

/// The two streams a terminal has.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fd {
    Stdout,
    Stderr,
}

type Sink = Box<dyn Fn(Fd, &[u8])>;

thread_local! {
    static SINK: RefCell<Option<Sink>> = const { RefCell::new(None) };
}

/// Route every write on this thread to `sink` instead of the process's
/// streams. Returns the sink that was installed before, if any.
pub fn install_sink(sink: Sink) -> Option<Sink> {
    SINK.with(|s| s.borrow_mut().replace(sink))
}

/// Remove an installed sink; writes go back to the process's streams.
pub fn uninstall_sink() -> Option<Sink> {
    SINK.with(|s| s.borrow_mut().take())
}

/// Write `bytes` to `fd`, through the installed sink or the platform's own
/// stream. Never fails: a terminal that cannot be written to is a
/// terminal that is gone, and nothing upstream has anything to do about
/// that but stop.
pub fn write(fd: Fd, bytes: &[u8]) {
    let handled = SINK.with(|s| match &*s.borrow() {
        Some(sink) => {
            sink(fd, bytes);
            true
        }
        None => false,
    });
    if handled {
        return;
    }
    #[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
    {
        // No fds here, and no sink installed: dropped, deliberately. A page
        // that wants output installs a sink; inventing `console.log` would
        // put a runtime's output somewhere a user is not looking.
        let _ = (fd, bytes);
    }
    #[cfg(not(all(target_arch = "wasm32", target_os = "unknown")))]
    {
        let result = match fd {
            Fd::Stdout => {
                let mut out = io::stdout().lock();
                out.write_all(bytes).and_then(|()| out.flush())
            }
            Fd::Stderr => {
                let mut err = io::stderr().lock();
                err.write_all(bytes).and_then(|()| err.flush())
            }
        };
        let _ = result;
    }
}

/// `std::io::Write` over [`write`], for `write!` and `writeln!`.
pub struct Stream(pub Fd);

impl Write for Stream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        write(self.0, buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

pub fn stdout() -> Stream {
    Stream(Fd::Stdout)
}

pub fn stderr() -> Stream {
    Stream(Fd::Stderr)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::rc::Rc;

    type Seen = Rc<RefCell<Vec<(Fd, Vec<u8>)>>>;

    #[test]
    fn an_installed_sink_sees_every_byte_and_which_stream() {
        let seen: Seen = Rc::new(RefCell::new(Vec::new()));
        let log = seen.clone();
        install_sink(Box::new(move |fd, b| {
            log.borrow_mut().push((fd, b.to_vec()))
        }));
        writeln!(stdout(), "answer").unwrap();
        write(Fd::Stderr, b"note");
        uninstall_sink();
        let got = seen.borrow();
        assert_eq!(got[0], (Fd::Stdout, b"answer\n".to_vec()));
        assert_eq!(got[1], (Fd::Stderr, b"note".to_vec()));
    }
}
