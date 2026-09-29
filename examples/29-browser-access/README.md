# 29-browser-access

A browser reaching this host directly, with this host doing its own
signaling. The two sides swap one small record each, once, over an ordinary
HTTP request, and then talk over WebRTC with nothing in between.

## Run it

```
cd examples/29-browser-access
drt start --config app.json
```

and, from a page (an ES module beside `drt_browser_access.js`, which every
release attaches):

```js
import { offer } from './drt_browser_access.js';

const pending = await offer();
const reply = await fetch('http://127.0.0.1:18490/session', { method: 'POST', body: pending.recordText });
const session = await pending.accept(await reply.text());
const ssh = session.connect('127.0.0.1', 22);   // a stream: ssh.readable, ssh.writable
```

`ssh.html`'s module takes that stream as it is: `Ssh.connect(ssh, pin)`.
`./demo.sh` plays the browser's half with `curl`, which is how the gate runs
it.

## What you should see

```
drt start: http listening on 127.0.0.1:18490
drt webrtc: serving on 127.0.0.1:18491, record on `webrtc`
HTTP/1.1 200 OK
access-control-allow-origin: *
{"v":1,"u":"<ufrag>","p":"<password>","f":"<fingerprint>","c":["candidate:… 127.0.0.1 18491 typ host"]}

browser-2	closed	record is not JSON: expected ident at line 1 column 2
```

## What it teaches

**Signaling is a program.** `app.dlua` holds the host's record from the
`webrtc` queue, answers `POST /session` with it, and hands the browser's
record to the host as `{command = "open"}`. That is all a signaling service
does for one host; a room for many hosts is the same program keeping a table.

**The record is not the traffic.** About 300 bytes cross the HTTP request,
once. Everything after goes browser to host, and the host only opens what
`scope` in `app.json` names: here, one sshd.

**A bad record is refused by the host, not the program.** `browser-2` was
never JSON; the program passed it on without reading it, and the host
closed it with the reason.

**With no program at all**, `"direct": true` in the `webrtc` block lets a
browser holding the record connect with nothing sent back
(`doc/BrowserAccess.md` §3.4): `ssh.html#rtc=<record>&user=you`.
