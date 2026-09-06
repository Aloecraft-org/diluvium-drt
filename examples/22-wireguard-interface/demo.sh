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

echo '$ drt wg --config fp.host.lua'
"$DRT" wg --config fp.host.lua &
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
