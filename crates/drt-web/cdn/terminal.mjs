// drt.js's `terminal()`: the drt CLI, live, in an element of the page.
//
// One call puts an xterm.js terminal in the element and runs a `$ ` shell
// in it whose one command is `drt`, the real binary's clap definition run
// in the page's kernel (browser-test/drt-term.js and shell.js, the same
// code the browser suite types at). The line editor is drt-web's
// `DrtEditor`, the native REPL's own `ego_cli` session: history, word
// motions, undo and Tab, and whatever highlighting that editor carries.
// xterm.js and its stylesheet are bundled into drt.js, so the page loads
// nothing else.
//
// ## surface block
//
// - Entry point: `terminal(target, options)` -> Promise of
//   { xterm, term, run(line), reset(), whenIdle(), dispose() }.
//   `target` is an element or its id. `term` is the DrtTerm (the
//   kernel); the rest are drt-term.js's.
//   Options:
//   - `backings`: the kernel's connectors by family, as `kernel()` takes
//     them; `{ fs }` gives it a filesystem (fs.mjs).
//   - `prompt` (default `$ `), `banner` (default none).
//   - `ssh` (default true): `drt ssh` and the REPL's `:ssh` through
//     drt-ssh-web, `--via rtc:` through the browser access client. false
//     leaves the SSH module packed, and those commands say the page has
//     no SSH client.
//   - `xterm`: options for xterm.js's `Terminal`, over XTERM_DEFAULTS.
//   - `fit` (default true): size the terminal to the element and follow
//     its size.
// - Configurable: XTERM_DEFAULTS; STYLE_ID, the one <style> the bundled
//   xterm.css goes in.
// - Fan-out: Tab at the `$ ` prompt completes `drt` and the subcommands
//   this build's `drt --help` lists (helpCommands); at the REPL's `dv> `
//   and `>> ` it completes the guest's names (drt-term.js).

import { Terminal } from '@xterm/xterm';
import { FitAddon } from '@xterm/addon-fit';
import XTERM_CSS from '@xterm/xterm/css/xterm.css';
import * as access from '../../drt-rtc/client/drt_browser_access.js';
import { attach } from '../browser-test/drt-term.js';
import { loadRuntime, loadSsh } from './drt.mjs';

export const XTERM_DEFAULTS = Object.freeze({ cursorBlink: true, convertEol: false });
export const STYLE_ID = 'drt-xterm-css';

export async function terminal(target, options = {}) {
  const element = typeof target === 'string' ? document.getElementById(target) : target;
  if (!element) throw new Error(`terminal: no element with id '${target}'`);
  const [runtime, ssh] = await Promise.all([
    loadRuntime(),
    options.ssh === false ? null : loadSsh(),
  ]);

  addStyle();
  const xterm = new Terminal({ ...XTERM_DEFAULTS, ...options.xterm });
  const fit = options.fit === false ? null : new FitAddon();
  if (fit) xterm.loadAddon(fit);
  xterm.open(element);
  const observer = fit ? follow(element, fit) : null;

  const cli = attach(runtime.DrtTerm, xterm, {
    prompt: options.prompt ?? '$ ',
    banner: options.banner ?? '',
    DrtEditor: runtime.DrtEditor,
    backings: options.backings,
    commands: ['drt', ...helpCommands(runtime.DrtTerm)],
    ssh: ssh ? { Ssh: ssh.Ssh, access } : undefined,
  });
  xterm.focus();
  return {
    ...cli,
    xterm,
    dispose() {
      cli.dispose();
      observer?.disconnect();
      xterm.dispose();
    },
  };
}

// depth: the stylesheet, sizing, and the subcommand list

function addStyle() {
  if (document.getElementById(STYLE_ID)) return;
  const style = document.createElement('style');
  style.id = STYLE_ID;
  style.textContent = XTERM_CSS;
  document.head.append(style);
}

function follow(element, fit) {
  fit.fit();
  const observer = new ResizeObserver(() => fit.fit());
  observer.observe(element);
  return observer;
}

/// The subcommands under "Commands:" in `drt --help`, asked of a scratch
/// kernel so the list is this build's and never a copy that drifts.
function helpCommands(DrtTerm) {
  let text = '';
  const decoder = new TextDecoder();
  const scratch = new DrtTerm((fd, bytes) => {
    if (fd === 1) text += decoder.decode(bytes, { stream: true });
  });
  const session = scratch.exec(['drt', '--help']);
  for (let i = 0; i < 1000; i++) {
    const step = session.tick();
    if (step.done || step.wantsInput) break;
  }
  session.free();
  scratch.free();
  const block = text.split(/^Commands:\s*$/m)[1]?.split(/^\S/m)[0] ?? '';
  return [...block.matchAll(/^ {2}([a-z][a-z0-9-]*)\s/gm)].map((m) => m[1]);
}
