// Assemble dist/drt.js and dist/drt.min.js from drt.mjs: the browser
// access client, both wasm-bindgen glues and both wasm modules in one ES
// module that fetches nothing of its own. Run by script/drt-js.sh once the
// modules and their inline glue are built.
//
// ## surface block
//
// - Entry point: `node build.mjs [VERSION]`; VERSION (default `dev`) is
//   what drt.js exports as `VERSION`.
// - Configurable: INPUTS, where each inlined piece is read from;
//   MAX_BYTES, the size dist/drt.js may not pass.
// - Fan-out: VIRTUAL, the modules drt.mjs imports by a `drt:` name, each
//   filled from INPUTS; OUTPUTS, the two files written and how.
//
// The wasm travels as base64 of its gzip, as in the SSH page
// (crates/drt-ssh-web/page/build.mjs), and is unpacked with
// DecompressionStream only when a page first asks for that module.

import fs from 'node:fs';
import path from 'node:path';
import zlib from 'node:zlib';
import { fileURLToPath, pathToFileURL } from 'node:url';
import * as esbuild from 'esbuild';

const here = path.dirname(fileURLToPath(import.meta.url));
const VERSION = process.argv[2] ?? 'dev';
const INPUTS = {
  webGlue: path.resolve(here, '../browser-test/pkg/inline/drt_web.js'),
  webWasm: path.resolve(here, '../browser-test/pkg/drt_web_bg.wasm'),
  sshGlue: path.resolve(here, '../../drt-ssh-web/page/pkg/inline/drt_ssh_web.js'),
  sshWasm: path.resolve(here, '../../drt-ssh-web/page/pkg/drt_ssh_web_bg.wasm'),
};
/** Both modules gzipped are about 2.9 MB of base64 once drt-web is wasm-opt'd, and xterm.js for `terminal()` about 0.45 MB more (3.3 MB, 2026-11-02); this is the alarm, not the goal. */
export const MAX_BYTES = 3.5 * 1024 * 1024;

const b64gz = (file) => zlib.gzipSync(fs.readFileSync(file), { level: 9 }).toString('base64');
const VIRTUAL = {
  'drt:web-glue': () => ({ path: INPUTS.webGlue }),
  'drt:ssh-glue': () => ({ path: INPUTS.sshGlue }),
  'drt:wasm': () => ({
    path: 'drt:wasm',
    namespace: 'drt-wasm',
    contents: [
      `export const VERSION = ${JSON.stringify(VERSION)};`,
      `export const WEB_WASM_GZ_B64 = ${JSON.stringify(b64gz(INPUTS.webWasm))};`,
      `export const SSH_WASM_GZ_B64 = ${JSON.stringify(b64gz(INPUTS.sshWasm))};`,
    ].join('\n'),
  }),
};
const OUTPUTS = [
  { file: 'dist/drt.js', minify: false },
  { file: 'dist/drt.min.js', minify: true },
];

// depth: the build

// Imported (e2e.mjs reads MAX_BYTES), it builds nothing.
if (import.meta.url === pathToFileURL(process.argv[1]).href) await build();

async function build() {

  for (const [name, file] of Object.entries(INPUTS)) {
    if (!fs.existsSync(file)) throw new Error(`build.mjs: no ${name} at ${file}; run script/drt-web.sh and script/drt-ssh-page.sh first`);
  }
  // A glue built with a default module path would name a sibling .wasm.
  for (const glue of [INPUTS.webGlue, INPUTS.sshGlue]) {
    if (/_bg\.wasm|import\.meta\.url/.test(fs.readFileSync(glue, 'utf8'))) {
      throw new Error(`build.mjs: ${glue} names a module path; it must be built with --omit-default-module-path`);
    }
  }

  const inline = {
    name: 'drt-inline',
    setup(build) {
      build.onResolve({ filter: /^drt:/ }, (args) => {
        const v = VIRTUAL[args.path];
        if (!v) throw new Error(`drt.mjs imports ${args.path}, which build.mjs does not provide`);
        const r = v();
        return r.namespace ? { path: r.path, namespace: r.namespace } : { path: r.path };
      });
      build.onLoad({ filter: /.*/, namespace: 'drt-wasm' }, () => ({ contents: VIRTUAL['drt:wasm']().contents, loader: 'js' }));
    },
  };

  const banner = `// drt.js ${VERSION}: DRT in a page, one ES module with its wasm inlined. https://github.com/Aloecraft-org/diluvium-drt (Apache-2.0). Includes xterm.js and @xterm/addon-fit, (c) The xterm.js authors (MIT).`;
  fs.mkdirSync(path.join(here, 'dist'), { recursive: true });
  for (const out of OUTPUTS) {
    await esbuild.build({
      entryPoints: [path.join(here, 'drt.mjs')],
      bundle: true,
      format: 'esm',
      platform: 'browser',
      target: 'es2022',
      minify: out.minify,
      legalComments: 'none',
      banner: { js: banner },
      outfile: path.join(here, out.file),
      plugins: [inline],
      loader: { '.css': 'text' },
      logLevel: 'warning',
    });
  }

  const text = fs.readFileSync(path.join(here, 'dist/drt.js'), 'utf8');
  // Nothing in the file may fetch a path of its own.
  for (const bad of ['import.meta.url', '_bg.wasm']) {
    if (text.includes(bad)) throw new Error(`build.mjs: dist/drt.js contains ${bad}`);
  }
  for (const out of OUTPUTS) {
    const bytes = fs.statSync(path.join(here, out.file)).size;
    const gz = zlib.gzipSync(fs.readFileSync(path.join(here, out.file)), { level: 9 }).length;
    if (bytes > MAX_BYTES) throw new Error(`build.mjs: ${out.file} is ${bytes} bytes, over MAX_BYTES (${MAX_BYTES})`);
    console.log(`build.mjs: ${out.file}: ${bytes} bytes (${gz} gzipped)`);
  }
}
