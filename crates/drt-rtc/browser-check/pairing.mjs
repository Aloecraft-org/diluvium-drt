// A page as the told side of pairing (doc/DRT-Signalling.md §6.2), in a
// real browser against real DRT: `drt p2p --match` is the signalling
// server, `drt p2p --park` a host parked at it serving the named service
// `ssh` (an echo here), and Chromium a page parked at it through `listen`
// with `pair: '*'`. The host asks the server for a call from the page; the
// server tells the page; the page calls with the caller token the entry
// carries, reaches the host's `ssh` over the session, and reports
// `connected`. A second page without consent is told the same and
// declines, and the server hears that too.
//
//   cargo build -p drt --features full
//   PLAYWRIGHT=$(npm root -g)/playwright node pairing.mjs
//
// ## surface block
//
// - Entry point: this script. Exit 0 only when every check passed.
// - Configurable: WAIT_MS, the limit on any one wait; the tokens, which
//   name who may do what at the match server.
// - Fan-out: the checks, in order: the page is present, the ask is taken,
//   the told page connects and reaches `ssh`, the server records the
//   outcome, a page without consent declines.

import { spawn } from 'node:child_process';
import { createInterface } from 'node:readline';
import { fileURLToPath } from 'node:url';
import { mkdtempSync, readFileSync } from 'node:fs';
import net from 'node:net';
import os from 'node:os';
import path from 'node:path';

const { chromium } = await import(process.env.PLAYWRIGHT ? path.join(process.env.PLAYWRIGHT, 'index.mjs') : 'playwright');
const here = path.dirname(fileURLToPath(import.meta.url));
const drt = path.resolve(here, '../../../target/debug/drt');
const WAIT_MS = 20000;
const HOST_TOKEN = 'host-token';
const HOST_CALLER = 'host-caller';
const PAGE_TOKEN = 'page-token';
const DEAF_TOKEN = 'deaf-token';
const children = [];
const cleanup = () => children.forEach((c) => c.kill());
process.on('exit', cleanup);
let failed = 0;

async function check(name, fn) {
  try {
    await fn();
    console.log(`ok       ${name}`);
  } catch (e) {
    failed++;
    const said = String(e.message).split('\n').slice(0, 3).map((l) => l.slice(0, 300));
    console.log(`FAILED   ${name}\n         ${said.join('\n         ')}`);
  }
}

// Each drt gets a home of its own: a serving peer keeps its identity under
// ~/.drt/p2p, and two must not race for one file.
function run(args, tag) {
  const home = mkdtempSync(path.join(os.tmpdir(), `drt-pairing-${tag}-`));
  const child = spawn(drt, args, { stdio: ['ignore', 'pipe', 'pipe'], env: { ...process.env, HOME: home } });
  children.push(child);
  const lines = [];
  for (const stream of [child.stdout, child.stderr]) {
    createInterface({ input: stream }).on('line', (l) => {
      console.log(`${tag}: ${l}`);
      lines.push(l);
    });
  }
  return { child, lines };
}

async function until(what, pred, ms = WAIT_MS) {
  const t0 = Date.now();
  for (;;) {
    const v = pred();
    if (v) return v;
    if (Date.now() - t0 > ms) throw new Error(`timed out waiting for ${what}`);
    await new Promise((r) => setTimeout(r, 50));
  }
}

const freePort = () => new Promise((resolve) => {
  const s = net.createServer();
  s.listen(0, '127.0.0.1', () => {
    const { port } = s.address();
    s.close(() => resolve(port));
  });
});

// depth: the server, the host and its target

const echo = net.createServer((s) => s.pipe(s));
await new Promise((r) => echo.listen(0, '127.0.0.1', r));
const echoPort = echo.address().port;
const matchPort = await freePort();
const base = `http://127.0.0.1:${matchPort}`;
const match = run(['p2p', '--match', String(matchPort), '--capacity', '8'], 'match');
await until('the match server', () => match.lines.some((l) => l.includes('admission is by token')));
const host = run([
  'p2p', '--park', `${base}/v1/host`, '--H', `auth=${HOST_TOKEN}`, '--H', `DRT-Caller-Token=${HOST_CALLER}`,
  '--forward', `ssh://127.0.0.1:${echoPort}`,
], 'host');
await until('the host present', () => host.lines.some((l) => l.includes('present at ')));

// depth: the pages

const library = readFileSync(path.resolve(here, '../client/drt_browser_access.js')).toString('base64');
const browser = await chromium.launch();
const pageErrors = [];
async function page(name, options) {
  const p = await browser.newPage();
  p.on('pageerror', (e) => pageErrors.push(e.message));
  if (process.env.VERBOSE) p.on('console', (m) => console.log(`${name}: ${m.text()}`));
  // The match server has no page to serve, and a page with no origin may
  // not call loopback at all, so the page's own URL is answered here.
  await p.route(`${base}/`, (r) => r.fulfill({ contentType: 'text/html', body: '<!doctype html><title>page</title>' }));
  await p.goto(`${base}/`);
  await p.evaluate(async ([src, base, name, options]) => {
    window.lib = await import(`data:text/javascript;base64,${src}`);
    window.reports = [];
    window.errors = [];
    window.listening = window.lib.listen(`${base}/v1/${name}`, {
      ...options,
      pollMs: 500,
      onPair: (r) => window.reports.push(r),
      onError: (e, who) => window.errors.push(`${who?.id ?? 'poll'}: ${e?.message ?? e}`),
    });
    // Present at the server only once a poll has claimed the name.
    await window.listening.poll();
  }, [library, base, name, options]);
  return p;
}
const told = await page('page', { token: PAGE_TOKEN, pair: '*', label: 'page' });
const deaf = await page('deaf', { token: DEAF_TOKEN, label: 'deaf' });

const ask = async (name) => {
  const res = await fetch(`${base}/v1/host/pair?k=${HOST_TOKEN}`, {
    method: 'POST', headers: { 'content-type': 'text/plain;charset=utf-8' }, body: JSON.stringify({ name }),
  });
  return { status: res.status, body: await res.text() };
};

await check('the host asks the server for a call from the page: 202, an id', async () => {
  const { status, body } = await ask('page');
  if (status !== 202) throw new Error(`asked: ${status} ${body}`);
  if (JSON.parse(body).id !== 'p1') throw new Error(`id: ${body}`);
});

await check('the told page calls with the host\'s caller token, connects, and reaches the service ssh', async () => {
  const r = await told.evaluate(async (waitMs) => {
    const t0 = Date.now();
    while (window.reports.length === 0) {
      if (Date.now() - t0 > waitMs) throw new Error(`no report; errors: ${window.errors.join(' | ')}`);
      await new Promise((r) => setTimeout(r, 50));
    }
    const [report] = window.reports;
    if (report.outcome !== 'connected') return { outcome: report.outcome, why: report.why, errors: window.errors };
    const s = report.session.connect('ssh');
    const w = s.writable.getWriter();
    await w.write(new TextEncoder().encode('ping over pairing'));
    const reader = s.readable.getReader();
    let got = '';
    while (got.length < 'ping over pairing'.length) {
      const { value, done } = await reader.read();
      if (done) break;
      got += new TextDecoder().decode(value);
    }
    s.close();
    return { outcome: report.outcome, why: report.why, hello: report.session.hello, got, errors: window.errors };
  }, WAIT_MS);
  if (r.outcome !== 'connected') throw new Error(`outcome ${r.outcome}: ${r.why}; ${r.errors.join(' | ')}`);
  if (!r.hello.services?.includes('ssh')) throw new Error(`hello names ${JSON.stringify(r.hello.services)}`);
  if (r.got !== 'ping over pairing') throw new Error(`echoed ${JSON.stringify(r.got)}`);
  if (r.errors.length) throw new Error(`errors: ${r.errors.join(' | ')}`);
});

await check('the host saw the page\'s call as a session and a stream to ssh', async () => {
  await until('the host\'s session', () => host.lines.some((l) => /drt p2p: c\d+: connected/.test(l)));
  await until('the stream to ssh', () => host.lines.some((l) => /drt p2p: c\d+\/\d+: ssh open/.test(l)));
});

await check('the server recorded the outcome the page reported', async () => {
  await until('the server\'s line', () => match.lines.some((l) => l.includes('host asked for a call from page: connected')));
});

await check('a page without consent declines, and the server hears why', async () => {
  const { status, body } = await ask('deaf');
  if (status !== 202) throw new Error(`asked: ${status} ${body}`);
  const r = await deaf.evaluate(async (waitMs) => {
    const t0 = Date.now();
    while (window.reports.length === 0) {
      if (Date.now() - t0 > waitMs) throw new Error(`no report; errors: ${window.errors.join(' | ')}`);
      await new Promise((r) => setTimeout(r, 50));
    }
    return window.reports[0];
  }, WAIT_MS);
  if (r.outcome !== 'declined' || r.why !== 'no pair') throw new Error(`${r.outcome}: ${r.why}`);
  await until('the server\'s line', () => match.lines.some((l) => l.includes('host asked for a call from deaf: declined (no pair)')));
});

await check('no page errors', async () => {
  if (pageErrors.length) throw new Error(pageErrors.join(' | '));
});

await browser.close();
cleanup();
console.log(failed ? `${failed} check(s) failed` : 'all checks passed');
process.exit(failed ? 1 : 0);
