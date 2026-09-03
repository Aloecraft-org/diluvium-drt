#!/usr/bin/env node
// run.mjs: the browser suite. examples/*/meta.json through drt-web, in
// Chromium, diffed against expected.txt (doc/Wasm.md §5, M4).
//
// The page-side twin of examples/run-all.sh, under the same rules: every
// examples/NN-*/ with a meta.json is one example; its files are seeded
// into the page's memory filesystem; its "cmd" runs through the in-page
// shell with stdout and stderr merged; "normalise" is applied to both
// sides; the two are diffed. A skip is named and is never a pass. Then
// the REPL parity check: repl-script.txt typed at drt-term.js, against
// the transcript the native binary produced for the same lines
// (repl-expected.txt).
//
// usage: node run.mjs [--net] [--list] [example ...]
//   --net    also run examples whose meta.json sets "needs_network"
//   --list   name the examples that would run, and exit
//   example  a substring of the directory name: "04", "files"
// env: TIMEOUT  seconds one example may take (default 120; 0 disables)
//      DRT_WEB_BUILDINFO  a path: the page's `drt buildinfo` is written there,
//                         which is how a release reads the profile off the
//                         module (release.yml, build-web)
// needs: pkg/ from script/drt-web.sh; `npm ci` for Playwright, whose
//        Chromium is `npx playwright install chromium`.

import { chromium } from 'playwright';
import fs from 'node:fs';
import http from 'node:http';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const HERE = path.dirname(fileURLToPath(import.meta.url));
const EXAMPLES = path.resolve(HERE, '../../../examples');
const MAXDIFF = 200;
const TIMED_OUT = Symbol('timed out');
const TIMEOUT = Number(process.env.TIMEOUT ?? 120);
const TYPES = {
  '.html': 'text/html; charset=utf-8',
  '.js': 'text/javascript',
  '.mjs': 'text/javascript',
  '.wasm': 'application/wasm',
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

const server = http.createServer((req, res) => {
  const url = decodeURIComponent(new URL(req.url, 'http://x').pathname);
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

const browser = await chromium.launch();
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
  if (meta.needs_build && meta.needs_build !== profile && profile !== 'unknown') {
    console.log(`skipped  ${name.padEnd(24)} (needs a ${meta.needs_build} build; this drt is ${profile})`);
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

// The REPL, typed at drt-term.js, against what the native binary said to
// the same lines. The page echoes what is typed and the native transcript
// (stdin from a file) does not, so the echoes are removed before the diff;
// everything else -- prompts, answers, the C core's print, the error --
// must match byte for byte.
{
  const name = 'repl-parity';
  const scriptLines = fs.readFileSync(path.join(HERE, 'repl-script.txt'), 'utf8').replace(/\n$/, '').split('\n');
  const expected = fs.readFileSync(path.join(HERE, 'repl-expected.txt'), 'utf8');
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
    actual = actual.replace(/\$ $/, '');
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
const nSkip = skipped.length + wrongBuild.length;
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
