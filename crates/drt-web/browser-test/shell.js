// The shell a page runs a `cmd` line through (doc/Wasm.md §5).
//
// Just enough sh for examples/*/meta.json: commands separated by `;`,
// words quoted with '...' or "...", `$?` for the last status, `echo`, and
// `drt` -- which is the real thing, parsed by the binary's own clap
// definition inside `term.exec`. Anything else is "command not found",
// status 127, in sh's words. Not a shell: a way to type the README's
// command lines at a DrtTerm and read the output the README shows.
//
// surface block:
//   makeShell({ term, write, sleep }) -> { run(line, io), status }
//     term   a DrtTerm (pkg/drt_web.js)
//     write  (fd, text): where stdout (1) and stderr (2) go
//     sleep  (ms) -> Promise; defaults to setTimeout
//     io     { readLine(continuing, session) -> Promise<string|null>,
//              stop() -> bool }
//            readLine answers a session that wants a line; null is end of
//            input, which is what ^D is to the native repl. The session is
//            passed so the host can refresh Tab's candidates from it and
//            abandon a line on ^C -- both the guest's business, asked
//            between lines (doc/Wasm.md §5). stop() polled
//            between ticks: true ends the command with status 130 (^C).
//            ssh(argv) -> Promise<status>, optional: what `drt ssh` runs.
//            running(session | null), optional: told when a drt command's
//            session starts and before it is freed.
//   ECHO, DRT: the two commands; everything else is not found.
//   BUSY_TICK_MS: how often a session that wants a line is still ticked
//     while something it spawned is alive (the repl's `busy`).

const ECHO = 'echo';
const DRT = 'drt';
const BUSY_TICK_MS = 20;
const STATUS = Symbol('$?');

export function makeShell({ term, write, sleep = wait }) {
  let last = 0;
  const shell = {
    get status() {
      return last;
    },
    async run(line, io = noInput) {
      for (const words of parse(line)) {
        const argv = words.map((w) => expand(w, last));
        if (argv.length === 0) continue;
        last = await command(argv, io);
      }
      return last;
    },
  };

  async function command(argv, io) {
    const [name, ...args] = argv;
    switch (name) {
      case ECHO:
        write(1, args.join(' ') + '\n');
        return 0;
      case DRT:
        // `drt ssh` is the page's own (ssh-command.js, by way of
        // drt-term.js): the runtime here has no socket for the native one.
        if (args[0] === 'ssh') {
          if (io.ssh) return io.ssh(args.slice(1));
          write(2, 'drt ssh: this page has no SSH client\n');
          return 1;
        }
        return drt(args, io);
      default:
        write(2, `sh: ${name}: command not found\n`);
        return 127;
    }
  }

  // depth: one drt command, ticked to its exit. The page owns the clock:
  // a sleep is a timer, a line is whatever io provides.
  async function drt(args, io) {
    const session = term.exec([DRT, ...args]);
    if (io.running) io.running(session);
    try {
      for (;;) {
        if (io.stop && io.stop()) return 130;
        const next = session.tick();
        if (next.sleepMs !== undefined) {
          await sleep(next.sleepMs);
          continue;
        }
        if (next.wantsInput) {
          // A REPL line may have spawned something; the page's loop is
          // here and the editor is async, so the child keeps running
          // while the next line is typed rather than only between lines.
          let ended = false;
          const ticker = next.busy
            ? setInterval(() => { if (session.tick().done) ended = true; }, BUSY_TICK_MS)
            : null;
          let line;
          try {
            line = await io.readLine(next.continuing, session);
          } finally {
            if (ticker) clearInterval(ticker);
          }
          if (ended) continue;
          if (line === null) {
            // End of input, newline included, as the native repl leaves.
            write(2, '\n');
            return 0;
          }
          session.feed(line);
          continue;
        }
        return next.status;
      }
    } finally {
      if (io.running) io.running(null);
      // A module a trap terminated refuses even `free`; the error that
      // terminated it is the one worth seeing, so this one is not.
      try {
        session.free();
      } catch (_) {
        /* terminated */
      }
    }
  }
  return shell;
}

const noInput = { readLine: async () => null };
const wait = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

function expand(word, status) {
  return word.map((part) => (part === STATUS ? String(status) : part)).join('');
}

// depth: the parser. A line is commands; a command is words; a word is
// parts, each a string or the `$?` marker, expanded when its command runs
// so `drt run x; echo "exit $?"` reports drt's status and not the shell's
// before the line began.
function parse(line) {
  const commands = [];
  let words = [];
  let word = [];
  let have = false;
  const part = (text) => {
    if (text !== '') word.push(text);
    have = true;
  };
  const flush = () => {
    if (have) words.push(word);
    word = [];
    have = false;
  };
  for (let i = 0; i < line.length; i++) {
    const c = line[i];
    if (c === "'") {
      const end = line.indexOf("'", i + 1);
      if (end < 0) throw new Error('unterminated single quote');
      part(line.slice(i + 1, end));
      i = end;
    } else if (c === '"') {
      let j = i + 1;
      let text = '';
      for (; j < line.length && line[j] !== '"'; j++) {
        if (line[j] === '\\' && '"\\$'.includes(line[j + 1] ?? '')) {
          text += line[++j];
        } else if (line[j] === '$' && line[j + 1] === '?') {
          part(text);
          text = '';
          word.push(STATUS);
          j++;
        } else {
          text += line[j];
        }
      }
      if (j >= line.length) throw new Error('unterminated double quote');
      part(text);
      i = j;
    } else if (c === '\\' && i + 1 < line.length) {
      part(line[++i]);
    } else if (c === ';') {
      flush();
      commands.push(words);
      words = [];
    } else if (c === ' ' || c === '\t') {
      flush();
    } else if (c === '$' && line[i + 1] === '?') {
      word.push(STATUS);
      have = true;
      i++;
    } else {
      part(c);
    }
  }
  flush();
  commands.push(words);
  return commands;
}
