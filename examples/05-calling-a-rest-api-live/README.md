# 05-calling-a-rest-api-live

The other half of `05-calling-a-rest-api`. There, every URL was outside the
allowlist and refused by name. This one is the granted origin, so the
connector opens a socket — which is the only example in the set that needs a
network.

## Run it

```
cd examples/05-calling-a-rest-api-live
drt run --config allowlist.json
```

## What you should see

One line, and which one depends on the network in front of you:

```
reply  ok  200
```

on a direct connection, or

```
reply  error  tls
```

behind a proxy that intercepts TLS — the connector answering, not crashing.
Either way it exits 0 and writes nothing. `run-all.sh` skips this example
until you pass `--net`, and folds those two lines into one placeholder;
anything else — a refusal, a denial, a panic — fails its diff.

## What it teaches

**The program cannot see the header that makes the call work.** The allow
entry in `allowlist.json` carries:

```json
"headers": { "user-agent": "drt-example/0.4.0" },
"allow_headers": ["accept"]
```

`headers` are set by the connector on every request to that origin. The
program cannot set them — it is refused by name if it tries, rather than
having its value quietly dropped — and they are not echoed back in the reply,
so it cannot read them either. `allow_headers` is then the exhaustive list of
what the program *may* set.

GitHub's API refuses a request carrying no user-agent, so that line is not
decoration: the call does not work without it, and `app.dlua` does not contain
it. Swap `user-agent` for an `authorization` and nothing in the program
changes. The credential lives in the deployment, on the other side of the
boundary from the code that spends it.

**Two statuses, deliberately not merged.** `status` is the reply's — whether
the call was permitted and completed. `resp.status` is HTTP's, and a 404 or a
500 is a perfectly successful call that got an unhappy answer.
