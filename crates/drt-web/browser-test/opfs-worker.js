// opfs-worker.js: the `aloelite-opfs` check's Worker. OPFS's synchronous
// access handles exist only in a Dedicated Worker, so drt-web, the aloelite
// engine and cdn/fs.mjs all run here, on one thread, as a page that keeps
// a node's files in OPFS would run them. One message in, one answer out:
// every step's result, for run.mjs to judge.
//
// Steps: a kernel on an OPFS volume writes a file; a second open of the
// same file fails with `busy`; the file is closed, opened again, and a new
// kernel reads what the first wrote; a read-only attach refuses a write;
// `export` is a SQLite file; opening more files than the pool has room for
// fails with `opfs` until `addCapacity`. Everything made is deleted.

import init, { DrtTerm } from './pkg/drt_web.js';
import * as engine from './aloelite/aloelite_wasm.js';
import { open, attach } from './page-fs.js';

const DIRECTORY = '.drt-browser-test';
const encode = (text) => new TextEncoder().encode(text);
const decode = (bytes) => new TextDecoder().decode(bytes);

async function kernel(fs, program) {
  let out = '';
  const decoder = new TextDecoder();
  const term = new DrtTerm((fd, bytes) => { out += decoder.decode(bytes, { stream: true }); }, { fs });
  term.putDir('/k/data');
  term.putFile('/k/app.dlua', encode(program));
  term.putFile('/k/deploy.json', encode(JSON.stringify({
    program: { path: 'app.dlua' },
    caps: [{ capability: 'host:fs/*' }],
    connectors: { fs: { scope: { scope: './data', access: 'readwrite' } } },
  })));
  term.setCwd('/k');
  const session = term.exec(['drt', 'run', '--config', 'deploy.json']);
  let status = null;
  const deadline = Date.now() + 10000;
  while (status === null && Date.now() < deadline) {
    const step = session.tick();
    if (step.done) status = step.status;
    else await new Promise((r) => setTimeout(r, 1));
  }
  session.free();
  return { term, status, out };
}

async function caught(f) {
  try {
    await f();
    return null;
  } catch (e) {
    return { code: e.code ?? null, message: String(e.message ?? e) };
  }
}

self.onmessage = async () => {
  const r = {};
  const opened = [];
  try {
    await init();
    await engine.default();
    const pool = await engine.Pool.install({ directory: DIRECTORY });
    for (const n of pool.list()) pool.delete(n);

    let file = await open(engine, { store: 'opfs', directory: DIRECTORY, name: 'node.fs' });
    opened.push(file);
    const first = await kernel(attach(file, { volume: 'node' }), "host.fs.write('note.txt', 'kept in OPFS')\nprint('wrote')\n");
    r.first = { status: first.status, out: first.out };
    first.term.free();

    r.busy = await caught(() => open(engine, { store: 'opfs', directory: DIRECTORY, name: 'node.fs' }));

    await file.close();
    file = await open(engine, { store: 'opfs', directory: DIRECTORY, name: 'node.fs' });
    opened.push(file);
    const again = await kernel(attach(file, { volume: 'node' }), "print(host.fs.read('note.txt'))\n");
    r.again = { status: again.status, out: again.out, note: decode(again.term.getFile('/k/data/note.txt') ?? new Uint8Array()) };
    again.term.free();

    const ro = attach(file, { volume: 'node', access: 'ro' });
    r.readOnly = await caught(() => ro.write('/k/data/note.txt', encode('changed'), false));
    r.readOnlyRead = decode(ro.read('/k/data/note.txt'));

    r.exported = decode(file.export().slice(0, 15));

    // Room for (capacity / 2) files, node.fs already among them.
    r.capacity = pool.capacity;
    const extra = [];
    let full = null;
    for (let i = 0; i < 8 && full === null; i += 1) {
      full = await caught(async () => extra.push(await open(engine, { store: 'opfs', directory: DIRECTORY, name: `extra-${i}.fs` })));
    }
    opened.push(...extra);
    r.full = full;
    r.extraBeforeFull = extra.length;
    await file.addCapacity(2);
    r.afterAdd = await caught(async () => opened.push(await open(engine, { store: 'opfs', directory: DIRECTORY, name: `extra-${extra.length}.fs` })));
  } catch (e) {
    r.threw = String(e.stack ?? e);
  } finally {
    for (const f of opened) await caught(() => f.delete());
  }
  self.postMessage(r);
};
