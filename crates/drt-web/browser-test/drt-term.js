// drt-term.js: a DrtTerm behind a terminal (doc/Wasm.md §4.4, §5).
//
// The terminal is anything with xterm.js's two methods, `write(text)` and
// `onData(callback)`. Given those, this file is the process a shell would
// be: a `$ ` prompt, a line editor, `drt ...` lines run through shell.js,
// and the REPL's `dv> ` and `>> ` prompts when a session asks for a line.
// The runtime's bytes arrive on fd 1 and 2 and reach the terminal as text
// with `\n` made `\r\n`, which is what a terminal wants.
//
// surface block:
//   attach(DrtTerm, terminal, { prompt }) -> { term, whenIdle(), dispose() }
//     term      the DrtTerm, to seed files into
//     whenIdle  a promise for the next moment a line is wanted
//   KEYS: what the editor answers to; every other control key is ignored.

import { makeShell } from './shell.js';

const KEYS = {
  enter: ['\r', '\n'],
  backspace: ['\x7f', '\b'],
  interrupt: '\x03',
  endOfInput: '\x04',
};

export function attach(DrtTerm, terminal, { prompt = '$ ' } = {}) {
  const decoders = [null, new TextDecoder(), new TextDecoder()];
  const write = (fd, text) => terminal.write(text.replace(/\r?\n/g, '\r\n'));
  const term = new DrtTerm((fd, bytes) =>
    write(fd, decoders[fd].decode(bytes, { stream: true })),
  );
  const shell = makeShell({ term, write });

  let line = '';
  let reader = null; // resolve(line | null) while a command reads
  let running = false;
  let interrupted = false;
  let waiters = [];
  let disposed = false;

  const idle = () => !running || reader !== null;
  const settle = () => {
    const w = waiters;
    waiters = [];
    for (const resolve of w) resolve();
  };
  const show = (text) => {
    terminal.write(text);
    settle();
  };

  const io = {
    readLine: (continuing) =>
      new Promise((resolve) => {
        reader = resolve;
        show(continuing ? '>> ' : 'dv> ');
      }),
    stop: () => interrupted,
  };

  async function submit(text) {
    terminal.write('\r\n');
    if (reader) {
      const answer = reader;
      reader = null;
      answer(text);
      return;
    }
    running = true;
    interrupted = false;
    try {
      await shell.run(text, io);
    } catch (e) {
      write(2, `drt-term: ${(e && e.message) || e}\n`);
    }
    running = false;
    if (!disposed) show(prompt);
  }

  function key(ch) {
    if (KEYS.enter.includes(ch)) {
      const text = line;
      line = '';
      submit(text);
    } else if (KEYS.backspace.includes(ch)) {
      if (line.length) {
        line = line.slice(0, -1);
        terminal.write('\b \b');
      }
    } else if (ch === KEYS.interrupt) {
      line = '';
      terminal.write('^C\r\n');
      if (reader) {
        const answer = reader;
        reader = null;
        interrupted = true;
        answer(null);
      } else if (running) {
        interrupted = true;
      } else {
        show(prompt);
      }
    } else if (ch === KEYS.endOfInput) {
      if (reader && line === '') {
        const answer = reader;
        reader = null;
        answer(null);
      }
    } else if (ch >= ' ') {
      line += ch;
      terminal.write(ch);
    }
  }

  const listener = terminal.onData((data) => {
    for (const ch of data) key(ch);
  });
  show(prompt);

  return {
    term,
    whenIdle: () =>
      idle() ? Promise.resolve() : new Promise((resolve) => waiters.push(resolve)),
    dispose() {
      disposed = true;
      if (listener && listener.dispose) listener.dispose();
    },
  };
}
