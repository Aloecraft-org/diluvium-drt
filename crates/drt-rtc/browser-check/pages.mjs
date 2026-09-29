// Two pages and no DRT (doc/BrowserAccess.md §10): page A answers a
// session and serves, page B calls, and each opens a stream to what the
// other serves. The shipped client library on both sides, in Chromium;
// this script is the signaling, carrying each record across once.
//
//   PLAYWRIGHT=$(npm root -g)/playwright node pages.mjs
//
// Exits 0 only when every check passed.
//
// ## surface block
//
// - Entry point: this script.
// - Configurable: TRANSFER (env), the bytes sent through A's echo and
//   expected back, default 1 MiB; WAIT_MS, the limit on any one wait.
// - Fan-out: `check(name, fn)`, one per claim of §10 that two pages can
//   settle; window.answerer and window.caller are each page's half.

import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import path from 'node:path';

const { chromium } = await import(process.env.PLAYWRIGHT ? path.join(process.env.PLAYWRIGHT, 'index.mjs') : 'playwright');
const here = path.dirname(fileURLToPath(import.meta.url));
const TRANSFER = Number(process.env.TRANSFER ?? 1 << 20);
const WAIT_MS = 15000;

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

// depth: two pages, each loading the library as the release serves it

const library = readFileSync(path.resolve(here, '../client/drt_browser_access.js')).toString('base64');
// Chromium names a page's own addresses with mDNS, and a record drops
// `.local` candidates (§2.1), so two pages on one machine with no STUN
// server have nothing to reach each other by. Across a network the
// server-reflexive candidates do that job (§10.4); here the flag does.
const browser = await chromium.launch({ args: ['--disable-features=WebRtcHideLocalIpsWithMdns'] });
const pageErrors = [];
async function page() {
  const p = await browser.newPage();
  p.on('pageerror', (e) => pageErrors.push(e.message));
  if (process.env.VERBOSE) p.on('console', (m) => console.log(`page: ${m.text()}`));
  await p.evaluate(async (src) => {
    window.lib = await import(`data:text/javascript;base64,${src}`);
  }, library);
  return p;
}
const [a, b] = [await page(), await page()];

// A serves `echo`; B serves `back`, which answers with its own greeting.
await a.evaluate((waitMs) => {
  const echo = async (stream) => {
    const reader = stream.readable.getReader();
    const writer = stream.writable.getWriter();
    for (;;) {
      const { value, done } = await reader.read();
      if (done) break;
      await writer.write(value);
    }
    await writer.close();
  };
  window.answerer = async (callerRecord) => {
    window.pending = await lib.answer(callerRecord, { services: { echo }, label: 'page A', timeoutMs: waitMs });
    window.pending.session.then((s) => (window.session = s));
    return window.pending.recordText;
  };
}, WAIT_MS);
await b.evaluate((waitMs) => {
  const back = async (stream) => {
    const writer = stream.writable.getWriter();
    await writer.write(new TextEncoder().encode('hello from B'));
    await writer.close();
  };
  window.caller = async () => {
    window.pending = await lib.offer({ services: { back } });
    return window.pending.recordText;
  };
  window.accept = async (answererRecord) => {
    window.session = await window.pending.accept(answererRecord, { timeoutMs: waitMs });
    return window.session.hello;
  };
}, WAIT_MS);

// depth: the checks

let hello;
await check('a page answers a page: records across once, then hello with its services', async () => {
  const callerRecord = await b.evaluate(() => window.caller());
  const answererRecord = await a.evaluate((r) => window.answerer(r), callerRecord);
  hello = await b.evaluate((r) => window.accept(r), answererRecord);
  if (hello.service !== 'page A' || JSON.stringify(hello.services) !== '["echo"]') {
    throw new Error(`hello was ${JSON.stringify(hello)}`);
  }
});

await check(`the caller's stream to a named service echoes ${TRANSFER} bytes through the page's credit`, async () => {
  const r = await b.evaluate(async (n) => {
    const s = window.session.connect('echo');
    const writer = s.writable.getWriter();
    const reader = s.readable.getReader();
    const big = new Uint8Array(n);
    for (let i = 0; i < n; i++) big[i] = (i * 31) & 0xff;
    // Wisp has no half-close: closing the writer ends the stream, so it
    // closes only once everything is back.
    const sent = writer.write(big);
    const got = new Uint8Array(n);
    let at = 0;
    while (at < n) {
      const { value, done } = await reader.read();
      if (done) break;
      got.set(value, at);
      at += value.length;
    }
    await sent;
    await writer.close();
    await s.closed;
    return { id: s.id, at, intact: got.every((v, i) => v === big[i]) };
  }, TRANSFER);
  if (r.id !== 1 || r.at !== TRANSFER || !r.intact) throw new Error(JSON.stringify(r));
});

await check('the answerer opens an even id to what the caller serves', async () => {
  const r = await a.evaluate(async () => {
    const s = (window.session ?? await window.pending.session).connect('back');
    const reader = s.readable.getReader();
    let text = '';
    for (;;) {
      const { value, done } = await reader.read();
      if (done) break;
      text += new TextDecoder().decode(value);
    }
    return { id: s.id, text };
  });
  if (r.id !== 2 || r.text !== 'hello from B') throw new Error(JSON.stringify(r));
});

await check('a name a page does not serve is refused with 0x48', async () => {
  const reason = await b.evaluate(async () => {
    const s = window.session.connect('telnet');
    return s.closed.then(() => 'closed cleanly', (e) => e.reason);
  });
  if (reason !== 0x48) throw new Error(`got ${reason}`);
});

await check('no page raised an uncaught error', async () => {
  if (pageErrors.length) throw new Error(pageErrors.join('\n'));
});

await browser.close();
console.log(failed ? `\n${failed} check(s) failed` : '\nall checks passed');
process.exit(failed ? 1 : 0);
