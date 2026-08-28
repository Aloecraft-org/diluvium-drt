# A rendezvous fetchpoint

The relay, its supervisor, and the two commands on either end of it.
Copy the directory, replace the keys, run it. See
[`doc/Relay.md`](../../doc/Relay.md) for what each piece is doing.

```
openssl rand -hex 24        # once per key; park and caller are different secrets
drt --config rendezvous.host.lua start
```

On the machine you want to reach, one long-running process:

```
drt tunnel --park "wss://rendezvous.example/park/xps?k=$PARK_KEY" --to 127.0.0.1:22
```

From anywhere:

```
ssh -o ProxyCommand="drt tunnel wss://rendezvous.example/s/xps?k=$CALLER_KEY" user@xps
```

`rsync -e`, `sftp -o`, `-L`/`-R` and agent forwarding all work through the
same ProxyCommand, because the bridge moves bytes and nothing else.

Two things to know before this faces the internet:

- **The key is in the URL**, which means it is in shell history and in any
  proxy log in front of the relay. That is the price of a leg a dumb
  `websocat` can speak, and it is why the keys are per-label and rotatable.
- **Terminate TLS in front of it.** The relay speaks plain WebSocket; put
  it behind whatever already answers 443 for you, so what crosses the
  internet is `wss://` and what middleboxes see is ordinary HTTPS.

A relay with no HTTP listener is fine — drop the `connectors.listen` block
entirely and the deployment is just the relay and its supervisor.
