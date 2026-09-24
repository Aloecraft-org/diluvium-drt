// The browser half of DRT browser access (doc/BrowserAccess.md §2-§6): a
// peer connection to a DRT host, its `hello`, and TCP streams to the host's
// scope over Wisp v1. One ES module, no dependencies, no build step.
//
// Signaling is the caller's. This module makes the browser's record and
// takes the host's; how the two cross -- the Discofetch API's socket and
// presence (§7.1) -- is the page's business, so this never opens a
// WebSocket and never polls.
//
//   const pending = await offer();              // gathers, then resolves
//   send(pending.record);                       // to the API, as the page does
//   const session = await pending.accept(hostRecord);
//   session.hello.scope;                        // what the host serves
//   const s = session.connect('127.0.0.1', 8123);
//   s.writable / s.readable / await s.closed    // Web Streams, bytes
//
// ## surface block
//
// - Entry points: `offer(options)` -> Pending {record, recordText,
//   accept(hostRecord, options)} -> Session {hello, connect(host, port),
//   close(), closed}; Session.connect -> Stream {id, readable, writable,
//   close(), closed}. And the pure pieces, for a client that drives its own
//   RTCPeerConnection: `parseRecord`, `recordFromSdp`, `answerSdp`,
//   `fingerprintHex`, `encodeWisp`, `decodeWisp`.
// - Configurable: GATHER_CAP_MS, how long `offer` waits for ICE gathering
//   before it publishes what it has (§3.1); ACCEPT_TIMEOUT_MS, how long
//   `accept` waits for the channels, `hello` and the first CONTINUE;
//   SEND_HIGH_WATER, the data channel buffer past which a stream's writer
//   waits. The wire's own limits -- RECORD_MAX_BYTES, MAX_CANDIDATES, the
//   ICE lengths, MESSAGE_MAX -- are constants of v1, not knobs: changing
//   one is a `v` bump.
// - Fan-out: `onWispPacket`, one branch per packet type the host sends
//   (DATA, CONTINUE, CLOSE); RecordError's `code`, one per rule a record can
//   break, named as crates/drt-rtc/src/record.rs names them; CLOSE_REASON,
//   the reasons a stream can end with (§6).

export const GATHER_CAP_MS = 2000;
export const ACCEPT_TIMEOUT_MS = 15000;
export const SEND_HIGH_WATER = 1 << 20;

export const RECORD_VERSION = 1;
export const RECORD_MAX_BYTES = 512;
export const MAX_CANDIDATES = 8;
export const UFRAG_LEN = [4, 32];
export const PWD_LEN = [22, 64];
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
  return { v: RECORD_VERSION, u, p, f, c: [...c] };
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

/** §2.1: the line as a record carries it, or null when it is skipped. */
function usableCandidate(line) {
  const m = line.match(/^(candidate:\S+ \d+ (\S+) \d+ (\S+) \d+ typ (\S+)(?: raddr \S+ rport \d+)?)/i);
  if (!m) return null;
  const [, kept, proto, addr, typ] = m;
  if (proto.toLowerCase() !== 'udp') return null;
  if (!['host', 'srflx', 'prflx'].includes(typ.toLowerCase())) return null;
  if (addr.toLowerCase().endsWith('.local')) return null;
  return kept;
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
  const r = parseRecord(hostRecord);
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
    'a=setup:passive',
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
      accept(hostRecord, acceptOptions = {}) {
        if (used) return Promise.reject(new Error('accept was already called; a session needs a fresh offer'));
        used = true;
        return open(pc, control, wisp, early, answerSdp(hostRecord, mid), acceptOptions);
      },
      close: () => pc.close(),
    };
  } catch (e) {
    pc.close();
    throw e;
  }
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

async function open(pc, control, wisp, early, answer, options) {
  const session = new Session(pc, control, wisp);
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

/** A connected session: its `hello`, and streams to the host's scope. */
class Session {
  constructor(pc, control, wisp) {
    this.pc = pc;
    this.control = control;
    this.wisp = wisp;
    /** The host's `hello` (§5): service, default, scope, limits. */
    this.hello = null;
    this.streams = new Map();
    this.nextId = 1;
    this.initialCredit = null;
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

  onChannelOpen() {
    this.channelsOpen++;
    this.onReady?.();
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
    const s = this.streams.get(p.stream);
    if (!s) return;
    if (p.type === WISP.DATA) s.receive(p.payload);
    else if (p.type === WISP.CONTINUE && p.buffer !== undefined) s.credit(p.buffer);
    else if (p.type === WISP.CLOSE) s.finish(new StreamClosed(p.reason ?? 0x01), false);
    // Unknown types are ignored (§6).
  }

  /**
   * A TCP stream to `host:port`, which must match a scope entry in
   * `hello.scope` (the host checks; §6). Wisp v1 has no "connected"
   * packet: the stream is usable at once, and a refusal arrives as its
   * `closed` rejecting with a StreamClosed.
   */
  connect(host, port) {
    if (this.ended) throw new Error('the session has ended');
    if (typeof host !== 'string' || host.length === 0) throw new TypeError('host must be a non-empty string');
    if (!Number.isInteger(port) || port < 1 || port > 65535) throw new TypeError('port must be 1..65535');
    const id = this.nextId++;
    const stream = new Stream(this, id, this.initialCredit);
    this.streams.set(id, stream);
    this.send(encodeWisp(WISP.CONNECT, id, { host, port }));
    return stream;
  }

  send(packet) {
    if (this.wisp.readyState !== 'open') throw new Error('the wisp channel is not open');
    this.wisp.send(packet);
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
  constructor(session, id, credit) {
    this.session = session;
    this.id = id;
    this.remaining = credit;
    this.done = false;
    this.waiters = [];
    let resolve, reject;
    this.closed = new Promise((res, rej) => ((resolve = res), (reject = rej)));
    this.closed.catch(() => {}); // a caller that never looks is not an unhandled rejection
    this.settle = { resolve, reject };
    this.readable = new ReadableStream({
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
    if (!this.done) this.reader.enqueue(payload.slice());
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
