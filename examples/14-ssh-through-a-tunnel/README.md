# 14-ssh-through-a-tunnel

You want a normal `ssh` session to a machine you cannot reach. Not a
hostcall, not a program — the `ssh` on your laptop, with a shell, scp and
port forwarding.

That works today, and the whole trick is one flag.

## The simplest case: something already listens

If the far end can accept a WebSocket, one command in front of its sshd:

```
drt tunnel --listen 0.0.0.0:8443 --to 127.0.0.1:22
```

and from anywhere:

```
ssh -o ProxyCommand="drt tunnel wss://gate.example:8443" user@host
```

`ProxyCommand` is OpenSSH's own escape hatch: instead of opening a socket
itself, it runs that command and speaks SSH over its stdin and stdout. `drt
tunnel` with a URL is exactly that shape — stdio in, WebSocket out — so ssh
never learns it is not holding a socket.

## The far end has no address at all

Behind CGNAT nothing can dial in, so both ends dial out and meet at a relay.
The client command does not change:

```
drt relay --config rendezvous.host.lua                                    # public
drt tunnel --park "wss://relay.example/park/xps?k=$PARK_KEY" --to 127.0.0.1:22   # device
ssh -o ProxyCommand="drt tunnel wss://relay.example/s/xps?k=$CALLER_KEY" user@xps
```

`11-tunnel-and-relay` is that arrangement in full, with the relay config.

## What comes free

Everything that rides SSH, because the bridge moves bytes and never looks
inside:

```
scp  -o ProxyCommand="drt tunnel wss://…" file user@host:
rsync -e 'ssh -o ProxyCommand="drt tunnel wss://…"' ./dir/ user@host:dir/
sftp -o ProxyCommand="drt tunnel wss://…" user@host
ssh  -o ProxyCommand="drt tunnel wss://…" -L 5432:localhost:5432 user@host
```

Agent forwarding works too. Put it in `~/.ssh/config` and stop typing it:

```
Host xps
    ProxyCommand drt tunnel wss://relay.example/s/xps?k=YOUR_KEY
    User you
```

Then `ssh xps` is all of it.

## This is not `ssh/exec`

`10-ssh-exec` is the other thing, and picking the wrong one costs an hour:

- **`host:ssh/exec`** is a *hostcall a program makes*. One connection, one
  command, output back as data — `ssh host "ls /var/log"`. No session, no
  shell, no PTY, and a fresh handshake every call.
- **`drt tunnel`** is *plumbing for a human's own client*. DRT never speaks
  SSH here; it carries bytes and OpenSSH does the rest.

Want a program to read a command's output? `ssh/exec`. Want a shell? Tunnel.

## The two warnings from `11`, because they apply here too

**The key is in the URL**, so it is in your shell history and in the logs of
any proxy in front of the relay. That is the price of a leg a plain
`websocat` can speak, and it is why keys are per-label and rotatable.

**Terminate TLS in front of it.** The relay speaks plain WebSocket; put it
behind whatever already answers 443, so what crosses the internet is `wss://`
and what middleboxes see is ordinary HTTPS.
