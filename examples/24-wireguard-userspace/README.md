# 24-wireguard-userspace

The `wireguard` block with **no privilege at all**. `mode = "userspace"`
puts a TCP/IP stack inside the process where kernel mode would create an
interface, and the tunnel is reached through two lists instead of an
address: `forward`, a port on localhost that reaches an address inside the
tunnel, and `expose`, an address inside the tunnel that reaches a local
port.

```text
  laptop.json                                    fetchpoint.json
  curl 127.0.0.1:18523 ─► forward ═══ tunnel ═══ expose ─► 127.0.0.1:18522
                          10.9.0.1               10.9.0.2       (device.json)
```

No `CAP_NET_ADMIN`, no `sudo`, no `wintun.dll`. The gate runs this one
without `--privileged`, which is the whole demonstration.

## Run it

Three terminals, or `./demo.sh` which is all of them:

```
cd examples/24-wireguard-userspace
drt start --config device.json          # what the fetchpoint serves
drt wg --config fetchpoint.json         # its end of the tunnel
drt wg --config laptop.json             # yours
curl http://127.0.0.1:18523/hello
```

## What you should see

```
$ drt wg check --config laptop.json
ok: userspace on port 18520, 1 peer(s)
    no interface; forwards 127.0.0.1:18523 -> 10.9.0.2:80
exit 0

$ drt wg --config laptop.json
drt wg: userspace up on port 18520, mtu 1420, address 10.9.0.1/24, public key lji56MXQ6Mhpi6WwKhqvSWth5YPmCTFq6dIl94sZt2Q=
drt wg: forward 127.0.0.1:18523 -> 10.9.0.2:80
drt wg: peer /a3GH0hkjdB0oghXkh7cDyfCQInPtj0VOMXNK8kVQ20= allowed 10.9.0.2/32 via 127.0.0.1:18521

$ curl http://127.0.0.1:18523/hello
answered through the tunnel: /hello
```

Every byte of that answer crossed a WireGuard handshake, was encrypted
and decrypted, and passed through a real TCP connection at each end.

## What it teaches

**The privilege was never the tunnel.** STUN, the rendezvous, the punch
and the protocol were unprivileged all along; `CAP_NET_ADMIN` bought one
thing, an adapter in the kernel's stack. This mode does without the
adapter, so a tool can demonstrably move bytes *before* it asks for a
privilege, and on a machine whose owner says no to `sudo` it moves them
anyway.

**Both directions, on purpose.** In kernel mode the kernel delivers
`10.9.0.2:80` to whatever listens on the box. Inside a userspace stack
nothing listens unless the config says what does, so `expose` is the
device half and `forward` the caller half, and a machine can carry either
or both. The fetchpoint dials `127.0.0.1:18522` lazily, on each connection
from the tunnel, never at startup.

**The program does not know.** `app.dlua` and `device.json` are
`17-serving-http`'s listener, unchanged, and a rendezvous program steering
the device over `reply_queue` cannot tell the modes apart from the queue.
`mode` is explicit and defaults to `kernel`: a config that did not ask is
not steered into the other mode by a missing privilege.

**Refused at startup, by name.** A userspace block with neither list, or
no `address`; a forward in kernel mode; two forwards on one port; a `to`
that is a network address, or a name (there is no resolver inside a
tunnel). A forward to an address no peer is allowed is a warning, because
that connection will time out fifteen seconds later and both numbers are
in the config.

**Why this example can carry a byte and `22-wireguard-interface` cannot.**
Two kernel interfaces on one host never cross a tunnel: their addresses
are both local, so the kernel routes between them directly. Two userspace
stacks know nothing of each other except through the tunnel, so this is
the first wireguard example that carries a request end to end in the gate.

## What it does not show

The punch. Both ends here know each other's address; `21-wireguard`'s
rendezvous config and `doc/WireGuard.md` §2 are where a peer with no
endpoint learns one at run time, and nothing about that changes in this
mode.
