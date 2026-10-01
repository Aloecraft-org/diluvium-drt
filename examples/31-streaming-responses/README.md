# 31-streaming-responses

Server-Sent Events from a program. `GET /events` gets one event every
200 ms, five in all, while the response is still open.

## Run it

```
cd examples/31-streaming-responses
drt start --config app.json
```

and in another terminal:

```
curl -N http://127.0.0.1:18496/events
```

`./demo.sh` runs both, plus a second client that leaves after the first
event, which is how the gate runs it.

## What you should see

```
drt start: http listening on 127.0.0.1:18496
data: 1

data: 2

data: 3

data: 4

data: 5

closed: client
```

## What it teaches

**A response can be written in pieces.** With `streaming` set on the
listener, a reply with `stream = true` sends the status and headers at
once. Each later reply naming the same `conn` with a `chunk` is written as
it arrives, and `done = true` ends the response.

**The program hears when a client leaves.** A stream that ends any other
way arrives on the request queue as `{conn, event = "closed", reason}`.
The reason is `client` when the client closed the connection, `idle` when
no chunk arrived within `stream_idle_ms`, and `backlog` when the program
wrote over a megabyte ahead of a client that was not reading.

**A stream holds a connection.** Each open stream counts against the
listener's `max_conns` until it ends.
