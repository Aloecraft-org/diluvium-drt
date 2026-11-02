// get-drt.js's check: Chromium on one origin importing get-drt.js and
// pulling drt.js from two others, a fake release mirror and a fake Pages
// site, each sending access-control-allow-origin as the real ones do.
//
//   node get-drt.test.mjs        (script/drt-js-gate.sh runs it)
//
// What it holds get-drt.js to: get-drt.sh's version names (latest, dev, a
// bare version, a tag, and the stamp as the default), the mirror before
// Pages, the release log when the mirror is down, sums verified and a
// mismatch refused, list and list-all. With dist/drt.min.js built, the
// real bundle goes through the same path and must import.
//
// Configurable: CHROMIUM names a Chromium binary instead of Playwright's.

import { chromium } from 'playwright';
import { createHash } from 'node:crypto';
import { existsSync, readFileSync } from 'node:fs';
import { createServer } from 'node:http';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const here = path.dirname(fileURLToPath(import.meta.url));
const REFUSED = 'http://127.0.0.1:9'; // a low port nothing listens on

// depth: fixtures

const sha = (b) => createHash('sha256').update(b).digest('hex');
const stub = (tag) => Buffer.from(`export const which = ${JSON.stringify(tag)};\n`);
const sums = (files) => Object.entries(files).map(([n, b]) => `${sha(b)}  ${n}`).join('\n') + '\n';

const v100 = stub('v1.0.0');
const v101 = stub('v1.0.1');
const dev3 = stub('v1.1.0-dev.3');
const dev3full = stub('v1.1.0-dev.3 full');
const real = path.join(here, 'dist', 'drt.min.js');
const v200 = existsSync(real) ? readFileSync(real) : null;

const mirror = {
  '/releases.json': JSON.stringify({
    latest: 'v1.0.0',
    releases: [
      { tag: 'v1.0.1', stable: true, status: 'unreleased' },
      { tag: 'v1.0.0', stable: true, status: 'released' },
      { tag: 'v0.9.0-rc.1', stable: false, status: 'released' },
    ],
  }),
  '/v1.0.0/drt.min.js': v100,
  '/v1.0.0/SHA256SUMS.txt': sums({ 'drt.min.js': v100 }),
  '/v1.0.1/drt.min.js': v101,
  '/v1.0.1/SHA256SUMS.txt': sums({ 'drt.min.js': v100 }), // wrong on purpose
};
if (v200) {
  mirror['/v2.0.0/drt.min.js'] = v200;
  mirror['/v2.0.0/SHA256SUMS.txt'] = sums({ 'drt.min.js': v200 });
}
const pages = {
  '/v1.1.0-dev.3/drt.min.js': dev3,
  '/v1.1.0-dev.3/drt.js': dev3full,
  '/v1.1.0-dev.3/SHA256SUMS.txt': sums({ 'drt.min.js': dev3 }), // drt.js unlisted
  '/release-log': '# comment\nv1.1.0-dev.3\tdev\t2026-11-02\nv1.0.0\tstable\t2026-11-01\nv0.9.0-rc.1\trc\t2026-10-01\n',
};
const source = readFileSync(path.join(here, 'get-drt.js'), 'utf8');
const stamped = source.replace(/^const STAMPED_VERSION = '';$/m, "const STAMPED_VERSION = 'v1.1.0-dev.3';");
if (stamped === source) throw new Error('the stamp line release.yml rewrites is not in get-drt.js');
const page = {
  '/': '<!doctype html><title>get-drt</title>',
  '/get-drt.js': source,
  '/stamped/get-drt.js': stamped,
};

function serve(files) {
  return new Promise((resolve) => {
    const s = createServer((req, res) => {
      const body = files[new URL(req.url, 'http://x').pathname];
      res.setHeader('access-control-allow-origin', '*');
      if (body === undefined) { res.statusCode = 404; res.end(); return; }
      res.setHeader('content-type', req.url.endsWith('.js') ? 'text/javascript' : req.url === '/' ? 'text/html' : 'text/plain');
      res.end(body);
    }).listen(0, '127.0.0.1', () => resolve(s));
  });
}
const origin = (s) => `http://127.0.0.1:${s.address().port}`;

// depth: checks

const servers = await Promise.all([serve(mirror), serve(pages), serve(page)]);
const [M, P, O] = servers.map(origin);
const browser = await chromium.launch(process.env.CHROMIUM ? { executablePath: process.env.CHROMIUM } : {});
const tab = await browser.newPage();
const warnings = [];
tab.on('console', (m) => { if (m.type() === 'warning') warnings.push(m.text()); });
await tab.goto(`${O}/`);

let failed = 0;
async function check(name, fn, arg) {
  const { want, ...rest } = arg;
  let got;
  try {
    got = await tab.evaluate(fn, { ...rest, M, P, REFUSED });
  } catch (e) {
    got = `threw: ${e.message.split('\n')[0]}`;
  }
  const ok = typeof want === 'function' ? want(got) : JSON.stringify(got) === JSON.stringify(want);
  console.log(`${ok ? 'ok  ' : 'FAIL'} ${name}${ok ? '' : `: got ${JSON.stringify(got)}`}`);
  if (!ok) failed++;
}

const call = async ({ module = '/get-drt.js', fn, args, M, P, REFUSED }) => {
  const g = await import(module);
  const fix = (a) => (a && typeof a === 'object'
    ? { mirror: M, pages: P, releaseLog: `${P}/release-log`, ...JSON.parse(JSON.stringify(a).replaceAll('REFUSED', REFUSED)) }
    : a);
  try {
    if (fn === 'version') return g.version;
    if (fn === 'getDrt') return (await g.getDrt(args[0], fix(args[1] || {}))).which;
    if (fn === 'fetchDrt') {
      const r = await g.fetchDrt(args[0], fix(args[1] || {}));
      return { tag: r.tag, from: r.source.startsWith(M) ? 'mirror' : 'pages', asset: r.asset, verified: r.verified };
    }
    if (fn === 'list' || fn === 'listAll') return await g[fn](fix(args[1] || {}));
    return await g[fn](args[0], fix(args[1] || {}));
  } catch (e) {
    return `error: ${e.message.split('\n')[0]}`;
  }
};

await check('resolve latest reads the mirror', call, { fn: 'resolve', args: ['latest'], want: 'v1.0.0' });
await check('resolve latest falls back to the release log', call, { fn: 'resolve', args: ['latest', { mirror: 'REFUSED' }], want: 'v1.0.0' });
await check('resolve dev reads the release log', call, { fn: 'resolve', args: ['dev'], want: 'v1.1.0-dev.3' });
await check('resolve a bare version', call, { fn: 'resolve', args: ['0.9.0'], want: 'v0.9.0' });
await check('resolve a tag', call, { fn: 'resolve', args: ['v0.9.0-rc.1'], want: 'v0.9.0-rc.1' });
await check('getDrt latest from the mirror', call, { fn: 'getDrt', args: ['latest'], want: 'v1.0.0' });
await check('getDrt dev from Pages', call, { fn: 'getDrt', args: ['dev'], want: 'v1.1.0-dev.3' });
await check('fetchDrt verifies and names its source', call, { fn: 'fetchDrt', args: ['dev'], want: { tag: 'v1.1.0-dev.3', from: 'pages', asset: 'drt.min.js', verified: true } });
await check('min: false takes drt.js; unlisted warns', call, { fn: 'fetchDrt', args: ['dev', { min: false }], want: { tag: 'v1.1.0-dev.3', from: 'pages', asset: 'drt.js', verified: false } });
await check('a checksum mismatch refuses', call, { fn: 'fetchDrt', args: ['1.0.1'], want: (g) => /checksum mismatch for drt\.min\.js/.test(g) });
await check('a release without drt.js says so', call, { fn: 'fetchDrt', args: ['0.9.0-rc.1'], want: (g) => /no drt\.min\.js for v0\.9\.0-rc\.1; tried/.test(g) });
await check('list is stable releases', call, { fn: 'list', args: [], want: ['v1.0.0'] });
await check('listAll is every release', call, { fn: 'listAll', args: [], want: [{ tag: 'v1.1.0-dev.3', channel: 'dev' }, { tag: 'v1.0.0', channel: 'stable' }, { tag: 'v0.9.0-rc.1', channel: 'rc' }] });
await check('listAll falls back to the mirror index', call, { fn: 'listAll', args: [undefined, { releaseLog: 'REFUSED/release-log' }], want: [{ tag: 'v1.0.0', channel: 'stable' }, { tag: 'v0.9.0-rc.1', channel: 'rc' }] });
await check('an unstamped copy has no version', call, { fn: 'version', args: [], want: '' });
await check('a stamped copy knows its release', call, { module: '/stamped/get-drt.js', fn: 'version', args: [], want: 'v1.1.0-dev.3' });
await check('a stamped copy gets its own release', call, { module: '/stamped/get-drt.js', fn: 'getDrt', args: [undefined], want: 'v1.1.0-dev.3' });
await check('an explicit version beats the stamp', call, { module: '/stamped/get-drt.js', fn: 'getDrt', args: ['latest'], want: 'v1.0.0' });
if (v200) {
  await check('the built drt.min.js imports through it', async ({ M, P }) => {
    const { getDrt } = await import('/get-drt.js');
    const drt = await getDrt('v2.0.0', { mirror: M, pages: P });
    if (!['call', 'connect', 'kv', 'loadRuntime'].every((k) => typeof drt[k] === 'function')) return false;
    // The inlined wasm unpacks from a blob: module too.
    return typeof (await drt.loadRuntime()).DrtTerm === 'function';
  }, { want: true });
} else {
  console.log('skip the built drt.min.js: dist/drt.min.js is not built');
}
await check('the unlisted asset warned', async () => true, { want: () => warnings.some((w) => /does not list drt\.js; not verified/.test(w)) });

await browser.close();
for (const s of servers) s.close();
if (failed) {
  console.error(`get-drt.js: ${failed} check(s) failed`);
  process.exit(1);
}
console.log('get-drt.js: every check passed');
