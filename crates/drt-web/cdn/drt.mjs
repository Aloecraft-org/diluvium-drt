// drt.js: DRT in a page as one ES module. build.mjs bundles this file, the
// browser access client (crates/drt-rtc/client/drt_browser_access.js), the
// wasm-bindgen glue of drt-web and of drt-ssh-web, and both modules' wasm,
// inlined as base64 of their gzip, into dist/drt.js and dist/drt.min.js. The
// file fetches nothing of its own: no sibling .wasm, no relative URL. A page
// imports it from wherever it is served:
//
//   import { call, loadSsh, kv } from 'https://cdn.example/drt@0.8.0/drt.js';
//
// ## surface block
//
// - Entry points:
//   - `call(peer, options)` -> Session: the caller's request of
//     doc/DRT-Signalling.md §3 to a parked or listening peer, or direct
//     mode for a record (doc/P2P.md §3's addresses, read by
//     `canonicalPeer`). Options: `token` (sent as `Authorization:
//     Bearer`), `headers`, `iceServers`, `fingerprint`, `holdMs`,
//     `timeoutMs`, `signal`. The session carries `service`, the name a
//     `drt+<service>://` address asked for. A refusal is a CallError.
//   - `connect(session, target)` -> Stream: `target` a service name, a
//     `host:port`, or nothing for what the peer forwards to.
//   - `loadSsh()` -> { Ssh, generateKey }: crates/drt-ssh-web, unpacked
//     on first use. `ssh(stream | wsUrl, pinned)` is `Ssh.connect` after it.
//   - `loadRuntime()` -> crates/drt-web's exports (DrtTerm, DrtSwarm, ...),
//     unpacked on first use.
//   - `kv(server, topic, options)` -> { get, put, delete, list, watch }:
//     the key-value store of doc/DRT-Signalling.md §10. Options: `token`,
//     `headers`.
//   - `session(options)` -> { info, token, logout }: stdlib:session's
//     /api/session, /api/token and /logout, with the browser's cookie;
//     `options.base` defaults to the page's origin, where a proxy must
//     mount stdlib:session's /api/* (crates/drt-web/npm/README.md says
//     why and how).
//   - Everything drt_browser_access.js exports: offer, direct, answer,
//     listen, canonicalPeer, parseRecord, ... (doc/BrowserAccess.md).
// - Configurable: HOLD_MARGIN_MS, what `call` allows past a server's hold
//   time (30 s at most, §3) before it gives up.
// - Fan-out: REFUSALS, what each status of a caller's request means;
//   `call`'s three kinds of address (signal, record, ws), of which ws is
//   refused: a relay is never used unless the page dials one itself.
//
// No ICE server is added here: `iceServers` is the caller's, and absent it
// is none (doc/BrowserAccess.md §3; no implicit relay or STUN).

import * as access from '../../drt-rtc/client/drt_browser_access.js';
import * as webGlue from 'drt:web-glue';
import * as sshGlue from 'drt:ssh-glue';
import { WEB_WASM_GZ_B64, SSH_WASM_GZ_B64, VERSION } from 'drt:wasm';

export * from '../../drt-rtc/client/drt_browser_access.js';
export { VERSION };

export const HOLD_MARGIN_MS = 5000;
/** The hold time doc/DRT-Signalling.md §3 allows a server at most. */
const HOLD_MAX_MS = 30000;

/** doc/DRT-Signalling.md §2's statuses, as a caller reads them. */
export const REFUSALS = Object.freeze({
  400: 'the request was malformed',
  401: 'no token, or one the server does not know',
  403: 'this token may not call',
  404: 'no such name on this server',
  410: 'the answerer refused the call',
  413: 'the record is too large',
  429: 'too many calls are waiting',
  503: 'no answerer is present',
  504: 'nobody answered in time',
});

/** A caller's request the server refused. `status` is the HTTP status. */
export class CallError extends Error {
  constructor(url, status, body) {
    const why = REFUSALS[status];
    super(`${url} answered ${status}${why ? `: ${why}` : ''}`);
    this.name = 'CallError';
    this.status = status;
    this.body = body;
  }
}

/** A kv request the server refused: 401, 403, 404 (no store), 412, ... */
export class KvError extends Error {
  constructor(what, status, body, etag) {
    super(`kv ${what}: ${status}${body ? ` ${body}` : ''}`);
    this.name = 'KvError';
    this.status = status;
    /** On a 412, the key's current version when it exists. */
    this.version = versionOf(etag);
  }
}

/** A stdlib:session request that was not answered 2xx. */
export class SessionError extends Error {
  constructor(what, status, body) {
    super(`session ${what}: ${status}${body ? ` ${body}` : ''}`);
    this.name = 'SessionError';
    this.status = status;
  }
}

// depth: calling a peer

export async function call(peer, options = {}) {
  options.signal?.throwIfAborted();
  const p = access.canonicalPeer(peer);
  if (p.kind === 'ws') {
    throw new Error(`${p.canonical} is a relay; call() never uses one. Dial it with loadSsh() or a WebSocket yourself.`);
  }
  if (p.kind === 'record') {
    const session = await access.direct(p.record, options);
    session.service = null;
    return session;
  }
  const fetchFn = options.fetch ?? globalThis.fetch.bind(globalThis);
  const pending = await access.offer(options);
  let reply;
  try {
    const abort = new AbortController();
    const stop = () => abort.abort();
    options.signal?.addEventListener('abort', stop);
    const timer = setTimeout(stop, (options.holdMs ?? HOLD_MAX_MS) + HOLD_MARGIN_MS);
    try {
      const headers = { 'content-type': 'text/plain;charset=utf-8', ...(options.headers ?? {}) };
      if (options.token) headers.authorization = `Bearer ${options.token}`;
      const res = await fetchFn(p.url, { method: 'POST', body: pending.recordText, headers, signal: abort.signal });
      reply = await res.text();
      if (!res.ok) throw new CallError(p.canonical, res.status, reply);
    } finally {
      clearTimeout(timer);
      options.signal?.removeEventListener('abort', stop);
    }
  } catch (e) {
    pending.close();
    throw e;
  }
  const session = await pending.accept(reply, options);
  session.service = p.service;
  return session;
}

export function connect(session, target) {
  if (target === undefined || target === null) {
    return session.service ? session.connect(session.service) : session.connect();
  }
  if (access.isServiceName(target)) return session.connect(target);
  const at = String(target).lastIndexOf(':');
  if (at < 0) throw new Error(`'${target}' is neither a service name nor host:port`);
  return session.connect(target.slice(0, at).replace(/^\[|\]$/g, ''), Number(target.slice(at + 1)));
}

// depth: the two wasm modules, unpacked on first use

let sshReady = null;
let webReady = null;

async function unpack(b64) {
  const gz = Uint8Array.from(atob(b64), (c) => c.charCodeAt(0));
  const raw = new Blob([gz]).stream().pipeThrough(new DecompressionStream('gzip'));
  return new Response(raw).arrayBuffer();
}

// A failed load is not kept: the next call tries again (an unpack can
// fail on a page that ran out of memory, and that need not be final).
export function loadSsh() {
  sshReady ??= unpack(SSH_WASM_GZ_B64)
    .then((bytes) => sshGlue.default({ module_or_path: bytes }))
    .then(() => ({ Ssh: sshGlue.Ssh, generateKey: sshGlue.generateKey }))
    .catch((e) => {
      sshReady = null;
      throw e;
    });
  return sshReady;
}

export async function ssh(target, pinned) {
  const { Ssh } = await loadSsh();
  return Ssh.connect(target, pinned || undefined);
}

export function loadRuntime() {
  webReady ??= unpack(WEB_WASM_GZ_B64)
    .then((bytes) => webGlue.default({ module_or_path: bytes }))
    .then(() => webGlue)
    .catch((e) => {
      webReady = null;
      throw e;
    });
  return webReady;
}

// depth: the key-value store (doc/DRT-Signalling.md §10)

function versionOf(etag) {
  const m = /^(?:W\/)?"?(\d+)"?$/.exec(etag ?? '');
  return m ? Number(m[1]) : null;
}

function serverBase(server) {
  const p = access.canonicalPeer(server);
  if (p.kind !== 'signal') throw new Error(`${server} is not a server address`);
  return p.canonical.replace(/\/v1(\/.*)?$/, '');
}

export function kv(server, topic, options = {}) {
  const fetchFn = options.fetch ?? globalThis.fetch.bind(globalThis);
  const root = `${serverBase(server)}/v1/${encodeURIComponent(topic)}/kv`;
  const keyUrl = (key) => `${root}/${String(key).split('/').map(encodeURIComponent).join('/')}`;
  const send = (method, url, extra = {}) => {
    const headers = { ...(options.headers ?? {}), ...(extra.headers ?? {}) };
    if (options.token) headers.authorization = `Bearer ${options.token}`;
    return fetchFn(url, { method, headers, body: extra.body, signal: extra.signal });
  };
  const query = (params) => {
    const q = new URLSearchParams();
    for (const [k, v] of Object.entries(params)) if (v !== undefined && v !== null) q.set(k, String(v));
    const s = q.toString();
    return s ? `?${s}` : '';
  };
  return {
    /** { value, version }, or null when the key is absent. */
    async get(key) {
      const res = await send('GET', keyUrl(key));
      if (res.status === 404) return null;
      const body = await res.text();
      if (!res.ok) throw new KvError(`get ${key}`, res.status, body);
      return { value: body, version: versionOf(res.headers.get('etag')) };
    },
    /** { key, version }. `ttl` seconds; `ifMatch` a version (or '*'); `ifNoneMatch: true` creates only. */
    async put(key, value, { ttl, ifMatch, ifNoneMatch } = {}) {
      const headers = { 'content-type': 'text/plain;charset=utf-8' };
      if (ifMatch !== undefined) headers['if-match'] = ifMatch === '*' ? '*' : `"${ifMatch}"`;
      if (ifNoneMatch) headers['if-none-match'] = '*';
      const res = await send('PUT', keyUrl(key) + query({ ttl }), { headers, body: String(value) });
      const body = await res.text();
      if (!res.ok) throw new KvError(`put ${key}`, res.status, body, res.headers.get('etag'));
      return JSON.parse(body);
    },
    /** true when deleted, false when it was absent. */
    async delete(key, { ifMatch } = {}) {
      const headers = {};
      if (ifMatch !== undefined) headers['if-match'] = ifMatch === '*' ? '*' : `"${ifMatch}"`;
      const res = await send('DELETE', keyUrl(key), { headers });
      if (res.status === 404) return false;
      const body = await res.text();
      if (!res.ok) throw new KvError(`delete ${key}`, res.status, body, res.headers.get('etag'));
      return true;
    },
    /** { revision, keys: [{ key, value, version, expires_in? }], more }. */
    async list({ prefix, after, limit } = {}) {
      const res = await send('GET', root + query({ prefix, after, limit }));
      const body = await res.text();
      if (!res.ok) throw new KvError('list', res.status, body);
      return JSON.parse(body);
    },
    /**
     * Every change under `prefix` after revision `since`, to `onChange`
     * ({ revision, op, key, version?, value? }), by long polls, until
     * `signal` aborts. `onReset()` is called when `since` is older than
     * the server keeps: list again from the listing's revision. Resolves
     * when stopped; rejects on a refusal.
     */
    async watch({ prefix = '', since = 0, onChange, onReset, signal, waitMs } = {}) {
      let at = since;
      while (!signal?.aborted) {
        let res;
        try {
          res = await send('GET', root + query({ watch: 'poll', since: at, prefix, wait: waitMs }), { signal });
        } catch (e) {
          if (signal?.aborted) return;
          throw e;
        }
        const body = await res.text();
        if (!res.ok) throw new KvError('watch', res.status, body);
        const batch = JSON.parse(body);
        if (batch.reset) {
          onReset?.();
          at = batch.revision ?? at;
          continue;
        }
        for (const e of batch.events ?? []) onChange?.(e);
        at = batch.revision ?? at;
      }
    },
  };
}

// depth: stdlib:session (doc/Session.md)

export function session(options = {}) {
  const fetchFn = options.fetch ?? globalThis.fetch.bind(globalThis);
  const base = String(options.base ?? globalThis.location.origin).replace(/\/+$/, '');
  const send = (method, path, init = {}) => fetchFn(`${base}${path}`, { method, credentials: 'include', ...init });
  return {
    /** The session ({ user, groups, amr, signed_in, expires_in, ... }), or null when there is none. */
    async info() {
      const res = await send('GET', '/api/session');
      if (res.status === 401) return null;
      const body = await res.text();
      if (!res.ok) throw new SessionError('info', res.status, body);
      return JSON.parse(body);
    },
    /** A JWT for this session ({ token, expires_in, token_type }); usable as a topic token. */
    async token() {
      const res = await send('POST', '/api/token');
      const body = await res.text();
      if (!res.ok) throw new SessionError('token', res.status, body);
      return JSON.parse(body);
    },
    /** Ends the session, and revokes the user's tokens when the server issues them. */
    async logout() {
      // /logout answers with a redirect to the login page, which a proxy
      // may send on to another origin; the redirect is the answer, so it
      // is not followed.
      const res = await send('POST', '/logout', { redirect: 'manual' });
      if (res.type !== 'opaqueredirect' && !res.ok) throw new SessionError('logout', res.status, await res.text());
    },
  };
}
