// The browser half of DRT browser access (doc/BrowserAccess.md §2-§6): a
// peer connection to a DRT host, its `hello`, and TCP streams to the host's
// scope over Wisp v1. One ES module, no dependencies, no build step.
//
// Signaling is mostly the page's. This module makes the browser's record
// and takes the host's; how the two cross -- the Discofetch API's socket
// and presence (§7.1), or a server of doc/DRT-Signalling.md -- is the
// page's business. The one exception is `listen`, the answerer's half of
// that profile, which is the only part of this module that makes requests.
//
//   const pending = await offer();              // gathers, then resolves
//   send(pending.record);                       // to the API, as the page does
//   const session = await pending.accept(hostRecord);
//   session.hello.services;                     // what the host serves, by name
//   const t = session.connect();                // whatever it forwards to
//   const s = session.connect('127.0.0.1', 8123);
//   s.writable / s.readable / await s.closed    // Web Streams, bytes
//
// Or, for a host with direct mode on (§3.4), no signaling at all:
//
//   const session = await direct(hostRecord);
//
// Or the page answers, and serves (§10): the caller's record in, the
// page's record back out through signaling, and streams to its services.
//
//   const a = await answer(callerRecord, { services: { ssh: (stream) => … } });
//   send(a.record);
//   const session = await a.session;
//
// Or the page answers every call that reaches it through a server of
// doc/DRT-Signalling.md, with no signalling code of its own:
//
//   const l = listen('https://signal.example/v1/page', { token, services: { ssh } });
//   l.close();
//
// ## surface block
//
// - Entry points: `offer(options)` -> Pending {record, recordText,
//   accept(hostRecord, options)} -> Session {hello, connect(host, port),
//   close(), closed}; `direct(hostRecord, options)` -> Session, for direct
//   mode; `answer(callerRecord, options)` -> Answering {record, recordText,
//   session}, for a page that answers (§10.4); `listen(base, options)` ->
//   Listening {cursor, streaming, poll(), close()}, which answers every
//   call a signalling server holds for one name (doc/DRT-Signalling.md
//   §4, §5); Session.connect(host, port)
//   or Session.connect(service) -> Stream {id, readable, writable, close(),
//   closed}; `options.services`, name -> (stream, session), for what a page
//   serves (§10.3). And the pure pieces, for a client that drives its own
//   RTCPeerConnection: `parseRecord`, `recordFromSdp`, `answerSdp`,
//   `offerSdp`, `withIceCredentials`, `fingerprintHex`, `isUsableCandidate`,
//   `isServiceName`, `encodeWisp`, `decodeWisp`.
// - Configurable: GATHER_CAP_MS, how long `offer` waits for ICE gathering
//   before it publishes what it has (§3.1); ACCEPT_TIMEOUT_MS, how long
//   `accept` and `direct` wait for the channels, `hello` and the first
//   CONTINUE;
//   SEND_HIGH_WATER, the data channel buffer past which a stream's writer
//   waits; SERVE_BUFFER and SERVE_MAX_STREAMS, what a page that serves
//   grants each stream and how many it holds open; LISTEN_POLL_MS, how
//   often `listen` polls while it holds no call notification stream. The
//   wire's own limits -- RECORD_MAX_BYTES, MAX_CANDIDATES, the
//   ICE lengths, DIRECT_UFRAG_LEN, MESSAGE_MAX -- are constants of v1, not
//   knobs: changing one is a `v` bump.
// - Fan-out: `onWispPacket`, one branch per packet type the host sends
//   (CONNECT, DATA, CONTINUE, CLOSE); RecordError's `code`, one per rule a record can
//   break, named as crates/drt-rtc/src/record.rs names them; CLOSE_REASON,
//   the reasons a stream can end with (§6); `listen`'s handling of one
//   call: refused by `accept` -> DELETE, a record `answer` cannot take ->
//   DELETE, otherwise answered -> `onSession`.

export const GATHER_CAP_MS = 2000;
export const ACCEPT_TIMEOUT_MS = 15000;
export const SEND_HIGH_WATER = 1 << 20;
/** Packets of DATA a page that serves lets each stream queue (§10.2, as §6). */
export const SERVE_BUFFER = 128;
/** Streams a page that serves holds open at once; one more is 0x49. */
export const SERVE_MAX_STREAMS = 64;
/**
 * How often `listen` polls while it holds no call notification stream:
 * well inside DRT-Signalling.md §4.2's 30 seconds, so the server counts
 * the page present, and short enough that a caller waits little.
 */
export const LISTEN_POLL_MS = 3000;

export const RECORD_VERSION = 1;
export const RECORD_MAX_BYTES = 512;
export const MAX_CANDIDATES = 8;
export const UFRAG_LEN = [4, 32];
export const PWD_LEN = [22, 64];
/** A direct-mode browser's ufrag, which is also its password (§3.4). */
export const DIRECT_UFRAG_LEN = 32;
/** A service's name (§10.3). */
const SERVICE_NAME = /^[a-z0-9][a-z0-9-]{0,31}$/;
/** Every message on either channel is at most this (§4). */
export const MESSAGE_MAX = 16384;
/** A DATA packet's payload: the message cap less the 5-byte header. */
export const DATA_MAX = MESSAGE_MAX - 5;

/** Wisp v1 packet types (§6). */
export const WISP = Object.freeze({ CONNECT: 0x01, DATA: 0x02, CONTINUE: 0x03, CLOSE: 0x04 });

/** Why a stream ended (§6's table), by the byte the host sends. */
export const CLOSE_REASON = Object.freeze({
  0x01: 'unspecified',
  0x02: 'closed',          // the target closed the connection
  0x03: 'network error',   // the target connection failed once established
  0x41: 'invalid',         // a malformed CONNECT, or a stream id in use
  0x42: 'unreachable',     // the hostname does not resolve
  0x43: 'timeout',         // no answer within the connect timeout
  0x44: 'refused',         // the target refused the connection
  0x47: 'idle',            // idle past the host's idle timeout
  0x48: 'blocked',         // out of scope, a special address, or UDP
  0x49: 'throttled',       // the session's stream cap is reached
});

/** A record that breaks a rule of §2. `code` names the rule. */
export class RecordError extends Error {
  constructor(code, detail) {
    super(detail ? `record: ${code}: ${detail}` : `record: ${code}`);
    this.name = 'RecordError';
    this.code = code;
  }
}

/** A stream the host closed, or a session that ended under it. */
export class StreamClosed extends Error {
  constructor(reason, detail) {
    super(detail ?? `stream closed: ${CLOSE_REASON[reason] ?? `0x${reason.toString(16)}`}`);
    this.name = 'StreamClosed';
    /** The CLOSE byte, or `null` when the session ended instead. */
    this.reason = reason;
  }
}

// depth: the record (§2)

const ICE_CHAR = /^[A-Za-z0-9+/]*$/;

/**
 * A record from either of its two forms (§2): an object, as the Discofetch
 * API sends it, or its JSON text. Refused whole, with a RecordError, when
 * it breaks a rule. Returns `{v, u, p, f, c}` with nothing else.
 */
export function parseRecord(input) {
  let obj = input;
  let text;
  if (typeof input === 'string') {
    text = input;
    if (utf8Length(text) > RECORD_MAX_BYTES) throw new RecordError('TooLong', `${utf8Length(text)} bytes`);
    try {
      obj = JSON.parse(text);
    } catch (e) {
      throw new RecordError('NotJson', e.message);
    }
  }
  if (obj === null || typeof obj !== 'object' || Array.isArray(obj)) throw new RecordError('NotAnObject');
  if (text === undefined) {
    text = JSON.stringify(obj);
    if (utf8Length(text) > RECORD_MAX_BYTES) throw new RecordError('TooLong', `${utf8Length(text)} bytes`);
  }
  for (const k of ['v', 'u', 'p', 'f', 'c']) {
    if (!(k in obj)) throw new RecordError('Missing', k);
  }
  if (obj.v !== RECORD_VERSION) throw new RecordError('Version');
  for (const k of ['u', 'p', 'f']) {
    if (typeof obj[k] !== 'string') throw new RecordError('WrongType', k);
  }
  if (!Array.isArray(obj.c) || obj.c.some((l) => typeof l !== 'string')) throw new RecordError('WrongType', 'c');
  const { u, p, f, c } = obj;
  if (u.length < UFRAG_LEN[0] || u.length > UFRAG_LEN[1] || !ICE_CHAR.test(u)) throw new RecordError('Ufrag');
  if (p.length < PWD_LEN[0] || p.length > PWD_LEN[1] || !ICE_CHAR.test(p)) throw new RecordError('Pwd');
  if (f.length !== 44 || fromBase64(f)?.length !== 32) throw new RecordError('Fingerprint');
  if (c.length > MAX_CANDIDATES) throw new RecordError('TooManyCandidates', `${c.length}`);
  for (const line of c) {
    if (!line.startsWith('candidate:') || /[\r\n]/.test(line)) throw new RecordError('Candidate', line);
  }
  // §2.1: a well-formed line v1 cannot use is skipped, not refused, so no
  // answer built from this record carries one.
  return { v: RECORD_VERSION, u, p, f, c: c.filter(isUsableCandidate) };
}

/**
 * The browser's own record, from its local description once gathering is
 * done (§2, §2.1): UDP only, no relay, no mDNS, extensions stripped, at
 * most MAX_CANDIDATES. Checked by parseRecord before it is returned, so a
 * page never publishes something the host would refuse.
 */
export function recordFromSdp(sdp) {
  const get = (k) => {
    const m = sdp.match(new RegExp(`^a=${k}:(.*)$`, 'm'));
    if (!m) throw new RecordError('Missing', `a=${k} in the local description`);
    return m[1].trim();
  };
  const fp = get('fingerprint').split(/\s+/);
  if (fp[0].toLowerCase() !== 'sha-256') throw new RecordError('Fingerprint', `${fp[0]}, not sha-256`);
  const c = [...sdp.matchAll(/^a=(candidate:.*)$/gm)]
    .map((m) => m[1].trim())
    .map(usableCandidate)
    .filter((l) => l !== null)
    .slice(0, MAX_CANDIDATES);
  return parseRecord({ v: RECORD_VERSION, u: get('ice-ufrag'), p: get('ice-pwd'), f: toBase64(fromHex(fp[1])), c });
}

/**
 * Whether v1 can use a candidate line (§2.1): it reads as RFC 8839 §5.1
 * through `typ <type>`, over UDP, of type host, srflx or prflx (never
 * relay: v1 has no TURN), at an address that is not an mDNS `.local`
 * name. Trailing extensions are allowed. The same rule as
 * crates/drt-rtc/src/record.rs's `usable_candidate`.
 */
export function isUsableCandidate(line) {
  if (typeof line !== 'string' || !line.startsWith('candidate:')) return false;
  const t = line.slice('candidate:'.length).split(/[ \t\n\r\f]+/).filter(Boolean);
  if (t.length < 8 || t[6] !== 'typ') return false;
  const numeric = (s) => /^[0-9]+$/.test(s);
  return numeric(t[1]) && numeric(t[3]) && numeric(t[5]) && Number(t[5]) <= 65535
    && t[2].toLowerCase() === 'udp'
    && ['host', 'srflx', 'prflx'].includes(t[7].toLowerCase())
    && !t[4].toLowerCase().endsWith('.local');
}

/** §2.1, for the writer: the line as a record carries it, or null. */
function usableCandidate(line) {
  if (!isUsableCandidate(line)) return null;
  // Everything after `typ <type>` and its `raddr`/`rport` is stripped: the
  // extensions are what would push a record past its budget.
  return line.match(/^candidate:\S+ \d+ \S+ \d+ \S+ \d+ typ \S+(?: raddr \S+ rport \d+)?/i)[0];
}

/** The fingerprint as SDP spells it: upper-case hex pairs joined by ":". */
export function fingerprintHex(f) {
  return [...fromBase64(f)].map((b) => b.toString(16).toUpperCase().padStart(2, '0')).join(':');
}

/**
 * The answer the browser builds from the host's record and its own offer's
 * `a=mid` (§3.2). Byte for byte what crates/drt-rtc/src/record.rs's
 * `answer_sdp` builds, and the vectors hold both to it.
 */
export function answerSdp(hostRecord, mid) {
  return sdpFromRecord(hostRecord, mid, 'passive');
}

/**
 * The offer a page that answers applies (§10.4): the caller's record as an
 * offer from a DTLS client, so the page's own answer makes it the server.
 */
export function offerSdp(callerRecord, mid = '0') {
  return sdpFromRecord(callerRecord, mid, 'active');
}

/** Whether `name` can name a service (§10.3). */
export function isServiceName(name) {
  return typeof name === 'string' && SERVICE_NAME.test(name);
}

function sdpFromRecord(record, mid, setup) {
  const r = parseRecord(record);
  return [
    'v=0',
    'o=- 0 2 IN IP4 127.0.0.1',
    's=-',
    't=0 0',
    `a=group:BUNDLE ${mid}`,
    'm=application 9 UDP/DTLS/SCTP webrtc-datachannel',
    'c=IN IP4 0.0.0.0',
    `a=mid:${mid}`,
    `a=ice-ufrag:${r.u}`,
    `a=ice-pwd:${r.p}`,
    `a=fingerprint:sha-256 ${fingerprintHex(r.f)}`,
    `a=setup:${setup}`,
    'a=sctp-port:5000',
    'a=max-message-size:262144',
    ...r.c.map((c) => `a=${c}`),
    'a=end-of-candidates',
    '',
  ].join('\r\n');
}

// depth: Wisp v1 packets (§6)

/**
 * One Wisp packet. `CONNECT` takes `{host, port}` (TCP only, as the host
 * serves nothing else), `DATA` takes `{data}`, `CONTINUE` takes `{buffer}`,
 * `CLOSE` takes `{reason}`.
 */
export function encodeWisp(type, stream, body = {}) {
  let payload;
  if (type === WISP.CONNECT) {
    const name = new TextEncoder().encode(body.host);
    payload = new Uint8Array(3 + name.length);
    payload[0] = 0x01;
    payload[1] = body.port & 0xff;
    payload[2] = (body.port >> 8) & 0xff;
    payload.set(name, 3);
  } else if (type === WISP.DATA) {
    payload = body.data;
  } else if (type === WISP.CONTINUE) {
    payload = new Uint8Array(4);
    new DataView(payload.buffer).setUint32(0, body.buffer, true);
  } else if (type === WISP.CLOSE) {
    payload = Uint8Array.of(body.reason);
  } else {
    throw new TypeError(`unknown wisp packet type ${type}`);
  }
  const out = new Uint8Array(5 + payload.length);
  out[0] = type;
  new DataView(out.buffer).setUint32(1, stream >>> 0, true);
  out.set(payload, 5);
  return out;
}

/** A packet from the host, or null when it is shorter than its header. */
export function decodeWisp(bytes) {
  const b = bytes instanceof Uint8Array ? bytes : new Uint8Array(bytes);
  if (b.length < 5) return null;
  const view = new DataView(b.buffer, b.byteOffset, b.byteLength);
  const packet = { type: b[0], stream: view.getUint32(1, true), payload: b.subarray(5) };
  if (packet.type === WISP.CONTINUE && packet.payload.length >= 4) packet.buffer = view.getUint32(5, true);
  if (packet.type === WISP.CLOSE && packet.payload.length >= 1) packet.reason = b[5];
  if (packet.type === WISP.CONNECT && packet.payload.length >= 3) {
    packet.kind = b[5];
    packet.port = view.getUint16(6, true);
    try {
      packet.host = new TextDecoder('utf-8', { fatal: true }).decode(b.subarray(8));
    } catch {
      packet.host = null;
    }
  }
  return packet;
}

// depth: the peer connection (§3, §4, §5)

/**
 * Start a session: a peer connection with both negotiated channels, an
 * ordinary offer, and gathering awaited up to `gatherTimeoutMs` (§3.1).
 * The returned record goes to the host through signaling; `accept` takes
 * the host's record back.
 *
 * Options: `iceServers` (STUN only is useful: v1 has no relay),
 * `gatherTimeoutMs`, and `RTCPeerConnection` for a runtime without a
 * global one.
 */
export async function offer(options = {}) {
  const PC = options.RTCPeerConnection ?? globalThis.RTCPeerConnection;
  if (!PC) throw new Error('no RTCPeerConnection in this runtime');
  const pc = new PC({ iceServers: options.iceServers ?? [] });
  const control = pc.createDataChannel('control', { negotiated: true, id: 0 });
  const wisp = pc.createDataChannel('wisp', { negotiated: true, id: 1 });
  wisp.binaryType = 'arraybuffer';
  const early = earlyInbox(control, wisp);
  try {
    await pc.setLocalDescription(await pc.createOffer());
    await gathered(pc, options.gatherTimeoutMs ?? GATHER_CAP_MS);
    const sdp = pc.localDescription.sdp;
    const mid = sdp.match(/^a=mid:(.*)$/m)[1].trim();
    const record = recordFromSdp(sdp);
    let used = false;
    return {
      record,
      recordText: JSON.stringify(record),
      pc,
      async accept(hostRecord, acceptOptions = {}) {
        if (used) throw new Error('accept was already called; a session needs a fresh offer');
        used = true;
        const merged = { ...options, ...acceptOptions };
        try {
          await trusted(hostRecord, merged);
        } catch (e) {
          pc.close();
          throw e;
        }
        return open(pc, control, wisp, early, answerSdp(hostRecord, mid), merged);
      },
      close: () => pc.close(),
    };
  } catch (e) {
    pc.close();
    throw e;
  }
}

/**
 * Direct mode (§3.4): a session from the host's record alone, for a host
 * whose `webrtc` block has `direct` on. The browser chooses its own ICE
 * ufrag, uses it as its password as well, and builds the host's answer
 * locally; the host makes the session from the first connectivity check.
 * Nothing is published and nothing waits for gathering.
 *
 * Options: `iceServers`, `timeoutMs` (as `accept` takes it), and
 * `RTCPeerConnection` for a runtime without a global one.
 */
export async function direct(hostRecord, options = {}) {
  parseRecord(hostRecord);
  await trusted(hostRecord, options);
  const PC = options.RTCPeerConnection ?? globalThis.RTCPeerConnection;
  if (!PC) throw new Error('no RTCPeerConnection in this runtime');
  const pc = new PC({ iceServers: options.iceServers ?? [] });
  const control = pc.createDataChannel('control', { negotiated: true, id: 0 });
  const wisp = pc.createDataChannel('wisp', { negotiated: true, id: 1 });
  wisp.binaryType = 'arraybuffer';
  const early = earlyInbox(control, wisp);
  try {
    const ufrag = iceChars(DIRECT_UFRAG_LEN);
    const made = await pc.createOffer();
    await pc.setLocalDescription({ type: 'offer', sdp: withIceCredentials(made.sdp, ufrag, ufrag) });
    const sdp = pc.localDescription.sdp;
    if (!sdp.includes(`a=ice-ufrag:${ufrag}`)) {
      throw new Error('this browser did not keep the ICE credentials direct mode chose');
    }
    const mid = sdp.match(/^a=mid:(.*)$/m)[1].trim();
    return await open(pc, control, wisp, early, answerSdp(hostRecord, mid), options);
  } catch (e) {
    pc.close();
    throw e;
  }
}

/**
 * A page answering (§10.4): take the caller's record, answer it, and hand
 * back the page's own record for signaling to carry to the caller. The
 * session resolves once both channels are open; the page is the answerer,
 * so it serves `options.services` and opens even stream ids.
 *
 * Options: `services` (name -> (stream, session)), `label` (the `service`
 * its hello carries), `iceServers`, `gatherTimeoutMs`, `timeoutMs`,
 * `certificates` (an `RTCCertificate` the page keeps, so its fingerprint
 * holds across sessions), and `RTCPeerConnection`.
 */
export async function answer(callerRecord, options = {}) {
  const offered = offerSdp(callerRecord);
  const PC = options.RTCPeerConnection ?? globalThis.RTCPeerConnection;
  if (!PC) throw new Error('no RTCPeerConnection in this runtime');
  const config = { iceServers: options.iceServers ?? [] };
  if (options.certificates) config.certificates = options.certificates;
  const pc = new PC(config);
  const control = pc.createDataChannel('control', { negotiated: true, id: 0 });
  const wisp = pc.createDataChannel('wisp', { negotiated: true, id: 1 });
  wisp.binaryType = 'arraybuffer';
  const session = new Session(pc, control, wisp, { role: 'answerer', ...options });
  const ready = session.readyAnswering(options.timeoutMs ?? ACCEPT_TIMEOUT_MS);
  ready.catch(() => {}); // the caller of answer() holds it through `session`
  control.onmessage = (e) => session.onControl(e.data);
  wisp.onmessage = (e) => session.onWispPacket(e.data);
  control.onopen = wisp.onopen = () => session.onChannelOpen();
  try {
    await pc.setRemoteDescription({ type: 'offer', sdp: offered });
    await pc.setLocalDescription(await pc.createAnswer());
    await gathered(pc, options.gatherTimeoutMs ?? GATHER_CAP_MS);
    const record = recordFromSdp(pc.localDescription.sdp);
    return {
      record,
      recordText: JSON.stringify(record),
      pc,
      session: ready.then(() => session),
      close: () => session.close(),
    };
  } catch (e) {
    session.close();
    throw e;
  }
}

/**
 * Answer every call a signalling server holds for one name
 * (doc/DRT-Signalling.md): `base` is the name's URL, `…/v1/<name>`, and
 * `options.token` its answerer token.
 *
 * It holds the call notification stream (§5) where the runtime has an
 * `EventSource`, and polls once when it connects and once per
 * notification. Without the stream, or while it is reconnecting, it polls
 * every `pollMs` (LISTEN_POLL_MS). Polls run one at a time and carry the
 * cursor, so no call is read twice.
 *
 * Each call is answered with `answer(call.record, options)` and the
 * page's record posted back; `options.onSession(session, call)` gets the
 * session once it is up. `options.accept(call)`, if given, may refuse a
 * call (false, or a promise of false), which withdraws it: the caller
 * gets 410. A record `answer` cannot take is refused the same way.
 * Failures that do not stop listening -- a poll the server refused, an
 * answer that came too late -- go to `options.onError(error, call)`.
 *
 * Every other option is `answer`'s. `fetch` and `EventSource` may be
 * given for a runtime without global ones; `events: false` polls only.
 */
export function listen(base, options = {}) {
  const fetchFn = options.fetch ?? globalThis.fetch?.bind(globalThis);
  if (!fetchFn) throw new Error('no fetch in this runtime');
  const ES = options.events === false ? null : (options.EventSource ?? globalThis.EventSource);
  const root = String(base).replace(/\/+$/, '');
  const token = options.token;
  const headers = token ? { authorization: `Bearer ${token}` } : {};
  const onError = options.onError ?? (() => {});
  const pollMs = options.pollMs ?? LISTEN_POLL_MS;
  const state = { cursor: '0', stopped: false, events: null, streaming: false, timer: null };
  let chain = Promise.resolve();

  const request = async (method, path, body) => {
    const init = { method, headers: { ...headers } };
    if (body !== undefined) {
      init.body = body;
      init.headers['content-type'] = 'text/plain;charset=utf-8';
    }
    const res = await fetchFn(`${root}${path}`, init);
    if (!res.ok) throw new Error(`${method} ${root}${path} answered ${res.status}`);
    return res;
  };
  const refuse = (call) => request('DELETE', `/calls/${encodeURIComponent(call.id)}`).catch((e) => onError(e, call));

  const take = async (call) => {
    try {
      if (options.accept && !(await options.accept(call))) return refuse(call);
    } catch (e) {
      onError(e, call);
      return refuse(call);
    }
    let answering;
    try {
      answering = await answer(call.record, options);
    } catch (e) {
      onError(e, call);
      return refuse(call);
    }
    try {
      await request('POST', `/calls/${encodeURIComponent(call.id)}/answer`, answering.recordText);
    } catch (e) {
      // Expired, withdrawn or answered elsewhere: nobody will connect.
      answering.close();
      return onError(e, call);
    }
    answering.session.then(
      (session) => options.onSession?.(session, call),
      (e) => onError(e, call),
    );
  };

  const pollOnce = async () => {
    if (state.stopped) return;
    let got;
    try {
      const res = await request('GET', `/calls?since=${encodeURIComponent(state.cursor)}`);
      got = await res.json();
    } catch (e) {
      return onError(e, null);
    }
    if (typeof got?.cursor === 'string') state.cursor = got.cursor;
    for (const call of Array.isArray(got?.calls) ? got.calls : []) {
      if (state.stopped) return;
      await take(call);
    }
  };
  const poll = () => (chain = chain.then(pollOnce));

  // Poll on a timer only while no stream is held (§5).
  const tick = () => {
    state.timer = null;
    if (state.stopped || state.streaming) return;
    poll();
    state.timer = setTimeout(tick, pollMs);
  };

  if (ES) {
    const k = token ? `?k=${encodeURIComponent(token)}` : '';
    const events = new ES(`${root}/events${k}`);
    state.events = events;
    events.addEventListener('open', () => {
      state.streaming = true;
      poll();
    });
    events.addEventListener('call', () => poll());
    events.addEventListener('error', () => {
      state.streaming = false;
      if (!state.timer && !state.stopped) tick();
    });
  }
  tick();

  return {
    get cursor() {
      return state.cursor;
    },
    /** Whether the call notification stream is open now. */
    get streaming() {
      return state.streaming;
    },
    /** Poll now, after any poll already running. */
    poll,
    /** Stop: close the stream and the timer. Sessions already made stay up. */
    close() {
      state.stopped = true;
      state.events?.close();
      if (state.timer) clearTimeout(state.timer);
      state.timer = null;
    },
  };
}

/** `sdp` with every `a=ice-ufrag` and `a=ice-pwd` line replaced. */
export function withIceCredentials(sdp, ufrag, pwd) {
  return sdp
    .replace(/^a=ice-ufrag:.*$/gm, `a=ice-ufrag:${ufrag}`)
    .replace(/^a=ice-pwd:.*$/gm, `a=ice-pwd:${pwd}`);
}

/** `n` random ice-chars (RFC 8839: ALPHA, DIGIT, `+`, `/`). */
function iceChars(n) {
  const alphabet = 'ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/';
  return Array.from(crypto.getRandomValues(new Uint8Array(n)), (b) => alphabet[b & 63]).join('');
}

function gathered(pc, capMs) {
  return new Promise((resolve) => {
    if (pc.iceGatheringState === 'complete') return resolve();
    const done = () => {
      pc.removeEventListener('icegatheringstatechange', check);
      clearTimeout(timer);
      resolve();
    };
    const check = () => pc.iceGatheringState === 'complete' && done();
    pc.addEventListener('icegatheringstatechange', check);
    const timer = setTimeout(done, capMs);
  });
}

/**
 * Messages that arrive between the channels opening and `accept`'s session
 * existing. The host sends `hello` and CONTINUE the moment the channels
 * open, which can be before `setRemoteDescription` resolves.
 */
function earlyInbox(control, wisp) {
  const inbox = { control: [], wisp: [], open: 0 };
  control.onmessage = (e) => inbox.control.push(e.data);
  wisp.onmessage = (e) => inbox.wisp.push(e.data);
  control.onopen = wisp.onopen = () => inbox.open++;
  return inbox;
}

/**
 * `options.fingerprint`: a `SHA256:<base64>` string the answerer's DTLS
 * fingerprint must equal, or `fp => boolean | Promise<boolean>` asked
 * with that fingerprint before the answer is applied, so a stored pin is a
 * comparison and a missing one is the "trust this peer?" prompt. The
 * digest is the record's `f`, so nothing has flowed when this runs.
 */
async function trusted(hostRecord, options) {
  const want = options.fingerprint;
  if (want === undefined || want === null) return;
  const have = fingerprintText(parseRecord(hostRecord).f);
  if (typeof want === 'function') {
    if (!(await want(have))) throw new Error(`the peer's fingerprint ${have} was not trusted`);
    return;
  }
  const expected = String(want).replace(/^sha256:/i, 'SHA256:').replace(/=+$/, '');
  if (expected !== have && expected !== `SHA256:${have}`) {
    throw new Error(`the peer's fingerprint is ${have}, not the ${want} expected; a signalling server answering with another peer's record looks exactly like this`);
  }
}

/** A record's `f` as `drt p2p` prints and takes it: `SHA256:` and base64 without padding. */
export function fingerprintText(f) {
  return `SHA256:${String(f).replace(/=+$/, '')}`;
}

async function open(pc, control, wisp, early, answer, options) {
  const session = new Session(pc, control, wisp, { role: 'caller', ...options });
  const ready = session.ready(options.timeoutMs ?? ACCEPT_TIMEOUT_MS);
  control.onmessage = (e) => session.onControl(e.data);
  wisp.onmessage = (e) => session.onWispPacket(e.data);
  control.onopen = wisp.onopen = () => session.onChannelOpen();
  for (let i = 0; i < early.open; i++) session.onChannelOpen();
  early.control.forEach((m) => session.onControl(m));
  early.wisp.forEach((m) => session.onWispPacket(m));
  try {
    await pc.setRemoteDescription({ type: 'answer', sdp: answer });
    await ready;
  } catch (e) {
    session.close();
    throw e;
  }
  return session;
}

/**
 * A connected session: the peer's `hello`, streams to what the peer serves,
 * and streams to what this side serves (§10).
 */
class Session {
  constructor(pc, control, wisp, options = {}) {
    this.pc = pc;
    this.control = control;
    this.wisp = wisp;
    /** The peer's `hello` (§5): service, default, scope, services, limits. */
    this.hello = null;
    this.streams = new Map();
    /** §10.2: the caller opens odd ids, the answerer even ones. */
    this.role = options.role ?? 'caller';
    this.nextId = this.role === 'caller' ? 1 : 2;
    this.initialCredit = null;
    this.services = new Map(Object.entries(options.services ?? {}));
    for (const name of this.services.keys()) {
      if (!isServiceName(name)) throw new TypeError(`"${name}" cannot name a service (§10.3)`);
    }
    this.label = options.label ?? '';
    this.announced = false;
    this.serving = 0;
    this.channelsOpen = 0;
    this.ended = false;
    this.waiters = [];
    let resolveClosed;
    /** Resolves when the session ends, for whatever reason. */
    this.closed = new Promise((r) => (resolveClosed = r));
    this.resolveClosed = resolveClosed;
    wisp.bufferedAmountLowThreshold = SEND_HIGH_WATER / 2;
    wisp.onbufferedamountlow = () => this.wake();
    const ended = () => this.end('the peer connection closed');
    control.onclose = wisp.onclose = ended;
    pc.addEventListener('connectionstatechange', () => {
      if (['failed', 'closed'].includes(pc.connectionState)) ended();
    });
  }

  ready(timeoutMs) {
    return new Promise((resolve, reject) => {
      const timer = setTimeout(() => {
        reject(new Error(`no session within ${timeoutMs} ms (ice ${this.pc.iceConnectionState}, `
          + `channels ${this.channelsOpen}/2, hello ${this.hello ? 'yes' : 'no'}, `
          + `credit ${this.initialCredit ?? 'none'})`));
      }, timeoutMs);
      this.onReady = () => {
        if (this.channelsOpen === 2 && this.hello && this.initialCredit !== null) {
          clearTimeout(timer);
          this.onReady = null;
          resolve();
        }
      };
      this.onEnded = (why) => {
        clearTimeout(timer);
        reject(new Error(why));
      };
    });
  }

  /** For a page that answers: ready once both channels are open (§10.4). */
  readyAnswering(timeoutMs) {
    return new Promise((resolve, reject) => {
      const timer = setTimeout(() => {
        reject(new Error(`no session within ${timeoutMs} ms (ice ${this.pc.iceConnectionState}, `
          + `channels ${this.channelsOpen}/2)`));
      }, timeoutMs);
      this.onReady = () => {
        if (this.channelsOpen === 2) {
          clearTimeout(timer);
          this.onReady = null;
          resolve();
        }
      };
      this.onEnded = (why) => {
        clearTimeout(timer);
        reject(new Error(why));
      };
    });
  }

  onChannelOpen() {
    this.channelsOpen++;
    if (this.channelsOpen === 2) this.announce();
    this.onReady?.();
  }

  /**
   * §10.2: a side that serves says so once the channels are open -- its
   * `hello` and its per-stream buffer. The answerer always does, since a
   * caller's `accept` waits for both.
   */
  announce() {
    if (this.announced || (this.role === 'caller' && this.services.size === 0)) return;
    this.announced = true;
    try {
      this.control.send(JSON.stringify({
        v: RECORD_VERSION, t: 'hello', service: this.label, scope: [],
        services: [...this.services.keys()], limits: { max_streams: SERVE_MAX_STREAMS },
      }));
      this.send(encodeWisp(WISP.CONTINUE, 0, { buffer: SERVE_BUFFER }));
    } catch {}
  }

  onControl(text) {
    let m;
    try {
      m = JSON.parse(text);
    } catch {
      return;
    }
    // §5: `hello` is v1's only message; any other `t` is ignored.
    if (m && m.t === 'hello' && !this.hello) {
      this.hello = m;
      this.onReady?.();
    }
  }

  onWispPacket(data) {
    const p = decodeWisp(data);
    if (!p) return;
    if (p.type === WISP.CONTINUE && p.stream === 0) {
      // The initial per-stream buffer, and the v1 marker (§6).
      if (this.initialCredit === null && p.buffer !== undefined) {
        this.initialCredit = p.buffer;
        this.onReady?.();
      }
      return;
    }
    if (p.type === WISP.CONNECT) return this.onConnect(p);
    const s = this.streams.get(p.stream);
    if (!s) return;
    if (p.type === WISP.DATA) s.receive(p.payload);
    else if (p.type === WISP.CONTINUE && p.buffer !== undefined) s.credit(p.buffer);
    else if (p.type === WISP.CLOSE) s.finish(new StreamClosed(p.reason ?? 0x01), false);
    // Unknown types are ignored (§6).
  }

  /**
   * The peer opened a stream to something this side serves (§10.3). Only
   * a named service is served by a page; an address, an unknown name, or
   * UDP is 0x48, and a malformed CONNECT, an id of this side's parity or
   * an id already open is 0x41.
   */
  onConnect(p) {
    const refuse = (reason) => {
      try {
        this.send(encodeWisp(WISP.CLOSE, p.stream, { reason }));
      } catch {}
    };
    const theirs = this.role === 'caller' ? 0 : 1;
    if (p.host === null || p.host === undefined || p.stream === 0 || p.stream % 2 !== theirs
        || this.streams.has(p.stream)) return refuse(0x41);
    const handler = p.kind === 0x01 && p.port === 0 ? this.services.get(p.host) : undefined;
    if (!handler) return refuse(0x48);
    if (this.serving >= SERVE_MAX_STREAMS) return refuse(0x49);
    const stream = new Stream(this, p.stream, Infinity, { served: true });
    this.streams.set(p.stream, stream);
    this.serving++;
    stream.closed.finally(() => this.serving--).catch(() => {});
    try {
      handler(stream, this);
    } catch {
      stream.close();
    }
  }

  /**
   * A stream to what the peer serves: `connect(host, port)` for an entry
   * in its `hello.scope` (the host checks; §6), or `connect(name)` for one
   * of its `hello.services` (§10.3). Wisp v1 has no "connected" packet:
   * the stream is usable at once, and a refusal arrives as its `closed`
   * rejecting with a StreamClosed.
   */
  connect(host, port) {
    if (this.ended) throw new Error('the session has ended');
    // `connect()` asks for whatever the peer forwards to, and `connect(80)`
    // for port 80 of it (doc/P2P.md §5.1): an empty host on the wire.
    if (host === undefined || host === '') {
      host = '';
      port = port ?? 0;
      if (port !== 0 && (!Number.isInteger(port) || port < 1 || port > 65535)) throw new TypeError('port must be 1..65535');
    } else if (typeof host === 'number' && port === undefined) {
      port = host;
      host = '';
      if (!Number.isInteger(port) || port < 1 || port > 65535) throw new TypeError('port must be 1..65535');
    } else if (typeof host !== 'string') {
      throw new TypeError('host must be a string');
    } else if (port === undefined) {
      if (!isServiceName(host)) throw new TypeError(`"${host}" cannot name a service (§10.3); pass a port for an address`);
      port = 0;
    } else if (!Number.isInteger(port) || port < 1 || port > 65535) throw new TypeError('port must be 1..65535');
    if (this.initialCredit === null) throw new Error('the peer serves nothing: it sent no credit (§10.2)');
    const id = this.nextId;
    this.nextId += 2;
    const stream = new Stream(this, id, this.initialCredit);
    this.streams.set(id, stream);
    this.send(encodeWisp(WISP.CONNECT, id, { host, port }));
    return stream;
  }

  send(packet) {
    if (this.wisp.readyState !== 'open') throw new Error('the wisp channel is not open');
    this.wisp.send(packet);
  }

  /**
   * Report a stream's terminal size over `control` (doc/P2P.md §5.2), for
   * a peer service that is a terminal, such as a DRT host's `repl`.
   */
  resize(stream, cols, rows) {
    if (this.control.readyState !== 'open') return;
    const id = typeof stream === 'number' ? stream : stream.id;
    this.control.send(JSON.stringify({ t: 'resize', stream: id, cols: cols | 0, rows: rows | 0 }));
  }

  /** Resolves when the channel has room for more; see SEND_HIGH_WATER. */
  room() {
    if (this.ended || this.wisp.bufferedAmount < SEND_HIGH_WATER) return Promise.resolve();
    return new Promise((r) => this.waiters.push(r));
  }

  wake() {
    const w = this.waiters;
    this.waiters = [];
    w.forEach((r) => r());
    for (const s of this.streams.values()) s.wake();
  }

  /** End the session: every stream fails, and the connection closes. */
  close() {
    this.end('the session was closed');
  }

  end(why) {
    if (this.ended) return;
    this.ended = true;
    for (const s of [...this.streams.values()]) s.finish(new StreamClosed(null, `session ended: ${why}`), false);
    this.onEnded?.(why);
    this.wake();
    try {
      this.pc.close();
    } catch {}
    this.resolveClosed(why);
  }
}

/**
 * One Wisp stream as a pair of Web Streams of bytes. `closed` resolves
 * when the target closed cleanly (0x02) or the page closed it, and
 * rejects with a StreamClosed for anything else.
 */
class Stream {
  constructor(session, id, credit, { served = false } = {}) {
    this.session = session;
    this.id = id;
    this.remaining = credit;
    this.done = false;
    this.waiters = [];
    let resolve, reject;
    this.closed = new Promise((res, rej) => ((resolve = res), (reject = rej)));
    this.closed.catch(() => {}); // a caller that never looks is not an unhandled rejection
    this.settle = { resolve, reject };
    // A stream this side serves grants the opener credit (§10.2, as §6):
    // SERVE_BUFFER packets, topped up with CONTINUE as the reader drains
    // them. What it reads is pulled one packet at a time so the count is
    // what the reader has taken, not what has arrived.
    this.served = served;
    this.inbox = [];
    this.taken = 0;
    this.readable = served
      ? new ReadableStream({
        start: (c) => (this.reader = c),
        pull: () => this.pull(),
        cancel: () => this.close(),
      }, { highWaterMark: 0 })
      : new ReadableStream({
        start: (c) => (this.reader = c),
        cancel: () => this.close(),
      });
    this.writable = new WritableStream({
      write: (chunk) => this.write(chunk),
      close: () => this.close(),
      abort: () => this.close(),
    });
  }

  async write(chunk) {
    const bytes = chunk instanceof Uint8Array ? chunk : new Uint8Array(chunk);
    for (let at = 0; at < bytes.length; at += DATA_MAX) {
      // Wisp's credit (§6): one packet per unit, topped up by CONTINUE.
      while (!this.done && this.remaining <= 0) await new Promise((r) => this.waiters.push(r));
      await this.session.room();
      if (this.done) throw this.failure ?? new StreamClosed(0x02);
      this.session.send(encodeWisp(WISP.DATA, this.id, { data: bytes.subarray(at, at + DATA_MAX) }));
      this.remaining--;
    }
  }

  receive(payload) {
    if (this.done) return;
    if (!this.served) return this.reader.enqueue(payload.slice());
    this.inbox.push(payload.slice());
    this.pending?.();
  }

  /** A served stream's reader wants a packet. */
  pull() {
    if (this.inbox.length === 0) {
      if (this.done) return;
      return new Promise((r) => (this.pending = () => {
        this.pending = null;
        r(this.pull());
      }));
    }
    this.reader.enqueue(this.inbox.shift());
    if (++this.taken >= SERVE_BUFFER / 2) {
      this.taken = 0;
      try {
        this.session.send(encodeWisp(WISP.CONTINUE, this.id, { buffer: SERVE_BUFFER - this.inbox.length }));
      } catch {}
    }
  }

  credit(buffer) {
    this.remaining = buffer;
    this.wake();
  }

  wake() {
    const w = this.waiters;
    this.waiters = [];
    w.forEach((r) => r());
  }

  /** Close from this side: sends CLOSE 0x02 and resolves `closed`. */
  close() {
    if (this.done) return;
    try {
      this.session.send(encodeWisp(WISP.CLOSE, this.id, { reason: 0x02 }));
    } catch {}
    this.finish(null, true);
  }

  finish(error, local) {
    if (this.done) return;
    this.done = true;
    this.pending?.();
    this.session.streams.delete(this.id);
    const clean = error === null || (!local && error.reason === 0x02);
    try {
      if (clean) this.reader.close();
      else this.reader.error(error);
    } catch {}
    if (clean) this.settle.resolve();
    else {
      this.failure = error;
      this.settle.reject(error);
    }
    this.wake();
  }
}

// depth: bytes

function utf8Length(s) {
  return new TextEncoder().encode(s).length;
}

function fromBase64(s) {
  if (!/^[A-Za-z0-9+/]*={0,2}$/.test(s) || s.length % 4 !== 0) return null;
  try {
    return Uint8Array.from(atob(s), (c) => c.charCodeAt(0));
  } catch {
    return null;
  }
}

function toBase64(bytes) {
  return btoa(String.fromCharCode(...bytes));
}

function fromHex(s) {
  const parts = (s ?? '').split(':');
  if (parts.length !== 32 || parts.some((h) => !/^[0-9A-Fa-f]{2}$/.test(h))) {
    throw new RecordError('Fingerprint', 'the local description has no sha-256 fingerprint');
  }
  return Uint8Array.from(parts, (h) => parseInt(h, 16));
}
