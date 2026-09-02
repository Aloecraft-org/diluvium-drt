# What only you can verify

**Audience: whoever is running the examples on a real machine.** Written
2026-09-01 against `v0.4.0rc1`.

Everything in `examples/` runs green in this container (`run-all.sh`: 15
ok, 0 failed, 1 skipped). That number is worth less than it looks, because
this container has no second machine, no reachable sshd, no NAT, and no
route to the open internet that a real deployment would recognise. The
list below is the part the gate cannot reach.

Each item says **what to run**, **what it proves**, and **what a failure
looks like** — that last one matters most, because several of these fail
by *succeeding wrongly*.

---

## surface block

1. §1 — the four that block the rc's honesty. Do these first.
2. §1.5 — the deployment that blocks all of `--reflect`. One command.
3. §2 — the network half of `examples/`. Needs real addresses.
4. §3 — the two-machine examples. Needs two machines.
5. §4 — what is already confirmed and needs nothing from you.

---

## 1. The four that block honesty

### 1.1 `drt netcheck` on a real network — the only run that matters

```sh
drt netcheck --stun stun1.discofetch.link:3478 --stun stun2.discofetch.link:3478
```

**What it proves.** The verdict tree. Every test in `netcheck.rs` feeds
`decide()` measurements I wrote by hand; not one of them has ever seen a
NAT. The tree is the thing the changelog calls "the part that will be
wrong first when real home networks surprise us," and it has not yet been
given the chance.

**Run it from three places if you can** — the XPS at your parents' (the
CGNAT case), a phone hotspot, and anything on plain residential cable.
Send me the whole evidence block from each, not the verdict line.

**A failure looks like** a verdict you would not have given by hand from
the evidence printed underneath it. `udp map` is the decisive line; if
`udp map` says `endpoint-independent` and the verdict says `relay`, or
the reverse, the table is wrong and I want the block that produced it.

**`address` is filled by STUN now**, so a two-server run reports the address
the world sees — and with it, whether you are behind CGNAT. That is the
answer §7 of the discofetch ask wants from the XPS, and it no longer waits
on the reflect edge.

**A non-failure that will look like one:** `tcp map` and `inbound` say
`not measured` on every network. That is correct for
this build — they are the reflect edges' half and there is no edge to
ask. `09-netcheck/README.md` says so.

**If `udp map` says `not measured`, read the reason in the brackets.** It
names which of four different problems you have:

```
(… needs two servers on separate addresses; 1 given)  -> pass both --stun flags
(could not resolve STUN server address '…')           -> DNS, or a typo
(no STUN response from … after 3 attempt(s))          -> the server is down,
                                                         or UDP is blocked on
                                                         the path
```

That last one is worth knowing before you blame the servers: a devcontainer
or a corporate network commonly drops outbound UDP, and `13-stun-server`
passing (it uses a *local* pair) while a remote pair fails is exactly that
shape. Testing from the host rather than inside a container separates them.

**The pre-condition.** This needs stun1 and stun2 on **separate
addresses**. One STUN server yields `not measured` and the relay
fallback, by design — `detect_mapping` refuses below two rather than
guessing. So standing those two up is what unblocks this whole line, and
it is the highest-value thing on your list.

### 1.2 The instruction-budget escape, confirmed here — confirm the blast radius there

I reproduced §1.2 of the discofetch ask, and it is worse than reported.
Two lines:

```lua
pcall(function() while true do end end)
while true do end
```

```sh
drt run --config cfg.json   # cfg names budget.instructions = 1000000
```

**What I measured.** The first loop trips the budget at ~250k steps and
`pcall` catches it as an ordinary Lua error. The second loop then runs
**forever** — I killed it at 20s. It is not that the budget is advisory;
it is that the budget is *switched off* by its own first firing, for the
rest of the process, and `drt run` still exits 0.

**What I need from you:** run the same thing on the XPS and confirm it
pins one core indefinitely rather than being an artifact of this
container's scheduler. Thirty seconds and `top` is enough.

**Why I am asking rather than just fixing it:** the mechanism is upstream,
not here — `src/dv.c:219`, `lua_sethook(L, NULL, 0, 0)` in the hook,
commented *"once is enough; the error is on its way."* It is the same in
diluvium `main` today, so **build12 does not fix it** and neither does the
pin bump.

### 1.3 SQL — reproduced, and the ask is aimed at the wrong thing

Also reproduced. But `begin`/`commit`/`rollback` **do** work:

```
begin -> ok, insert -> ok, commit -> ok    ... second process sees the row
begin -> ok, insert -> ok, (no commit)     ... second process sees nothing
```

So discofetch's preferred fix #1 is already implemented. The real defect
is narrower and nastier: **an open transaction at process exit is rolled
back silently**, no error, no warning, exit 0. Which is exactly correct
SQLite behaviour and exactly the failure discofetch cannot defend against.

**What I need from you:** nothing to reproduce — it is confirmed. What I
need is a decision, in §3 of the reply doc.

### 1.4 `examples/05-calling-a-rest-api-live` — the one the gate skips

```sh
cd examples/05-calling-a-rest-api-live && ./run.sh
# or, for the whole set:
examples/run-all.sh --net
```

**What it proves.** That the `rest` connector's owned-runtime fallback
holds against a real TLS endpoint and a real DNS answer, not against this
container's proxy. This is the fix that examples/05 found in the first
place; it has been exercised here, but through an agent proxy with a
custom CA bundle, which is not the shape a fetchpoint sees.

**A failure looks like** exit 101 and `there is no reactor running`. That
is the original bug and it would mean the fallback did not take.

---

## 1.5 The one that blocks §2: reflect is not deployed

**Checked against the live service on 2026-09-02, and this is the finding
that decides when `--reflect` can be built.**

`api/supervisor.lua` at discofetch HEAD implements `?format=addr-port`,
`observed.port` and `observed.edge` (commit `e7bddc4`). **The deployed
service does not.** Three requests pin it:

```sh
curl -sS "https://reflect.discofetch.link/?format=text"
# -> text/plain, "160.79.106.134"          the `text` branch IS deployed

curl -sS "https://reflect.discofetch.link/?format=addr-port"
# -> application/json, the full body, query echoed back
#    HEAD would answer text/plain with "ADDRESS PORT" or an empty line

curl -sS "https://reflect.discofetch.link/"
# -> observed = { forwarded, address }     no `port`, no `edge`
```

So the running code has the `text` branch and not the `addr-port` branch:
it predates `e7bddc4`. `doc/REFLECT-NAT.md` says *"`curl -s
https://reflect.discofetch.link/` shows `observed.port` … `observed.edge =
"gate1"`"*, and that is not what it shows today.

**What you need to do, in order:**

1. **Deploy the current `api/supervisor.lua`.** Until then `--reflect` has
   nothing to talk to, and any DRT client for it is written against a
   document rather than a service.
2. **Then check the edge actually sets `x-real-port`.** `observed_port(req)`
   reads that header and nothing else, so if nginx is not sending it the
   port stays unobserved *forever* and — by the all-or-nothing rule —
   `?format=addr-port` answers an **empty line** on every request. The
   check is one command, and it distinguishes "not deployed" from
   "deployed, header missing":

   ```sh
   curl -sS "https://reflect.discofetch.link/?format=addr-port"
   # empty line      -> deployed, but the edge is not sending x-real-port
   # "ADDR PORT"     -> deployed and observing; --reflect can be built
   # JSON            -> not deployed yet
   ```

3. **`observed.edge` needs the same treatment**, and it is what keys the
   two-edge comparison. One edge that never names itself is one vantage.

**The probe (inbound test) is a separate and larger gap.** `deploy/probe/`
is a written kit, `REFLECT-NAT.md` §5 calls the probe *"an edge service, not
a Lua call"* and lists it as a seam arriving with fetch2, and the 0.5.0 ask
says the written kit does the *wrong thing* (it probes back from the edge
the caller just talked to). So `--port` is not blocked on DRT: it is blocked
on an endpoint that does not exist in any form DRT should be written
against. Nothing in DRT should guess its shape.

---

## 2. The network half of the examples

### 2.1 `10-ssh-exec` — I could not run this at all

No sshd, no `ssh-keygen`, and no working `cryptography` module in this
container. The connector's no-reactor fix (`d629341`) is **the same edit**
as `rest`'s, which is verified — but I want that stated plainly rather
than implied: **`ssh/exec` has never been executed against a real sshd by
me, and `connectors/ssh` has zero tests.**

```sh
cd examples/10-ssh-exec
# generate a key, point the config at a host you control, then:
./run.sh
```

**A failure looks like** exit 101 / `there is no reactor running` — the
fallback did not take, same as 1.4. Anything else (auth refused, host
unreachable) is the connector working and the deployment being wrong;
send me the message and I will tell you which.

### 2.2 `13-stun-server`

```sh
cd examples/13-stun-server && ./run.sh
```

Then, from **another machine**, point a STUN client at it. The example
verifies that the server starts and counts; it does not verify that a
binding response is correct on the wire, because a loopback request sees a
loopback mapping and proves nothing. `stunclient <host> 3478` or a second
`drt netcheck --stun <host>:3478` is the real check.

**A failure looks like** a mapped address that is the machine's LAN
address rather than its public one.

---

## 3. The two-machine examples

`11-tunnel-and-relay` and `14-ssh-through-a-tunnel` both document a
topology this container cannot host. They run their local halves in the
gate; the interesting half is untested.

**What to run.** Relay on a public host, `--park` on the machine behind
NAT, and a real `ssh` through the `ProxyCommand` in
`14-ssh-through-a-tunnel`.

**What it proves.** Two things I care about separately:

- The `ProxyCommand` line is correct as written. It is the reason 14
  exists — someone searching for "ssh" needs to find a line they can
  paste.
- **The lazy local dial.** `park_once` dials the local socket only after
  a claim, because sshd's `LoginGraceTime` drops an idle connection. If
  that ordering is wrong, the symptom is a session that works when you
  connect immediately and fails when you leave the tunnel parked for a
  minute first. **Please test the parked-for-two-minutes case
  specifically** — it is the one that would pass a quick check and fail
  in production.

**A failure looks like** `ssh` hanging at `debug1: Connecting to ...`
rather than refusing.

---

## 4. What needs nothing from you

Recorded so you do not spend time on it.

- **§1.1, budget attenuation.** Confirmed by both of us independently:
  `fits_within`/`check_attenuation` have no caller in the workspace. Not
  worth a third confirmation.
- **`host.exec`.** Both readings are right about different things. There
  is no `exec` in `crates/drt-swarm/` or `crates/drt-hostcall/` — that
  grep is correct — *and* `host.exec` is a live table in the binary,
  because it lives in diluvium's guest library (`src/dhostlib.c`),
  compiled in, not in DRT's Rust. `print(host.exec ~= nil)` prints `true`;
  `host.exec.run(...)` answers `denied: no connector is wired for
  'exec/run' in this process`. The examples README's wording — no
  connector implements one, `exec/run` is denied — is accurate as it
  stands and was not edited toward a false statement.
- **FM-2.** Fixed upstream in build12. The rc still pins `f137b30`, so
  the `drt-swarm` mutex is still what closes it here; the comment at the
  lock says when it is safe to remove.
