// The browser half of the M0 check (doc/Plan-0.8.0.md §3.3): Chromium does
// exactly what doc/BrowserAccess.md §3 says a client does -- negotiated
// channels, an ordinary offer, a record published after gathering, and an
// answer built locally from the host's record -- against a real host. Several
// sessions at once, all through one host socket and one host ufrag.
//
// Two ways to run it, and both exit 0 only when every session echoed through
// a Wisp stream:
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
//   any one wait.
// - Fan-out: `run` starts the host side in either mode; window.start,
//   window.signal and window.finish are the client's three steps.

import { spawn } from 'node:child_process';
import { createInterface } from 'node:readline';
import { fileURLToPath } from 'node:url';
import net from 'node:net';
import path from 'node:path';

const { chromium } = await import(process.env.PLAYWRIGHT ? path.join(process.env.PLAYWRIGHT, 'index.mjs') : 'playwright');
const here = path.dirname(fileURLToPath(import.meta.url));
const target = path.resolve(here, '../../../target/debug');
const SESSIONS = Number(process.env.SESSIONS ?? 2);
const MOCK_PORT = 18787;
const ECHO_PORT = 18788;
const GATHER_CAP_MS = 2000;
const WAIT_MS = 15000;
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
await page.evaluate(([GATHER_CAP_MS, WAIT_MS]) => {
  const b64 = (hex) => btoa(String.fromCharCode(...hex.split(':').map((h) => parseInt(h, 16))));
  const hex = (b) => [...atob(b)].map((c) => c.charCodeAt(0).toString(16).toUpperCase().padStart(2, '0')).join(':');
  const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

  // §2: the record, from our own local description.
  window.recordFrom = (sdp) => {
    const get = (k) => sdp.match(new RegExp(`^a=${k}:(.*)$`, 'm'))[1].trim();
    const c = [...sdp.matchAll(/^a=(candidate:.*)$/gm)]
      .map((m) => m[1].trim())
      .filter((l) => / udp /i.test(l) && !/\.local /.test(l) && !/ typ relay/.test(l))
      .map((l) => l.replace(/^(.* typ \S+(?: raddr \S+ rport \d+)?).*$/, '$1'))
      .slice(0, 8);
    return JSON.stringify({ v: 1, u: get('ice-ufrag'), p: get('ice-pwd'), f: b64(get('fingerprint').split(' ')[1]), c });
  };

  // §3.2: the answer, from the host's record and the offer's mid.
  window.answerFrom = (rtc, mid) => {
    const r = JSON.parse(rtc);
    return [
      'v=0', 'o=- 0 2 IN IP4 127.0.0.1', 's=-', 't=0 0', `a=group:BUNDLE ${mid}`,
      'm=application 9 UDP/DTLS/SCTP webrtc-datachannel', 'c=IN IP4 0.0.0.0', `a=mid:${mid}`,
      `a=ice-ufrag:${r.u}`, `a=ice-pwd:${r.p}`, `a=fingerprint:sha-256 ${hex(r.f)}`,
      'a=setup:passive', 'a=sctp-port:5000', 'a=max-message-size:262144',
      ...r.c.map((c) => `a=${c}`), 'a=end-of-candidates', '',
    ].join('\r\n');
  };

  window.sessions = [];
  window.start = async () => {
    const pc = new RTCPeerConnection();
    const s = { pc, inbox: [], hello: null, open: 0 };
    s.control = pc.createDataChannel('control', { negotiated: true, id: 0 });
    s.wisp = pc.createDataChannel('wisp', { negotiated: true, id: 1 });
    s.wisp.binaryType = 'arraybuffer';
    s.control.onmessage = (e) => (s.hello = e.data);
    s.wisp.onmessage = (e) => s.inbox.push(new Uint8Array(e.data));
    s.control.onopen = s.wisp.onopen = () => s.open++;
    await pc.setLocalDescription(await pc.createOffer());
    // §3.1: publish once gathering completes, or at the cap. In this
    // container Chromium's gathering never reaches `complete`.
    await new Promise((r) => {
      if (pc.iceGatheringState === 'complete') return r();
      pc.addEventListener('icegatheringstatechange', () => pc.iceGatheringState === 'complete' && r());
      setTimeout(r, GATHER_CAP_MS);
    });
    s.mid = pc.localDescription.sdp.match(/^a=mid:(.*)$/m)[1].trim();
    s.record = window.recordFrom(pc.localDescription.sdp);
    window.sessions.push(s);
    return { index: window.sessions.length - 1, record: s.record };
  };

  // §7: join the room, publish, and wait for the host's record.
  window.signal = async (i, base) => {
    const s = window.sessions[i];
    const joined = await (await fetch(`${base}/join`, { method: 'POST', body: '{"credential":{}}' })).json();
    const headers = { authorization: `Bearer ${joined.session_token}`, 'content-type': 'application/json' };
    await fetch(`${base}/presence`, { method: 'POST', headers, body: JSON.stringify({ kind: 'browser', rtc: s.record }) });
    for (const t0 = Date.now(); Date.now() - t0 < WAIT_MS; await sleep(200)) {
      const { peers } = await (await fetch(`${base}/presence`, { headers })).json();
      const h = peers.find((p) => p.kind === 'host');
      if (h) return h.rtc;
    }
    throw new Error('no host in the room');
  };

  window.finish = async (i, hostRtc, port) => {
    const s = window.sessions[i];
    await s.pc.setRemoteDescription({ type: 'answer', sdp: window.answerFrom(hostRtc, s.mid) });
    const until = async (what, pred, ms = WAIT_MS) => {
      const t0 = Date.now();
      while (!pred()) {
        if (Date.now() - t0 > ms) throw new Error(`session ${i}: timed out waiting for ${what} (ice ${s.pc.iceConnectionState}, pc ${s.pc.connectionState})`);
        await sleep(20);
      }
    };
    await until('both channels to open', () => s.open === 2);
    await until('hello', () => s.hello !== null);
    await until('CONTINUE on stream 0', () => s.inbox.length > 0);
    const first = s.inbox.shift();
    const le32 = (b, o) => b[o] | (b[o + 1] << 8) | (b[o + 2] << 16) | (b[o + 3] << 24);
    if (first[0] !== 3 || le32(first, 1) !== 0 || le32(first, 5) !== 128) throw new Error('first wisp packet is not CONTINUE(0, 128)');
    // CONNECT stream 1 to the echo server, then DATA.
    const host = new TextEncoder().encode('127.0.0.1');
    const connect = new Uint8Array(8 + host.length);
    connect.set([1, 1, 0, 0, 0, 1, port & 0xff, port >> 8]);
    connect.set(host, 8);
    s.wisp.send(connect);
    const msg = new TextEncoder().encode(`ping from session ${i}`);
    const data = new Uint8Array(5 + msg.length);
    data.set([2, 1, 0, 0, 0]);
    data.set(msg, 5);
    s.wisp.send(data);
    await until('the echo', () => s.inbox.some((p) => p[0] === 2));
    const echoed = new TextDecoder().decode(s.inbox.find((p) => p[0] === 2).slice(5));
    return { hello: JSON.parse(s.hello), echoed };
  };
}, [GATHER_CAP_MS, WAIT_MS]);

// depth: run the sessions and judge them

const started = [];
for (let i = 0; i < SESSIONS; i++) started.push(await page.evaluate(() => window.start()));
const results = await Promise.allSettled(
  started.map(async (s) => {
    console.log(`browser record ${s.index} (${s.record.length} bytes): ${s.record}`);
    let rtc = hostRecord;
    if (DRT) rtc = await page.evaluate(([i, base]) => window.signal(i, base), [s.index, ROOM]);
    else host.child.stdin.write(s.record + '\n');
    return page.evaluate(([i, h, port]) => window.finish(i, h, port), [s.index, rtc, echoPort]);
  }),
);
let failed = false;
results.forEach((r, i) => {
  if (r.status === 'fulfilled' && r.value.echoed === `ping from session ${i}` && r.value.hello.t === 'hello') {
    console.log(`session ${i}: ok, echoed "${r.value.echoed}", hello ${JSON.stringify(r.value.hello)}`);
  } else {
    failed = true;
    console.log(`session ${i}: FAILED`, r.status === 'rejected' ? r.reason.message : JSON.stringify(r.value));
  }
});
await browser.close();
cleanup();
process.exit(failed ? 1 : 0);
