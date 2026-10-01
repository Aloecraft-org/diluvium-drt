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
// `drt ssh` and the REPL's `:ssh` are answered here rather than by the
// runtime (ssh-command.js): a page has no socket for the native client,
// and the SSH client it does have is ssh.html's. While a session runs, the
// keyboard is the session's. That is what the gate below is for: the
// editor queues every key it is handed, so a key typed into a remote shell
// would otherwise come back as the next line at this prompt.
//
// surface block:
//   attach(DrtTerm, terminal, { prompt, banner, DrtEditor, ssh })
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
//   ssh: { Ssh, access?, store? } (optional) -- drt-ssh-web's `Ssh`, the
//               browser access client for `--via rtc:`, and where host keys
//               and the page's key are kept. Without it `drt ssh` and
//               `:ssh <host>` say the page has no SSH client.
//   INTERRUPT: the one key this file still reads for itself, and only
//               while a command is running -- the editor is not reading
//               then, so nothing else would see it.

import { makeShell } from './shell.js';
import { sshCommand, words } from './ssh-command.js';

const INTERRUPT = '\x03';

/// The REPL's bare `:ssh`: the config's `host:ssh/shell`, with a reply
/// deadline as long as a session may be -- the native REPL's line exactly.
const SSH_SHELL_LINE = "return host.call('ssh/shell', nil, 2147483647)";

export function attach(DrtTerm, terminal, { prompt = '$ ', banner = '', DrtEditor, ssh } = {}) {
  const decoders = [null, new TextDecoder(), new TextDecoder()];
  const write = (fd, text) => terminal.write(text.replace(/\r?\n/g, '\r\n'));
  const term = new DrtTerm((fd, bytes) =>
    write(fd, decoders[fd].decode(bytes, { stream: true })),
  );
  const shell = makeShell({ term, write });

  // The gate: keys go to `sink` while something holds the keyboard (a
  // session, or a prompt it asked), and to the editor otherwise. The editor
  // gets a view of the terminal whose `onData` is the gate's.
  let sink = null;
  const editorKeys = [];
  const view = {
    write: (text) => terminal.write(text),
    onData(callback) {
      editorKeys.push(callback);
      return {
        dispose() {
          const i = editorKeys.indexOf(callback);
          if (i >= 0) editorKeys.splice(i, 1);
        },
      };
    },
    get cols() {
      return terminal.cols;
    },
    get rows() {
      return terminal.rows;
    },
  };
  const gate = terminal.onData((data) => {
    if (sink) sink(data);
    else for (const k of editorKeys) k(data);
  });
  const editor = DrtEditor.attach(view);
  const sshClient = ssh ? { ...ssh, store: ssh.store ?? remembered() } : null;
  const sshIo = {
    write: (text) => terminal.write(text),
    size: () => ({ cols: terminal.cols, rows: terminal.rows }),
    keys(handler) {
      sink = handler;
      return () => {
        if (sink === handler) sink = null;
      };
    },
    ask: (question, { secret = false } = {}) => askLine(question, secret),
  };
  /// One line from the keyboard while a command holds it: what a
  /// host-key question or a password needs, before a session starts.
  function askLine(question, secret) {
    terminal.write(question);
    return new Promise((resolve) => {
      let line = '';
      const done = (value, echo) => {
        sink = null;
        terminal.write(echo);
        resolve(value);
      };
      sink = (data) => {
        for (const c of data) {
          if (c === '\r' || c === '\n') return done(line, '\r\n');
          if (c === '\x03') return done(null, '^C\r\n');
          if (c === '\x04' && !line) return done(null, '\r\n');
          if (c === '\x7f' || c === '\b') {
            if (line) {
              line = line.slice(0, -1);
              if (!secret) terminal.write('\b \b');
            }
          } else if (c >= ' ') {
            line += c;
            if (!secret) terminal.write(c);
          }
        }
      };
    });
  }
  const runSsh = (argv, { announce = false } = {}) =>
    sshCommand(argv, { client: sshClient, io: sshIo, announce });

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
      for (;;) {
        const outcome = await editor.readLine(continuing ? '>> ' : 'dv> ');
        if (outcome.line === undefined) return replOutcome(outcome, session);
        const meta = !continuing && outcome.line.match(/^\s*:ssh(?:\s+(.*))?$/);
        if (!meta) return outcome.line;
        // `:ssh` alone is the config's grant; `:ssh <host> …` is the
        // person at this terminal naming one, as `drt ssh` would.
        if (!meta[1] || !meta[1].trim()) return SSH_SHELL_LINE;
        let argv;
        try {
          argv = words(meta[1]);
        } catch (e) {
          write(2, `:ssh: ${e.message}\n`);
          continue;
        }
        await runSsh(argv, { announce: true });
        if (session) editor.setCandidates(session.names());
        settle();
      }
    },
    ssh: runSsh,
    stop: () => interrupted,
  };
  /// A read that ended without a line: ^C abandons the unfinished one and
  /// asks again; ^D is end of input.
  function replOutcome(outcome, session) {
    if (outcome.interrupted) {
      if (session) session.abandon();
      return '';
    }
    return null;
  }

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
    // While a session holds the keyboard, ^C is the remote shell's.
    if (running && !sink && data.includes(INTERRUPT)) interrupted = true;
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
      if (gate && gate.dispose) gate.dispose();
    },
  };
}

// Host keys and the page's key, for as long as the page lives, when the
// host gives no store of its own.
function remembered() {
  const m = new Map();
  return { get: async (k) => m.get(k), set: async (k, v) => void m.set(k, v) };
}
