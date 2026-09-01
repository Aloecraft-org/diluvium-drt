#!/usr/bin/env bash
# The two servers and the client, orchestrated so the gate can run them
# unattended. A person types the three commands in README.md instead — this
# file exists because a README command block should not contain job control.
set -u
DRT="${DRT:-drt}"
"$DRT" stun --config stun1.json >server1.log 2>&1 &
"$DRT" stun --config stun2.json >server2.log 2>&1 &
sleep 1
"$DRT" netcheck --stun 127.0.0.1:34780 --stun 127.0.0.1:34781
kill %1 %2 2>/dev/null
wait 2>/dev/null
