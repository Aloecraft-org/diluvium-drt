#!/usr/bin/env node
// run.mjs: the browser suite. examples/*/meta.json through drt-web, in
// Chromium, diffed against expected.txt (doc/Wasm.md §5, M4).
//
// Also the embedding contract, against a real xterm.js Terminal
// (xterm.html), because that is the object every host actually has.
//
// The page-side twin of examples/run-all.sh, under the same rules: every
// examples/NN-*/ with a meta.json is one example; its files are seeded
// into the page's memory filesystem; its "cmd" runs through the in-page
// shell with stdout and stderr merged; "normalise" is applied to both
// sides; the two are diffed. A skip is named and is never a pass.
//
// Then the checks that are not examples, because what they exercise is a
// page rather than a program: `xterm-embedding`, the contract against a
// real Terminal; `swarm-table`, the instances table driven the way a host
// drives it; `socket-echo`, the byte stream a page owns;
// `ssh-into-the-page` and `ssh-through-a-relay`, a standard `ssh` client
// reaching a shell in the page -- over a bridge this file makes, and then
// over a real relay (`drt start`) with `drt tunnel` as the `ProxyCommand`
// (doc/SshInBrowser.md); and `repl-parity`, repl-script.txt typed at
// drt-term.js against the transcript the native binary produced for the
// same lines (repl-expected.txt).
//
// usage: node run.mjs [--net] [--list] [example ...]
//   --net    also run examples whose meta.json sets "needs_network"
//   --list   name the examples that would run, and exit
//   example  a substring of the directory name: "04", "files"
// env: TIMEOUT  seconds one example may take (default 120; 0 disables)
//      EXAMPLES_DIR  another directory in the examples' layout to run
//                    instead (tests/determinism/), with the page-only
//                    checks after the examples left out
//      DRT_BIN  a native `drt` carrying `relay` and `tunnel`, for the
//               whole-chain check. Found under target/ if unset.
//      DRT_WEB_BUILDINFO  a path: the page's `drt buildinfo` is written there,
//                         which is how a release reads the profile off the
//                         module (release.yml, build-web)
// needs: pkg/ from script/drt-web.sh; `npm ci` for Playwright and
//        xterm.js, and `npx playwright install chromium` for the browser.

import { chromium } from 'playwright';
import fs from 'node:fs';
import http from 'node:http';
import net from 'node:net';
import os from 'node:os';
import path from 'node:path';
import { execFileSync, spawn, spawnSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';

const HERE = path.dirname(fileURLToPath(import.meta.url));
const EXAMPLES = process.env.EXAMPLES_DIR
  ? path.resolve(process.env.EXAMPLES_DIR)
  : path.resolve(HERE, '../../../examples');
// The page-only checks (xterm, the swarm table, REPL parity) are about
// the embedding, not about a directory of programs, so they run with the
// examples and not with any other directory.
const PAGE_CHECKS = !process.env.EXAMPLES_DIR;
const MAXDIFF = 200;
const TIMED_OUT = Symbol('timed out');
const TIMEOUT = Number(process.env.TIMEOUT ?? 120);
const TYPES = {
  '.html': 'text/html; charset=utf-8',
  '.js': 'text/javascript',
  '.mjs': 'text/javascript',
  '.wasm': 'application/wasm',
  '.css': 'text/css',
};

// ---------------------------------------------------------------------------
// Arguments and the example list
// ---------------------------------------------------------------------------

let wantNet = false;
let doList = false;
const selectors = [];
for (const arg of process.argv.slice(2)) {
  if (arg === '--net') wantNet = true;
  else if (arg === '--list') doList = true;
  else if (arg === '-h' || arg === '--help') {
    console.log('usage: node run.mjs [--net] [--list] [example ...]  (see the header of run.mjs)');
    process.exit(0);
  } else if (arg.startsWith('-')) {
    console.error(`run.mjs: unknown option ${arg} (try --help)`);
    process.exit(2);
  } else selectors.push(arg);
}
if (!Number.isInteger(TIMEOUT) || TIMEOUT < 0) {
  console.error(`run.mjs: TIMEOUT=${process.env.TIMEOUT} is not a whole number of seconds`);
  process.exit(2);
}

/// The native `drt` the whole-chain check runs as the relay and as the
/// `ProxyCommand`. Named by `DRT_BIN`, or found where cargo leaves one.
/// It must carry `relay` and `tunnel`, which the `full` profile does.
const drtBin = (() => {
  const named = process.env.DRT_BIN;
  if (named) return fs.existsSync(named) ? named : null;
  const root = path.resolve(HERE, '../../..');
  for (const p of ['target/debug/drt', 'target/release/drt']) {
    const full = path.join(root, p);
    if (fs.existsSync(full)) return full;
  }
  return null;
})();

const examples = [];
const uncovered = [];
for (const name of fs.readdirSync(EXAMPLES).sort()) {
  const dir = path.join(EXAMPLES, name);
  if (!/^[0-9][0-9]-/.test(name) || !fs.statSync(dir).isDirectory()) continue;
  if (selectors.length && !selectors.some((s) => name.includes(s))) continue;
  (fs.existsSync(path.join(dir, 'meta.json')) ? examples : uncovered).push(name);
}
if (examples.length === 0 && uncovered.length === 0) {
  console.error(
    selectors.length
      ? `run.mjs: no example in ${EXAMPLES} matches: ${selectors.join(' ')}`
      : `run.mjs: found no NN-*/meta.json under ${EXAMPLES}`,
  );
  process.exit(2);
}
if (doList) {
  for (const n of examples) console.log(n);
  for (const n of uncovered) console.log(`${n}   (no meta.json)`);
  process.exit(0);
}
if (!fs.existsSync(path.join(HERE, 'pkg', 'drt_web.js'))) {
  console.error('run.mjs: no pkg/drt_web.js here; build it first:\n    script/drt-web.sh');
  process.exit(2);
}

// ---------------------------------------------------------------------------
// The page: served from this directory, driven through window.drtBrowserTest
// ---------------------------------------------------------------------------

// Two files from outside this directory, for the WebRTC check: the browser
// access client as the release serves it, and the SSH page from
// crates/drt-ssh-web with its CSP taken out, since there it is the harness
// that signals (the page's own CSP admits only WebSocket connections).
const BROWSER_ACCESS = path.resolve(HERE, '../../drt-rtc/client/drt_browser_access.js');
const SSH_PAGE = path.resolve(HERE, '../../drt-ssh-web/page/dist/ssh.html');
// ssh.html's client as its own module (script/drt-ssh-page.sh leaves it in
// pkg/), for `drt ssh` and `:ssh` in a page.
const SSH_CLIENT = path.resolve(HERE, '../../drt-ssh-web/page/pkg');
const server = http.createServer((req, res) => {
  const url = decodeURIComponent(new URL(req.url, 'http://x').pathname);
  if (url === '/drt_ssh_web.js' || url === '/drt_ssh_web_bg.wasm') {
    const file = path.join(SSH_CLIENT, url.slice(1));
    if (fs.existsSync(file)) {
      res.writeHead(200, { 'content-type': TYPES[path.extname(file)] });
      res.end(fs.readFileSync(file));
      return;
    }
  }
  if (url === '/drt_browser_access.js') {
    res.writeHead(200, { 'content-type': TYPES['.js'] });
    res.end(fs.readFileSync(BROWSER_ACCESS));
    return;
  }
  // The SSH page as it ships, CSP and all: what row 8 opens from a link.
  if (url === '/ssh.html' && fs.existsSync(SSH_PAGE)) {
    res.writeHead(200, { 'content-type': TYPES['.html'] });
    res.end(fs.readFileSync(SSH_PAGE));
    return;
  }
  if (url === '/ssh-rtc.html' && fs.existsSync(SSH_PAGE)) {
    res.writeHead(200, { 'content-type': TYPES['.html'] });
    res.end(fs.readFileSync(SSH_PAGE, 'utf8').replace(/<meta http-equiv="Content-Security-Policy"[^>]*>/, ''));
    return;
  }
  const file = path.join(HERE, url === '/' ? 'index.html' : url);
  if (!file.startsWith(HERE)) {
    res.writeHead(403);
    res.end();
    return;
  }
  fs.readFile(file, (err, data) => {
    if (err) {
      res.writeHead(404);
      res.end('not here');
      return;
    }
    res.writeHead(200, { 'content-type': TYPES[path.extname(file)] ?? 'application/octet-stream' });
    res.end(data);
  });
});
await new Promise((resolve) => server.listen(0, '127.0.0.1', resolve));
const origin = `http://127.0.0.1:${server.address().port}`;

// WebRtcHideLocalIpsWithMdns off: two pages on one machine with no STUN
// server otherwise have no address to give each other, since a record
// drops `.local` candidates (doc/BrowserAccess.md §2.1, §10.4).
const browser = await chromium.launch({ args: ['--disable-features=WebRtcHideLocalIpsWithMdns'] });
let page;
const consoleLines = [];
async function open() {
  page = await browser.newPage();
  page.on('console', (m) => consoleLines.push(m.text()));
  page.on('pageerror', (e) => consoleLines.push(`pageerror: ${e.message}`));
  await page.goto(`${origin}/`);
  await page.evaluate(() => window.drtBrowserTest.ready);
}
await open();

const wasmBytes = fs.statSync(path.join(HERE, 'pkg', 'drt_web_bg.wasm')).size;
console.log(`drt-web: pkg/drt_web_bg.wasm, ${wasmBytes} bytes, in Chromium ${browser.version()}`);
const info = await page.evaluate(() => window.drtBrowserTest.buildInfo(false));
for (const line of info.split('\n')) {
  if (/^(version|profile): /.test(line)) console.log(`     ${line}`);
}
const profile = (info.match(/^profile: (.*)$/m) ?? [])[1] ?? 'unknown';
// What the core carries, as run-all.sh reads it. Empty when unreadable,
// and then nothing is skipped for it: a gate that silently stops checking
// is worse than one that reports a diff.
const features = ((info.match(/^features: (.*)$/m) ?? [])[1] ?? '').split(',').filter(Boolean);
if (process.env.DRT_WEB_BUILDINFO) fs.writeFileSync(process.env.DRT_WEB_BUILDINFO, info);
console.log('');

// ---------------------------------------------------------------------------
// Run
// ---------------------------------------------------------------------------

let nOk = 0;
let nFail = 0;
const failed = [];
const skipped = [];
const wrongBuild = [];
const noSocket = [];
// No `ssh` on this machine: the one check that needs a client outside
// the page cannot run, and says so rather than passing.
const noSsh = [];
// No native `drt` carrying `relay` and `tunnel`, so the whole-chain check
// has no carrier to run over.
const noRelay = [];
// No dist/ssh.html (script/drt-ssh-page.sh), so the WebRTC check has no
// SSH client to run in the second page.
const noSshPage = [];

const fail = (name, why) => {
  console.log(`FAILED   ${name.padEnd(24)} ${why}`);
  nFail += 1;
  failed.push(name);
};

for (const name of examples) {
  const dir = path.join(EXAMPLES, name);
  let meta;
  try {
    meta = JSON.parse(fs.readFileSync(path.join(dir, 'meta.json'), 'utf8'));
  } catch (e) {
    fail(name, `meta.json: ${e.message}`);
    continue;
  }
  if (!meta.cmd) {
    fail(name, 'meta.json has no "cmd"');
    continue;
  }
  if (meta.needs_network === true && !wantNet) {
    console.log(`skipped  ${name.padEnd(24)} (needs network) — pass --net to run it`);
    skipped.push(name);
    continue;
  }
  if (meta.needs_listener === true) {
    console.log(`skipped  ${name.padEnd(24)} (binds a port, and a page cannot)`);
    noSocket.push(name);
    continue;
  }
  // One profile name or a list of them, read the way run-all.sh reads it.
  const builds = meta.needs_build == null ? [] : [].concat(meta.needs_build);
  if (builds.length > 0 && !builds.includes(profile) && profile !== 'unknown') {
    console.log(`skipped  ${name.padEnd(24)} (needs a ${builds.join(' or ')} build; this drt is ${profile})`);
    wrongBuild.push(name);
    continue;
  }
  const wantFeatures = meta.needs_features ?? [];
  const missingFeatures = wantFeatures.filter((f) => !features.includes(f));
  if (features.length > 0 && missingFeatures.length > 0) {
    console.log(`skipped  ${name.padEnd(24)} (needs ${missingFeatures.join(', ')} in the core; this drt has ${features.join(',')})`);
    wrongBuild.push(name);
    continue;
  }
  const expectedFile = path.join(dir, 'expected.txt');
  if (!fs.existsSync(expectedFile)) {
    fail(name, 'no expected.txt beside meta.json');
    continue;
  }
  let rules;
  try {
    rules = (meta.normalise ?? meta.normalize ?? []).map(sedSubstitution);
  } catch (e) {
    fail(name, `meta.json "normalise" is not sed I can translate: ${e.message}`);
    continue;
  }

  // The example's directory, at /examples/<name> in the page, as the
  // working directory: what `cd examples/NN-*` is to run-all.sh.
  const cwd = `/examples/${name}`;
  const files = [];
  const dirs = [];
  walk(dir, (rel, isDir) => {
    if (isDir) dirs.push(`${cwd}/${rel}`);
    else files.push({ path: `${cwd}/${rel}`, data: fs.readFileSync(path.join(dir, rel)).toString('base64') });
  });
  await page.evaluate((seed) => window.drtBrowserTest.seed(seed), { cwd, files, dirs });

  const started = Date.now();
  let result;
  try {
    result = await withTimeout(
      page.evaluate((cmd) => window.drtBrowserTest.run(cmd), meta.cmd),
      TIMEOUT * 1000,
    );
  } catch (e) {
    if (e === TIMED_OUT) {
      fail(name, `timed out after ${TIMEOUT}s (set TIMEOUT= to change)`);
      // The page may be stuck inside a tick; start over with a fresh one.
      await page.close().catch(() => {});
      await open();
    } else {
      fail(name, `the page threw: ${e.message}`);
    }
    continue;
  }
  const elapsed = Date.now() - started;

  const actual = normalise(result.output, rules);
  const expected = normalise(fs.readFileSync(expectedFile, 'utf8'), rules);
  if (actual === expected) {
    const status = result.status === 0 ? '' : `   [exit ${result.status}]`;
    console.log(`ok       ${name.padEnd(24)} ${meta.cmd}${status}   (${elapsed} ms)`);
    nOk += 1;
  } else {
    fail(name, `${meta.cmd}   [exit ${result.status}]`);
    console.log('           --- expected.txt      +++ actual');
    const lines = diff(expected.split('\n'), actual.split('\n'));
    for (const line of lines.slice(0, MAXDIFF)) console.log(`           ${line}`);
    if (lines.length > MAXDIFF) console.log(`           ... ${lines.length - MAXDIFF} more diff lines not shown`);
  }
}

// The embedding contract, against a real xterm.js Terminal (doc/Wasm.md
// M5). index.html drives drt-term.js through a fake terminal, which
// proves the adapter's logic against a scriptable object; this proves the
// claim the contract makes to a host -- that `attach` drives the xterm.js
// `Terminal` the homepage panel and the Lab already have -- by typing
// real keystrokes into xterm's own input handling and reading back the
// terminal's own rendered buffer.
if (PAGE_CHECKS) {
  const name = 'xterm-embedding';
  try {
    const xterm = await browser.newPage();
    xterm.on('pageerror', (e) => consoleLines.push(`xterm pageerror: ${e.message}`));
    await xterm.goto(`${origin}/xterm.html`);
    await xterm.waitForFunction(() => window.drtXtermTest !== undefined, null, { timeout: 30000 });
    await xterm.evaluate(() => window.drtXtermTest.idle());
    // Typed, not injected: through the textarea xterm.js listens on, so
    // the path under test is the page's own keyboard -> onData -> attach.
    await xterm.click('#term');
    await xterm.keyboard.type('drt run hellp.dlua');
    for (let i = 0; i < 6; i++) await xterm.keyboard.press('Backspace');
    await xterm.keyboard.type('o.dlua');
    await xterm.keyboard.press('Enter');
    // Waited for by content, not by a promise: `whenIdle` answers about
    // the adapter's state, and what this check is about is what the
    // terminal ends up showing a person.
    await xterm
      .waitForFunction(() => window.drtXtermTest.screen().includes('hello from a page'), null, {
        timeout: 15000,
      })
      .catch(() => {});
    // And the same thing again without a keyboard: `run` is what a panel
    // with a "try this" button calls, and it resolves when the command is
    // over rather than leaving the host to guess.
    const status = await xterm.evaluate(() => window.drtXtermTest.run('drt buildinfo'));
    // `run` resolves when the command is over; what the terminal shows
    // arrives on xterm.js's own write schedule, and on a slow runner the
    // screen read the instant the promise settled was two characters into
    // the echo. Waited for by content, as above, with the same ceiling.
    await xterm
      .waitForFunction(() => window.drtXtermTest.screen().includes('profile: web'), null, {
        timeout: 15000,
      })
      .catch(() => {});
    const screen = await xterm.evaluate(() => window.drtXtermTest.screen());
    await xterm.close();

    const want = ['$ drt run hello.dlua', 'hello from a page', '2', 'profile: web'];
    if (status !== 0) throw new Error(`run('drt buildinfo') answered ${status}`);
    const missing = want.filter((line) => !screen.split('\n').some((l) => l.trim() === line));
    if (missing.length === 0) {
      console.log(`ok       ${name.padEnd(24)} a real xterm.js Terminal, typed into and read back`);
      nOk += 1;
    } else {
      fail(name, `the terminal never showed: ${missing.join(' | ')}`);
      for (const line of screen.split('\n')) console.log(`           |${line}`);
    }
  } catch (e) {
    fail(name, `the page threw: ${e.message}`);
  }
}

// The swarm table (doc/Wasm.md M5): the Lab's Instances panel replaces
// sixteen `dvs_*` calls with these, so the check is that they drive a real
// swarm rather than that they exist. A program is rooted, stepped to
// completion, and asked what it holds -- `host:time` yes and `lifecycle`
// no, from the same `host:*` ceiling a config-less run gets, which is a
// question about the capability set and not about which connectors the
// build happens to carry.
if (PAGE_CHECKS) {
  const name = 'swarm-table';
  try {
    const r = await withTimeout(
      page.evaluate(() => window.drtBrowserTest.swarmRoundTrip('print("swarm")\n')),
      TIMEOUT * 1000,
    );
    const wrong = [];
    if (r.root !== 1) wrong.push(`root id ${r.root}, expected 1`);
    if (r.parent !== 0) wrong.push(`root has parent ${r.parent}, expected 0`);
    if (r.parentOfNobody !== null) wrong.push('an id not in the roster has a parent');
    if (!r.holdsTime) wrong.push('root does not hold host:time');
    if (r.holdsLifecycle) wrong.push('root holds lifecycle, which host:* does not imply');
    if (!r.resident) wrong.push('root is not resident');
    if (r.slots !== 1) wrong.push(`${r.slots} slots allocated, expected 1`);
    if (r.aliveAfter !== 0) wrong.push(`${r.aliveAfter} alive after it exited`);
    if (r.idsAfter.length !== 0) wrong.push(`roster is ${JSON.stringify(r.idsAfter)} after it exited`);
    if (wrong.length === 0) {
      console.log(`ok       ${name.padEnd(24)} root, step to exit, roster and caps back`);
      nOk += 1;
    } else {
      fail(name, wrong.join('; '));
    }
  } catch (e) {
    fail(name, e === TIMED_OUT ? `timed out after ${TIMEOUT}s` : `the page threw: ${e.message}`);
  }
}

// The transport (doc/SshInBrowser.md): the page owns the socket, Rust owns
// a `Send` byte stream, and bytes cross both ways. What makes this worth a
// browser check rather than only the native tests in ws.rs is the boundary
// itself -- a Uint8Array in, a promise per chunk out, and the page's pump
// loop learning from `undefined` that the session is over. `startEcho`
// upper-cases so the answer cannot be an echo of the delivery path.
{
  const name = 'socket-echo';
  const said = ['ssh ', 'in a page'];
  try {
    const r = await withTimeout(
      page.evaluate((m) => window.drtBrowserTest.socketEcho(m), said),
      TIMEOUT * 1000,
    );
    const wrong = [];
    if (!r.sent) wrong.push('a deliver to a live socket was refused');
    if (r.echoed !== said.join('').toUpperCase()) {
      wrong.push(`came back as ${JSON.stringify(r.echoed)}`);
    }
    if (r.afterClose) wrong.push('a deliver after close was accepted');
    if (!r.ended) wrong.push("closing the wire did not end the page's pump loop");
    if (wrong.length === 0) {
      console.log(`ok       ${name.padEnd(24)} bytes out and back in ${r.chunks} chunk(s), then EOF`);
      nOk += 1;
    } else {
      fail(name, wrong.join('; '));
    }
  } catch (e) {
    fail(name, e === TIMED_OUT ? `timed out after ${TIMEOUT}s` : `the page threw: ${e.message}`);
  }
}

// SSH into the page, with the client `ssh(1)` (doc/SshInBrowser.md).
//
// The product's claim, run rather than argued: a *standard* client, its
// own keys, its own pty, reaching a terminal inside a page. Nothing here
// speaks SSH -- Node listens on a TCP port and shuttles bytes between
// that socket and the page's, which is what `drt tunnel` does over a
// relay and a WebSocket instead. Everything above the bytes is the real
// thing on both sides: OpenSSH's client, russh's server, and behind it
// M8's editor and shell.js.
//
// Skipped, and named, when there is no `ssh` to run.
{
  const name = 'ssh-into-the-page';
  const keys = fs.mkdtempSync(path.join(os.tmpdir(), 'drt-ssh-'));
  const key = path.join(keys, 'id_ed25519');
  let client = null;
  try {
    execFileSync('ssh-keygen', ['-q', '-t', 'ed25519', '-N', '', '-C', 'drt-web-suite', '-f', key]);
  } catch {
    noSsh.push(name);
  }
  if (!noSsh.includes(name)) {
    // `held` is not an optimisation. The server writes its SSH id the
    // moment it is served, which is before `ssh` has connected, and a
    // dropped id line means OpenSSH reads the first binary packet as the
    // banner and refuses the connection with "invalid characters".
    const bridge = { toPage: Promise.resolve(), socket: null, held: [] };
    try {
      await page.exposeFunction('sshOutgoing', (_id, data) => {
        const bytes = Buffer.from(data, 'base64');
        if (bridge.socket) bridge.socket.write(bytes);
        else bridge.held.push(bytes);
      });
      await page.exposeFunction('sshClosed', (_id) => {
        if (bridge.socket) bridge.socket.end();
      });
      const hostKey = await page.evaluate(() => window.drtBrowserTest.sshHostKey());
      const authorized = fs.readFileSync(`${key}.pub`, 'utf8');
      const server = await page.evaluate(
        ([hk, ak]) => window.drtBrowserTest.sshServe(1, hk, ak),
        [hostKey, authorized],
      );

      const listener = net.createServer((socket) => {
        bridge.socket = socket;
        for (const bytes of bridge.held.splice(0)) socket.write(bytes);
        // Killing the client resets the connection, which is a normal end
        // here and an unhandled 'error' event otherwise.
        socket.on('error', () => {});
        socket.on('data', (chunk) => {
          // Chained, not fired: two `evaluate`s in flight could deliver
          // the stream out of order, and a reordered SSH packet is a
          // failed key exchange.
          bridge.toPage = bridge.toPage.then(() =>
            page.evaluate(
              ([id, data]) => window.drtBrowserTest.sshDeliver(id, data),
              [1, chunk.toString('base64')],
            ),
          );
        });
        socket.on('close', () => page.evaluate(() => window.drtBrowserTest.sshClose(1)));
      });
      listener.on('error', () => {});
      await new Promise((resolve) => listener.listen(0, '127.0.0.1', resolve));
      const port = listener.address().port;

      client = spawn('ssh', [
        '-tt', // a pty, because what is behind this is a line editor
        '-i', key,
        '-o', 'IdentitiesOnly=yes',
        '-o', 'StrictHostKeyChecking=no',
        '-o', 'UserKnownHostsFile=/dev/null',
        '-o', 'GlobalKnownHostsFile=/dev/null',
        '-o', 'LogLevel=ERROR',
        '-p', String(port),
        'whoever@127.0.0.1',
      ]);
      let transcript = '';
      client.on('error', (e) => (transcript += `ssh: ${e.message}\n`));
      client.stdout.on('data', (b) => (transcript += b.toString('utf8')));
      client.stderr.on('data', (b) => (transcript += b.toString('utf8')));
      client.stdin.write('drt run hello.dlua\r');

      // What proves it went all the way through: a string the page's own
      // runtime printed, which nothing in the transcript typed.
      const said = await waitFor(() => transcript.includes('hello over ssh'), TIMEOUT * 1000);
      const wrong = [];
      if (server.authorized !== 1) wrong.push(`${server.authorized} authorized keys, expected 1`);
      if (!/^SHA256:/.test(server.fingerprint)) wrong.push('the host key has no fingerprint');
      if (!said) wrong.push(`the program never printed: ${JSON.stringify(plain(transcript).slice(-300))}`);
      else if (!plain(transcript).includes('drt in a page')) wrong.push('the banner never arrived');
      if (wrong.length === 0) {
        console.log(`ok       ${name.padEnd(24)} ssh -tt -p PORT, ${server.fingerprint.slice(0, 18)}...`);
        nOk += 1;
      } else {
        fail(name, wrong.join('; '));
      }
      listener.close();
    } catch (e) {
      fail(name, `the bridge threw: ${e.message}`);
    } finally {
      if (client) client.kill('SIGKILL');
      fs.rmSync(keys, { recursive: true, force: true });
    }
  } else {
    fs.rmSync(keys, { recursive: true, force: true });
  }
}

// The whole chain, with nothing bridged by this file
// (doc/SshInBrowser.md): a real relay (`drt start`), a page parking a leg on it by
// label, and `ssh -o ProxyCommand="drt tunnel ..."` claiming it. This is
// the shape the README documents and the reason `drt tunnel` exists --
// a device with no inbound address, reached by a standard client -- and
// the device here is a browser tab.
//
// The previous check bridged TCP itself, which proved the server. This
// one proves the *carrier*: park and claim by URL, spliced by the relay,
// with the page's WebSocket and the binary's WebSocket at the two ends.
//
// Skipped, and named, without an `ssh` or a `drt` binary carrying
// `relay` and `tunnel` (the `full` profile).
{
  const name = 'ssh-through-a-relay';
  const keys = fs.mkdtempSync(path.join(os.tmpdir(), 'drt-relay-'));
  const key = path.join(keys, 'id_ed25519');
  const PARK = 'pk-browser-suite-0123456789';
  const CALLER = 'ck-browser-suite-9876543210';
  let relay = null;
  let client = null;
  try {
    if (!drtBin) noRelay.push(name);
    else execFileSync('ssh-keygen', ['-q', '-t', 'ed25519', '-N', '', '-C', 'drt-web-relay', '-f', key]);
  } catch {
    if (!noSsh.includes(name)) noSsh.push(name);
  }
  if (!noRelay.includes(name) && !noSsh.includes(name)) {
    try {
      // A port the relay can have. Bound and released rather than
      // guessed, and the relay is waited for rather than slept on.
      const port = await freePort();
      const config = path.join(keys, 'relay.json');
      fs.writeFileSync(config, JSON.stringify({
        entry: 'stdlib:relay',
        relay: { bind: `127.0.0.1:${port}`, labels: { page: { park_key: PARK, caller_key: CALLER } } },
      }));
      relay = spawn(drtBin, ['--config', config, 'start'], { stdio: ['ignore', 'pipe', 'pipe'] });
      let relaySaid = '';
      relay.stdout.on('data', (b) => (relaySaid += b));
      relay.stderr.on('data', (b) => (relaySaid += b));
      relay.on('error', (e) => (relaySaid += `relay: ${e.message}\n`));
      const up = await waitFor(() => accepting(port), 10000);
      if (!up) throw new Error(`the relay never listened on ${port}: ${relaySaid.trim()}`);

      const hostKey = await page.evaluate(() => window.drtBrowserTest.sshHostKey());
      await page.evaluate(
        ([url, hk, ak]) => window.drtBrowserTest.sshPark(url, hk, ak),
        [`ws://127.0.0.1:${port}/park/page?k=${PARK}`, hostKey, fs.readFileSync(`${key}.pub`, 'utf8')],
      );
      // A claim that beats the park is told "not home", so the leg has to
      // be up before `ssh` runs. The page says when it is.
      const parked = await waitFor(
        async () => (await page.evaluate(() => window.drtBrowserTest.sshParkEvents())).includes('parked'),
        10000,
      );
      if (!parked) throw new Error('the page never parked a leg');

      client = spawn('ssh', [
        '-tt',
        '-i', key,
        '-o', `ProxyCommand=${drtBin} tunnel ws://127.0.0.1:${port}/s/page?k=${CALLER}`,
        '-o', 'IdentitiesOnly=yes',
        '-o', 'StrictHostKeyChecking=no',
        '-o', 'UserKnownHostsFile=/dev/null',
        '-o', 'GlobalKnownHostsFile=/dev/null',
        '-o', 'LogLevel=ERROR',
        'whoever@page',
      ]);
      let transcript = '';
      client.on('error', (e) => (transcript += `ssh: ${e.message}\n`));
      client.stdout.on('data', (b) => (transcript += b.toString('utf8')));
      client.stderr.on('data', (b) => (transcript += b.toString('utf8')));
      client.stdin.write('drt run hello.dlua\r');

      const said = await waitFor(() => transcript.includes('hello through a tunnel'), TIMEOUT * 1000);
      const events = await page.evaluate(() => window.drtBrowserTest.sshParkEvents());
      const wrong = [];
      if (!said) wrong.push(`the program never printed: ${JSON.stringify(plain(transcript).slice(-300))}`);
      if (!events.includes('claimed')) wrong.push('the page never saw the claim');
      // Replenish-on-claim: a claimed leg is a session, so a fresh one is
      // parked at once. Without it the second caller finds nobody home.
      if (events.filter((e) => e === 'parked').length < 2) {
        wrong.push(`the page parked ${events.filter((e) => e === 'parked').length} time(s), so it did not replenish`);
      }
      if (wrong.length === 0) {
        console.log(`ok       ${name.padEnd(24)} ssh -o ProxyCommand="drt tunnel ws://.../s/page"`);
        nOk += 1;
      } else {
        fail(name, wrong.join('; '));
      }
      await page.evaluate(() => window.drtBrowserTest.sshUnpark());
    } catch (e) {
      fail(name, e.message);
    } finally {
      if (client) client.kill('SIGKILL');
      if (relay) relay.kill('SIGKILL');
      fs.rmSync(keys, { recursive: true, force: true });
    }
  } else {
    fs.rmSync(keys, { recursive: true, force: true });
  }
}

// Row 8 of doc/ssh-transport-matrix.md: a second page, running the SSH
// page's module, reaches this page's SSH server over WebRTC. This page
// answers the session and serves `ssh` (doc/BrowserAccess.md §10); the
// suite carries the two records across, which is all signaling does. The
// proof is a string the page's own runtime printed, read back by the SSH
// client in the other page.
{
  const name = 'ssh-over-webrtc-to-a-page';
  if (!fs.existsSync(SSH_PAGE)) noSshPage.push(name);
  else {
    let caller;
    try {
      caller = await browser.newPage();
      caller.on('pageerror', (e) => consoleLines.push(`caller pageerror: ${e.message}`));
      await caller.goto(`${origin}/ssh-rtc.html`);
      await caller.waitForFunction(() =>
        !document.getElementById('nokey').hidden || !document.getElementById('haskey').hidden);
      const key = await caller.evaluate(async () => {
        window.lib = await import('/drt_browser_access.js');
        const k = wasm_bindgen.generateKey('rtc-suite');
        window.pending = await window.lib.offer();
        return { priv: k.privateOpenssh, pub: k.publicOpenssh, record: window.pending.recordText };
      });
      const hostKey = await page.evaluate(() => window.drtBrowserTest.sshHostKey());
      const answered = await page.evaluate(
        ([hk, ak, rec]) => window.drtBrowserTest.sshAnswer(hk, ak, rec),
        [hostKey, key.pub, key.record],
      );
      const r = await caller.evaluate(async ([record, pin, priv, ms]) => {
        const sleep = (t) => new Promise((ok) => setTimeout(ok, t));
        const session = await window.pending.accept(record, { timeoutMs: ms });
        const ssh = await wasm_bindgen.Ssh.connect(session.connect('ssh'), pin);
        const signedIn = await ssh.authKey('whoever', priv);
        let out = '';
        await ssh.shell(80, 24, (b) => (out += new TextDecoder().decode(b)), () => {});
        ssh.write(new TextEncoder().encode('drt run hello.dlua\r'));
        for (const t0 = Date.now(); !out.includes('hello over webrtc') && Date.now() - t0 < ms; ) await sleep(100);
        const services = session.hello.services;
        ssh.close();
        session.close();
        return { signedIn, services, out: out.slice(-400) };
      }, [answered.record, answered.fingerprint, key.priv, TIMEOUT * 1000]);
      const wrong = [];
      if (!r.signedIn) wrong.push('the key was not accepted');
      if (JSON.stringify(r.services) !== '["ssh"]') wrong.push(`hello named ${JSON.stringify(r.services)}`);
      if (!r.out.includes('hello over webrtc')) wrong.push(`the program never printed: ${JSON.stringify(plain(r.out))}`);
      if (wrong.length === 0) {
        console.log(`ok       ${name.padEnd(24)} ssh.html -> connect('ssh') -> a page, ${answered.fingerprint.slice(0, 18)}...`);
        nOk += 1;
      } else {
        fail(name, wrong.join('; '));
      }
    } catch (e) {
      fail(name, `threw: ${String(e.message).split('\n')[0].slice(0, 300)}`);
    } finally {
      await caller?.close();
    }
  }
}

// Row 6: stock `ssh` with `ProxyCommand="drt tunnel rtc:<server>/v1/page/calls"`
// reaches this page's SSH server over WebRTC. The signalling server is
// examples/30-signaling-room, program and config unchanged but for this
// suite's port; the page holds its call notification stream, polls on each
// notification, and answers each call. No relay and nothing bridged by
// this file: the bytes go native caller to page.
{
  const name = 'ssh-rtc-into-a-page';
  const keys = fs.mkdtempSync(path.join(os.tmpdir(), 'drt-rtc-'));
  const key = path.join(keys, 'id');
  let room;
  let client;
  if (!drtBin) noRelay.push(name);
  else {
    try {
      execFileSync('ssh-keygen', ['-q', '-t', 'ed25519', '-N', '', '-C', 'drt-web-rtc', '-f', key]);
    } catch {
      noSsh.push(name);
    }
  }
  if (!noRelay.includes(name) && !noSsh.includes(name)) {
    try {
      const port = await freePort();
      const example = path.resolve(HERE, '../../../examples/30-signaling-room');
      const roomConfig = JSON.parse(fs.readFileSync(path.join(example, 'app.json'), 'utf8'));
      roomConfig.program.path = path.join(example, 'app.dlua');
      roomConfig.listeners[0].address = `127.0.0.1:${port}`;
      const { name: roomName, answerer_token: answererToken, caller_token: callerToken } = roomConfig.args;
      const config = path.join(keys, 'room.json');
      fs.writeFileSync(config, JSON.stringify(roomConfig));
      room = spawn(drtBin, ['--config', config, 'start'], { stdio: ['ignore', 'pipe', 'pipe'] });
      let roomSaid = '';
      room.stdout.on('data', (b) => (roomSaid += b));
      room.stderr.on('data', (b) => (roomSaid += b));
      if (!(await waitFor(() => accepting(port), 10000))) throw new Error(`the room never listened: ${roomSaid.trim()}`);

      const hostKey = await page.evaluate(() => window.drtBrowserTest.sshHostKey());
      const listening = await page.evaluate(
        ([hk, ak, url, k]) => window.drtBrowserTest.sshListen(hk, ak, url, k),
        [hostKey, fs.readFileSync(`${key}.pub`, 'utf8'), `http://127.0.0.1:${port}/v1/${roomName}`, answererToken],
      );
      client = spawn('ssh', [
        '-tt', '-i', key, '-o', 'IdentitiesOnly=yes',
        '-o', 'StrictHostKeyChecking=no', '-o', 'UserKnownHostsFile=/dev/null',
        '-o', 'GlobalKnownHostsFile=/dev/null', '-o', 'LogLevel=ERROR',
        '-o', `ProxyCommand=${drtBin} tunnel 'rtc:http://127.0.0.1:${port}/v1/${roomName}/calls?k=${callerToken}'`,
        'whoever@page',
      ]);
      let transcript = '';
      client.on('error', (e) => (transcript += `ssh: ${e.message}\n`));
      client.stdout.on('data', (b) => (transcript += b.toString('utf8')));
      client.stderr.on('data', (b) => (transcript += b.toString('utf8')));
      client.stdin.write('drt run hello.dlua\r');
      const said = await waitFor(() => transcript.includes('hello from a page, over webrtc'), TIMEOUT * 1000);
      if (said) {
        console.log(`ok       ${name.padEnd(24)} ssh -o ProxyCommand="drt tunnel rtc:<server>/v1/page/calls", ${listening.fingerprint.slice(0, 18)}...`);
        nOk += 1;
      } else {
        fail(name, `the program never printed: ${JSON.stringify(plain(transcript).slice(-300))}`);
      }
    } catch (e) {
      fail(name, `threw: ${String(e.message).split('\n')[0].slice(0, 300)}`);
    } finally {
      await page.evaluate(() => window.drtBrowserTest.sshStopListening()).catch(() => {});
      if (client) client.kill('SIGKILL');
      if (room) room.kill('SIGKILL');
    }
  }
  fs.rmSync(keys, { recursive: true, force: true });
}

// `drt ssh` and `:ssh` in a page: typed at a real xterm.js Terminal
// attached with ssh.html's client, reaching OpenSSH's sshd through a
// WebSocket bridge (`drt tunnel --listen`, the device side of a relay
// without the relay). From the shell, then from inside `drt repl`, whose
// state must survive the session and whose prompt must not receive a key
// typed into the remote shell.
{
  const name = 'drt-ssh-in-the-page';
  const sshdBin = process.env.SSHD ?? '/usr/sbin/sshd';
  const keys = fs.mkdtempSync(path.join(os.tmpdir(), 'drt-page-ssh-'));
  let sshd;
  let bridge;
  let slot = null;
  if (!fs.existsSync(path.join(SSH_CLIENT, 'drt_ssh_web.js'))) noSshPage.push(name);
  else if (!drtBin) noRelay.push(name);
  else if (!fs.existsSync(sshdBin)) noSsh.push(name);
  else {
    try {
      const keygen = (n) => execFileSync('ssh-keygen', ['-q', '-t', 'ed25519', '-N', '', '-C', n, '-f', path.join(keys, n)]);
      keygen('host');
      keygen('client');
      fs.copyFileSync(path.join(keys, 'client.pub'), path.join(keys, 'authorized'));
      const user = os.userInfo().username;
      const sshdPort = await freePort();
      fs.writeFileSync(path.join(keys, 'sshd_config'), [
        `Port ${sshdPort}`, 'ListenAddress 127.0.0.1', `HostKey ${path.join(keys, 'host')}`,
        `AuthorizedKeysFile ${path.join(keys, 'authorized')}`, 'PasswordAuthentication no',
        'KbdInteractiveAuthentication no', 'UsePAM yes', 'StrictModes no',
        'PermitRootLogin prohibit-password', 'PrintMotd no', 'PrintLastLog no',
        `PidFile ${path.join(keys, 'pid')}`,
      ].join('\n') + '\n');
      // Root, through sudo when this is not: an unprivileged sshd ends
      // every pty session it opens (crates/drt-ssh-web/page/e2e.mjs).
      const [cmd, ...pre] = process.getuid() === 0 ? [sshdBin] : ['sudo', '-n', sshdBin];
      sshd = spawn(cmd, [...pre, '-D', '-e', '-f', path.join(keys, 'sshd_config')], { stdio: 'ignore' });
      // Its pid file, written once it has bound: a port that answers could
      // be anyone's.
      if (!(await waitFor(() => fs.existsSync(path.join(keys, 'pid')), 10000))) throw new Error('sshd never listened');
      const wsPort = await freePort();
      bridge = spawn(drtBin, ['tunnel', '--listen', `127.0.0.1:${wsPort}`, '--to', `127.0.0.1:${sshdPort}`], { stdio: 'ignore' });
      if (!(await waitFor(() => accepting(wsPort), 10000))) throw new Error('the bridge never listened');
      const via = `ws://127.0.0.1:${wsPort}/`;

      slot = await page.evaluate((k) => window.drtBrowserTest.sshTerminal(k), fs.readFileSync(path.join(keys, 'client'), 'utf8'));
      const screen = () => page.evaluate((i) => window.drtBrowserTest.sshScreen(i), slot);
      const type = (text) => page.evaluate(([i, t]) => window.drtBrowserTest.sshType(i, t), [slot, text]);
      const see = async (text, after = 0) => {
        if (await waitFor(async () => (await screen()).indexOf(text, after) >= 0, TIMEOUT * 1000)) {
          return (await screen()).indexOf(text, after) + text.length;
        }
        throw new Error(`never saw ${JSON.stringify(text)}; the terminal shows:\n${(await screen()).slice(-1500)}`);
      };
      // Settled: the screen grew and then stopped changing for a moment,
      // which is a remote shell's prompt whatever it looks like.
      const settle = async () => {
        const start = (await screen()).length;
        let last = start;
        let since = Date.now();
        await waitFor(async () => {
          const now = (await screen()).length;
          if (now !== last) {
            last = now;
            since = Date.now();
          }
          return now > start && Date.now() - since > 400;
        }, TIMEOUT * 1000);
      };
      const wrong = [];

      // From the shell: first contact asks, the key signs in, the remote
      // status is `$?`.
      await page.evaluate((i) => window.drtBrowserTest.sshIdle(i), slot);
      await type(`drt ssh ${user}@box --via ${via}\r`);
      let at = await see('(yes/no) ');
      await type('yes\r');
      at = await see('Remembered box.', at);
      await settle();
      await type('stty size; echo PAGE-$((6*7)); exit 6\r');
      at = await see('30 100', at);
      at = await see('PAGE-42', at);
      await page.evaluate((i) => window.drtBrowserTest.sshIdle(i), slot);
      await type('echo "status $?"\r');
      at = await see('status 6', at);

      // A pin that is not the host's key: refused before anything signs in.
      await type(`drt ssh ${user}@box --via ${via} --hostkey SHA256:not-it\r`);
      at = await see('not the one --hostkey pins', at);

      // From the REPL: known now, so no question; the instance survives.
      await page.evaluate((i) => window.drtBrowserTest.sshIdle(i), slot);
      await type('drt repl\r');
      at = await see('dv> ', at);
      await type('x = 40\r');
      at = await see('dv> ', at);
      await type(`:ssh ${user}@box --via ${via}\r`);
      await settle();
      const asked = (await screen()).slice(at).includes('(yes/no)');
      if (asked) wrong.push('asked about a host already remembered');
      await type('echo INSIDE-$((6*7)); exit 2\r');
      at = await see('INSIDE-42', at);
      at = await see('closed, exit 2', at);
      at = await see('dv> ', at);
      await type('x + 2\r');
      at = await see('42', at);
      if (/syntax error|unexpected symbol/.test(await screen())) wrong.push('a key typed into the session reached the REPL');
      await type('\x04');
      await page.evaluate((i) => window.drtBrowserTest.sshIdle(i), slot);

      if (wrong.length === 0) {
        console.log(`ok       ${name.padEnd(24)} drt ssh and :ssh --via ws://… to sshd: $? 6, exit 2, REPL state kept`);
        nOk += 1;
      } else {
        fail(name, wrong.join('; '));
      }
    } catch (e) {
      fail(name, `threw: ${String(e.message).split('\n').slice(0, 30).join('\n')}`);
    } finally {
      if (slot !== null) await page.evaluate((i) => window.drtBrowserTest.sshDispose(i), slot).catch(() => {});
      if (bridge) bridge.kill('SIGKILL');
      if (sshd) {
        const pid = fs.existsSync(path.join(keys, 'pid')) ? fs.readFileSync(path.join(keys, 'pid'), 'utf8').trim() : null;
        if (pid && process.getuid() !== 0) spawnSync('sudo', ['-n', 'kill', pid]);
        else if (pid) spawnSync('kill', [pid]);
        sshd.kill('SIGKILL');
      }
    }
  }
}

// Row 8, from a link: the SSH page as it ships opens
// `ssh.html#call=<server>/v1/page/calls?k=…`, calls through
// examples/30-signaling-room, and signs in to this page's SSH server,
// which answers through the client library's `listen`. Two pages, one
// signalling server, and no DRT process in the session's path.
{
  const name = 'ssh-html-call-to-a-page';
  let room;
  let caller;
  if (!fs.existsSync(SSH_PAGE)) noSshPage.push(name);
  else if (!drtBin) noRelay.push(name);
  else {
    try {
      const port = await freePort();
      const example = path.resolve(HERE, '../../../examples/30-signaling-room');
      const roomConfig = JSON.parse(fs.readFileSync(path.join(example, 'app.json'), 'utf8'));
      roomConfig.program.path = path.join(example, 'app.dlua');
      roomConfig.listeners[0].address = `127.0.0.1:${port}`;
      const { name: roomName, answerer_token: answererToken, caller_token: callerToken } = roomConfig.args;
      const config = path.join(os.tmpdir(), `drt-room-${port}.json`);
      fs.writeFileSync(config, JSON.stringify(roomConfig));
      room = spawn(drtBin, ['--config', config, 'start'], { stdio: ['ignore', 'pipe', 'pipe'] });
      let roomSaid = '';
      room.stdout.on('data', (b) => (roomSaid += b));
      room.stderr.on('data', (b) => (roomSaid += b));
      if (!(await waitFor(() => accepting(port), 10000))) throw new Error(`the server never listened: ${roomSaid.trim()}`);

      // The SSH page makes this browser's key, as a person would.
      const context = await browser.newContext();
      caller = await context.newPage();
      caller.on('pageerror', (e) => consoleLines.push(`ssh.html pageerror: ${e.message}`));
      await caller.goto(`${origin}/ssh.html`);
      await caller.click('#makekey');
      await caller.waitForFunction(() => !document.getElementById('haskey').hidden);
      const pub = await caller.inputValue('#pub');

      const base = `http://127.0.0.1:${port}/v1/${roomName}`;
      const hostKey = await page.evaluate(() => window.drtBrowserTest.sshHostKey());
      const listening = await page.evaluate(
        ([hk, ak, url, k]) => window.drtBrowserTest.sshListen(hk, ak, url, k),
        [hostKey, pub, base, answererToken],
      );
      const link = new URLSearchParams({
        call: `${base}/calls?k=${callerToken}`, user: 'whoever', hostkey: listening.fingerprint,
      });
      const linked = await context.newPage();
      linked.on('pageerror', (e) => consoleLines.push(`ssh.html pageerror: ${e.message}`));
      await caller.close();
      caller = linked;
      await caller.goto(`${origin}/ssh.html#${link}`);
      await caller.waitForFunction(() => !!window.drtSsh?.term, null, { timeout: TIMEOUT * 1000 }).catch(async () => {
        throw new Error(`no shell; the page says: ${await caller.textContent('#status')}`);
      });
      await caller.keyboard.type('drt run hello.dlua\n');
      const screen = () => caller.evaluate(() => {
        const b = window.drtSsh.term.buffer.active;
        const lines = [];
        for (let i = 0; i < b.length; i++) lines.push(b.getLine(i).translateToString(true));
        return lines.join('\n');
      });
      let shown = '';
      const said = await waitFor(async () => (shown = await screen()).includes('hello from a page, over webrtc'), TIMEOUT * 1000);
      const status = await caller.textContent('#status');
      if (said && status.includes(`call:127.0.0.1:${port}/v1/${roomName}/calls/ssh`)) {
        console.log(`ok       ${name.padEnd(24)} ssh.html#call=<server>/v1/page/calls -> a page, ${listening.fingerprint.slice(0, 18)}...`);
        nOk += 1;
      } else {
        fail(name, `status ${JSON.stringify(status)}; screen ${JSON.stringify(plain(shown).slice(-300))}`);
      }
    } catch (e) {
      fail(name, `threw: ${String(e.message).split('\n')[0].slice(0, 300)}`);
    } finally {
      await page.evaluate(() => window.drtBrowserTest.sshStopListening()).catch(() => {});
      await caller?.context().close().catch(() => {});
      if (room) room.kill('SIGKILL');
    }
  }
}

// The REPL, typed at drt-term.js, against what the native binary said to
// the same lines. The page echoes what is typed and the native transcript
// (stdin from a file) does not, so the echoes are removed before the diff;
// everything else -- prompts, answers, the C core's print, the error --
// must match.
//
// Read from a real terminal's rendered buffer rather than from the bytes
// the adapter wrote. Since M8 the page's editor is `ego_cli`, which moves
// the cursor and redraws, so the byte stream is no longer a transcript of
// anything a person saw; the screen is. Two consequences, both the
// terminal's doing rather than the runtime's. A tab is movement to the
// next 8-column stop, and it lands on a different column in each -- the
// native prompt shares a line with the output after it, the page's output
// starts a fresh line under the echo -- so runs of horizontal whitespace
// are collapsed on both sides: what that keeps is the answer, `print`'s
// separator included, and what it gives up is how far a terminal moved,
// which is the terminal's business. And the shell prompt ^D returns to is
// a line of its own rather than a dangling `$ `.
if (PAGE_CHECKS) {
  const name = 'repl-parity';
  const scriptLines = fs.readFileSync(path.join(HERE, 'repl-script.txt'), 'utf8').replace(/\n$/, '').split('\n');
  const expected = oneSpace(
    fs.readFileSync(path.join(HERE, 'repl-expected.txt'), 'utf8'),
  );
  try {
    const raw = await withTimeout(
      page.evaluate((lines) => window.drtBrowserTest.replTranscript(lines), scriptLines),
      TIMEOUT * 1000,
    );
    let actual = raw.replace(/\r\n/g, '\n');
    actual = actual.replace(/^\$ drt repl\n/, '');
    for (const line of scriptLines) {
      actual = actual.replace(new RegExp(`(dv> |>> )${escapeRegExp(line)}\n`), '$1');
    }
    // After ^D: the blank line the repl leaves, then the shell prompt.
    actual = oneSpace(actual.replace(/\n*\$ ?\n*$/, '\n'));
    if (actual === expected) {
      console.log(`ok       ${name.padEnd(24)} drt repl < repl-script.txt, typed at drt-term.js`);
      nOk += 1;
    } else {
      fail(name, 'the page repl and the native repl differ');
      console.log('           --- repl-expected.txt +++ actual');
      for (const line of diff(expected.split('\n'), actual.split('\n'))) console.log(`           ${line}`);
    }
  } catch (e) {
    fail(name, e === TIMED_OUT ? `timed out after ${TIMEOUT}s` : `the page threw: ${e.message}`);
  }
}

// ---------------------------------------------------------------------------
// Summary
// ---------------------------------------------------------------------------

for (const n of uncovered) console.log(`NO META  ${n.padEnd(24)} not checked by anything — add a meta.json`);
const nSkip = skipped.length + wrongBuild.length + noSocket.length + noSsh.length + noRelay.length
  + noSshPage.length;
const total = nOk + nFail + nSkip + uncovered.length;
console.log('');
console.log(`${total} check(s): ${nOk} ok, ${nFail} failed, ${nSkip} skipped, ${uncovered.length} without a meta.json`);
if (skipped.length) {
  console.log(`skipped for needing a network (NOT a pass): ${skipped.join(' ')}`);
  console.log('run with --net to include them.');
}
if (wrongBuild.length) {
  console.log(`skipped for needing another build (NOT a pass): ${wrongBuild.join(' ')}`);
  console.log('the browser is the `web` profile; the native gate covers the rest.');
}
if (noSocket.length) {
  console.log(`skipped for binding a port (NOT a pass): ${noSocket.join(' ')}`);
  console.log('a page has no socket to bind; the native gate and the wasmtime one cover these.');
}
if (noSsh.length) {
  console.log(`skipped for needing an ssh client (NOT a pass): ${noSsh.join(' ')}`);
  console.log('install openssh-client; crates/drt-web/tests/ssh.rs covers the server natively.');
}
if (noRelay.length) {
  console.log(`skipped for needing a native drt (NOT a pass): ${noRelay.join(' ')}`);
  console.log('build one -- cargo build -p drt --no-default-features --features full -- or set DRT_BIN.');
}
if (noSshPage.length) {
  console.log(`skipped for needing the SSH page (NOT a pass): ${noSshPage.join(' ')}`);
  console.log('build it -- script/drt-ssh-page.sh -- so dist/ssh.html is there.');
}
if (uncovered.length) console.log(`no meta.json, so unchecked: ${uncovered.join(' ')}`);
if (nFail) console.log(`failed: ${failed.join(' ')}`);
if (consoleLines.length) {
  console.log('');
  console.log('the page said:');
  for (const line of consoleLines.slice(0, 40)) console.log(`  ${line}`);
}

await browser.close();
server.close();
process.exit(nFail || uncovered.length ? 1 : 0);

// ---------------------------------------------------------------------------
// depth: helpers
// ---------------------------------------------------------------------------

/// Poll until `done()` or the deadline. Used where the thing being waited
/// for is a byte arriving in a transcript rather than a promise resolving.
async function waitFor(done, ms) {
  const deadline = Date.now() + ms;
  while (Date.now() < deadline) {
    if (await done()) return true;
    await new Promise((r) => setTimeout(r, 25));
  }
  return await done();
}

/// A pty transcript without the escape sequences a line editor emits.
function plain(text) {
  return text
    .replace(/\u001b\][^\u0007\u001b]*(\u0007|\u001b\\)/g, '')
    .replace(/\u001b\[[0-9;?]*[ -/]*[@-~]/g, '')
    .replace(/\u001b[@-_]/g, '')
    .replace(/\r/g, '\n');
}

/// A port nothing is on: bound and released rather than guessed, which
/// is what the relay's own tests do for the same reason.
function freePort() {
  return new Promise((resolve, reject) => {
    const probe = net.createServer();
    probe.on('error', reject);
    probe.listen(0, '127.0.0.1', () => {
      const { port } = probe.address();
      probe.close(() => resolve(port));
    });
  });
}

/// Whether something is accepting on `port` yet. The relay prints a line
/// when it binds, but a line on stderr is not the same fact as a socket
/// that answers.
function accepting(port) {
  return new Promise((resolve) => {
    const probe = net.connect({ port, host: '127.0.0.1' });
    probe.setTimeout(200);
    const done = (yes) => {
      probe.destroy();
      resolve(yes);
    };
    probe.on('connect', () => done(true));
    probe.on('error', () => done(false));
    probe.on('timeout', () => done(false));
  });
}

function withTimeout(promise, ms) {
  if (!ms) return promise;
  let timer;
  const clock = new Promise((_, reject) => {
    timer = setTimeout(() => reject(TIMED_OUT), ms);
  });
  return Promise.race([promise, clock]).finally(() => clearTimeout(timer));
}

function walk(root, visit, rel = '') {
  for (const entry of fs.readdirSync(path.join(root, rel), { withFileTypes: true }).sort((a, b) => a.name.localeCompare(b.name))) {
    const here = rel ? `${rel}/${entry.name}` : entry.name;
    if (entry.isDirectory()) {
      visit(here, true);
      walk(root, visit, here);
    } else if (entry.isFile()) visit(here, false);
  }
}

/// Every run of tabs and spaces as one space.
///
/// A tab survives a byte stream and does not survive a screen, where it
/// has already become however many columns the cursor moved. Applied to
/// both sides of the repl diff so it is about what was said rather than
/// about where a terminal put it.
function oneSpace(text) {
  return text.replace(/[ \t]+/g, ' ');
}

function escapeRegExp(text) {
  return text.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
}

// One sed `s` command -- `s|pattern|replacement|flags`, the pattern a
// POSIX basic regular expression -- as a JavaScript regex and replacement.
// Applied one line at a time, first match only unless `g`, which is what
// sed does with it.
function sedSubstitution(expr) {
  if (expr[0] !== 's' || expr.length < 4) throw new Error(`not an s command: ${expr}`);
  const delim = expr[1];
  const parts = [];
  let cur = '';
  for (let i = 2; i < expr.length; i++) {
    const c = expr[i];
    if (c === '\\' && i + 1 < expr.length) {
      cur += c + expr[++i];
    } else if (c === delim) {
      parts.push(cur);
      cur = '';
    } else cur += c;
  }
  parts.push(cur);
  if (parts.length < 3) throw new Error(`unterminated s command: ${expr}`);
  const [pattern, replacement, flags = ''] = parts;
  const re = new RegExp(basicToJs(pattern), flags.includes('g') ? 'g' : '');
  const rep = replacement
    .replace(/\$/g, '$$$$')
    .replace(/\\([0-9])/g, '$$$1')
    .replace(/(^|[^\\])&/g, '$1$$&')
    .replace(/\\&/g, '&');
  return { re, rep };
}

// BRE to JavaScript: `\(` `\)` `\{` `\}` `\|` `\+` `\?` are the operators
// and the bare characters are literal, which is JavaScript's rule turned
// around; bracket expressions pass through with their backslashes
// doubled, since inside them a backslash is a character.
function basicToJs(pattern) {
  let out = '';
  for (let i = 0; i < pattern.length; i++) {
    const c = pattern[i];
    if (c === '\\') {
      const n = pattern[++i];
      if (n === undefined) out += '\\\\';
      else if ('(){}|+?'.includes(n)) out += n;
      else out += `\\${n}`;
    } else if ('(){}|+?'.includes(c)) {
      out += `\\${c}`;
    } else if (c === '[') {
      let j = i + 1;
      if (pattern[j] === '^') j++;
      if (pattern[j] === ']') j++;
      while (j < pattern.length && pattern[j] !== ']') j++;
      out += pattern.slice(i, j + 1).replace(/\\/g, '\\\\');
      i = j;
    } else out += c;
  }
  return out;
}

function normalise(text, rules) {
  if (!rules.length) return text;
  return text
    .split('\n')
    .map((line) => rules.reduce((l, { re, rep }) => l.replace(re, rep), line))
    .join('\n');
}

// A line diff, by longest common subsequence: `-` expected, `+` actual.
function diff(a, b) {
  const n = a.length;
  const m = b.length;
  const lcs = Array.from({ length: n + 1 }, () => new Uint32Array(m + 1));
  for (let i = n - 1; i >= 0; i--) {
    for (let j = m - 1; j >= 0; j--) {
      lcs[i][j] = a[i] === b[j] ? lcs[i + 1][j + 1] + 1 : Math.max(lcs[i + 1][j], lcs[i][j + 1]);
    }
  }
  const out = [];
  let i = 0;
  let j = 0;
  while (i < n && j < m) {
    if (a[i] === b[j]) {
      out.push(` ${a[i]}`);
      i++;
      j++;
    } else if (lcs[i + 1][j] >= lcs[i][j + 1]) out.push(`-${a[i++]}`);
    else out.push(`+${b[j++]}`);
  }
  while (i < n) out.push(`-${a[i++]}`);
  while (j < m) out.push(`+${b[j++]}`);
  return out;
}
