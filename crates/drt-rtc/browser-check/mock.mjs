// The M0 signaling mock (doc/BrowserAccess.md §7): rooms, peers, presence
// with a TTL, and nothing else. Not Discofetch's API -- the shape both sides
// agreed to point at until Discofetch has the `rtc` field and CORS.
//
//   node mock.mjs [port]        (default 18787)
//
// No dependencies. CORS is open, because the page calling it is on another
// origin, exactly as a Discofetch service origin will be.
//
// ## surface block
//
// - Entry points: the three routes of doc/BrowserAccess.md §7, and `GET /`
//   (a blank page, for a check to run from on this origin).
// - Configurable: PORT (argv or MOCK_PORT), TTL_S (PRESENCE_TTL_S), MAX_RTC.
// - Fan-out: the route match in the request handler.

import http from 'node:http';
import { randomBytes } from 'node:crypto';

const PORT = Number(process.argv[2] ?? process.env.MOCK_PORT ?? 18787);
const TTL_S = Number(process.env.PRESENCE_TTL_S ?? 30);
const MAX_RTC = 1024; // the Discofetch spec's cap on the field

const rooms = new Map(); // room -> Map(peer_id -> {token, kind, rtc, expires})
const tokens = new Map(); // token -> {room, peer_id}

function room(name) {
  if (!rooms.has(name)) rooms.set(name, new Map());
  return rooms.get(name);
}

function send(res, status, body) {
  res.writeHead(status, {
    'content-type': 'application/json',
    'access-control-allow-origin': '*',
    'access-control-allow-headers': 'authorization, content-type',
    'access-control-allow-methods': 'GET, POST, OPTIONS',
  });
  res.end(body === undefined ? '' : JSON.stringify(body));
}

function bearer(req) {
  const m = /^Bearer (.+)$/i.exec(req.headers.authorization ?? '');
  return m && tokens.get(m[1]);
}

http
  .createServer((req, res) => {
    if (req.method === 'OPTIONS') return send(res, 204);
    // A blank page on this origin, for a check to run from: Chromium will
    // not let a page with no origin (about:blank) call loopback at all.
    if (req.method === 'GET' && req.url === '/') {
      res.writeHead(200, { 'content-type': 'text/html' });
      return res.end('<!doctype html><title>m0</title>');
    }
    const m = /^\/v1\/rooms\/([^/]+)\/(join|presence)$/.exec(new URL(req.url, 'http://x').pathname);
    if (!m) return send(res, 404, { error: 'no such path' });
    const [, name, what] = m;
    let body = '';
    req.on('data', (c) => (body += c));
    req.on('end', () => {
      const now = Date.now();
      const peers = room(name);
      for (const [id, p] of peers) if (p.expires < now) peers.delete(id);

      if (what === 'join' && req.method === 'POST') {
        const peer_id = 'p' + randomBytes(4).toString('hex');
        const token = randomBytes(16).toString('hex');
        tokens.set(token, { room: name, peer_id });
        peers.set(peer_id, { kind: null, rtc: null, expires: now + TTL_S * 1000 });
        console.log(`mock: ${name}: ${peer_id} joined`);
        return send(res, 200, { peer_id, session_token: token, presence_ttl_s: TTL_S });
      }
      const who = bearer(req);
      if (!who || who.room !== name) return send(res, 401, { error: 'bad or missing bearer token' });
      if (req.method === 'POST') {
        let p;
        try {
          p = JSON.parse(body);
        } catch {
          return send(res, 400, { error: 'body is not JSON' });
        }
        if (!['host', 'browser'].includes(p.kind) || typeof p.rtc !== 'string' || p.rtc.length > MAX_RTC) {
          return send(res, 400, { error: 'want {kind: host|browser, rtc: string <= 1 KiB}' });
        }
        peers.set(who.peer_id, { kind: p.kind, rtc: p.rtc, expires: now + TTL_S * 1000 });
        console.log(`mock: ${name}: ${who.peer_id} published ${p.kind} (${p.rtc.length} bytes)`);
        return send(res, 204);
      }
      const list = [...peers]
        .filter(([, p]) => p.kind)
        .map(([peer_id, p]) => ({ peer_id, kind: p.kind, rtc: p.rtc, expires_at: new Date(p.expires).toISOString() }));
      return send(res, 200, { peers: list });
    });
  })
  .listen(PORT, '127.0.0.1', () => console.log(`mock: listening on http://127.0.0.1:${PORT}`));
