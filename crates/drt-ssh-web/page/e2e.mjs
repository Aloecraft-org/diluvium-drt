// The end-to-end check for SSH in a browser (doc/Plan-0.8.0.md §2.2): one
// stock OpenSSH sshd, one relay (`drt start`), one parked device (`drt
// tunnel --park`), and three kinds of caller on the one label -- stock
// `ssh` through `ProxyCommand`, and dist/ssh.html in Chromium, twice.
//
// Then the same sshd over WebRTC (doc/ssh-transport-matrix.md, row 5): a
// browser access host (`drt start` with the `webrtc` block, its scope the
// sshd), signaling through the M0 mock (crates/drt-rtc/browser-check), and
// the page's module running SSH over a stream from the shipped client
// library, drt_browser_access.js. And once more in direct mode
// (doc/BrowserAccess.md §3.4): a second host with `direct` on, whose record
// is all the page is given -- no room, no signaling. And through
// examples/29-browser-access's program, unchanged: the host signalling for
// itself over the caller's request of doc/DRT-Signalling.md, as that
// example's README shows a page.
//
//   script/drt-ssh-page.sh && cargo build -p drt --features full
//   cd crates/drt-ssh-web/page && npm test
//
// Exits 0 only when every check passes. Key authentication throughout, so
// the check needs no account's password.
//
// sshd runs as root, through `sudo -n` when this does not: an unprivileged
// sshd cannot give a pty to the tty group or write login records, and ends
// every pty session it opens -- which is every session the page opens.
// UsePAM, because without it a root sshd refuses a locked account (`!` in
// shadow), which is what a fresh CI user's is.
//
// ## surface block
//
// - Entry point: `node e2e.mjs`.
// - Configurable: DRT and SSHD (env), PORTS, LABEL, WAIT_MS, ROOM.
// - Fan-out: the checks at the bottom, one `check(name, fn)` each; the
//   last one fails if any page raised an uncaught error along the way.

import { spawn, spawnSync } from 'node:child_process';
import { createInterface } from 'node:readline';
import { fileURLToPath } from 'node:url';
import crypto from 'node:crypto';
import fs from 'node:fs';
import http from 'node:http';
import os from 'node:os';
import path from 'node:path';

const here = path.dirname(fileURLToPath(import.meta.url));
const DRT = process.env.DRT ?? path.resolve(here, '../../../target/debug/drt');
const SSHD = process.env.SSHD ?? '/usr/sbin/sshd';
const PORTS = { sshd: 18222, relay: 18443, page: 18480, mock: 18787, direct: 18790, post: 18792, postRtc: 18793 };
const LABEL = 'box';
const ROOM = 'ssh';
const WAIT_MS = 20000;
const USER = os.userInfo().username;

const { chromium } = await import(process.env.PLAYWRIGHT ? path.join(process.env.PLAYWRIGHT, 'index.mjs') : 'playwright');
const tmp = fs.mkdtempSync(path.join(os.tmpdir(), 'drt-ssh-e2e-'));
const children = [];
const cleanup = () => children.forEach((c) => c.kill());
process.on('exit', cleanup);

function run(tag, cmd, args) {
  const child = spawn(cmd, args, { stdio: ['ignore', 'pipe', 'pipe'] });
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
    console.log(`FAILED   ${name}\n         ${e.message.split('\n').join('\n         ')}`);
  }
}

// depth: sshd, the relay, the device

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

const parkKey = crypto.randomBytes(24).toString('hex');
const callerKey = crypto.randomBytes(24).toString('hex');
fs.writeFileSync(path.join(tmp, 'relay.json'), JSON.stringify({
  entry: 'stdlib:relay',
  relay: { bind: `127.0.0.1:${PORTS.relay}`, labels: { [LABEL]: { park_key: parkKey, caller_key: callerKey } } },
}));
const relay = run('relay', DRT, ['--config', path.join(tmp, 'relay.json'), 'start']);
await until('the relay', () => relay.lines.some((l) => /listening|relay/i.test(l)));
const device = run('device', DRT, ['tunnel', '--park', `ws://127.0.0.1:${PORTS.relay}/park/${LABEL}?k=${parkKey}`,
  '--to', `127.0.0.1:${PORTS.sshd}`]);
await until('the device to park', () => device.lines.some((l) => l.includes('parked')));
const claim = `ws://127.0.0.1:${PORTS.relay}/s/${LABEL}?k=${callerKey}`;

// depth: the browser access host, its room, and the client library

const browserCheck = path.resolve(here, '../../drt-rtc/browser-check');
const mock = run('mock', 'node', [path.join(browserCheck, 'mock.mjs'), String(PORTS.mock)]);
await until('the mock', () => mock.lines.some((l) => l.includes('listening')));
const signal = `http://127.0.0.1:${PORTS.mock}`;
fs.writeFileSync(path.join(tmp, 'host.json'), JSON.stringify({
  program: { path: path.join(browserCheck, 'host.dlua') },
  caps: [{ capability: 'host:rest/*' }, { capability: 'host:time/monotonic' }],
  connectors: { time: {}, rest: { scope: { allow: [{ origin: signal }], allow_private: true } } },
  args: { signal, room: ROOM },
  // `hello_scope`: this host names no service, so the page reads its scope.
  webrtc: { identity_file: path.join(tmp, 'host-identity.json'), service: 'SSH', scope: [`ssh://127.0.0.1:${PORTS.sshd}`], hello_scope: true },
}));
const rtcHost = run('rtc-host', DRT, ['--config', path.join(tmp, 'host.json'), 'start']);
await until('the browser access host in the room', () => rtcHost.lines.some((l) => l.startsWith('signal: joined')));

// Direct mode's host has no signaling to do, so its program only prints
// what the host reports: the record, which is how the page is given it.
fs.writeFileSync(path.join(tmp, 'direct.dlua'), [
  "local reports = queue.declare('webrtc', {capacity = 64})",
  "queue.declare('webrtc_cmd', {capacity = 8, exported = true})",
  'while true do',
  '  local _, m = queue.wait({reports})',
  "  if m.event == 'webrtc_record' then print('record ' .. m.rtc)",
  "  elseif m.event == 'webrtc_session' then print('session ' .. m.peer .. ' ' .. m.state)",
  "  elseif m.event == 'webrtc_stream' then print('stream ' .. m.host .. ':' .. m.port .. ' ' .. m.state) end",
  'end',
].join('\n'));
fs.writeFileSync(path.join(tmp, 'direct.json'), JSON.stringify({
  program: { path: path.join(tmp, 'direct.dlua') },
  webrtc: { bind: `127.0.0.1:${PORTS.direct}`, identity_file: path.join(tmp, 'direct-identity.json'),
            direct: true, scope: [`ssh://127.0.0.1:${PORTS.sshd}`],
            services: { ssh: `ssh://127.0.0.1:${PORTS.sshd}` } },
}));
// The example's program under a config of the harness's: the same
// listener and block, on the harness's ports, with the test sshd in scope.
fs.writeFileSync(path.join(tmp, 'post.json'), JSON.stringify({
  program: { path: path.resolve(here, '../../../examples/29-browser-access/app.dlua') },
  listeners: [{ scheme: 'http', address: `127.0.0.1:${PORTS.post}`, queue: 'http_in', reply_queue: 'http_out',
                resp_headers: ['access-control-allow-origin', 'location'] }],
  webrtc: { bind: `127.0.0.1:${PORTS.postRtc}`, identity_file: path.join(tmp, 'post-identity.json'),
            scope: [`ssh://127.0.0.1:${PORTS.sshd}`], hello_scope: true },
}));
const postHost = run('post-host', DRT, ['--config', path.join(tmp, 'post.json'), 'start']);
await until('the example\'s listener', () => postHost.lines.some((l) => l.includes(`listening on 127.0.0.1:${PORTS.post}`)));

const directHost = run('direct-host', DRT, ['--config', path.join(tmp, 'direct.json'), 'start']);
const directRecord = (await until('the direct host\'s record',
  () => directHost.lines.find((l) => l.startsWith('record ')))).slice('record '.length);

// The page is served over http so it has an origin, and IndexedDB, as it
// would anywhere it is embedded.
const html = fs.readFileSync(path.join(here, 'dist/ssh.html'));
// /rtc.html is the same build with its CSP taken out, and nothing else
// changed. The shipped page admits only WebSocket connections, and these
// checks signal over fetch and import the client library, which are the
// harness's doing rather than the page's.
const harness = html.toString().replace(/<meta http-equiv="Content-Security-Policy"[^>]*>/, '');
if (harness === html.toString()) throw new Error('dist/ssh.html has no CSP meta for the harness to take out');
const library = fs.readFileSync(path.resolve(here, '../../drt-rtc/client/drt_browser_access.js'));
http.createServer((req, res) => {
  if (req.url === '/rtc.html') res.writeHead(200, { 'content-type': 'text/html' }).end(harness);
  else if (req.url === '/drt_browser_access.js') res.writeHead(200, { 'content-type': 'text/javascript' }).end(library);
  else res.writeHead(200, { 'content-type': 'text/html' }).end(html);
}).listen(PORTS.page, '127.0.0.1');
const pageUrl = `http://127.0.0.1:${PORTS.page}/ssh.html`;
const rtcUrl = `http://127.0.0.1:${PORTS.page}/rtc.html`;

function nativeSsh(command, proxy = `${DRT} tunnel ${claim}`) {
  return new Promise((ok) => {
    const c = spawn('ssh', ['-F', '/dev/null', '-i', client, '-o', 'IdentitiesOnly=yes', '-o', 'BatchMode=yes',
      '-o', `UserKnownHostsFile=${tmp}/known_hosts`, '-o', 'StrictHostKeyChecking=accept-new',
      '-o', `ProxyCommand=${proxy}`, `${USER}@${LABEL}`, command]);
    let out = '';
    let err = '';
    c.stdout.on('data', (d) => (out += d));
    c.stderr.on('data', (d) => (err += d));
    c.on('close', (code) => ok({ code, out, err }));
  });
}

// depth: the page

const browser = await chromium.launch();
const context = await browser.newContext();
const pageErrors = [];
context.on('page', (page) => page.on('pageerror', (e) => pageErrors.push(e.message)));

async function screen(page) {
  return page.evaluate(() => {
    const b = window.drtSsh?.term?.buffer.active;
    if (!b) return '';
    const lines = [];
    for (let i = 0; i < b.length; i++) lines.push(b.getLine(i).translateToString(true));
    return lines.join('\n');
  });
}

async function openSession(fragment, { onDialog } = {}) {
  const page = await context.newPage();
  if (process.env.VERBOSE) {
    page.on('console', (m) => console.log(`page: ${m.text()}`));
    page.on('pageerror', (e) => console.log(`page error: ${e.message}`));
  }
  page.on('dialog', (d) => (onDialog ? onDialog(d) : d.dismiss()));
  await page.goto(`${pageUrl}#${fragment}`);
  return page;
}

async function typeAndWait(page, command, expect) {
  await page.keyboard.type(`${command}\n`);
  return until(`"${expect}" on the page`, async () => (await screen(page)).includes(expect));
}

const link = (extra = {}) => new URLSearchParams({ url: claim, user: USER, ...extra }).toString();

// depth: the checks

await check('stock ssh through ProxyCommand reaches sshd via the relay', async () => {
  const r = await nativeSsh('echo native-ok');
  if (r.code !== 0 || !r.out.includes('native-ok')) throw new Error(`exit ${r.code}\n${r.err}`);
});

let a;
await check('the page makes a key, and with the host key pinned signs in and runs a command', async () => {
  const setup = await context.newPage();
  if (process.env.VERBOSE) setup.on('pageerror', (e) => console.log(`page error: ${e.message}`));
  await setup.goto(pageUrl);
  await setup.click('#makekey');
  const pub = await until('the public key', async () => setup.inputValue('#pub'));
  fs.appendFileSync(authorized, `${pub}\n`);
  await setup.close();
  a = await openSession(link({ hostkey: fingerprint }));
  await until('a shell', () => a.evaluate(() => !!window.drtSsh?.term)).catch(async (e) => {
    throw new Error(`${e.message}; the page says: ${await a.textContent('#status')}`);
  });
  await typeAndWait(a, 'echo page-ok-$((6*7))', 'page-ok-42');
});

await check('a resize reaches the pty', async () => {
  await a.evaluate(() => window.drtSsh.term.resize(100, 30));
  await typeAndWait(a, 'stty size', '30 100');
});

await check('a second page and stock ssh claim at once, beside the open session', async () => {
  const [b, native] = await Promise.all([
    openSession(link({ hostkey: fingerprint })).then(async (b) => {
      await until('a second shell', () => b.evaluate(() => !!window.drtSsh?.term));
      await typeAndWait(b, 'echo second-ok', 'second-ok');
      return b;
    }),
    nativeSsh('echo native-again'),
  ]);
  if (native.code !== 0 || !native.out.includes('native-again')) throw new Error(`native: exit ${native.code}\n${native.err}`);
  await b.close();
  await typeAndWait(a, 'echo first-still-ok', 'first-still-ok');
});

await check('a wrong pin is refused before anything authenticates', async () => {
  const wrong = 'SHA256:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA';
  const p = await openSession(link({ hostkey: wrong }));
  const status = await until('the refusal', async () => {
    const t = await p.textContent('#status');
    return t.includes('pinned') ? t : '';
  });
  if (!status.includes(fingerprint)) throw new Error(`the refusal does not name the real key: ${status}`);
  const logged = sshd.lines.filter((l) => /Accepted publickey/.test(l)).length;
  await p.close();
  const after = sshd.lines.filter((l) => /Accepted publickey/.test(l)).length;
  if (after !== logged) throw new Error('something authenticated during a refused connect');
});

await check('with no pin, the host key is shown and trusted on confirmation', async () => {
  let asked = '';
  const p = await openSession(link(), { onDialog: (d) => { asked = d.message(); d.accept(); } });
  await until('a shell', () => p.evaluate(() => !!window.drtSsh?.term));
  if (!asked.includes(fingerprint)) throw new Error(`the prompt did not show the key: ${asked}`);
  await typeAndWait(p, 'echo tofu-ok', 'tofu-ok');
  await p.close();
});

await check('exit ends the session with its status', async () => {
  await a.keyboard.type('exit 3\n');
  const ended = await until('the end', () => a.evaluate(() => window.drtSsh.ended));
  if (ended !== 3) throw new Error(`ended with ${ended}`);
});

// depth: SSH over browser access, driven through the page's module

// One session per call: offer, the mock's room, the host's record, a
// browser access stream to `port`, and `Ssh.connect` over it. Then, when
// `command` is given, sign in with the test's key and run it in a shell.
async function overRtc({ port = PORTS.sshd, pin = fingerprint, command, direct, post } = {}) {
  const page = await context.newPage();
  await page.goto(rtcUrl);
  await until('the module', () => page.evaluate(() =>
    !document.getElementById('nokey').hidden || !document.getElementById('haskey').hidden));
  return page.evaluate(async ({ base, port, pin, user, key, command, waitMs, direct, post }) => {
    const lib = await import('/drt_browser_access.js');
    const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
    const session = direct ? await lib.direct(direct, { timeoutMs: waitMs })
      : post ? await posted() : await signaled();
    let ssh;
    try {
      ssh = await wasm_bindgen.Ssh.connect(session.connect('127.0.0.1', port), pin);
    } catch (e) {
      session.close();
      return { refused: e.message, code: e.code };
    }
    const result = { hostKey: ssh.hostKey };
    if (command) {
      result.signedIn = await ssh.authKey(user, key);
      let out = '';
      let end;
      const ended = new Promise((ok) => (end = ok));
      await ssh.shell(80, 24, (b) => (out += new TextDecoder().decode(b)), end);
      ssh.write(new TextEncoder().encode(`${command}\n`));
      result.status = await Promise.race([ended, sleep(waitMs).then(() => 'timed out')]);
      result.out = out;
    }
    ssh.close();
    session.close();
    return result;

    // examples/29-browser-access's README, as a page would run it.
    async function posted() {
      const pending = await lib.offer();
      const reply = await fetch(post, { method: 'POST', body: pending.recordText });
      return pending.accept(await reply.text(), { timeoutMs: waitMs });
    }

    async function signaled() {
    const pending = await lib.offer({ gatherTimeoutMs: 2000 });
    const joined = await (await fetch(`${base}/join`, { method: 'POST', body: '{"credential":{}}' })).json();
    const headers = { authorization: `Bearer ${joined.session_token}`, 'content-type': 'application/json' };
    await fetch(`${base}/presence`, { method: 'POST', headers, body: JSON.stringify({ kind: 'browser', rtc: pending.recordText }) });
    let hostRecord;
    for (const t0 = Date.now(); !hostRecord && Date.now() - t0 < waitMs; await sleep(200)) {
      const { peers } = await (await fetch(`${base}/presence`, { headers })).json();
      hostRecord = peers.find((p) => p.kind === 'host')?.rtc;
    }
    if (!hostRecord) throw new Error('no host in the room');
    return pending.accept(hostRecord, { timeoutMs: waitMs });
    }
  }, { base: `${signal}/v1/rooms/${ROOM}`, port, pin, user: USER, key: fs.readFileSync(client, 'utf8'),
       command, waitMs: WAIT_MS, direct, post });
}

await check('over browser access, the page signs in to sshd and runs a command', async () => {
  const logged = sshd.lines.filter((l) => /Accepted publickey/.test(l)).length;
  const r = await overRtc({ command: 'echo rtc-ok-$((6*7)); exit 5' });
  if (r.refused) throw new Error(`refused: ${r.refused} (${r.code})`);
  if (r.hostKey !== fingerprint) throw new Error(`host key ${r.hostKey}, expected ${fingerprint}`);
  if (!r.signedIn) throw new Error('the key was not accepted');
  if (!r.out.includes('rtc-ok-42')) throw new Error(`the shell printed: ${JSON.stringify(r.out)}`);
  if (r.status !== 5) throw new Error(`ended with ${r.status}`);
  if (!sshd.lines.slice(logged).some((l) => /Accepted publickey/.test(l))) throw new Error('sshd logged no sign-in');
  if (!rtcHost.lines.some((l) => l.includes(`127.0.0.1:${PORTS.sshd}`))) throw new Error('the host reported no stream to sshd');
});

await check('over browser access, a wrong pin is refused before anything authenticates', async () => {
  const logged = sshd.lines.filter((l) => /Accepted publickey/.test(l)).length;
  const r = await overRtc({ pin: 'SHA256:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA' });
  if (r.code !== 'hostkey' || !r.refused.includes(fingerprint)) throw new Error(`got ${JSON.stringify(r)}`);
  if (sshd.lines.filter((l) => /Accepted publickey/.test(l)).length !== logged) throw new Error('something authenticated');
});

await check('over browser access, a port out of scope fails the connect with the host\'s reason', async () => {
  const r = await overRtc({ port: PORTS.sshd + 1 });
  if (r.code !== 0x48) throw new Error(`got ${JSON.stringify(r)}`);
});

await check('in direct mode, the page signs in with only the host\'s record', async () => {
  const r = await overRtc({ direct: directRecord, command: 'echo direct-ok-$((6*7)); exit 6' });
  if (r.refused) throw new Error(`refused: ${r.refused} (${r.code})`);
  if (r.hostKey !== fingerprint || !r.signedIn) throw new Error(`got ${JSON.stringify(r)}`);
  if (!r.out.includes('direct-ok-42')) throw new Error(`the shell printed: ${JSON.stringify(r.out)}`);
  if (r.status !== 6) throw new Error(`ended with ${r.status}`);
  if (!directHost.lines.some((l) => /^session direct:\S+ connected$/.test(l))) throw new Error('the host reported no direct session');
});

await check('examples/29-browser-access\'s program signals for its own host, and the page signs in', async () => {
  const r = await overRtc({ post: `http://127.0.0.1:${PORTS.post}/v1/box/calls`, command: 'echo posted-$((6*7)); exit 8' });
  if (r.refused) throw new Error(`refused: ${r.refused} (${r.code})`);
  if (r.hostKey !== fingerprint || !r.signedIn) throw new Error(`got ${JSON.stringify(r)}`);
  if (!r.out.includes('posted-42')) throw new Error(`the shell printed: ${JSON.stringify(r.out)}`);
  if (r.status !== 8) throw new Error(`ended with ${r.status}`);
  if (!postHost.lines.some((l) => /^browser-1\s+connected/.test(l))) throw new Error('the program reported no session');
});

await check('the shipped page, CSP and all, follows a direct-mode link to a shell', async () => {
  const link = new URLSearchParams({ rtc: directRecord, user: USER, hostkey: fingerprint }).toString();
  const p = await openSession(link);
  await until('a shell', () => p.evaluate(() => !!window.drtSsh?.term)).catch(async (e) => {
    throw new Error(`${e.message}; the page says: ${await p.textContent('#status')}`);
  });
  await typeAndWait(p, 'echo page-direct-$((6*7))', 'page-direct-42');
  await p.keyboard.type('exit 7\n');
  const ended = await until('the end', () => p.evaluate(() => window.drtSsh.ended));
  if (ended !== 7) throw new Error(`ended with ${ended}`);
  await p.close();
});

await check('the shipped page follows a call= link through examples/29 to sshd', async () => {
  const call = `http://127.0.0.1:${PORTS.post}/v1/box/calls`;
  const p = await openSession(new URLSearchParams({ call, user: USER, hostkey: fingerprint }).toString());
  await until('a shell', () => p.evaluate(() => !!window.drtSsh?.term)).catch(async (e) => {
    throw new Error(`${e.message}; the page says: ${await p.textContent('#status')}`);
  });
  await typeAndWait(p, 'echo page-called-$((6*7))', 'page-called-42');
  // A host serves no named service, so the page read its scope.
  const status = await p.textContent('#status');
  if (!status.includes(`call:127.0.0.1:${PORTS.post}/v1/box/calls/127.0.0.1:${PORTS.sshd}`)) {
    throw new Error(`the status says ${JSON.stringify(status)}`);
  }
  await p.keyboard.type('exit 6\n');
  const ended = await until('the end', () => p.evaluate(() => window.drtSsh.ended));
  if (ended !== 6) throw new Error(`ended with ${ended}`);
  await p.close();
});

await check('a call the server refuses is reported by what its status means', async () => {
  const call = `http://127.0.0.1:${PORTS.post}/v1/nobody/calls`;
  const p = await openSession(new URLSearchParams({ call, user: USER, hostkey: fingerprint }).toString());
  const said = await until('a refusal', async () => {
    const s = await p.textContent('#status');
    return s.includes('answered') ? s : false;
  });
  if (!said.includes('answered 404: no such name on this server')) throw new Error(`the page says ${JSON.stringify(said)}`);
  await p.close();
});

// depth: the native caller, `drt tunnel rtc:` (rows 3 and 6 of the matrix)

await check('stock ssh through ProxyCommand="drt tunnel rtc:<record>" reaches the named service ssh', async () => {
  const file = path.join(tmp, 'direct.record.json');
  fs.writeFileSync(file, directRecord);
  const r = await nativeSsh('echo rtc-direct-ok', `${DRT} tunnel rtc:${file}`);
  if (r.code !== 0 || !r.out.includes('rtc-direct-ok')) throw new Error(`exit ${r.code}\n${r.err}`);
  if (!directHost.lines.some((l) => /^session direct:\S+ connected$/.test(l))) throw new Error('the host saw no direct session');
});

await check('stock ssh through "drt tunnel rtc:http://…/v1/box/calls" signals through examples/29 and reaches sshd', async () => {
  const r = await nativeSsh('echo rtc-posted-ok',
    `${DRT} tunnel rtc:http://127.0.0.1:${PORTS.post}/v1/box/calls --to 127.0.0.1:${PORTS.sshd}`);
  if (r.code !== 0 || !r.out.includes('rtc-posted-ok')) throw new Error(`exit ${r.code}\n${r.err}`);
});

await check('drt tunnel rtc: names why a stream was refused', async () => {
  const r = await nativeSsh('true', `${DRT} tunnel rtc:${path.join(tmp, 'direct.record.json')} --to telnet`);
  if (r.code === 0 || !r.err.includes('blocked (0x48)')) throw new Error(`exit ${r.code}\n${r.err}`);
});

await check('no page raised an uncaught error', async () => {
  if (pageErrors.length) throw new Error(pageErrors.join('\n'));
});

await browser.close();
cleanup();
console.log(failed ? `\n${failed} check(s) failed` : '\nall checks passed');
process.exit(failed ? 1 : 0);
