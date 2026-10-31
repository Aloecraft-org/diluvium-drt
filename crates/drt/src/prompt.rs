//! Reading a `{"prompt": ..}` secret (`drt_config::secret`) from the
//! controlling terminal, or refusing at once when there is none.
//!
//! The terminal, not stdin: `drt p2p <peer>` with no `-p` makes stdio the
//! session (the `ProxyCommand` form), and a prompt read from it would eat
//! the session's first bytes. `/dev/tty` on unix and the console on
//! Windows are where a person is, when there is one.
//!
//! **No terminal is a refusal, never a wait.** Under a systemd unit, a
//! Windows scheduled task or a pipeline there is nobody to type, and a
//! process blocked on a read nobody will answer looks exactly like one
//! that is working. The refusal names the field and the two spellings
//! that work unattended.
//!
//! ## surface block
//!
//! - Entry points: [`terminal`], the [`drt_config::secret::Prompter`] the
//!   binary passes to every [`drt_config::Secret::resolve`];
//!   [`has_terminal`], the check it makes first.
//! - Configurable values: none.
//! - Fan-out: [`Echo`], hidden or visible; with no `cli` feature there is
//!   no terminal reader and every prompt is refused.

use drt_config::secret::Echo;

/// Ask for `what` on the controlling terminal, or refuse now.
pub fn terminal(what: &str, echo: Echo) -> Result<String, String> {
    if !has_terminal() {
        return Err(no_terminal(what));
    }
    read(what, echo)
}

/// The refusal, by name, with the two spellings that need nobody.
pub fn no_terminal(what: &str) -> String {
    format!(
        "{what} is read from a prompt, and there is no terminal to ask on; \
         for an unattended run give it {{\"env\": \"NAME\"}} or {{\"file\": \"/path\"}}"
    )
}

/// Whether a person could answer: the controlling terminal opens.
pub fn has_terminal() -> bool {
    #[cfg(unix)]
    {
        std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/tty")
            .is_ok()
    }
    #[cfg(windows)]
    {
        std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open("CONIN$")
            .is_ok()
    }
    #[cfg(not(any(unix, windows)))]
    {
        false
    }
}

// depth: the read itself

#[cfg(all(feature = "cli", any(unix, windows)))]
fn read(what: &str, echo: Echo) -> Result<String, String> {
    use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
    use std::io::Write;

    // The prompt goes to stderr, as the REPL's do, so stdout stays the
    // session's or the command's.
    let mut err = std::io::stderr();
    let _ = write!(err, "{what}: ");
    let _ = err.flush();
    crossterm::terminal::enable_raw_mode().map_err(|e| format!("{what}: the terminal: {e}"))?;
    let mut line = String::new();
    let outcome = loop {
        let ev = match event::read() {
            Ok(ev) => ev,
            Err(e) => break Err(format!("{what}: the terminal: {e}")),
        };
        let Event::Key(key) = ev else { continue };
        if key.kind == KeyEventKind::Release {
            continue;
        }
        match key.code {
            KeyCode::Enter => break Ok(()),
            KeyCode::Char('c') | KeyCode::Char('d')
                if key.modifiers.contains(KeyModifiers::CONTROL) =>
            {
                break Err(format!("{what}: no value given"));
            }
            KeyCode::Esc => break Err(format!("{what}: no value given")),
            KeyCode::Backspace => {
                if line.pop().is_some() && echo == Echo::Visible {
                    let _ = write!(err, "\u{8} \u{8}");
                    let _ = err.flush();
                }
            }
            KeyCode::Char(c) => {
                line.push(c);
                if echo == Echo::Visible {
                    let _ = write!(err, "{c}");
                    let _ = err.flush();
                }
            }
            _ => {}
        }
    };
    let _ = crossterm::terminal::disable_raw_mode();
    let _ = write!(err, "\r\n");
    let _ = err.flush();
    outcome.map(|()| line)
}

#[cfg(not(all(feature = "cli", any(unix, windows))))]
fn read(what: &str, _echo: Echo) -> Result<String, String> {
    Err(format!(
        "{what} is read from a prompt, and this build reads no terminal; \
         give it {{\"env\": \"NAME\"}} or {{\"file\": \"/path\"}}"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The test runner has no controlling terminal (`cargo test` under CI,
    /// or here with setsid): the prompt must answer at once, by name. When
    /// a terminal is attached the test has nothing to say.
    #[test]
    fn with_no_terminal_a_prompt_is_refused_at_once_by_name() {
        if has_terminal() {
            return;
        }
        let started = std::time::Instant::now();
        let e = terminal("commands.p2p.headers.DRT-Caller-Token", Echo::Hidden).unwrap_err();
        assert!(e.contains("commands.p2p.headers.DRT-Caller-Token"), "{e}");
        assert!(e.contains("no terminal"), "{e}");
        assert!(started.elapsed() < std::time::Duration::from_secs(1));
    }
}
