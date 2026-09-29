# 30-signaling-room

Signaling for an answerer that cannot host an endpoint. A browser page
cannot take an HTTP request, so `29`'s one-endpoint signaling does not work
for a page that answers. Here the page polls instead, and the caller never
knows the difference.

## Run it

```
cd examples/30-signaling-room
drt start --config app.json
```

A page serving `ssh` polls the room and answers each caller
(`drt_browser_access.js`, `doc/BrowserAccess.md` §10.4):

```js
const calls = await (await fetch('http://127.0.0.1:18495/calls')).json();
for (const call of calls) {
  const a = await answer(call.record, { services: { ssh } });
  await fetch(`http://127.0.0.1:18495/answer/${call.id}`, { method: 'POST', body: a.recordText });
}
```

and from anywhere with `drt`:

```
ssh -o ProxyCommand="drt tunnel rtc:http://127.0.0.1:18495/call" you@page
```

`./demo.sh` plays both halves with `curl`, which is how the gate runs it.

## What you should see

```
drt start: http listening on 127.0.0.1:18495
calls:  [{"id":1,"record":"{\"caller\":\"record\"}"}]
answer: 204
caller: {"answerer":"record"}
calls:  []
```

## What it teaches

**A held request is a callback.** The program does not answer `POST /call`
when it arrives. It keeps the request's `conn` and replies to it when the
answerer's `POST /answer/<id>` comes in, so the caller's one request gets
the answerer's record back. `conn_deadline_ms` is how long a caller waits.

**The room never reads a record.** It moves text between two parties that
will then talk directly. What they send each other after that does not
pass through here.

**One answerer, as written.** Every caller goes to whoever polls. Rooms
per page, or keys per caller, are a table and a lookup more.
