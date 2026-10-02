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
  RecordError, StreamClosed, WISP, DATA_MAX,
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

async function answeringSession(services) {
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
  const a = await answer(vectors.records[0].rtc, { RTCPeerConnection: PC, services, label: 'page' });
  const [control, wisp] = channels;
  control.sentText = [];
  control.send = (t) => control.sentText.push(JSON.parse(t));
  control.onopen();
  wisp.onopen();
  const session = await a.session;
  const peer = (packet) => wisp.onmessage({ data: packet.buffer });
  return { a, session, control, wisp, peer, pc: session.pc };
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
  const { session, peer } = await answeringSession({});
  assert.throws(() => session.connect('dom'), /serves nothing/);
  peer(encodeWisp(WISP.CONTINUE, 0, { buffer: 16 }));
  assert.deepEqual([session.connect('dom').id, session.connect('dom').id], [2, 4]);
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

/** One name's calls, served the way §2 says, and every request it saw. */
function fakeServer(calls) {
  const seen = [];
  let cursor = 0;
  const waiting = new Map();
  const add = (record) => {
    cursor += 1;
    waiting.set(`c${cursor}`, { id: `c${cursor}`, record, expires_in: 25, n: cursor });
  };
  for (const r of calls) add(r);
  const reply = (status, body) => ({ ok: status < 300, status, json: async () => body });
  const fetch = async (url, init = {}) => {
    const u = new URL(url);
    const method = init.method ?? 'GET';
    seen.push({ method, path: u.pathname, query: u.search, auth: init.headers?.authorization, body: init.body });
    if (method === 'GET' && u.pathname.endsWith('/calls')) {
      const since = Number(u.searchParams.get('since') ?? 0);
      const out = [...waiting.values()].filter((c) => c.n > since).map(({ n, ...c }) => c);
      return reply(200, { cursor: String(cursor), calls: out });
    }
    const id = u.pathname.split('/')[4];
    if (!waiting.has(id)) return reply(404, { error: 'no such call' });
    waiting.delete(id);
    return reply(method === 'POST' || method === 'DELETE' ? 204 : 404, null);
  };
  return { fetch, seen, add };
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

const until = async (what, cond) => {
  for (let i = 0; i < 200; i++) {
    if (cond()) return;
    await new Promise((r) => setTimeout(r, 5));
  }
  assert.fail(`never: ${what}`);
};

test('listen answers each waiting call, posts the page\'s record, and carries the cursor', async () => {
  const { listen } = await import('./drt_browser_access.js');
  const server = fakeServer([vectors.records[0].rtc, vectors.records[1].rtc]);
  const sessions = [];
  const l = listen('https://signal.example/v1/page/', {
    token: 'tok', events: false, pollMs: 20, fetch: server.fetch, RTCPeerConnection: answeringPC(),
    services: { ssh: () => {} }, onSession: (s, call) => sessions.push(call.id),
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
