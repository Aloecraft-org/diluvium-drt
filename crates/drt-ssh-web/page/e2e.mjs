// The end-to-end check for SSH in a browser (doc/Plan-0.8.0.md §2.2): one
// stock OpenSSH sshd, one relay (`drt start`), one parked device (`drt
// tunnel --park`), and three kinds of caller on the one label -- stock
// `ssh` through `ProxyCommand`, and dist/ssh.html in Chromium, twice.
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
// - Configurable: DRT and SSHD (env), PORTS, LABEL, WAIT_MS.
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
const PORTS = { sshd: 18222, relay: 18443, page: 18480 };
const LABEL = 'box';
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

// The page is served over http so it has an origin, and IndexedDB, as it
// would anywhere it is embedded.
const html = fs.readFileSync(path.join(here, 'dist/ssh.html'));
http.createServer((_, res) => res.writeHead(200, { 'content-type': 'text/html' }).end(html)).listen(PORTS.page, '127.0.0.1');
const pageUrl = `http://127.0.0.1:${PORTS.page}/ssh.html`;

function nativeSsh(command) {
  return new Promise((ok) => {
    const c = spawn('ssh', ['-F', '/dev/null', '-i', client, '-o', 'IdentitiesOnly=yes', '-o', 'BatchMode=yes',
      '-o', `UserKnownHostsFile=${tmp}/known_hosts`, '-o', 'StrictHostKeyChecking=accept-new',
      '-o', `ProxyCommand=${DRT} tunnel ${claim}`, `${USER}@${LABEL}`, command]);
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

await check('no page raised an uncaught error', async () => {
  if (pageErrors.length) throw new Error(pageErrors.join('\n'));
});

await browser.close();
cleanup();
console.log(failed ? `\n${failed} check(s) failed` : '\nall checks passed');
process.exit(failed ? 1 : 0);
