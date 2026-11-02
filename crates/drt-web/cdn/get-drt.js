// get-drt.js: drt.js for a page, from a DRT release, picked the way
// get-drt.sh picks one. A release asset; a page imports it:
//
//   import { getDrt } from 'https://software.aloecraft.org/releases/diluvium-drt/latest/get-drt.js';
//   const drt = await getDrt();              // the release this copy came from
//   const dev = await getDrt('dev');         // or 'latest', '0.8.0', 'v0.8.0-dev.59'
//   const cli = await drt.terminal('cli');
//
// For pages built on a release. Nothing in this repository loads drt this
// way: examples and tests use the drt.js the checkout builds.
//
// release.yml stamps STAMPED_VERSION in each copy, as it stamps install.sh:
// a module cannot see which release served it, so the stamp is how a copy
// knows. The checkout's copy is unstamped and defaults to `latest`.
//
// Why two hosts. GitHub release downloads send no
// Access-Control-Allow-Origin, on github.com's redirect or on the asset
// host behind it, so a page cannot fetch them (checked in Chromium,
// 2026-11-02). The release mirror can, and carries stable releases and the
// newest candidate; dev builds are only on GitHub, so release.yml also
// deploys every release's drt.js to GitHub Pages, which sends
// `access-control-allow-origin: *`. The mirror first, then Pages, the
// order get-drt.sh tries the mirror and then GitHub.
//
// ## surface block
//
// - Entry points:
//   - `getDrt(version?, options?)` or `getDrt(options)` -> drt.js's module
//     namespace (crates/drt-web/cdn/drt.mjs lists its exports). `version`
//     is get-drt.sh's VERSION: `latest` the newest stable release, `dev`
//     the newest dev build, `0.8.0` or `v0.8.0-dev.59` that release, and
//     nothing this copy's release (or `latest`, unstamped). The bytes are
//     checked against the release's SHA256SUMS.txt before they run.
//   - `fetchDrt(version?, options?)` -> { tag, source, asset, bytes,
//     verified }: the same download and check, without importing.
//   - `resolve(version?, options?)` -> the tag `version` names.
//   - `list(options?)` -> stable release tags, newest first (`list`).
//   - `listAll(options?)` -> [{ tag, channel }] for every release,
//     candidates and dev builds included (`list-all`).
//   - `version`: the release this copy came from, '' unstamped
//     (`--version` alone).
//   - `usage`: what get-drt.sh's help says, for this module.
// - Configurable values: the stamp, the hosts below, and per call
//   `options` = { version, min, mirror, pages, releaseLog }. `min: false`
//   loads drt.js instead of drt.min.js; the rest replace a host, as
//   DRT_MIRROR and DRT_RELEASE_LOG do for get-drt.sh.
// - Fan-out: resolve() by the shape of `version` (latest, dev, a tag);
//   resolveLatest() and listAll() over their sources in order.

// Stamped by release.yml. Empty means this copy came from no release.
const STAMPED_VERSION = '';

const REPO = 'Aloecraft-org/diluvium-drt';
const MIRROR = 'https://software.aloecraft.org/releases/diluvium-drt';
const PAGES = 'https://aloecraft-org.github.io/diluvium-drt';
const RAW = `https://raw.githubusercontent.com/${REPO}`;
// Every published release, newest first: tag, channel, date. release.yml
// rewrites it on its own branch; raw GitHub content sends CORS.
const RELEASE_LOG = `${RAW}/release-log/.github/release-log`;
// GitHub's API sends CORS but is rate limited per address, so it is the
// last word on `latest` and never the first.
const API_LATEST = `https://api.github.com/repos/${REPO}/releases/latest`;
const ASSET = { min: 'drt.min.js', full: 'drt.js' };

export const version = STAMPED_VERSION;

export const usage = `get-drt.js: drt.js from a DRT release, for a page.

  getDrt()            ${STAMPED_VERSION ? `${STAMPED_VERSION}, the release this copy came from` : 'latest (an unstamped copy)'}
  getDrt(VERSION)     VERSION: latest, dev, 0.8.0 or v0.8.0-dev.59
  getDrt({ min: false })  drt.js rather than drt.min.js
  fetchDrt(VERSION)   the verified bytes, not imported
  resolve(VERSION)    the tag VERSION names
  list()              stable releases
  listAll()           every release, candidates and dev builds included
  version             the release this copy came from

Options: version, min, mirror, pages, releaseLog.
From ${MIRROR}/<tag>/ first, then ${PAGES}/<tag>/.`;

export async function getDrt(versionOrOptions, options) {
  const got = await fetchDrt(versionOrOptions, options);
  return importBytes(got.bytes);
}

export async function fetchDrt(versionOrOptions, options) {
  const o = opts(versionOrOptions, options);
  const tag = await resolve(o.version, o);
  return download(tag, o.min ? ASSET.min : ASSET.full, o);
}

export async function resolve(versionOrOptions, options) {
  const o = opts(versionOrOptions, options);
  const v = o.version;
  if (v === 'latest') return resolveLatest(o);
  if (v === 'dev') {
    const row = (await releaseLog(o)).find((r) => r.channel === 'dev');
    if (!row) throw new Error(`get-drt.js: cannot find a dev build in ${o.releaseLog}`);
    return row.tag;
  }
  return v.startsWith('v') ? v : `v${v}`;
}

export async function list(options) {
  return (await listAll(options)).filter((r) => r.channel === 'stable').map((r) => r.tag);
}

// The release log first: it is the one source that has dev builds. The
// mirror's index next, then the changelog on main.
export async function listAll(options) {
  const o = opts(options);
  for (const rows of [
    () => releaseLog(o),
    async () => jsonReleases(await getJson(`${o.mirror}/releases.json`)),
    async () => jsonReleases(await getJson(`${RAW}/main/changelog.json`)),
  ]) {
    const r = await rows().catch(() => []);
    if (r.length) return r;
  }
  throw new Error('get-drt.js: cannot list releases: the release log, the mirror and GitHub all failed');
}

// depth: options, sources and parsing

function opts(versionOrOptions, options) {
  const o = typeof versionOrOptions === 'object' && versionOrOptions !== null
    ? versionOrOptions
    : { ...(options || {}), version: versionOrOptions ?? options?.version };
  return {
    version: o.version || STAMPED_VERSION || 'latest',
    min: o.min !== false,
    mirror: trim(o.mirror || MIRROR),
    pages: trim(o.pages || PAGES),
    releaseLog: o.releaseLog || RELEASE_LOG,
  };
}

const trim = (url) => String(url).replace(/\/+$/, '');

async function getText(url) {
  const res = await fetch(url, { cache: 'no-cache' });
  if (!res.ok) throw new Error(`${url}: HTTP ${res.status}`);
  return res.text();
}

async function getJson(url) {
  return JSON.parse(await getText(url));
}

// "tag<TAB>channel<TAB>date" lines, comments and blanks dropped.
async function releaseLog(o) {
  return (await getText(o.releaseLog))
    .split('\n')
    .filter((l) => l.trim() && !l.startsWith('#'))
    .map((l) => {
      const [tag, channel] = l.split('\t');
      return { tag, channel };
    });
}

// A releases.json or a changelog.json: entries in `releases`, newest
// first. One not released yet is left out, as get-drt.sh leaves it out.
function jsonReleases(doc) {
  return (doc.releases || [])
    .filter((r) => r.tag && r.stable !== undefined && r.status !== 'unreleased')
    .map((r) => ({
      tag: r.tag,
      channel: /-dev\.\d+$/.test(r.tag) ? 'dev' : r.stable ? 'stable' : 'rc',
    }));
}

// Every source that can name the newest stable release, in get-drt.sh's
// order: the mirror, then what GitHub can serve, and the changelog on main
// last, since it can call a release out before its assets are published.
async function resolveLatest(o) {
  for (const source of [
    async () => (await getJson(`${o.mirror}/releases.json`)).latest,
    async () => (await releaseLog(o)).find((r) => r.channel === 'stable')?.tag,
    async () => (await getJson(API_LATEST)).tag_name,
    async () => (await getJson(`${RAW}/main/changelog.json`)).latest,
  ]) {
    const tag = await source().catch(() => '');
    if (tag) return tag;
  }
  throw new Error('get-drt.js: cannot tell which release is latest: the mirror, GitHub and the release log all failed');
}

// depth: download and verify

async function download(tag, asset, o) {
  const tried = [];
  for (const base of [`${o.mirror}/${tag}`, `${o.pages}/${tag}`]) {
    let res;
    try {
      res = await fetch(`${base}/${asset}`);
    } catch (e) {
      tried.push(`${base}/${asset} (${e.message})`);
      continue;
    }
    if (!res.ok) {
      tried.push(`${base}/${asset} (HTTP ${res.status})`);
      continue;
    }
    const bytes = new Uint8Array(await res.arrayBuffer());
    const verified = await verify(bytes, asset, base);
    return { tag, source: base, asset, bytes, verified };
  }
  throw new Error(`get-drt.js: no ${asset} for ${tag}; tried ${tried.join(', ')}. ` +
    'Releases before v0.8.0 carry no drt.js (listAll() names them).');
}

// As get-drt.sh: a missing sums file, an asset it does not list, or no
// SHA-256 (crypto.subtle exists only in a secure context) warns rather
// than refuses. A mismatch always refuses.
async function verify(bytes, asset, base) {
  const sums = await getText(`${base}/SHA256SUMS.txt`).catch(() => '');
  const line = sums.split('\n').find((l) => l.trim().split(/\s+/)[1]?.replace(/^\*/, '') === asset);
  if (!line) {
    console.warn(`get-drt.js: ${base}/SHA256SUMS.txt does not list ${asset}; not verified`);
    return false;
  }
  if (!globalThis.crypto?.subtle) {
    console.warn(`get-drt.js: no SHA-256 outside a secure context; ${asset} not verified`);
    return false;
  }
  const want = line.trim().split(/\s+/)[0].toLowerCase();
  const digest = new Uint8Array(await crypto.subtle.digest('SHA-256', bytes));
  const have = Array.from(digest, (b) => b.toString(16).padStart(2, '0')).join('');
  if (have !== want) {
    throw new Error(`get-drt.js: checksum mismatch for ${asset} from ${base}\n  expected ${want}\n  got      ${have}`);
  }
  return true;
}

// The checked bytes run, not a second fetch of the URL: importing the URL
// would run whatever the host serves next. drt.js fetches nothing relative
// to itself (build.mjs refuses import.meta.url), so a blob: URL loses
// nothing. A page whose CSP has script-src needs blob: in it.
async function importBytes(bytes) {
  const url = URL.createObjectURL(new Blob([bytes], { type: 'text/javascript' }));
  try {
    return await import(/* @vite-ignore */ url);
  } finally {
    URL.revokeObjectURL(url);
  }
}
