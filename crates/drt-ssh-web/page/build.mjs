// Assemble dist/ssh.html: ssh.html with xterm.js, its fit addon, the
// wasm-bindgen glue and the wasm itself inlined, so the page is one file
// that fetches nothing. Run by script/drt-ssh-page.sh after the module and
// its glue are in pkg/.
//
// ## surface block
//
// - Entry point: `node build.mjs`.
// - Configurable: INPUTS, the files each placeholder is filled from.
// - Fan-out: the placeholder map; `@@NAME@@` in ssh.html is replaced by
//   INPUTS[NAME], made safe for the element it sits in.
//
// The wasm travels as base64 of its gzip and the page unpacks it with
// DecompressionStream: about half the size of base64 of the raw module
// (doc/Plan-0.8.0.md §2.1).

import fs from 'node:fs';
import path from 'node:path';
import zlib from 'node:zlib';
import { fileURLToPath } from 'node:url';

const here = path.dirname(fileURLToPath(import.meta.url));
const read = (p) => fs.readFileSync(path.join(here, p));
const INPUTS = {
  XTERM_CSS: () => read('node_modules/@xterm/xterm/css/xterm.css').toString(),
  XTERM_JS: () => read('node_modules/@xterm/xterm/lib/xterm.js').toString(),
  FIT_JS: () => read('node_modules/@xterm/addon-fit/lib/addon-fit.js').toString(),
  GLUE_JS: () => read('pkg/drt_ssh_web.js').toString(),
  WASM_GZ_B64: () => zlib.gzipSync(read('pkg/drt_ssh_web_bg.wasm'), { level: 9 }).toString('base64'),
};

// Inlined text must not end the element it is inlined into.
const safe = (name, text) =>
  name.endsWith('_CSS') ? text.replaceAll('</style', '<\\/style') : text.replaceAll('</script', '<\\/script');

const template = read('ssh.html').toString();
const page = template.replace(/@@([A-Z0-9_]+)@@/g, (whole, name) => {
  if (!(name in INPUTS)) throw new Error(`ssh.html names ${whole}, which build.mjs does not fill`);
  return safe(name, INPUTS[name]());
});
fs.mkdirSync(path.join(here, 'dist'), { recursive: true });
const out = path.join(here, 'dist/ssh.html');
fs.writeFileSync(out, page);
const wasm = read('pkg/drt_ssh_web_bg.wasm').length;
const gz = zlib.gzipSync(page, { level: 9 }).length;
console.log(`build.mjs: ${out}: ${page.length} bytes (${gz} gzipped); the module inside is ${wasm} bytes`);
