//! This process's terminal, for an interactive session: raw mode, the
//! window size, keystrokes as bytes, and a line read without echo. One
//! surface, a Unix half and a Windows half.
//!
//! **Unix** is termios on stdin and `TIOCGWINSZ`, and the reader is
//! `poll(2)` with a timeout.
//!
//! **Windows** is the console API, set up the way Windows' own OpenSSH
//! client sets it up. On input: line editing, echo and Ctrl+C processing
//! off, `ENABLE_VIRTUAL_TERMINAL_INPUT` on, so the console hands over keys
//! as the VT sequences a remote pty expects (`ESC [ A` for an arrow), and
//! Ctrl+C as the byte `0x03` for the remote shell. On output:
//! `ENABLE_VIRTUAL_TERMINAL_PROCESSING`, so the remote side's escapes are
//! drawn rather than printed, and `DISABLE_NEWLINE_AUTO_RETURN`, so a bare
//! `\n` moves down without returning, as it does in a raw Unix tty. The
//! reader waits on the input handle with a timeout, and reads only when a
//! character is waiting: the handle is also signalled for focus, mouse and
//! resize events, and `ReadConsoleW` would block on those until the next
//! key. The console reads UTF-16, which [`Utf16`] turns into UTF-8 bytes,
//! carrying a surrogate split across two reads.
//!
//! Both readers end within a [`POLL_MS`] of being told to stop. A reader
//! left blocked would take the first key typed at the REPL prompt after
//! the session.
//!
//! ## surface block
//!
//! - Entry points: [`Raw::enter`] (raw mode, restored on drop); [`size`];
//!   [`read_stdin`]; [`without_echo`].
//! - Configurable: [`POLL_MS`].
//! - Fan-out: `cfg(unix)` and `cfg(windows)` for each of the four; on
//!   Windows, the input records `read_stdin` sorts into "a character is
//!   waiting" and "nothing to read".

use std::sync::atomic::{AtomicBool, Ordering};

use tokio::sync::mpsc;

/// How often a reader looks up to see whether it has been told to stop.
pub const POLL_MS: u32 = 100;

/// Raw mode for as long as this lives.
pub struct Raw {
    #[cfg(unix)]
    saved: libc::termios,
    #[cfg(windows)]
    saved: (u32, u32),
}

// depth: unix

#[cfg(unix)]
impl Raw {
    pub fn enter() -> Result<Raw, String> {
        // SAFETY: tcgetattr/cfmakeraw/tcsetattr on fd 0 with termios
        // values this function owns.
        unsafe {
            let mut saved: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(0, &mut saved) != 0 {
                return Err(format!("raw mode: {}", std::io::Error::last_os_error()));
            }
            let mut raw = saved;
            libc::cfmakeraw(&mut raw);
            if libc::tcsetattr(0, libc::TCSANOW, &raw) != 0 {
                return Err(format!("raw mode: {}", std::io::Error::last_os_error()));
            }
            Ok(Raw { saved })
        }
    }
}

#[cfg(unix)]
impl Drop for Raw {
    fn drop(&mut self) {
        // SAFETY: restoring the settings `enter` read from the same fd.
        unsafe { libc::tcsetattr(0, libc::TCSANOW, &self.saved) };
    }
}

/// Columns and rows, or none when the terminal declines to say (a pty
/// opened without a size answers zero).
#[cfg(unix)]
pub fn size() -> Option<(u32, u32)> {
    for fd in [1, 0, 2] {
        // SAFETY: TIOCGWINSZ into a winsize this function owns.
        let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
        if unsafe { libc::ioctl(fd, libc::TIOCGWINSZ, &mut ws) } == 0
            && ws.ws_col > 0
            && ws.ws_row > 0
        {
            return Some((u32::from(ws.ws_col), u32::from(ws.ws_row)));
        }
    }
    None
}

/// stdin to `tx` until `stop`.
#[cfg(unix)]
pub fn read_stdin(tx: mpsc::UnboundedSender<Vec<u8>>, stop: &AtomicBool) {
    let mut buf = [0u8; 4096];
    while !stop.load(Ordering::Relaxed) {
        let mut fd = libc::pollfd {
            fd: 0,
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: one pollfd on the stack, for its length.
        let ready = unsafe { libc::poll(&mut fd, 1, POLL_MS as i32) };
        if ready < 0 {
            if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            break;
        }
        if ready == 0 {
            continue;
        }
        // SAFETY: reading into a buffer this function owns, at most its length.
        let n = unsafe { libc::read(0, buf.as_mut_ptr().cast(), buf.len()) };
        if n <= 0 || tx.send(buf[..n as usize].to_vec()).is_err() {
            break;
        }
    }
}

/// `f` with echo off on stdin, newline still echoed.
#[cfg(unix)]
pub fn without_echo<T>(f: impl FnOnce() -> T) -> T {
    // SAFETY: tcgetattr/tcsetattr on fd 0 with a termios this function
    // owns; the saved settings are restored on every path out.
    unsafe {
        let mut saved: libc::termios = std::mem::zeroed();
        if libc::tcgetattr(0, &mut saved) != 0 {
            return f();
        }
        let mut quiet = saved;
        quiet.c_lflag &= !libc::ECHO;
        quiet.c_lflag |= libc::ECHONL;
        libc::tcsetattr(0, libc::TCSANOW, &quiet);
        let out = f();
        libc::tcsetattr(0, libc::TCSANOW, &saved);
        out
    }
}

// depth: windows

#[cfg(windows)]
mod win {
    pub use windows_sys::Win32::Foundation::{HANDLE, WAIT_OBJECT_0, WAIT_TIMEOUT};
    pub use windows_sys::Win32::System::Console::{
        GetConsoleMode, GetConsoleScreenBufferInfo, GetNumberOfConsoleInputEvents, GetStdHandle,
        PeekConsoleInputW, ReadConsoleInputW, ReadConsoleW, SetConsoleMode,
        CONSOLE_SCREEN_BUFFER_INFO, DISABLE_NEWLINE_AUTO_RETURN, ENABLE_ECHO_INPUT,
        ENABLE_LINE_INPUT, ENABLE_PROCESSED_INPUT, ENABLE_PROCESSED_OUTPUT,
        ENABLE_VIRTUAL_TERMINAL_INPUT, ENABLE_VIRTUAL_TERMINAL_PROCESSING, INPUT_RECORD, KEY_EVENT,
        STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
    };
    pub use windows_sys::Win32::System::Threading::WaitForSingleObject;

    pub fn input() -> HANDLE {
        // SAFETY: GetStdHandle has no preconditions.
        unsafe { GetStdHandle(STD_INPUT_HANDLE) }
    }

    pub fn output() -> HANDLE {
        // SAFETY: as above.
        unsafe { GetStdHandle(STD_OUTPUT_HANDLE) }
    }

    pub fn mode(h: HANDLE) -> Option<u32> {
        let mut m = 0;
        // SAFETY: a handle from GetStdHandle and a u32 this function owns.
        (unsafe { GetConsoleMode(h, &mut m) } != 0).then_some(m)
    }

    pub fn set_mode(h: HANDLE, m: u32) -> bool {
        // SAFETY: as above.
        unsafe { SetConsoleMode(h, m) != 0 }
    }
}

#[cfg(windows)]
impl Raw {
    pub fn enter() -> Result<Raw, String> {
        use win::*;
        let (input, output) = (win::input(), win::output());
        let in_mode = mode(input).ok_or("raw mode: stdin is not a console")?;
        let out_mode = mode(output).ok_or("raw mode: stdout is not a console")?;
        let raw_in = (in_mode & !(ENABLE_ECHO_INPUT | ENABLE_LINE_INPUT | ENABLE_PROCESSED_INPUT))
            | ENABLE_VIRTUAL_TERMINAL_INPUT;
        if !set_mode(input, raw_in) {
            return Err(format!(
                "raw mode: this console takes no VT input ({})",
                std::io::Error::last_os_error()
            ));
        }
        let raw_out = out_mode
            | ENABLE_PROCESSED_OUTPUT
            | ENABLE_VIRTUAL_TERMINAL_PROCESSING
            | DISABLE_NEWLINE_AUTO_RETURN;
        if !set_mode(output, raw_out) {
            set_mode(input, in_mode);
            return Err(format!(
                "raw mode: this console draws no VT output; Windows 10 1809 or later is needed ({})",
                std::io::Error::last_os_error()
            ));
        }
        Ok(Raw {
            saved: (in_mode, out_mode),
        })
    }
}

#[cfg(windows)]
impl Drop for Raw {
    fn drop(&mut self) {
        win::set_mode(win::input(), self.saved.0);
        win::set_mode(win::output(), self.saved.1);
    }
}

/// Columns and rows of the console's visible window.
#[cfg(windows)]
pub fn size() -> Option<(u32, u32)> {
    // SAFETY: a zeroed out-struct for GetConsoleScreenBufferInfo to fill.
    let mut info: win::CONSOLE_SCREEN_BUFFER_INFO = unsafe { std::mem::zeroed() };
    if unsafe { win::GetConsoleScreenBufferInfo(win::output(), &mut info) } == 0 {
        return None;
    }
    let w = &info.srWindow;
    let cols = i32::from(w.Right) - i32::from(w.Left) + 1;
    let rows = i32::from(w.Bottom) - i32::from(w.Top) + 1;
    (cols > 0 && rows > 0).then_some((cols as u32, rows as u32))
}

/// The console's keystrokes to `tx` until `stop`.
#[cfg(windows)]
pub fn read_stdin(tx: mpsc::UnboundedSender<Vec<u8>>, stop: &AtomicBool) {
    use win::*;
    let h = win::input();
    let mut utf16 = Utf16::default();
    let mut records: Vec<INPUT_RECORD> = Vec::new();
    let mut units = [0u16; 1024];
    while !stop.load(Ordering::Relaxed) {
        // SAFETY: a console input handle and a timeout.
        match unsafe { WaitForSingleObject(h, POLL_MS) } {
            WAIT_OBJECT_0 => {}
            WAIT_TIMEOUT => continue,
            _ => break,
        }
        let mut pending = 0u32;
        // SAFETY: an out-count this function owns.
        if unsafe { GetNumberOfConsoleInputEvents(h, &mut pending) } == 0 {
            break;
        }
        if pending == 0 {
            continue;
        }
        // SAFETY: INPUT_RECORD is plain data; zeroed is a valid value.
        records.resize(pending as usize, unsafe { std::mem::zeroed() });
        let mut seen = 0u32;
        // SAFETY: a buffer of `pending` records and an out-count.
        if unsafe { PeekConsoleInputW(h, records.as_mut_ptr(), pending, &mut seen) } == 0 {
            break;
        }
        if !records[..seen as usize].iter().any(is_char) {
            // Focus, mouse, menu or resize: nothing to read, and left in
            // the queue they would keep the handle signalled. The size is
            // polled by the session, so a resize needs nothing here.
            // SAFETY: as for the peek.
            unsafe { ReadConsoleInputW(h, records.as_mut_ptr(), seen, &mut seen) };
            continue;
        }
        let mut read = 0u32;
        // SAFETY: a buffer of `units.len()` u16s and an out-count; a
        // character is waiting, so this returns without blocking.
        let ok = unsafe {
            ReadConsoleW(
                h,
                units.as_mut_ptr().cast(),
                units.len() as u32,
                &mut read,
                std::ptr::null(),
            )
        };
        if ok == 0 {
            break;
        }
        let bytes = utf16.push(&units[..read as usize]);
        if !bytes.is_empty() && tx.send(bytes).is_err() {
            break;
        }
    }
}

/// A key going down that carries a character. With VT input on, an arrow
/// or a function key is several of these, one per byte of its sequence.
#[cfg(windows)]
fn is_char(r: &win::INPUT_RECORD) -> bool {
    if u32::from(r.EventType) != win::KEY_EVENT {
        return false;
    }
    // SAFETY: EventType says the union holds a KEY_EVENT_RECORD.
    let key = unsafe { r.Event.KeyEvent };
    key.bKeyDown != 0 && unsafe { key.uChar.UnicodeChar } != 0
}

/// `f` with echo off on the console, line input left as it was.
#[cfg(windows)]
pub fn without_echo<T>(f: impl FnOnce() -> T) -> T {
    let h = win::input();
    let Some(saved) = win::mode(h) else {
        return f();
    };
    win::set_mode(h, saved & !win::ENABLE_ECHO_INPUT);
    let out = f();
    win::set_mode(h, saved);
    out
}

// depth: UTF-16 to UTF-8, a read at a time

/// The console's UTF-16, as UTF-8 bytes. A character outside the basic
/// plane is two units, and a read can end between them, so a high
/// surrogate waits for the next read. A lone surrogate becomes U+FFFD.
#[derive(Default)]
pub struct Utf16 {
    high: Option<u16>,
}

impl Utf16 {
    pub fn push(&mut self, units: &[u16]) -> Vec<u8> {
        let mut all: Vec<u16> = self.high.take().into_iter().collect();
        all.extend_from_slice(units);
        if let Some(&last) = all.last() {
            if (0xD800..0xDC00).contains(&last) {
                self.high = all.pop();
            }
        }
        String::from_utf16_lossy(&all).into_bytes()
    }
}

#[cfg(test)]
mod tests {
    use super::Utf16;

    #[test]
    fn utf16_becomes_utf8_and_a_split_pair_waits_for_its_half() {
        let mut d = Utf16::default();
        assert_eq!(d.push(&"ls\r".encode_utf16().collect::<Vec<_>>()), b"ls\r");
        // ESC [ A: an up arrow, as VT input sends it.
        assert_eq!(d.push(&[0x1b, b'[' as u16, b'A' as u16]), b"\x1b[A");
        let crab: Vec<u16> = "é🦀".encode_utf16().collect();
        assert_eq!(crab.len(), 3);
        assert_eq!(d.push(&crab[..2]), "é".as_bytes());
        assert_eq!(d.push(&crab[2..]), "🦀".as_bytes());
        // A low surrogate on its own is not a character.
        assert_eq!(d.push(&[0xDC00]), "\u{FFFD}".as_bytes());
    }
}
