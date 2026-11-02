// The CLI: an xterm.js terminal where each `drt ...` line runs in the page.
// example: omits quoting, `;` and `$?` (crates/drt-web/browser-test/shell.js has them)
export function startTerminal(drt, element) {
  const terminal = new Terminal({ convertEol: true });
  terminal.open(element);
  const decoder = new TextDecoder();
  const term = new drt.DrtTerm((fd, bytes) => terminal.write(decoder.decode(bytes, { stream: true })));
  const cli = { term, session: null, stop: false };

  // At a prompt the editor gets the keys; while a command runs only ^C counts.
  let prompting = true;
  const editor = drt.DrtEditor.attach({
    write: (text) => terminal.write(text),
    onData: (key) => terminal.onData((d) => (prompting ? key(d) : d === '\x03' && (cli.stop = true))),
    get cols() { return terminal.cols; },
    get rows() { return terminal.rows; },
  });
  // A REPL line is code: coloured as it is typed, and Tab completes the
  // guest's names. A `$ ` line is a command, so neither applies.
  const ask = async (prompt, session) => {
    editor.setCandidates(session ? session.names() : []);
    prompting = true;
    const { line } = await editor.readLine(prompt, session !== null);
    prompting = false;
    return line;
  };

  async function run(argv) {
    const session = (cli.session = term.exec(argv));
    cli.stop = false;
    for (;;) {
      const step = session.tick();
      if (step.done || cli.stop) break;
      if (step.wantsInput) {
        const line = await ask(step.continuing ? '>> ' : 'dv> ', session);
        if (line === undefined) break;   // ^C or ^D at the REPL ends it
        session.feed(line);
      } else await new Promise((r) => setTimeout(r, step.sleepMs));
    }
    cli.session = null;
    session.free();
  }

  // A shell with one program on its path: drt, with no config and no files.
  (async () => {
    await run(['drt']);
    for (;;) {
      const argv = ((await ask('$ ', null)) ?? '').trim().split(/\s+/);
      if (argv[0] === 'drt') await run(argv);
      else if (argv[0]) terminal.write(`${argv[0]}: command not found\n`);
    }
  })();
  return cli;
}
