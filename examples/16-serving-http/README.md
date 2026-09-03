# 16-serving-http

A deployment that serves. `drt start` binds the port `app.json` names, turns
every request into a message on a queue, and turns the program's reply on
another queue into the response. The listener is a queue bridge, and the
program never sees a socket.

## Run it

```
cd examples/16-serving-http
drt start --config app.json
```

and, from a second shell:

```
curl http://127.0.0.1:18475/hello
curl -H 'X-Name: curl' http://127.0.0.1:18475/hello
curl -d 'a body' http://127.0.0.1:18475/echo
```

`^C` stops the deployment. It serves from wasmtime too, unchanged:
`DRT=../../script/drt-wasip2.sh ./demo.sh` runs the whole exchange there.

## What you should see

```
drt start: http listening on 127.0.0.1:18475
hello from the deployment
hello from curl
POST /echo: 6 bytes
```

## What it teaches

**A request is a message.** `{conn, method, path, body, headers}` arrives on
`http_in`; the program answers `{conn, status, body, content_type}` on
`http_out`, and `conn` is what pairs them. One request per connection and no
keep-alive: the edge in front of a deployment holds the clients.

**The port is the deployment's, not the program's.** The address is in
`app.json`; the program declares two queues and waits. Move the port, or add
a second listener feeding the same queue, without touching `fetchpoint.dlua`.

**Headers cross by allowlist only.** `X-Name` reaches the program as
`headers['x-name']` because `app.json` names it. A header the config does not
name is dropped before the program can see it: the boundary is the config's.
