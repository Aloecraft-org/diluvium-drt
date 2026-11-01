// The end-to-end check for drt.js: Chromium on one origin imports
// dist/drt.js and dist/drt.min.js from another, served as a CDN serves a
// file (application/javascript and access-control-allow-origin, nothing
// else, and no sibling to fetch), and drives real DRT with it: a call to a
// peer parked at `drt p2p --match`, an SSH session to a stock sshd over the
// peer's named service `ssh`, a byte stream, the key-value store, the
// browser build of drt, and the refusals a page should see.
//
//   script/drt-js.sh && cargo build -p drt --features full
//   cd crates/drt-web/cdn && npm test
//
// sshd runs as root, through `sudo -n` when this does not, for the reasons
// crates/drt-ssh-web/page/e2e.mjs gives. Exits 0 only when every check
// passes.
//
// ## surface block
//
// - Entry point: `node e2e.mjs`.
// - Configurable: DRT and SSHD (env), PORTS, TOKENS, WAIT_MS.
// - Configurable: EXAMPLE_CDN, the URLs examples/35-drt-js names, each
//   answered from this directory's node_modules so the check needs no
//   network.
// - Fan-out: the checks at the bottom, one `check(name, fn)` each; the
//   last one fails if any page raised an uncaught error along the way.

import { spawn, spawnSync } from 'node:child_process';
import { createInterface } from 'node:readline';
import { fileURLToPath } from 'node:url';
import fs from 'node:fs';
import http from 'node:http';
import net from 'node:net';
import os from 'node:os';
import path from 'node:path';
import { MAX_BYTES } from './build.mjs';

const here = path.dirname(fileURLToPath(import.meta.url));
const DRT = process.env.DRT ?? path.resolve(here, '../../../target/debug/drt');
const SSHD = process.env.SSHD ?? '/usr/sbin/sshd';
const PORTS = { sshd: 18322, match: 18330, cdn: 18331, page: 18332, example: 18333 };
const TOKENS = { host: 'cdn-host-token', caller: 'cdn-caller-token', kv: 'cdn-kv-token', reader: 'cdn-kv-reader' };
const WAIT_MS = 20000;
const USER = os.userInfo().username;
const EXAMPLE_CDN = {
  'https://cdn.jsdelivr.net/npm/@xterm/xterm@5.5.0/css/xterm.css': 'node_modules/@xterm/xterm/css/xterm.css',
  'https://cdn.jsdelivr.net/npm/@xterm/xterm@5.5.0/lib/xterm.js': 'node_modules/@xterm/xterm/lib/xterm.js',
  'https://cdn.jsdelivr.net/npm/alpinejs@3.14.9/dist/module.esm.js': 'node_modules/alpinejs/dist/module.esm.js',
};

const { chromium } = await import(process.env.PLAYWRIGHT ? path.join(process.env.PLAYWRIGHT, 'index.mjs') : 'playwright');
const tmp = fs.mkdtempSync(path.join(os.tmpdir(), 'drt-js-e2e-'));
const children = [];
const cleanup = () => children.forEach((c) => c.kill());
process.on('exit', cleanup);

function run(tag, cmd, args, env = {}) {
  const child = spawn(cmd, args, { stdio: ['ignore', 'pipe', 'pipe'], env: { ...process.env, ...env } });
  children.push(child);
  const lines = [];
  for (const s of [child.stdout, child.stderr]) {
    createInterface({ input: s }).on('line', (l) => {
      lines.push(l);
      if (process.env.VERBOSE) console.log(`${tag}: ${l}`);
    });
  }
  return { child, lines };
}

async function until(what, pred, ms = WAIT_MS) {
  const t0 = Date.now();
  for (;;) {
    const v = await pred();
    if (v) return v;
    if (Date.now() - t0 > ms) throw new Error(`timed out waiting for ${what}`);
    await new Promise((r) => setTimeout(r, 100));
  }
}

let failed = 0;
async function check(name, fn) {
  try {
    await fn();
    console.log(`ok       ${name}`);
  } catch (e) {
    failed++;
    console.log(`FAILED   ${name}\n         ${String(e.message).split('\n').join('\n         ')}`);
  }
}

// depth: sshd, an echo, the match server and two parked peers

const key = (name) => {
  spawnSync('ssh-keygen', ['-q', '-t', 'ed25519', '-N', '', '-C', name, '-f', path.join(tmp, name)]);
  return path.join(tmp, name);
};
const hostKey = key('host_key');
const client = key('client');
const authorized = path.join(tmp, 'authorized_keys');
fs.copyFileSync(`${client}.pub`, authorized);
fs.chmodSync(authorized, 0o600);
const fingerprint = spawnSync('ssh-keygen', ['-lf', `${hostKey}.pub`]).stdout.toString().split(' ')[1];
fs.writeFileSync(path.join(tmp, 'sshd_config'), [
  `Port ${PORTS.sshd}`, 'ListenAddress 127.0.0.1', `HostKey ${hostKey}`,
  `AuthorizedKeysFile ${authorized}`, 'PasswordAuthentication no', 'KbdInteractiveAuthentication no',
  'UsePAM yes', 'StrictModes no', 'PermitRootLogin prohibit-password', `PidFile ${tmp}/sshd.pid`,
].join('\n') + '\n');
const [sshdCmd, ...sshdPre] = process.getuid() === 0 ? [SSHD] : ['sudo', '-n', SSHD];
const sshd = run('sshd', sshdCmd, [...sshdPre, '-D', '-e', '-f', path.join(tmp, 'sshd_config')]);

const echo = net.createServer((s) => s.pipe(s));
await new Promise((r) => echo.listen(0, '127.0.0.1', r));

fs.writeFileSync(path.join(tmp, 'topics.json'), JSON.stringify({ topics: { home: { tokens: [
  { token: TOKENS.host, roles: ['park'] },
  { token: TOKENS.caller, roles: ['call'] },
  { token: TOKENS.kv, roles: ['read', 'write'] },
  { token: TOKENS.reader, roles: ['read'] },
] } } }));
const match = run('match', DRT, ['p2p', '--match', String(PORTS.match), '--kv', path.join(tmp, 'kv'),
  '--topics', path.join(tmp, 'topics.json')], { HOME: fs.mkdtempSync(path.join(tmp, 'home-')) });
await until('the match server', () => match.lines.some((l) => l.includes('admission is by token')));
const server = `http://127.0.0.1:${PORTS.match}`;
// Each parked peer gets a home of its own: it keeps its identity there.
const park = (name, target) => {
  const p = run(name, DRT, ['p2p', '--park', `${server}/v1/home/${name}`, '--H', `auth=${TOKENS.host}`,
    '--forward', target], { HOME: fs.mkdtempSync(path.join(tmp, 'home-')) });
  return until(`${name} present`, () => p.lines.some((l) => l.includes('present at '))).then(() => p);
};
const box = await park('box', `ssh://127.0.0.1:${PORTS.sshd}`);
await park('echo', `ssh://127.0.0.1:${echo.address().port}`);

// depth: the CDN and the page, on two origins

const cdnLog = [];
const files = { '/drt.js': 'dist/drt.js', '/drt.min.js': 'dist/drt.min.js' };
http.createServer((req, res) => {
  cdnLog.push(req.url);
  const file = files[req.url];
  if (!file) return res.writeHead(404, { 'access-control-allow-origin': '*' }).end();
  res.writeHead(200, { 'content-type': 'application/javascript', 'access-control-allow-origin': '*' })
    .end(fs.readFileSync(path.join(here, file)));
}).listen(PORTS.cdn, '127.0.0.1');
const cdn = `http://127.0.0.1:${PORTS.cdn}`;
http.createServer((req, res) => {
  res.writeHead(200, { 'content-type': 'text/html' }).end('<!doctype html><title>a page</title>');
}).listen(PORTS.page, '127.0.0.1');
// localhost, not 127.0.0.1: another origin from the CDN's.
const pageUrl = `http://localhost:${PORTS.page}/`;

const browser = await chromium.launch({ executablePath: process.env.CHROMIUM || undefined });
const pageErrors = [];
const otherRequests = [];
async function page(file = 'drt.min.js') {
  const p = await browser.newPage();
  p.on('pageerror', (e) => pageErrors.push(e.message));
  p.on('request', (r) => {
    const u = new URL(r.url());
    if (u.origin === cdn && !files[u.pathname]) otherRequests.push(r.url());
    if (/\.wasm$/.test(u.pathname)) otherRequests.push(r.url());
  });
  if (process.env.VERBOSE) p.on('console', (m) => console.log(`page: ${m.text()}`));
  await p.goto(pageUrl);
  await p.evaluate(async (src) => { window.drt = await import(src); }, `${cdn}/${file}`);
  return p;
}

// depth: the checks

await check(`dist/drt.js is under ${MAX_BYTES} bytes, drt.min.js smaller, and neither names a sibling file`, async () => {
  const full = fs.readFileSync(path.join(here, 'dist/drt.js'), 'utf8');
  const min = fs.readFileSync(path.join(here, 'dist/drt.min.js'), 'utf8');
  if (full.length > MAX_BYTES) throw new Error(`drt.js is ${full.length} bytes`);
  if (min.length >= full.length) throw new Error(`drt.min.js (${min.length}) is not smaller than drt.js (${full.length})`);
  for (const [name, text] of [['drt.js', full], ['drt.min.js', min]]) {
    if (/_bg\.wasm|import\.meta\.url/.test(text)) throw new Error(`${name} names a module path`);
    if (/^\s*import\s/m.test(text)) throw new Error(`${name} imports something`);
  }
});

let a;
await check('a page on another origin imports drt.min.js served as application/javascript', async () => {
  a = await page();
  const names = await a.evaluate(() => Object.keys(window.drt).sort());
  for (const n of ['call', 'connect', 'loadSsh', 'ssh', 'loadRuntime', 'kv', 'session', 'offer', 'direct', 'listen', 'canonicalPeer', 'VERSION']) {
    if (!names.includes(n)) throw new Error(`no export ${n}; it has ${names.join(', ')}`);
  }
});

await check('call() reaches a parked peer with a caller token and no ICE server it was not given', async () => {
  const r = await a.evaluate(async ({ peer, token }) => {
    const s = await window.drt.call(peer, { token });
    const out = { services: s.hello.services, service: s.service, ice: s.pc.getConfiguration().iceServers.length };
    s.close();
    return out;
  }, { peer: `drt+ssh://127.0.0.1:${PORTS.match}/v1/home/box`, token: TOKENS.caller });
  if (!r.services?.includes('ssh') || r.service !== 'ssh') throw new Error(JSON.stringify(r));
  if (r.ice !== 0) throw new Error(`the peer connection has ${r.ice} ICE servers`);
});

await check('SSH over the peer\'s service: pinned host key, key sign-in, a command and its exit status', async () => {
  const logged = sshd.lines.filter((l) => /Accepted publickey/.test(l)).length;
  const r = await a.evaluate(async ({ peer, token, pin, user, key, waitMs }) => {
    const { drt } = window;
    const session = await drt.call(peer, { token });
    const ssh = await drt.ssh(drt.connect(session), pin);
    const result = { hostKey: ssh.hostKey, signedIn: await ssh.authKey(user, key) };
    let out = '';
    let end;
    const ended = new Promise((ok) => (end = ok));
    await ssh.shell(80, 24, (b) => (out += new TextDecoder().decode(b)), end);
    ssh.write(new TextEncoder().encode('echo cdn-$((6*7)); exit 9\n'));
    result.status = await Promise.race([ended, new Promise((r) => setTimeout(() => r('timed out'), waitMs))]);
    result.out = out;
    ssh.close();
    session.close();
    return result;
  }, { peer: `drt+ssh://127.0.0.1:${PORTS.match}/v1/home/box`, token: TOKENS.caller, pin: fingerprint,
       user: USER, key: fs.readFileSync(client, 'utf8'), waitMs: WAIT_MS });
  if (r.hostKey !== fingerprint || !r.signedIn) throw new Error(JSON.stringify(r));
  if (!r.out.includes('cdn-42')) throw new Error(`the shell printed ${JSON.stringify(r.out)}`);
  if (r.status !== 9) throw new Error(`ended with ${r.status}`);
  if (!sshd.lines.slice(logged).some((l) => /Accepted publickey/.test(l))) throw new Error('sshd logged no sign-in');
  if (!box.lines.length) throw new Error('the parked peer said nothing');
});

await check('a wrong pin is refused before anything authenticates', async () => {
  const r = await a.evaluate(async ({ peer, token }) => {
    const session = await window.drt.call(peer, { token });
    try {
      await window.drt.ssh(window.drt.connect(session, 'ssh'), 'SHA256:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA');
      return { code: 'connected' };
    } catch (e) {
      return { code: e.code, message: e.message };
    } finally {
      session.close();
    }
  }, { peer: `drt://127.0.0.1:${PORTS.match}/v1/home/box`, token: TOKENS.caller });
  if (r.code !== 'hostkey' || !r.message.includes(fingerprint)) throw new Error(JSON.stringify(r));
});

await check('a byte stream round-trips through a second parked peer, called with headers the page set', async () => {
  const got = await a.evaluate(async ({ peer, token }) => {
    // The token as a header the page sets itself, not through `token`.
    const session = await window.drt.call(peer, { headers: { authorization: `Bearer ${token}` } });
    const s = window.drt.connect(session, 'ssh');
    const w = s.writable.getWriter();
    await w.write(new TextEncoder().encode('ping through drt.js'));
    const reader = s.readable.getReader();
    let text = '';
    while (text.length < 'ping through drt.js'.length) {
      const { value, done } = await reader.read();
      if (done) break;
      text += new TextDecoder().decode(value);
    }
    s.close();
    session.close();
    return text;
  }, { peer: `drt://127.0.0.1:${PORTS.match}/v1/home/echo`, token: TOKENS.caller });
  if (got !== 'ping through drt.js') throw new Error(`echoed ${JSON.stringify(got)}`);
});

await check('a call without a token, or to no such name, is a CallError with its status', async () => {
  const r = await a.evaluate(async ({ server }) => {
    const out = [];
    for (const [peer, token] of [[`${server}/v1/home/box`, undefined], [`${server}/v1/home/nobody`, 'cdn-caller-token']]) {
      try {
        (await window.drt.call(peer, { token })).close();
        out.push('connected');
      } catch (e) {
        out.push(`${e.name} ${e.status}`);
      }
    }
    return out;
  }, { server: `drt://127.0.0.1:${PORTS.match}` });
  if (r[0] !== 'CallError 401' || !/^CallError (404|503)$/.test(r[1])) throw new Error(JSON.stringify(r));
});

await check('kv: put, get, compare-and-set, list, watch and delete, with a topic token', async () => {
  const r = await a.evaluate(async ({ server, token, reader }) => {
    const { kv } = window.drt;
    const store = kv(server, 'home', { token });
    const out = {};
    const first = await store.put('notes/a', 'one');
    out.got = await store.get('notes/a');
    try {
      await store.put('notes/a', 'stale', { ifMatch: first.version + 1000 });
      out.cas = 'written';
    } catch (e) {
      out.cas = `${e.status} ${e.version}`;
    }
    const second = await store.put('notes/a', 'two', { ifMatch: first.version });
    const listed = await store.list({ prefix: 'notes/' });
    out.listed = listed.keys.map((k) => `${k.key}=${k.value}`);
    const stop = new AbortController();
    const seen = [];
    const watching = store.watch({ prefix: 'notes/', since: listed.revision, signal: stop.signal, waitMs: 2000,
      onChange: (e) => { seen.push(`${e.op} ${e.key}`); if (seen.length === 2) stop.abort(); } });
    await store.put('notes/b', 'three', { ttl: 60 });
    out.deleted = await store.delete('notes/a', { ifMatch: second.version });
    await watching;
    out.seen = seen;
    out.absent = await store.get('notes/a');
    try {
      await kv(server, 'home', { token: reader }).put('notes/c', 'x');
      out.readerPut = 'written';
    } catch (e) {
      out.readerPut = e.status;
    }
    try {
      await kv(server, 'home').get('notes/b');
      out.noToken = 'read';
    } catch (e) {
      out.noToken = e.status;
    }
    return out;
  }, { server: `drt://127.0.0.1:${PORTS.match}`, token: TOKENS.kv, reader: TOKENS.reader });
  const want = {
    got: { value: 'one' }, listed: ['notes/a=two'], deleted: true, seen: ['put notes/b', 'delete notes/a'],
    absent: null, readerPut: 403, noToken: 401,
  };
  if (r.got?.value !== 'one' || typeof r.got.version !== 'number') throw new Error(`get: ${JSON.stringify(r.got)}`);
  if (!/^412 \d+$/.test(r.cas)) throw new Error(`compare-and-set: ${r.cas}`);
  for (const k of ['listed', 'deleted', 'seen', 'absent', 'readerPut', 'noToken']) {
    if (JSON.stringify(r[k]) !== JSON.stringify(want[k])) throw new Error(`${k}: ${JSON.stringify(r[k])}, expected ${JSON.stringify(want[k])}`);
  }
});

await check('the SSH module makes a key, and the browser build of drt loads, from drt.js unminified', async () => {
  const p = await page('drt.js');
  const r = await p.evaluate(async () => {
    const { generateKey } = await window.drt.loadSsh();
    const k = generateKey('drt.js test');
    const rt = await window.drt.loadRuntime();
    return { pub: k.publicOpenssh, abi: rt.abiVersion() };
  });
  if (!/^ssh-ed25519 \S+ drt\.js test$/.test(r.pub)) throw new Error(`key: ${r.pub}`);
  if (!(r.abi > 0)) throw new Error(`abiVersion: ${r.abi}`);
  await p.close();
});

await check('examples/35-drt-js: bare drt prints its usage, drt repl evaluates, and the panel says nothing is running', async () => {
  // Served from the repo root, as its README says: main.js imports ../../crates/drt-web/cdn/dist/drt.min.js.
  const root = path.resolve(here, '../../..');
  const types = { '.html': 'text/html', '.js': 'application/javascript', '.css': 'text/css' };
  http.createServer((req, res) => {
    const name = req.url.endsWith('/') ? `${req.url.slice(1)}index.html` : req.url.slice(1);
    const file = path.join(root, name);
    if (name.includes('..') || !fs.existsSync(file)) return res.writeHead(404).end();
    res.writeHead(200, { 'content-type': types[path.extname(file)] ?? 'text/plain' }).end(fs.readFileSync(file));
  }).listen(PORTS.example, '127.0.0.1');
  const p = await browser.newPage();
  p.on('pageerror', (e) => pageErrors.push(`35-drt-js: ${e.message}`));
  for (const [url, file] of Object.entries(EXAMPLE_CDN)) {
    const type = url.endsWith('.css') ? 'text/css' : 'application/javascript';
    await p.route(url, (r) => r.fulfill({ contentType: type, body: fs.readFileSync(path.join(here, file)) }));
  }
  await p.goto(`http://127.0.0.1:${PORTS.example}/examples/35-drt-js/`);
  const screen = () => p.evaluate(() => document.querySelector('.xterm-rows')?.innerText ?? '');
  const panel = () => p.evaluate(() => [...document.querySelectorAll('aside tr')].slice(1).map((tr) => tr.innerText.replace(/\s+/g, ' ').trim()));
  await until('bare drt: its usage, then the prompt', async () => { const t = await screen(); return t.includes('Print version') && t.trimEnd().endsWith('$'); });
  await p.click('#terminal');
  await p.keyboard.type('ls\r');
  await until('a line that is not drt', async () => (await screen()).includes('ls: command not found'));
  await p.keyboard.type('drt repl\r');
  await until('the REPL prompt', async () => (await screen()).trimEnd().endsWith('dv>'));
  // Only `drt start` drives a swarm, and it needs a config and a program file this page has none of.
  await until('the panel says nothing is running', async () => (await p.innerText('aside h2')).includes('nothing running') && (await panel()).length === 0);
  await p.keyboard.type('x = 41\r');
  await p.keyboard.type('print(x + 1)\r');
  await until('the REPL evaluates', async () => /^42$/m.test(await screen()));
  await p.keyboard.press('Control+D');
  await until('the prompt after the REPL', async () => (await screen()).trimEnd().endsWith('$'));
  await p.close();
});

await check('nothing but drt.js and drt.min.js was fetched from the CDN, and no .wasm from anywhere', async () => {
  if (otherRequests.length) throw new Error(otherRequests.join('\n'));
  if (!cdnLog.includes('/drt.js') || !cdnLog.includes('/drt.min.js')) throw new Error(`the CDN saw ${cdnLog.join(', ')}`);
});

await check('no page raised an uncaught error', async () => {
  if (pageErrors.length) throw new Error(pageErrors.join('\n'));
});

await browser.close();
cleanup();
console.log(failed ? `\n${failed} check(s) failed` : '\nall checks passed');
process.exit(failed ? 1 : 0);
