#!/usr/bin/env bash
# Bring the interface up and ask the KERNEL what it made, not DRT.
#
# This needs CAP_NET_ADMIN (or root), which is why meta.json marks it
# `needs_privilege` and the gate skips it without --privileged. Everything
# read below is /sys, so this needs coreutils and no iproute2.
set -u
DRT="${DRT:-drt}"

# A fresh key each run: the config carries the shape, never the value.
FP_PRIVATE_KEY=$("$DRT" wg keygen | head -1)
export FP_PRIVATE_KEY

# `drt start`, not bare `drt wg`: the block is served by a deployment now,
# and `fp.json` names `stdlib:wg` as the program that reads its reports.
#
# `1>/dev/null` keeps those reports out of this transcript. They go to
# stdout because `print` does, and what this example is about is what the
# KERNEL made -- the two `drt wg:` lines below are stderr and stay.
echo '$ drt start --config fp.json'
"$DRT" start --config fp.json 1>/dev/null &
sleep 2

echo
echo '$ cat /sys/class/net/drt-fp/mtu'
cat /sys/class/net/drt-fp/mtu

kill %1 2>/dev/null
wait 2>/dev/null
sleep 1

echo
echo '$ ls /sys/class/net/drt-fp   # after the process exits'
ls /sys/class/net/drt-fp 2>&1 | head -1
