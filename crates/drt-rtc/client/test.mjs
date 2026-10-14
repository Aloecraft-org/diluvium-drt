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
  parseRecord, recordFromSdp, answerSdp, fingerprintHex, fingerprintText, canonicalPeer, isUsableCandidate, encodeWisp, decodeWisp,
  RecordError, StreamClosed, WISP, DATA_MAX, reflect,
} from './drt_browser_access.js';

const vectors = JSON.parse(readFileSync(new URL('../vectors/browser-access-v1.json', import.meta.url)));
const hex = (b) => Buffer.from(b).toString('hex');

test('canonicalPeer gives every spelling of a peer one form, as drt p2p --show does', () => {
  const c = (a) => canonicalPeer(a).canonical;
  assert.equal(c('drt://signal.example/v1/mypc'), 'https://signal.example/v1/mypc');
  assert.equal(c('drt+ssh://signal.example/v1/mypc/'), 'https://signal.example/v1/mypc');
  assert.equal(c('https://signal.example/v1/mypc/calls?k=tok'), 'https://signal.example/v1/mypc');
  assert.equal(c('signal.example'), 'https://signal.example');
  assert.equal(c('drt://127.0.0.1:5001'), 'http://127.0.0.1:5001');
  assert.equal(c('drt://[::1]:5001/v1/a'), 'http://[::1]:5001/v1/a');
  assert.equal(c('wss://relay.example/s/xps?k=1'), 'wss://relay.example/s/xps');
  const p = canonicalPeer('drt+ssh://signal.example/v1/mypc?k=tok');
  assert.deepEqual([p.kind, p.service, p.name, p.url], ['signal', 'ssh', 'mypc', 'https://signal.example/v1/mypc/calls?k=tok']);
  assert.equal(canonicalPeer('drt://localhost').url, 'http://localhost/');
  const r = canonicalPeer(vectors.records[0].rtc);
  assert.equal(r.kind, 'record');
  assert.equal(r.canonical, `record:${fingerprintText(parseRecord(vectors.records[0].rtc).f)}`);
  assert.throws(() => canonicalPeer('ftp://x'), /a peer is/);
  assert.throws(() => canonicalPeer('drt+Bad_Name://x'), /cannot name a service/);
  assert.throws(() => canonicalPeer(''), /empty/);
});

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
  assert.equal(hex(encodeWisp(WISP.END, 1)), byName('END stream 1'));
  assert.equal(decodeWisp(Buffer.from(byName('END stream 1'), 'hex')).type, WISP.END);
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
    this.sent.push(typeof b === 'string' ? b : decodeWisp(b));
  }
}

async function fakeSession(acceptOptions = {}) {
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
  const session = await pending.accept(vectors.records[0].rtc, acceptOptions);
  const host = (packet) => channels[1].onmessage({ data: packet.buffer });
  return { session, wisp: channels[1], control: channels[0], host };
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

test('connect() with no target, or a port alone, is an empty host on the wire (doc/P2P.md §5.1)', async () => {
  const { session, wisp } = await fakeSession();
  session.connect();
  session.connect(8080);
  session.connect('', 22);
  const asked = wisp.sent.filter((p) => p.type === WISP.CONNECT).map((p) => [p.host, p.port]);
  assert.deepEqual(asked, [['', 0], ['', 8080], ['', 22]]);
  assert.throws(() => session.connect(0), /1\.\.65535/);
  assert.throws(() => session.connect('', 70000), /1\.\.65535/);
});

test('resize reports a stream\'s terminal size on control', async () => {
  const { session, control } = await fakeSession();
  const s = session.connect('repl');
  session.resize(s, 120, 40);
  session.resize(s.id, 80, 24);
  // The features message went first, when the channels opened (§6).
  assert.deepEqual(control.sent.map((m) => JSON.parse(m)).filter((m) => m.t === 'resize'), [
    { t: 'resize', stream: s.id, cols: 120, rows: 40 },
    { t: 'resize', stream: s.id, cols: 80, rows: 24 },
  ]);
  assert.deepEqual(JSON.parse(control.sent[0]), { t: 'features', half_close: true });
});

test('a fingerprint is checked before the answer is applied: a pin compares, a function asks', async () => {
  const text = fingerprintText(parseRecord(vectors.records[0].rtc).f);
  assert.match(text, /^SHA256:[A-Za-z0-9+/]+$/);
  // A matching pin, padded or not, and a function that says yes.
  for (const fingerprint of [text, `${text}=`, async (fp) => fp === text]) {
    const { session } = await fakeSession({ fingerprint });
    assert.equal(session.hello.service, 'test');
  }
  // A pin that differs, and a function that says no, before anything flows.
  await assert.rejects(fakeSession({ fingerprint: 'SHA256:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA' }), /not the/);
  await assert.rejects(fakeSession({ fingerprint: () => false }), /not trusted/);
});

test('granted on control settles the stream\'s caps; a stream that ends first rejects it', async () => {
  const { session, control } = await fakeSession();
  const a = session.connect('repl');
  const b = session.connect('ssh');
  control.onmessage({ data: JSON.stringify({ t: 'granted', stream: a.id, caps: ['host:time/*'] }) });
  assert.deepEqual(await a.granted, ['host:time/*']);
  assert.deepEqual(a.caps, ['host:time/*']);
  b.close();
  await assert.rejects(b.granted, /ended before/);
  assert.equal(b.caps, null);
});

test('closing the writable half-closes when the peer has END, and the answer still arrives', async () => {
  const { session, wisp, host } = await fakeSession();
  // The fake host's hello says nothing of half_close: closing the writable closes.
  const a = session.connect('127.0.0.1', 8123);
  await a.writable.close();
  assert.equal(wisp.sent.filter((p) => p.stream === a.id).pop().type, WISP.CLOSE);
  // Told, the same close is END, the stream reads on, and the peer's END ends it.
  session.peerHalfClose = true;
  const b = session.connect('127.0.0.1', 8123);
  await b.writable.close();
  assert.equal(wisp.sent.filter((p) => p.stream === b.id).pop().type, WISP.END);
  assert.equal(b.done, false);
  host(encodeWisp(WISP.DATA, b.id, { data: new TextEncoder().encode('late answer') }));
  host(encodeWisp(WISP.END, b.id));
  const reader = b.readable.getReader();
  assert.equal(new TextDecoder().decode((await reader.read()).value), 'late answer');
  assert.equal((await reader.read()).done, true);
  await b.closed;
  assert.equal(wisp.sent.filter((p) => p.stream === b.id).pop().type, WISP.CLOSE);
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
  // A caller opens odd ids (§10.2).
  assert.deepEqual([a.id, b.id], [1, 3]);
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

test('direct mode applies one chosen ufrag as both ufrag and password, and connects', async () => {
  const { direct, withIceCredentials, DIRECT_UFRAG_LEN } = await import('./drt_browser_access.js');
  const made = 'v=0\r\na=ice-ufrag:abcd\r\na=ice-pwd:0123456789012345678901\r\na=mid:0\r\n';
  assert.equal(withIceCredentials(made, 'U', 'P'), 'v=0\r\na=ice-ufrag:U\r\na=ice-pwd:P\r\na=mid:0\r\n');

  const channels = [];
  let applied;
  class PC extends EventTarget {
    createDataChannel() {
      const c = new FakeChannel();
      channels.push(c);
      return c;
    }
    async createOffer() {
      return { type: 'offer', sdp: made };
    }
    async setLocalDescription(d) {
      applied = d.sdp;
      this.localDescription = d;
    }
    async setRemoteDescription(d) {
      this.remote = d.sdp;
      const [control, wisp] = channels;
      control.onopen();
      wisp.onopen();
      control.onmessage({ data: JSON.stringify({ v: 1, t: 'hello', scope: [], limits: { max_streams: 64 } }) });
      wisp.onmessage({ data: encodeWisp(WISP.CONTINUE, 0, { buffer: 2 }).buffer });
    }
    close() {}
  }
  const session = await direct(vectors.records[0].rtc, { RTCPeerConnection: PC });
  const ufrag = applied.match(/^a=ice-ufrag:(.*)$/m)[1].trim();
  const pwd = applied.match(/^a=ice-pwd:(.*)$/m)[1].trim();
  assert.equal(ufrag.length, DIRECT_UFRAG_LEN);
  assert.match(ufrag, /^[A-Za-z0-9+/]+$/);
  assert.equal(pwd, ufrag);
  assert.equal(session.hello.t, 'hello');
  assert.equal(session.pc.remote, answerSdp(vectors.records[0].rtc, '0'));
});

test('direct mode refuses a bad host record before making a connection', async () => {
  const { direct } = await import('./drt_browser_access.js');
  let made = 0;
  class PC { constructor() { made++; } }
  await assert.rejects(direct('{}', { RTCPeerConnection: PC }), RecordError);
  assert.equal(made, 0);
});

// depth: §10, either peer serves and services have names

test('offerSdp is answerSdp with the caller as DTLS client', async () => {
  const { offerSdp } = await import('./drt_browser_access.js');
  const rtc = vectors.records[0].rtc;
  assert.equal(offerSdp(rtc, '0'), answerSdp(rtc, '0').replace('a=setup:passive', 'a=setup:active'));
});

test('a named CONNECT is port 0 and the name, and decodes back', async () => {
  const { isServiceName } = await import('./drt_browser_access.js');
  const p = decodeWisp(encodeWisp(WISP.CONNECT, 3, { host: 'ssh', port: 0 }));
  assert.deepEqual([p.type, p.stream, p.kind, p.port, p.host], [WISP.CONNECT, 3, 1, 0, 'ssh']);
  for (const ok of ['ssh', 'a', 'dom-debug', '9p']) assert.ok(isServiceName(ok), ok);
  for (const bad of ['', '-x', 'SSH', 'a b', 'x'.repeat(33), 'ssh.local']) assert.ok(!isServiceName(bad), bad);
});

/**
 * A page answering a caller. The caller serves (sends hello and credit)
 * unless `caller: 'silent'`, in which case the session settles after
 * `settleMs` with nothing from it.
 */
async function answeringSession(services, { caller = 'serving', settleMs = 20 } = {}) {
  const { answer } = await import('./drt_browser_access.js');
  const channels = [];
  const fp = vectors.records[0].decoded.f_hex;
  class PC extends EventTarget {
    constructor() {
      super();
      this.iceGatheringState = 'complete';
    }
    createDataChannel() {
      const c = new FakeChannel();
      channels.push(c);
      return c;
    }
    async setRemoteDescription(d) {
      this.offered = d;
    }
    async createAnswer() {
      return { type: 'answer', sdp: `a=ice-ufrag:pageUfrag\r\na=ice-pwd:pagePassword0123456789ab\r\na=fingerprint:sha-256 ${fp}\r\na=mid:0\r\n` };
    }
    async setLocalDescription(d) {
      this.localDescription = d;
    }
    close() {}
  }
  const a = await answer(vectors.records[0].rtc, { RTCPeerConnection: PC, services, label: 'page', settleMs });
  const [control, wisp] = channels;
  control.sentText = [];
  control.send = (t) => control.sentText.push(JSON.parse(t));
  control.onopen();
  wisp.onopen();
  const peer = (packet) => wisp.onmessage({ data: packet.buffer });
  const say = (m) => control.onmessage({ data: JSON.stringify(m) });
  if (caller === 'serving') {
    say({ v: 1, t: 'hello', service: 'caller', scope: [], services: ['back'], limits: { max_streams: 64 } });
    peer(encodeWisp(WISP.CONTINUE, 0, { buffer: 16 }));
  }
  const session = await a.session;
  return { a, session, control, wisp, peer, say, pc: session.pc };
}

test('a page answers: the caller\'s record as an offer, its own record back, then hello and credit', async () => {
  const { a, control, wisp, pc } = await answeringSession({ ssh: () => {} });
  assert.equal(pc.offered.type, 'offer');
  assert.match(pc.offered.sdp, /a=setup:active/);
  assert.equal(a.record.u, 'pageUfrag');
  assert.deepEqual(control.sentText[0].services, ['ssh']);
  assert.equal(control.sentText[0].service, 'page');
  assert.deepEqual([wisp.sent[0].type, wisp.sent[0].stream, wisp.sent[0].buffer], [WISP.CONTINUE, 0, 128]);
});

test('a served stream reaches its service, and the reader\'s pace is the opener\'s credit', async () => {
  let got;
  const { wisp, peer } = await answeringSession({ ssh: (stream) => (got = stream) });
  peer(encodeWisp(WISP.CONNECT, 1, { host: 'ssh', port: 0 }));
  assert.equal(got.id, 1);
  for (let i = 0; i < 70; i++) peer(encodeWisp(WISP.DATA, 1, { data: Uint8Array.of(i) }));
  const reader = got.readable.getReader();
  for (let i = 0; i < 64; i++) assert.equal((await reader.read()).value[0], i);
  // Half the buffer taken: CONTINUE with the space left, 128 less the 6 queued.
  const cont = wisp.sent.filter((p) => p.type === WISP.CONTINUE && p.stream === 1);
  assert.deepEqual(cont.map((p) => p.buffer), [122]);
  const writer = got.writable.getWriter();
  await writer.write(Uint8Array.of(9));
  assert.deepEqual([...wisp.sent.at(-1).payload], [9]);
});

test('what a page does not serve is refused by the rule that names it', async () => {
  const { wisp, peer } = await answeringSession({ ssh: () => assert.fail('not this one') });
  const closeFor = (id) => wisp.sent.find((p) => p.type === WISP.CLOSE && p.stream === id)?.reason;
  peer(encodeWisp(WISP.CONNECT, 1, { host: 'telnet', port: 0 }));
  peer(encodeWisp(WISP.CONNECT, 3, { host: '127.0.0.1', port: 22 }));
  peer(encodeWisp(WISP.CONNECT, 2, { host: 'ssh', port: 0 }));
  assert.deepEqual([closeFor(1), closeFor(3), closeFor(2)], [0x48, 0x48, 0x41]);
});

test('the answerer opens even ids to what the caller serves', async () => {
  const { session } = await answeringSession({});
  assert.deepEqual(session.hello.services, ['back']);
  assert.deepEqual([session.connect('back').id, session.connect('back').id], [2, 4]);
});

test('an answered session is ready once the caller\'s hello and credit are in, and connect works at once', async () => {
  const { answer } = await import('./drt_browser_access.js');
  const channels = [];
  const fp = vectors.records[0].decoded.f_hex;
  class PC extends EventTarget {
    constructor() { super(); this.iceGatheringState = 'complete'; }
    createDataChannel() { const c = new FakeChannel(); channels.push(c); return c; }
    async setRemoteDescription() {}
    async createAnswer() {
      return { type: 'answer', sdp: `a=ice-ufrag:pageUfrag\r\na=ice-pwd:pagePassword0123456789ab\r\na=fingerprint:sha-256 ${fp}\r\na=mid:0\r\n` };
    }
    async setLocalDescription(d) { this.localDescription = d; }
    close() {}
  }
  const a = await answer(vectors.records[0].rtc, { RTCPeerConnection: PC, settleMs: 200 });
  const [control, wisp] = channels;
  control.send = () => {};
  control.onopen();
  wisp.onopen();
  let settled = false;
  a.session.then(() => (settled = true));
  await new Promise((r) => setTimeout(r, 30));
  assert.equal(settled, false, 'channels open is not yet a session');
  // hello alone is not enough: a serving caller sends credit too.
  control.onmessage({ data: JSON.stringify({ v: 1, t: 'hello', service: 'c', scope: [], services: ['x'], limits: { max_streams: 64 } }) });
  await new Promise((r) => setTimeout(r, 30));
  assert.equal(settled, false, 'hello without credit keeps waiting');
  wisp.onmessage({ data: encodeWisp(WISP.CONTINUE, 0, { buffer: 8 }).buffer });
  const session = await a.session;
  assert.equal(session.connect('x').id, 2);
});

test('a caller that serves nothing settles the answered session after the grace, with no hello', async () => {
  const { session } = await answeringSession({}, { caller: 'silent', settleMs: 20 });
  assert.equal(session.hello, null);
  assert.throws(() => session.connect('dom'), /serves nothing/);
});

// depth: listen, against a fake signalling server (doc/DRT-Signalling.md)

/** A peer connection that answers at once and whose channels open. */
function answeringPC() {
  const fp = vectors.records[0].decoded.f_hex;
  return class extends EventTarget {
    constructor() {
      super();
      this.iceGatheringState = 'complete';
      this.channels = [];
    }
    createDataChannel() {
      const c = new FakeChannel();
      this.channels.push(c);
      return c;
    }
    async setRemoteDescription() {}
    async createAnswer() {
      return { type: 'answer', sdp: `a=ice-ufrag:pageUfrag\r\na=ice-pwd:pagePassword0123456789ab\r\na=fingerprint:sha-256 ${fp}\r\na=mid:0\r\n` };
    }
    async setLocalDescription(d) {
      this.localDescription = d;
      const [control, wisp] = this.channels;
      control.send = () => {};
      queueMicrotask(() => {
        control.onopen();
        wisp.onopen();
      });
    }
    close() {}
  };
}

/**
 * A peer connection for a page that both answers and, told to, calls: as
 * `answeringPC`, plus the offer side of `fakeSession`'s, whose far end
 * serves (hello naming `back`, and credit) the moment the answer lands.
 */
function pairingPC(made = [], { connects = true } = {}) {
  const fp = vectors.records[0].decoded.f_hex;
  return class extends EventTarget {
    constructor() {
      super();
      this.iceGatheringState = 'complete';
      this.connectionState = 'new';
      this.channels = [];
      made.push(this);
    }
    createDataChannel() {
      const c = new FakeChannel();
      this.channels.push(c);
      return c;
    }
    async createOffer() {
      this.offering = true;
      return { type: 'offer', sdp: '' };
    }
    async createAnswer() {
      return { type: 'answer', sdp: `a=ice-ufrag:pageUfrag\r\na=ice-pwd:pagePassword0123456789ab\r\na=fingerprint:sha-256 ${fp}\r\na=mid:0\r\n` };
    }
    async setLocalDescription(d) {
      this.localDescription = this.offering
        ? { sdp: `a=ice-ufrag:abcd\r\na=ice-pwd:0123456789012345678901\r\na=fingerprint:sha-256 ${fp}\r\na=mid:0\r\n` }
        : d;
      if (this.offering) return;
      const [control, wisp] = this.channels;
      control.send = () => {};
      queueMicrotask(() => {
        control.onopen();
        wisp.onopen();
      });
    }
    async setRemoteDescription() {
      if (!this.offering || !connects) return;
      const [control, wisp] = this.channels;
      control.onopen();
      wisp.onopen();
      control.onmessage({ data: JSON.stringify({ v: 1, t: 'hello', service: 'asker', scope: [], services: ['back'], limits: { max_streams: 64 } }) });
      wisp.onmessage({ data: encodeWisp(WISP.CONTINUE, 0, { buffer: 4 }).buffer });
    }
    close() {
      this.connectionState = 'closed';
    }
  };
}

/**
 * One name's calls and pair entries, served the way §2 and §6.2 say, and
 * every request it saw. The page's name is `page`; the caller's request
 * for any other name is answered with records[1] as the asker's record,
 * or with `callStatus` and its reason.
 */
function fakeServer(calls, { callStatus = 200, callError = '', holdCalls = false } = {}) {
  const seen = [];
  let cursor = 0;
  const waiting = new Map();
  const pairs = new Map();
  const results = [];
  // With `holdCalls`, the caller's request is held until release(), as a
  // server holds it for the name to answer (§3).
  let release = () => {};
  const held = new Promise((r) => (release = r));
  const add = (record) => {
    cursor += 1;
    waiting.set(`c${cursor}`, { id: `c${cursor}`, record, expires_in: 25, n: cursor });
  };
  const addPair = ({ name, token, server, expires_in = 25 }) => {
    cursor += 1;
    const e = { id: `p${cursor}`, name, expires_in, n: cursor };
    if (token !== undefined) e.token = token;
    if (server !== undefined) e.server = server;
    pairs.set(e.id, e);
  };
  for (const r of calls) add(r);
  const reply = (status, body, text = '') => ({ ok: status < 300, status, json: async () => body, text: async () => text });
  const fetch = async (url, init = {}) => {
    const u = new URL(url);
    const method = init.method ?? 'GET';
    seen.push({ method, path: u.pathname, query: u.search, auth: init.headers?.authorization, body: init.body });
    const parts = u.pathname.split('/');
    const name = parts[2];
    if (name !== 'page' && method === 'POST' && u.pathname.endsWith('/calls')) {
      if (holdCalls) await held;
      if (callStatus !== 200) return reply(callStatus, { error: callError });
      return reply(200, null, vectors.records[1].rtc);
    }
    if (method === 'GET' && u.pathname.endsWith('/calls')) {
      const since = Number(u.searchParams.get('since') ?? 0);
      const out = [...waiting.values()].filter((c) => c.n > since).map(({ n, ...c }) => c);
      const pair = [...pairs.values()].filter((p) => p.n > since).map(({ n, ...p }) => p);
      return reply(200, { cursor: String(cursor), calls: out, pair });
    }
    if (method === 'POST' && parts[3] === 'pair' && parts[5] === 'result') {
      if (!pairs.has(parts[4])) return reply(404, { error: 'no such pairing' });
      results.push({ id: parts[4], ...JSON.parse(init.body) });
      return reply(204, null);
    }
    const id = parts[4];
    if (!waiting.has(id)) return reply(404, { error: 'no such call' });
    waiting.delete(id);
    return reply(method === 'POST' || method === 'DELETE' ? 204 : 404, null);
  };
  return { fetch, seen, add, addPair, results, release: () => release() };
}

/** An EventSource the test opens, notifies and breaks by hand. */
function fakeEvents() {
  const made = [];
  class ES extends EventTarget {
    constructor(url) {
      super();
      this.url = url;
      made.push(this);
    }
    close() {
      this.closed = true;
    }
    fire(type) {
      this.dispatchEvent(new Event(type));
    }
  }
  return { ES, made };
}

const until = async (what, cond, tries = 200) => {
  for (let i = 0; i < tries; i++) {
    if (cond()) return;
    await new Promise((r) => setTimeout(r, 5));
  }
  assert.fail(`never: ${what}`);
};

test('listen answers each waiting call, posts the page\'s record, and carries the cursor', async () => {
  const { listen } = await import('./drt_browser_access.js');
  const server = fakeServer([vectors.records[0].rtc, vectors.records[1].rtc]);
  const sessions = [];
  // The fake caller serves nothing: each session settles after `settleMs`.
  const l = listen('https://signal.example/v1/page/', {
    token: 'tok', events: false, pollMs: 20, fetch: server.fetch, RTCPeerConnection: answeringPC(),
    services: { ssh: () => {} }, onSession: (s, call) => sessions.push(call.id), settleMs: 10,
  });
  await until('two sessions', () => sessions.length === 2);
  const answers = server.seen.filter((r) => r.method === 'POST');
  assert.deepEqual(answers.map((r) => r.path), ['/v1/page/calls/c1/answer', '/v1/page/calls/c2/answer']);
  assert.equal(JSON.parse(answers[0].body).u, 'pageUfrag');
  assert.equal(answers[0].auth, 'Bearer tok');
  // The next poll passes back the cursor it was given.
  await until('a poll from cursor 2', () => server.seen.some((r) => r.query.includes('since=2')));
  assert.equal(l.cursor, '2');
  server.add(vectors.records[0].rtc);
  await until('the third', () => sessions.length === 3);
  assert.deepEqual(sessions, ['c1', 'c2', 'c3']);
  l.close();
});

test('listen withdraws a call accept refuses, and one whose record answer cannot take', async () => {
  const { listen } = await import('./drt_browser_access.js');
  const server = fakeServer([vectors.records[0].rtc, 'not a record']);
  const errors = [];
  const l = listen('https://signal.example/v1/page', {
    events: false, pollMs: 1000, fetch: server.fetch, RTCPeerConnection: answeringPC(),
    accept: (call) => call.id !== 'c1', onError: (e, call) => errors.push(call?.id),
  });
  await until('two withdrawals', () => server.seen.filter((r) => r.method === 'DELETE').length === 2);
  assert.deepEqual(server.seen.filter((r) => r.method === 'DELETE').map((r) => r.path),
    ['/v1/page/calls/c1', '/v1/page/calls/c2']);
  assert.deepEqual(errors, ['c2']);
  assert.equal(server.seen.filter((r) => r.method === 'POST').length, 0);
  l.close();
});

test('listen holds the call notification stream, and polls on a timer only while it has none', async () => {
  const { listen } = await import('./drt_browser_access.js');
  const server = fakeServer([]);
  const { ES, made } = fakeEvents();
  const polls = () => server.seen.filter((r) => r.method === 'GET').length;
  const l = listen('https://signal.example/v1/page', {
    token: 'a b', pollMs: 30, fetch: server.fetch, EventSource: ES, RTCPeerConnection: answeringPC(),
  });
  assert.equal(made[0].url, 'https://signal.example/v1/page/events?k=a%20b');
  await until('the first poll', () => polls() === 1);
  made[0].fire('open');
  assert.equal(l.streaming, true);
  await until('a poll on connecting', () => polls() === 2);
  // Streaming: the timer stops, and a notification is what polls.
  await new Promise((r) => setTimeout(r, 120));
  assert.equal(polls(), 2);
  server.add(vectors.records[0].rtc);
  made[0].fire('call');
  await until('the answer', () => server.seen.some((r) => r.method === 'POST'));
  // The stream breaks: the timer is back until it reconnects.
  made[0].fire('error');
  assert.equal(l.streaming, false);
  await until('timer polls again', () => polls() >= 5);
  l.close();
  assert.equal(made[0].closed, true);
  const after = polls();
  await new Promise((r) => setTimeout(r, 100));
  assert.equal(polls(), after);
});

test('listen closes an answer the server no longer wanted, and keeps listening', async () => {
  const { listen } = await import('./drt_browser_access.js');
  const server = fakeServer([vectors.records[0].rtc]);
  const real = server.fetch;
  // The call expires between the poll and the answer.
  const fetch = async (url, init = {}) => {
    if (init.method === 'POST') return { ok: false, status: 404, json: async () => ({}) };
    return real(url, init);
  };
  const errors = [];
  const l = listen('https://signal.example/v1/page', {
    events: false, pollMs: 1000, fetch, RTCPeerConnection: answeringPC(),
    onError: (e, call) => errors.push([call?.id, String(e.message)]),
  });
  await until('the error', () => errors.length === 1);
  assert.equal(errors[0][0], 'c1');
  assert.match(errors[0][1], /answered 404/);
  l.close();
});

// depth: a page as the told side (doc/DRT-Signalling.md §6.2)

test('the pair rule is any name here or a pattern at a named server, as the Rust twin reads it', async () => {
  const { parsePairRule, pairAllows } = await import('./drt_browser_access.js');
  const here = 'http://127.0.0.1:9000';
  const any = parsePairRule('*');
  assert.deepEqual(any, { server: null, glob: '*' });
  assert.ok(pairAllows(any, here, here, 'room-7'));
  assert.ok(pairAllows(any, here, 'http://127.0.0.1:9000/', 'x'));
  assert.ok(pairAllows(any, here, 'HTTP://127.0.0.1:9000', 'x'));
  assert.ok(!pairAllows(any, here, 'https://elsewhere.example', 'room-7'));
  const at = parsePairRule('drt://127.0.0.1:9000/v1/room-*');
  assert.deepEqual(at, { server: 'http://127.0.0.1:9000', glob: 'room-*' });
  assert.ok(pairAllows(at, here, here, 'room-7'));
  assert.ok(!pairAllows(at, here, here, 'lobby'));
  assert.ok(!pairAllows(at, here, 'http://127.0.0.1:9001', 'room-7'));
  // A name without a server is not a rule: the risk is the server.
  assert.throws(() => parsePairRule('room-*'), /drt:\/\/<server>\/v1\/<glob>/);
  assert.throws(() => parsePairRule('drt://127.0.0.1:9000'), TypeError);
  // The glob: `*` and nothing else.
  const g = (glob, name) => pairAllows({ server: here, glob }, here, here, name);
  assert.ok(g('*', 'anything'));
  assert.ok(g('room-*', 'room-7') && !g('room-*', 'lobby-room-7'));
  assert.ok(g('*-7', 'room-7') && g('r*m*7', 'room-7'));
  assert.ok(g('exact', 'exact') && !g('exact', 'exactly'));
  assert.ok(!g('a.b', 'aXb'));
});

/** A listening page with a fake server and the pairing peer connection; resolves once `n` pair reports are in. */
async function pagedListen(server, options, n = 1) {
  const { listen } = await import('./drt_browser_access.js');
  const reports = [];
  const errors = [];
  const made = [];
  const l = listen('https://signal.example/v1/page', {
    token: 'page-token', events: false, pollMs: 1000, fetch: server.fetch, RTCPeerConnection: pairingPC(made),
    services: { ssh: () => {} }, label: 'page', pairConnectMs: 500,
    onPair: (r) => reports.push(r), onError: (e, who) => errors.push([who?.id, String(e?.message ?? e)]),
    ...options,
  });
  await until(`${n} pair report(s)`, () => reports.length >= n);
  return { l, reports, errors, made };
}

/** The hellos a page sent on its control channels, oldest first. */
const hellosSent = (made) => made
  .flatMap((pc) => pc.channels[0]?.sent ?? [])
  .filter((t) => typeof t === 'string')
  .map((t) => JSON.parse(t))
  .filter((m) => m.t === 'hello');

test('without consent a pair entry is declined, and the server is told', async () => {
  const server = fakeServer([]);
  server.addPair({ name: 'host', token: 'host-caller' });
  const { l, reports } = await pagedListen(server, {});
  assert.deepEqual(reports.map((r) => [r.outcome, r.why, r.session]), [['declined', 'no pair', null]]);
  assert.deepEqual(server.results, [{ id: 'p1', outcome: 'declined', why: 'no pair' }]);
  assert.equal(server.seen.filter((r) => r.method === 'POST' && r.path.endsWith('/calls')).length, 0, 'no call was made');
  l.close();
});

test('a pair entry outside the rule is declined by name and server', async () => {
  const server = fakeServer([]);
  server.addPair({ name: 'host', server: 'https://elsewhere.example' });
  const { l, reports } = await pagedListen(server, { pair: '*' });
  assert.equal(reports[0].outcome, 'declined');
  assert.equal(reports[0].why, 'host at https://elsewhere.example is outside pair');
  l.close();
});

test('told to call, the page calls with the entry\'s token, serves on the session, and reports connected', async () => {
  const server = fakeServer([]);
  server.addPair({ name: 'host', token: 'host-caller' });
  const { l, reports, errors, made } = await pagedListen(server, { pair: '*' });
  assert.deepEqual(errors, []);
  const [r] = reports;
  assert.equal(r.outcome, 'connected');
  assert.equal(r.why, '');
  assert.equal(r.entry.id, 'p1');
  assert.deepEqual(r.session.hello.services, ['back'], 'the asker\'s hello');
  assert.equal(r.session.role, 'caller');
  assert.equal(r.session.connect('back').id, 1, 'the told side is the caller: odd ids');
  // The page serves on the session it was told to open: its hello names
  // its services, and a CONNECT to one of them reaches the handler.
  const [hello] = hellosSent(made);
  assert.deepEqual([hello.service, hello.services], ['page', ['ssh']]);
  const call = server.seen.find((x) => x.method === 'POST' && x.path === '/v1/host/calls');
  assert.equal(call.auth, 'Bearer host-caller');
  assert.equal(JSON.parse(call.body).u, 'abcd', 'the page\'s own record is the request');
  assert.deepEqual(server.results, [{ id: 'p1', outcome: 'connected', why: '' }]);
  const result = server.seen.find((x) => x.path === '/v1/page/pair/p1/result');
  assert.equal(result.auth, 'Bearer page-token', 'the result is signed with the answerer token');
  l.close();
});

test('an entry without a token makes the call with no authorization at all', async () => {
  const server = fakeServer([]);
  server.addPair({ name: 'open-host' });
  const { l, reports } = await pagedListen(server, { pair: '*' });
  assert.equal(reports[0].outcome, 'connected');
  const call = server.seen.find((x) => x.method === 'POST' && x.path === '/v1/open-host/calls');
  assert.equal(call.auth, undefined);
  l.close();
});

test('410 on the call is refused, anything else is unreachable with the server\'s reason', async () => {
  const refused = fakeServer([], { callStatus: 410, callError: 'the call was withdrawn or refused' });
  refused.addPair({ name: 'host' });
  let { l, reports } = await pagedListen(refused, { pair: '*' });
  assert.deepEqual([reports[0].outcome, reports[0].why], ['refused', '410 the call was withdrawn or refused']);
  l.close();
  const absent = fakeServer([], { callStatus: 503, callError: 'no answerer is present' });
  absent.addPair({ name: 'host' });
  ({ l, reports } = await pagedListen(absent, { pair: '*' }));
  assert.deepEqual([reports[0].outcome, reports[0].why], ['unreachable', '503 no answerer is present']);
  assert.deepEqual(absent.results[0], { id: 'p1', outcome: 'unreachable', why: '503 no answerer is present' });
  l.close();
});

test('a call that never connects is reported unreachable inside the budget, and leaves no connection behind', async () => {
  const server = fakeServer([]);
  server.addPair({ name: 'host', token: 't' });
  const made = [];
  const { listen } = await import('./drt_browser_access.js');
  const reports = [];
  const l = listen('https://signal.example/v1/page', {
    token: 'page-token', events: false, pollMs: 100000, fetch: server.fetch,
    RTCPeerConnection: pairingPC(made, { connects: false }), pair: '*', pairConnectMs: 150,
    onPair: (r) => reports.push(r),
  });
  await until('the report', () => reports.length === 1);
  assert.equal(reports[0].outcome, 'unreachable');
  assert.match(reports[0].why, /no session within/);
  assert.deepEqual(server.results, [{ id: 'p1', outcome: 'unreachable', why: reports[0].why }]);
  assert.equal(made.length, 1);
  assert.equal(made[0].connectionState, 'closed', 'the peer connection was closed');
  l.close();
});

test('an entry\'s hold bounds the follow below pairConnectMs', async () => {
  const { PAIR_MARGIN_MS } = await import('./drt_browser_access.js');
  const server = fakeServer([]);
  // One second over the margin: the follow has one second, not the ceiling.
  server.addPair({ name: 'host', expires_in: (PAIR_MARGIN_MS + 1000) / 1000 });
  const made = [];
  const { listen } = await import('./drt_browser_access.js');
  const reports = [];
  const t0 = Date.now();
  const l = listen('https://signal.example/v1/page', {
    token: 'page-token', events: false, pollMs: 100000, fetch: server.fetch,
    RTCPeerConnection: pairingPC(made, { connects: false }), pair: '*', pairConnectMs: 60000,
    onPair: (r) => reports.push(r),
  });
  await until('the report', () => reports.length === 1, 400);
  assert.equal(reports[0].outcome, 'unreachable');
  assert.ok(Date.now() - t0 < 3000, 'reported inside the hold, not at the ceiling');
  l.close();
});

test('a follow in flight when listen closes makes no session', async () => {
  const server = fakeServer([], { holdCalls: true });
  server.addPair({ name: 'host', token: 't' });
  const made = [];
  const { listen } = await import('./drt_browser_access.js');
  const reports = [];
  const l = listen('https://signal.example/v1/page', {
    token: 'page-token', events: false, pollMs: 100000, fetch: server.fetch,
    RTCPeerConnection: pairingPC(made), pair: '*', pairConnectMs: 500, onPair: (r) => reports.push(r),
  });
  await until('the held call', () => server.seen.some((x) => x.method === 'POST' && x.path === '/v1/host/calls'));
  l.close();
  server.release();
  await until('the report', () => reports.length === 1);
  assert.equal(reports[0].outcome, 'unreachable');
  assert.equal(reports[0].why, 'listening stopped');
  assert.equal(reports[0].session, null);
  assert.equal(made[0].connectionState, 'closed');
});

test('a pair notification on the stream wakes a poll, and a follow does not hold up the calls beside it', async () => {
  const { listen } = await import('./drt_browser_access.js');
  const server = fakeServer([], { holdCalls: true });
  const { ES, made } = fakeEvents();
  // A call to answer and an entry to follow arrive in one poll.
  server.addPair({ name: 'host', token: 't' });
  server.add(vectors.records[0].rtc);
  const sessions = [];
  const reports = [];
  const l = listen('https://signal.example/v1/page', {
    token: 'page-token', pollMs: 100000, fetch: server.fetch, EventSource: ES, RTCPeerConnection: pairingPC(),
    pair: '*', pairConnectMs: 500, settleMs: 10, onSession: (s, call) => sessions.push(call.id), onPair: (r) => reports.push(r),
  });
  await until('the stream', () => made.length === 1);
  const polls = () => server.seen.filter((r) => r.method === 'GET' && r.path.endsWith('/calls')).length;
  const before = polls();
  made[0].fire('open');
  await until('the poll the open caused', () => polls() === before + 1);
  // The caller's request is held by the server; the call beside it is
  // answered meanwhile, which a follow inside the poll could not allow.
  await until('the call answered', () => sessions.length === 1);
  assert.equal(reports.length, 0, 'the follow is still waiting on its held request');
  server.release();
  await until('the follow reported', () => reports.length === 1);
  assert.equal(reports[0].outcome, 'connected');
  const n = polls();
  made[0].fire('pair');
  await until('a poll from the pair event', () => polls() === n + 1);
  l.close();
});

test('a bad pair rule fails listen() itself, before any poll', async () => {
  const { listen } = await import('./drt_browser_access.js');
  const server = fakeServer([]);
  assert.throws(() => listen('https://signal.example/v1/page', { fetch: server.fetch, pair: 'room-*' }), TypeError);
  assert.equal(server.seen.length, 0);
});

// A peer connection that gathers what `answers` says each server maps it
// to, dropping an address another server already gave, as Chromium does.
function fakePeerConnection(answers) {
  return class {
    constructor({ iceServers }) {
      this.urls = iceServers.flatMap((s) => [s.urls].flat());
      this.iceGatheringState = 'new';
      this.listeners = [];
    }
    addEventListener(_, f) { this.listeners.push(f); }
    removeEventListener() {}
    createDataChannel() {}
    async createOffer() { return {}; }
    async setLocalDescription() {
      const seen = new Set();
      for (const url of this.urls) {
        for (const [address, port] of answers[url] ?? []) {
          if (seen.has(`${address} ${port}`)) continue;
          seen.add(`${address} ${port}`);
          this.listeners.forEach((f) => f({ candidate: { type: 'srflx', address, port } }));
        }
      }
      this.iceGatheringState = 'complete';
    }
    close() {}
  };
}

test('reflect asks each server alone, then together for the mapping', async () => {
  const run = (answers, servers) => reflect(servers, { RTCPeerConnection: fakePeerConnection(answers), gatherTimeoutMs: 50 });
  const one = [['203.0.113.7', 40000]];
  const independent = await run({ 'stun:a:3478': one, 'stun:b:3478': one }, ['a', 'drt+reflect://b']);
  assert.deepEqual(independent.udp, { code: 'ok', mapped: ['203.0.113.7:40000'] });
  assert.deepEqual(independent.mapping, { code: 'ok', mapping: 'endpoint_independent' });
  const dependent = await run({ 'stun:a:3478': one, 'stun:b:3479': [['203.0.113.7', 40001]] }, ['stun:a:3478', 'b:3479']);
  assert.equal(dependent.mapping.mapping, 'endpoint_dependent');
  assert.deepEqual(dependent.udp.mapped, ['203.0.113.7:40000', '203.0.113.7:40001']);
  // Two interfaces behind one independent NAT are not a dependent mapping.
  const two = [['203.0.113.7', 40000], ['203.0.113.7', 40002]];
  assert.equal((await run({ 'stun:a:3478': two, 'stun:b:3478': two }, ['a', 'b'])).mapping.mapping, 'endpoint_independent');
  const v6 = await run({ 'stun:[2001:db8::1]:3478': [['2001:db8::7', 5000]] }, ['[2001:db8::1]', 'silent']);
  assert.deepEqual(v6.servers.map((s) => s.code), ['ok', 'udp_blocked']);
  assert.deepEqual(v6.udp.mapped, ['[2001:db8::7]:5000']);
  assert.deepEqual(v6.mapping, { code: 'no_peer' });
  assert.deepEqual((await run({}, ['a', 'b'])).mapping, { code: 'udp_blocked' });
});
