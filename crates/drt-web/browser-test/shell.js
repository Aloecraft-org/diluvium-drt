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
//     io     { readLine(continuing) -> Promise<string|null>, stop() -> bool }
//            readLine answers a session that wants a line; null is end of
//            input, which is what ^D is to the native repl. stop() polled
//            between ticks: true ends the command with status 130 (^C).
//   ECHO, DRT: the two commands; everything else is not found.

const ECHO = 'echo';
const DRT = 'drt';
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
    try {
      for (;;) {
        if (io.stop && io.stop()) return 130;
        const next = session.tick();
        if (next.sleepMs !== undefined) {
          await sleep(next.sleepMs);
          continue;
        }
        if (next.wantsInput) {
          const line = await io.readLine(next.continuing);
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
