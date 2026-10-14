// A page asking reflect servers (doc/Reflect.md, In a page), in a real
// browser against real DRT: two `drt p2p --reflect` gates on loopback,
// and Chromium loading the shipped library and calling `reflect`. With no
// NAT between them the page's mapping is independent; a port nothing
// answers on reads as `udp_blocked`, and one server alone leaves the
// mapping `no_peer`.
//
//   cargo build -p drt --features full
//   PLAYWRIGHT=$(npm root -g)/playwright node reflect.mjs
//
// ## surface block
//
// - Entry point: this script. Exit 0 only when every check passed.
// - Configurable: GATES, the two reflect ports; SILENT, a port nothing
//   answers on; WAIT_MS, the limit on any one wait.
// - Fan-out: the checks, in order: both servers answer, the mapping is
//   independent, a silent server is blocked, one server is `no_peer`.

import { spawn } from 'node:child_process';
import { createInterface } from 'node:readline';
import { fileURLToPath } from 'node:url';
import { mkdtempSync, readFileSync, rmSync } from 'node:fs';
import assert from 'node:assert/strict';
import os from 'node:os';
import path from 'node:path';

const { chromium } = await import(process.env.PLAYWRIGHT ? path.join(process.env.PLAYWRIGHT, 'index.mjs') : 'playwright');
const here = path.dirname(fileURLToPath(import.meta.url));
const drt = path.resolve(here, '../../../target/debug/drt');
const GATES = [34870, 34871];
// Below 1024 and unprivileged here, so nothing listens on it.
const SILENT = 9;
const WAIT_MS = 15000;
const children = [];
const homes = [];
const cleanup = () => {
  children.forEach((c) => c.kill());
  homes.forEach((h) => rmSync(h, { recursive: true, force: true }));
};
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

// depth: the gates and the page

function gate(port) {
  const home = mkdtempSync(path.join(os.tmpdir(), 'drt-reflect-'));
  homes.push(home);
  const child = spawn(drt, ['p2p', '--reflect', String(port)], { stdio: ['ignore', 'pipe', 'pipe'], env: { ...process.env, HOME: home } });
  children.push(child);
  const lines = [];
  for (const stream of [child.stdout, child.stderr]) {
    createInterface({ input: stream }).on('line', (l) => {
      console.log(`gate ${port}: ${l}`);
      lines.push(l);
    });
  }
  return lines;
}

async function until(what, pred) {
  for (const t0 = Date.now(); !pred(); await new Promise((r) => setTimeout(r, 50))) {
    if (Date.now() - t0 > WAIT_MS) throw new Error(`timed out waiting for ${what}`);
  }
}

for (const port of GATES) {
  const lines = gate(port);
  await until(`the gate on ${port}`, () => lines.some((l) => l.includes('reflect offers')));
}

const browser = await chromium.launch();
const page = await browser.newPage();
const library = readFileSync(path.resolve(here, '../client/drt_browser_access.js')).toString('base64');
await page.evaluate(async (src) => {
  window.lib = await import(`data:text/javascript;base64,${src}`);
}, library);
const ask = (servers) => page.evaluate((s) => window.lib.reflect(s, { gatherTimeoutMs: 2000 }), servers);

const both = await ask(GATES.map((p) => `127.0.0.1:${p}`));
console.log(`report: ${JSON.stringify(both)}`);
await check('both servers answer with the page\'s mapped address', () => {
  assert.deepEqual(both.servers.map((s) => s.code), ['ok', 'ok']);
  assert.equal(both.udp.code, 'ok');
  assert.match(both.udp.mapped[0], /^127\.0\.0\.1:\d+$/);
});
await check('the mapping with no NAT is endpoint_independent', () => {
  assert.deepEqual(both.mapping, { code: 'ok', mapping: 'endpoint_independent' });
});
const silent = await ask([`drt+stun://127.0.0.1:${GATES[0]}`, `127.0.0.1:${SILENT}`]);
await check('a server that does not answer is udp_blocked', () => {
  assert.deepEqual(silent.servers.map((s) => s.code), ['ok', 'udp_blocked']);
});
await check('one server answering leaves the mapping no_peer', () => {
  assert.deepEqual(silent.mapping, { code: 'no_peer' });
});

await browser.close();
cleanup();
process.exit(failed ? 1 : 0);
