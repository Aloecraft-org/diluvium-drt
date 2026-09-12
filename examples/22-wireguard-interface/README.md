# 22-wireguard-interface

The half of the `wireguard` block that needs a privilege: the tunnel
interface is really created, and the kernel is the one asked about it.

**This example is skipped without `--privileged`**, because creating a tunnel
interface needs `CAP_NET_ADMIN` (or root) and an ordinary CI job has neither.
A skip is never a pass — the summary names it.

## Run it

```
cd examples/22-wireguard-interface
sudo -E drt start --config fp.json        # needs CAP_NET_ADMIN
cat /sys/class/net/drt-fp/mtu
```

Or through the gate, as a user that has the capability:

```
cd examples && ./run-all.sh --privileged 22
```

`FP_PRIVATE_KEY` must be in the environment; `demo.sh` mints a fresh one with
`drt wg keygen` each run.

## What you should see

```
$ drt start --config fp.json
drt wg: drt-fp up on port 51820, mtu 1420, address 10.9.0.1/24, public key <this run's>
drt wg: peer p6Vqzz…= allowed 10.9.0.2/32 via 127.0.0.1:51821

$ cat /sys/class/net/drt-fp/mtu
1420

$ ls /sys/class/net/drt-fp   # after the process exits
ls: cannot access '/sys/class/net/drt-fp': No such file or directory
```

## What this example does not show

**It does not carry traffic between two peers.** Two WireGuard interfaces on
one host cannot demonstrate a tunnel: their addresses are both local, so the
kernel routes between them directly and nothing ever enters the tunnel.
Proving it needs two machines, or network namespaces, which is more
machinery than an example should carry.

Where the packet-crossing *is* proven is
`crates/drt/tests/wireguard.rs`: two devices in one process, two loopback UDP
sockets, a real handshake, and a real IPv4 packet in one end and the same
bytes out the other, in both directions. That test runs in CI with no
privilege at all, because gotatun's device is generic over its IP side — so
DRT hands it a kernel interface here and a pair of channels there.

## What it teaches

**The interface is real, and the kernel says so.** Everything read here is
`/sys`, not DRT's own startup line: the MTU is the one the config named, and
it is the kernel reporting it. Taking a program's word for what it did to the
system is how a config that silently did nothing survives a demo.

**It is the process's, and it goes with the process.** No `wg-quick down`, no
leftover interface after a crash, nothing to clean up in a unit file. The
directory under `/sys/class/net` is gone the moment `drt wg` exits.

**One privilege, and only one.** Creating the interface is all that needs it —
`CAP_NET_ADMIN` on Linux, root on macOS, `wintun.dll` on Windows. No kernel
module, no `wg` tools, no `wg-quick`, no second daemon. `21-wireguard` is
everything that needs none of it.

**`operstate` reads `unknown`, and that is normal.** A point-to-point tun
device has no carrier to report, so the file says `unknown` rather than `up`
on a healthy interface. This example reads `mtu` instead, which is
unambiguous.
