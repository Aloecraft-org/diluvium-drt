// The client library's pure half against crates/drt-rtc/vectors/, the same
// file the Rust host is held to, plus the stream logic against a fake
// channel. The live half -- a real RTCPeerConnection to a real host -- is
// crates/drt-rtc/browser-check/check.mjs, which drives this library in
// Chromium.
//
//   node --test crates/drt-rtc/client/test.mjs
//
// ## surface block
//
// - Entry point: this file, under `node --test`.
// - Fan-out: one `test` per rule; the vectors' three sections each get one.

import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import {
  parseRecord, recordFromSdp, answerSdp, fingerprintHex, isUsableCandidate, encodeWisp, decodeWisp,
  RecordError, StreamClosed, WISP, DATA_MAX,
} from './drt_browser_access.js';

const vectors = JSON.parse(readFileSync(new URL('../vectors/browser-access-v1.json', import.meta.url)));
const hex = (b) => Buffer.from(b).toString('hex');

test('every valid record parses from its text and from its object, and answers byte for byte', () => {
  for (const v of vectors.records) {
    for (const form of [v.rtc, JSON.parse(v.rtc)]) {
      const r = parseRecord(form);
      assert.equal(r.u, v.decoded.u, v.name);
      assert.equal(r.p, v.decoded.p, v.name);
      assert.deepEqual(r.c, v.decoded.c, v.name);
      assert.equal(fingerprintHex(r.f), v.decoded.f_hex, v.name);
      assert.equal(answerSdp(form, v.mid), v.answer_sdp, v.name);
    }
    // A record that has been read reads the same again: the §2.1 skip
    // happens once, on the way in.
    const once = parseRecord(v.rtc);
    assert.deepEqual(parseRecord(JSON.stringify(once)), once, `${v.name}: reads the same twice`);
  }
});

test('every invalid record is refused with the rule the host names', () => {
  for (const v of vectors.invalid_records) {
    assert.throws(() => parseRecord(v.rtc), (e) => e instanceof RecordError && e.code === v.error, v.name);
  }
});

test('the limits of §2', () => {
  const base = JSON.parse(vectors.records[0].rtc);
  const refused = (r, code) => assert.throws(() => parseRecord(r), (e) => e.code === code, code);
  refused({ ...base, c: Array(9).fill('candidate:1 1 udp 1 10.0.0.1 1 typ host') }, 'TooManyCandidates');
  refused({ ...base, u: 'x'.repeat(33) }, 'Ufrag');
  refused({ ...base, p: 'x'.repeat(65) }, 'Pwd');
  refused({ ...base, v: '1' }, 'Version');
  refused({ ...base, u: 7 }, 'WrongType');
  refused({ ...base, pad: 'x'.repeat(512) }, 'TooLong');
  refused(JSON.stringify({ ...base, pad: 'x'.repeat(512) }), 'TooLong');
  refused('{not json', 'NotJson');
  refused(null, 'NotAnObject');
  // Unknown keys are ignored, and not carried.
  assert.deepEqual(Object.keys(parseRecord({ ...base, x: 1 })), ['v', 'u', 'p', 'f', 'c']);
});

test('a browser record from its local description keeps only what §2.1 allows', () => {
  const v = vectors.records.find((r) => r.name.startsWith('browser record'));
  const want = JSON.parse(v.rtc);
  const sdp = [
    'v=0', 'o=- 1 2 IN IP4 127.0.0.1', 's=-', 't=0 0', 'a=group:BUNDLE 0',
    'm=application 9 UDP/DTLS/SCTP webrtc-datachannel', 'c=IN IP4 0.0.0.0',
    'a=candidate:1 1 udp 2113937151 3f1a7c2e-0000-4000-8000-000000000000.local 61234 typ host generation 0 network-cost 999',
    `a=${want.c[0]} generation 0 ufrag ${want.u} network-id 1`,
    'a=candidate:3 1 tcp 1518280447 192.168.1.20 9 typ host tcptype active generation 0',
    'a=candidate:4 1 udp 41885439 203.0.113.9 3478 typ relay raddr 198.51.100.23 rport 61234 generation 0',
    `a=ice-ufrag:${want.u}`, `a=ice-pwd:${want.p}`, 'a=ice-options:trickle',
    `a=fingerprint:sha-256 ${v.decoded.f_hex}`, 'a=setup:actpass', 'a=mid:0', 'a=sctp-port:5000', '',
  ].join('\r\n');
  const got = recordFromSdp(sdp);
  assert.equal(JSON.stringify(got), v.rtc);
  assert.equal(Buffer.byteLength(JSON.stringify(got)), v.bytes);
});

test('a candidate is usable only as §2.1 says, by the same rule as the host', () => {
  const yes = [
    'candidate:1 1 udp 2130706431 192.168.1.20 50212 typ host',
    'candidate:2 1 UDP 1694498815 203.0.113.7 50212 typ srflx raddr 0.0.0.0 rport 0',
    'candidate:3 1 udp 1845501695 198.51.100.9 4000 typ prflx',
    'candidate:4 1 udp 2130706431 fd00::1 50212 typ host generation 0',
  ];
  const no = [
    'candidate:1 1 tcp 1518280447 192.0.2.5 9 typ host tcptype active',
    'candidate:1 1 udp 41885439 198.51.100.1 3478 typ relay raddr 0.0.0.0 rport 0',
    'candidate:1 1 udp 2113937151 x.local 61234 typ host',
    'candidate:1 1 udp 2130706431 192.0.2.1 +5 typ host',
    'candidate:1 1 udp 2130706431 192.0.2.1 70000 typ host',
    'candidate:1 1 udp 2130706431 192.0.2.1 5000 host',
    'candidate:1 x udp 2130706431 192.0.2.1 5000 typ host',
    'candidate:not a candidate',
    'a=candidate:1 1 udp 2130706431 192.0.2.1 5000 typ host',
  ];
  for (const l of yes) assert.ok(isUsableCandidate(l), l);
  for (const l of no) assert.ok(!isUsableCandidate(l), l);
});

test('a local description without a sha-256 fingerprint is refused, not published', () => {
  const sdp = 'a=ice-ufrag:abcd\r\na=ice-pwd:0123456789012345678901\r\na=fingerprint:sha-1 00:11\r\n';
  assert.throws(() => recordFromSdp(sdp), (e) => e.code === 'Fingerprint');
});

test('every wisp vector encodes and decodes', () => {
  const byName = (s) => vectors.wisp.find((w) => w.name.startsWith(s)).hex;
  assert.equal(hex(encodeWisp(WISP.CONTINUE, 0, { buffer: 128 })), byName('CONTINUE on stream 0'));
  assert.equal(hex(encodeWisp(WISP.CONNECT, 1, { host: '127.0.0.1', port: 8123 })), byName('CONNECT stream 1'));
  assert.equal(hex(encodeWisp(WISP.CONNECT, 0x01020304, { host: 'fd00::1', port: 22 })), byName('CONNECT stream 0x01020304'));
  assert.equal(hex(encodeWisp(WISP.DATA, 1, { data: new TextEncoder().encode('GET / HTTP/1.1\r\n') })), byName('DATA stream 1'));
  assert.equal(hex(encodeWisp(WISP.CONTINUE, 1, { buffer: 64 })), byName('CONTINUE stream 1'));
  assert.equal(hex(encodeWisp(WISP.CLOSE, 1, { reason: 0x02 })), byName('CLOSE stream 1'));
  assert.equal(hex(encodeWisp(WISP.CLOSE, 2, { reason: 0x48 })), byName('CLOSE stream 2'));
  for (const w of vectors.wisp) {
    const p = decodeWisp(Buffer.from(w.hex, 'hex'));
    assert.equal(hex(encodeWisp(p.type, p.stream, {
      host: p.type === WISP.CONNECT ? new TextDecoder().decode(p.payload.subarray(3)) : undefined,
      port: p.type === WISP.CONNECT ? p.payload[1] | (p.payload[2] << 8) : undefined,
      data: p.payload, buffer: p.buffer, reason: p.reason,
    })), w.hex, w.name);
  }
  assert.equal(decodeWisp(new Uint8Array(4)), null, 'shorter than the header');
});

// depth: the stream logic, over a fake peer connection

class FakeChannel extends EventTarget {
  constructor() {
    super();
    this.readyState = 'open';
    this.bufferedAmount = 0;
    this.sent = [];
  }
  send(b) {
    this.sent.push(decodeWisp(b));
  }
}

async function fakeSession() {
  // Reach the Session class through offer(), with a peer connection whose
  // every step succeeds at once and whose host answers as §5 and §6 say.
  const { offer } = await import('./drt_browser_access.js');
  const channels = [];
  const fp = vectors.records[0].decoded.f_hex;
  class PC extends EventTarget {
    constructor() {
      super();
      this.iceGatheringState = 'complete';
      this.connectionState = 'new';
    }
    createDataChannel() {
      const c = new FakeChannel();
      channels.push(c);
      return c;
    }
    async createOffer() {
      return { type: 'offer', sdp: '' };
    }
    async setLocalDescription() {
      this.localDescription = {
        sdp: `a=ice-ufrag:abcd\r\na=ice-pwd:0123456789012345678901\r\na=fingerprint:sha-256 ${fp}\r\na=mid:0\r\n`,
      };
    }
    async setRemoteDescription() {
      const [control, wisp] = channels;
      control.onopen();
      wisp.onopen();
      control.onmessage({ data: JSON.stringify({ v: 1, t: 'hello', service: 'test', scope: [], limits: { max_streams: 64 } }) });
      wisp.onmessage({ data: encodeWisp(WISP.CONTINUE, 0, { buffer: 2 }).buffer });
    }
    close() {
      this.connectionState = 'closed';
    }
  }
  const pending = await offer({ RTCPeerConnection: PC });
  const session = await pending.accept(vectors.records[0].rtc);
  const host = (packet) => channels[1].onmessage({ data: packet.buffer });
  return { session, wisp: channels[1], host };
}

test('accept waits for both channels, hello and the initial credit', async () => {
  const { session } = await fakeSession();
  assert.equal(session.hello.service, 'test');
  assert.equal(session.initialCredit, 2);
});

test('a stream sends CONNECT, splits DATA at 16379 bytes, and waits for credit', async () => {
  const { session, wisp, host } = await fakeSession();
  const s = session.connect('127.0.0.1', 8123);
  assert.deepEqual([wisp.sent[0].type, wisp.sent[0].stream], [WISP.CONNECT, 1]);
  const w = s.writable.getWriter();
  let written = false;
  const big = new Uint8Array(DATA_MAX * 3);
  const p = w.write(big).then(() => (written = true));
  await new Promise((r) => setTimeout(r, 20));
  // Credit was 2: two packets out, the third waits.
  assert.equal(wisp.sent.filter((x) => x.type === WISP.DATA).length, 2);
  assert.equal(written, false);
  host(encodeWisp(WISP.CONTINUE, 1, { buffer: 5 }));
  await p;
  const data = wisp.sent.filter((x) => x.type === WISP.DATA);
  assert.deepEqual(data.map((x) => x.payload.length), [DATA_MAX, DATA_MAX, DATA_MAX]);
});

test('data arrives on readable, and a clean CLOSE ends it', async () => {
  const { session, host } = await fakeSession();
  const s = session.connect('127.0.0.1', 8123);
  host(encodeWisp(WISP.DATA, 1, { data: new TextEncoder().encode('hi') }));
  host(encodeWisp(WISP.CLOSE, 1, { reason: 0x02 }));
  const chunks = [];
  for await (const c of s.readable) chunks.push(...c);
  assert.equal(new TextDecoder().decode(Uint8Array.from(chunks)), 'hi');
  await s.closed;
});

test('a refusal rejects closed with its reason, and later writes fail', async () => {
  const { session, host } = await fakeSession();
  const s = session.connect('10.0.0.1', 22);
  host(encodeWisp(WISP.CLOSE, 1, { reason: 0x48 }));
  await assert.rejects(s.closed, (e) => e instanceof StreamClosed && e.reason === 0x48 && /blocked/.test(e.message));
  await assert.rejects(s.writable.getWriter().write(new Uint8Array(1)));
});

test('closing the session fails every stream and resolves closed', async () => {
  const { session } = await fakeSession();
  const a = session.connect('127.0.0.1', 1);
  const b = session.connect('127.0.0.1', 2);
  assert.deepEqual([a.id, b.id], [1, 2]);
  session.close();
  await assert.rejects(a.closed, (e) => e.reason === null);
  await assert.rejects(b.closed, (e) => e.reason === null);
  assert.equal(await session.closed, 'the session was closed');
  assert.throws(() => session.connect('127.0.0.1', 3));
});

test('a closed stream ends cleanly from this side and tells the host', async () => {
  const { session, wisp } = await fakeSession();
  const s = session.connect('127.0.0.1', 8123);
  s.close();
  await s.closed;
  const last = wisp.sent.at(-1);
  assert.deepEqual([last.type, last.stream, last.reason], [WISP.CLOSE, 1, 0x02]);
});
