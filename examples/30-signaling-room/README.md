# 30-signaling-room

A signalling server for `doc/DRT-Signalling.md`, written as a `drt start`
program. It is for an answerer that cannot take a request, such as a
browser page: the answerer reads calls from the server, and the caller's
one request is held until the answerer replies.

## Run it

```
cd examples/30-signaling-room
drt start --config app.json
```

A page serving `ssh` reads calls and answers each one
(`drt_browser_access.js`, `doc/BrowserAccess.md` §10.4):

```js
const base = 'http://127.0.0.1:18495/v1/page', k = '?k=answerer-token-for-the-example-only';
let cursor = '0';
async function poll() {
  const got = await (await fetch(`${base}/calls${k}&since=${cursor}`)).json();
  cursor = got.cursor;
  for (const call of got.calls) {
    const a = await answer(call.record, { services: { ssh } });
    await fetch(`${base}/calls/${call.id}/answer${k}`, { method: 'POST', body: a.recordText });
  }
}
const events = new EventSource(`${base}/events${k}`);
events.onopen = poll;                     // once on connecting, then once per call
events.addEventListener('call', poll);
```

and from anywhere with `drt`:

```
ssh -o ProxyCommand="drt tunnel 'rtc:http://127.0.0.1:18495/v1/page/calls?k=caller-token-for-the-example-only'" you@page
```

`./demo.sh` plays both halves with `curl`, which is how the gate runs it.

## What you should see

```
drt start: http listening on 127.0.0.1:18495
call:    {"error":"no answerer is present"} 503
poll:    {"cursor":"0","calls":[]}
poll:    {"cursor":"1","calls":[{"id":"c1","record":"{\"caller\":\"record\"}","expires_in":25}]}
answer:  204
caller:  HTTP/1.1 200 OK location: /v1/page/calls/c1 {"answerer":"record"}
poll:    {"cursor":"1","calls":[]}
again:   {"error":"the call was already answered"} 409
wrong:   {"error":"this token may not do that"} 403
events:
  retry: 2000

  id: 1
  event: call
  data: {"cursor":"1"}
```

## What it teaches

**A held request is a callback.** `room.dlua` does not reply to a call
when it arrives. It keeps the request's `conn` and replies when the
answerer's answer comes in, or with 504 after the hold time.

**The cursor means nothing is read twice.** Each poll passes back the
cursor it was given and gets only the calls that came after it.

**The event stream only says when to poll.** `events.dlua` writes one
event per call, carrying the newest cursor and no record. An answerer
that loses the stream, or never opens it, polls and misses nothing.

**Two tokens.** The caller token may only call. The answerer token may
read calls and answer them. Both come from `args` in `app.json`.

**One name, as written.** Several names would mean a table of rooms keyed
by name, each with its own two tokens.
