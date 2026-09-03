// drt-term.js: a DrtTerm behind a terminal (doc/Wasm.md §4.4, §5, M8).
//
// The terminal is an xterm.js `Terminal`, or anything duck-typed like one:
// `write(text)`, `onData(callback)`, `cols`, `rows`, and -- for `run` and
// `reset`, which put input in without a keyboard -- `input(data)`. Given
// that, this file
// is the process a shell would be -- a `$ ` prompt, `drt ...` lines run
// through shell.js, and the REPL's `dv> ` and `>> ` prompts when a session
// asks for a line. The runtime's bytes arrive on fd 1 and 2 and reach the
// terminal as text with `\n` made `\r\n`, which is what a terminal wants.
//
// The editing is not here. `DrtEditor` is `ego_cli`'s `Session` over the
// same terminal object, so a line typed in a page gets the history, word
// motions, undo and Tab a tty gets, from one implementation rather than
// two that drift (D8). This file decides only *when* a line is wanted and
// with which prompt -- §5's rule that a host calls `read_line` at exactly
// one point, where the driver parks on input.
//
// surface block:
//   attach(DrtTerm, terminal, { prompt, banner, DrtEditor })
//       -> { term, run(line), reset(), whenIdle(), dispose() }
//     term      the DrtTerm, to seed files into
//     run       submit a line as though it were typed -- through the
//               terminal's own `input`, so it really is typed and the
//               editor treats it identically. Resolves with its exit
//               status. For a host that has buttons as well as a
//               keyboard -- a "try this" link, a panel restoring a
//               session.
//     reset     abandon whatever is running and return to the prompt. The
//               filesystem survives, because the instance is what a
//               restart is about: every command already runs a fresh one.
//     whenIdle  a promise for the next moment a line is wanted. It answers
//               about now, not about a keystroke the terminal has not
//               delivered yet, so a host sequencing commands should await
//               `run` instead.
//   INTERRUPT: the one key this file still reads for itself, and only
//               while a command is running -- the editor is not reading
//               then, so nothing else would see it.

import { makeShell } from './shell.js';

const INTERRUPT = '\x03';

export function attach(DrtTerm, terminal, { prompt = '$ ', banner = '', DrtEditor } = {}) {
  const decoders = [null, new TextDecoder(), new TextDecoder()];
  const write = (fd, text) => terminal.write(text.replace(/\r?\n/g, '\r\n'));
  const term = new DrtTerm((fd, bytes) =>
    write(fd, decoders[fd].decode(bytes, { stream: true })),
  );
  const shell = makeShell({ term, write });
  const editor = DrtEditor.attach(terminal);

  let running = false;
  let interrupted = false;
  let waiters = [];
  let disposed = false;
  let pending = null; // resolve(status) for the `run` in flight

  const idle = () => !running;
  const settle = () => {
    const w = waiters;
    waiters = [];
    for (const resolve of w) resolve();
  };

  /// The REPL's side of §5: a line when the session parks on input, with
  /// the prompt its `continuing()` chooses, and the candidate snapshot
  /// refreshed from the guest after every accepted line.
  const io = {
    readLine: async (continuing, session) => {
      if (session) editor.setCandidates(session.names());
      settle();
      const outcome = await editor.readLine(continuing ? '>> ' : 'dv> ');
      if (outcome.line !== undefined) return outcome.line;
      if (outcome.interrupted) {
        if (session) session.abandon();
        return '';
      }
      return null; // eof
    },
    stop: () => interrupted,
  };

  async function loop() {
    while (!disposed) {
      settle();
      const outcome = await editor.readLine(prompt);
      if (disposed) return;
      if (outcome.eof || outcome.interrupted) continue;
      const text = outcome.line;
      if (!text.trim()) continue;
      running = true;
      interrupted = false;
      let status = 1;
      try {
        status = await shell.run(text, io);
      } catch (e) {
        write(2, `drt-term: ${(e && e.message) || e}\n`);
      }
      running = false;
      if (pending) {
        pending(status);
        pending = null;
      }
    }
  }

  const listener = terminal.onData((data) => {
    // While a command runs the editor is not reading, so this is the only
    // thing that sees Ctrl+C. At a prompt the editor has it, and clearing
    // the line is its business rather than this file's.
    if (running && data.includes(INTERRUPT)) interrupted = true;
  });

  if (banner) write(1, banner.endsWith('\n') ? banner : `${banner}\n`);
  loop();

  return {
    term,
    /// Type `line` and submit it: literally typed, through the terminal's
    /// own `input`, so the editor echoes and edits it exactly as it would
    /// a person's keystrokes and there is one input path rather than two.
    /// Needs a terminal with xterm.js's `input`, which is what the method
    /// means there -- "as if the user typed this".
    run(line) {
      if (running) return Promise.reject(new Error('the terminal is busy'));
      if (typeof terminal.input !== 'function') {
        return Promise.reject(
          new Error('run() needs a terminal with input(), the way xterm.js has one'),
        );
      }
      const done = new Promise((resolve) => {
        pending = resolve;
      });
      terminal.input(`${line}\r`, true);
      return done;
    },
    /// Abandon whatever is running and return to a fresh prompt.
    ///
    /// While a command runs this is the stop the shell polls; at a prompt
    /// it is Ctrl+C, which is the editor's to interpret.
    reset() {
      if (running) interrupted = true;
      else if (typeof terminal.input === 'function') terminal.input(INTERRUPT, true);
    },
    whenIdle: () =>
      idle() ? Promise.resolve() : new Promise((resolve) => waiters.push(resolve)),
    dispose() {
      disposed = true;
      if (listener && listener.dispose) listener.dispose();
    },
  };
}
