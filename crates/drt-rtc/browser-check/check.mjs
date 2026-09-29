// The browser half of the M0 check (doc/Plan-0.8.0.md §3.3), and the live
// test of the shipped client library (crates/drt-rtc/client/): Chromium
// loads drt_browser_access.js as the release serves it and does through it
// exactly what doc/BrowserAccess.md §3 says a client does -- negotiated
// channels, an ordinary offer, a record published after gathering, and an
// answer built locally from the host's record -- against a real host.
// Several sessions at once, all through one host socket and one host ufrag.
//
// Two ways to run it, and both exit 0 only when every session echoed, moved
// TRANSFER bytes intact through a Wisp stream, and saw an out-of-scope
// connect refused with 0x48:
//
//   cargo build -p drt-rtc --example browser_check
//   PLAYWRIGHT=$(npm root -g)/playwright node check.mjs
//     The host is examples/browser_check.rs; this script plays signaling
//     over its stdin and stdout.
//
//   cargo build -p drt --features full
//   PLAYWRIGHT=$(npm root -g)/playwright node check.mjs --drt
//     The whole M0 rehearsal: mock.mjs is the room, `drt start` runs
//     host.json (the `webrtc` block, host.dlua doing the signaling over
//     `rest`), and the page joins the room and signals with fetch, CORS
//     and all, as the real client does against Discofetch.
//
// ## surface block
//
// - Entry point: this script; `--drt` picks the second mode, and any other
//   argument is passed to the example host as its bind address.
// - Configurable: SESSIONS (env), MOCK_PORT and ECHO_PORT, which host.json
//   names too; GATHER_CAP_MS, how long a page waits for ICE gathering
//   before it publishes (doc/BrowserAccess.md §3.1); WAIT_MS, the limit on
//   any one wait; TRANSFER (env), the bytes each session sends through the
//   echo and expects back, default 1 MiB.
// - Fan-out: `run` starts the host side in either mode; window.start,
//   window.signal and window.finish are the client's three steps.

import { spawn } from 'node:child_process';
import { createInterface } from 'node:readline';
import { fileURLToPath } from 'node:url';
import net from 'node:net';
import { readFileSync } from 'node:fs';
import path from 'node:path';

const { chromium } = await import(process.env.PLAYWRIGHT ? path.join(process.env.PLAYWRIGHT, 'index.mjs') : 'playwright');
const here = path.dirname(fileURLToPath(import.meta.url));
const target = path.resolve(here, '../../../target/debug');
const SESSIONS = Number(process.env.SESSIONS ?? 2);
const MOCK_PORT = 18787;
const ECHO_PORT = 18788;
const GATHER_CAP_MS = 2000;
const WAIT_MS = 15000;
const TRANSFER = Number(process.env.TRANSFER ?? 1 << 20);
const DRT = process.argv.includes('--drt');
const ROOM = `http://127.0.0.1:${MOCK_PORT}/v1/rooms/m0`;
const children = [];
const cleanup = () => children.forEach((c) => c.kill());
process.on('exit', cleanup);

function run(cmd, args, opts, tag) {
  const child = spawn(cmd, args, { stdio: ['pipe', 'pipe', 'pipe'], ...opts });
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

// depth: the host, either way

let echoPort, hostRecord, host;
if (DRT) {
  echoPort = ECHO_PORT;
  net.createServer((s) => s.pipe(s)).listen(echoPort, '127.0.0.1');
  const mock = run('node', [path.join(here, 'mock.mjs'), String(MOCK_PORT)], {}, 'mock');
  await until('the mock', () => mock.lines.some((l) => l.includes('listening')));
  host = run(path.join(target, 'drt'), ['--config', 'host.json', 'start'], { cwd: here }, 'drt');
  await until('the host in the room', () => host.lines.some((l) => l.startsWith('signal: joined')));
} else {
  host = run(process.env.HOST_BIN ?? path.join(target, 'examples/browser_check'), process.argv.slice(2), {}, 'host');
  const json = () => host.lines.filter((l) => l.startsWith('{')).map((l) => JSON.parse(l));
  echoPort = (await until('the echo port', () => json().find((e) => e.event === 'echo'))).port;
  hostRecord = (await until('the host record', () => json().find((e) => e.event === 'record'))).rtc;
}

// depth: the page, which is the client

const browser = await chromium.launch();
const page = await browser.newPage();
page.on('console', (m) => console.log('page:', m.text()));
// In --drt mode the page runs on the mock's origin; see mock.mjs.
await page.goto(DRT ? `http://127.0.0.1:${MOCK_PORT}/` : 'about:blank');
// The client is the shipped library, loaded as the release serves it: one
// ES module, imported as-is.
const library = readFileSync(path.resolve(here, '../client/drt_browser_access.js'));
await page.evaluate(async ([src, GATHER_CAP_MS, WAIT_MS]) => {
  const lib = await import(`data:text/javascript;base64,${src}`);
  const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
  window.sessions = [];

  window.start = async () => {
    const pending = await lib.offer({ gatherTimeoutMs: GATHER_CAP_MS });
    window.sessions.push({ pending });
    return { index: window.sessions.length - 1, record: pending.recordText };
  };

  // §7.2: join the mock's room, publish, and wait for the host's record.
  window.signal = async (i, base) => {
    const s = window.sessions[i];
    const joined = await (await fetch(`${base}/join`, { method: 'POST', body: '{"credential":{}}' })).json();
    const headers = { authorization: `Bearer ${joined.session_token}`, 'content-type': 'application/json' };
    await fetch(`${base}/presence`, { method: 'POST', headers, body: JSON.stringify({ kind: 'browser', rtc: s.pending.recordText }) });
    for (const t0 = Date.now(); Date.now() - t0 < WAIT_MS; await sleep(200)) {
      const { peers } = await (await fetch(`${base}/presence`, { headers })).json();
      const h = peers.find((p) => p.kind === 'host');
      if (h) return h.rtc;
    }
    throw new Error('no host in the room');
  };

  // Every session: hello, an echo, a transfer big enough to spend Wisp
  // credit many times over and to be split at 16379 bytes, and a refusal.
  window.finish = async (i, hostRtc, port, bytes) => {
    const s = window.sessions[i];
    // Object or text, as the host's record may arrive either way (§2).
    const session = await s.pending.accept(i % 2 ? JSON.parse(hostRtc) : hostRtc, { timeoutMs: WAIT_MS });
    const stream = session.connect('127.0.0.1', port);
    const writer = stream.writable.getWriter();
    const reader = stream.readable.getReader();
    const ping = new TextEncoder().encode(`ping from session ${i}`);
    await writer.write(ping);
    const big = new Uint8Array(bytes);
    for (let k = 0; k < big.length; k++) big[k] = (k * 31 + i) & 0xff;
    const sent = writer.write(big);
    const want = ping.length + big.length;
    const got = new Uint8Array(want);
    let n = 0;
    while (n < want) {
      const { value, done } = await reader.read();
      if (done) throw new Error(`stream ended after ${n} of ${want} bytes`);
      got.set(value, n);
      n += value.length;
    }
    await sent;
    const echoed = new TextDecoder().decode(got.subarray(0, ping.length));
    const intact = got.subarray(ping.length).every((b, k) => b === big[k]);
    await writer.close();
    await stream.closed;
    // Out of scope: refused by the host with 0x48 before it connects.
    const refused = session.connect('127.0.0.1', port + 1);
    const reason = await refused.closed.then(() => 'closed cleanly', (e) => e.reason);
    session.close();
    return { hello: session.hello, echoed, intact, bytes: n - ping.length, reason };
  };
}, [library.toString('base64'), GATHER_CAP_MS, WAIT_MS]);

// depth: run the sessions and judge them

const started = [];
for (let i = 0; i < SESSIONS; i++) started.push(await page.evaluate(() => window.start()));
const results = await Promise.allSettled(
  started.map(async (s) => {
    console.log(`browser record ${s.index} (${s.record.length} bytes): ${s.record}`);
    let rtc = hostRecord;
    if (DRT) rtc = await page.evaluate(([i, base]) => window.signal(i, base), [s.index, ROOM]);
    else host.child.stdin.write(s.record + '\n');
    return page.evaluate(([i, h, port, n]) => window.finish(i, h, port, n), [s.index, rtc, echoPort, TRANSFER]);
  }),
);
let failed = false;
results.forEach((r, i) => {
  const v = r.value;
  if (r.status === 'fulfilled' && v.echoed === `ping from session ${i}` && v.hello.t === 'hello'
      && v.intact && v.bytes === TRANSFER && v.reason === 0x48) {
    console.log(`session ${i}: ok, echoed "${v.echoed}" and ${v.bytes} bytes intact, out of scope refused 0x48, hello ${JSON.stringify(v.hello)}`);
  } else {
    failed = true;
    console.log(`session ${i}: FAILED`, r.status === 'rejected' ? r.reason.message : JSON.stringify(r.value));
  }
});
await browser.close();
cleanup();
process.exit(failed ? 1 : 0);
