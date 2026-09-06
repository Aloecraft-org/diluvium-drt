#!/usr/bin/env bash
# The three processes a tunnel needs, on one machine, so the example runs
# unattended. A person types them in three terminals; this file exists
# because a README command block should not contain job control.
set -u
DRT="${DRT:-drt}"
RV="ws://127.0.0.1:18490"

"$DRT" relay --config rendezvous.host.lua 2>/dev/null &
"$DRT" start --config device.json 2>/dev/null &
for _ in $(seq 1 50); do curl -s -o /dev/null "http://127.0.0.1:18491/" && break; sleep 0.1; done

# The device holds an outbound leg open, ready to be claimed.
"$DRT" tunnel --park "$RV/park/fp?k=park-key-for-the-example-only" --to 127.0.0.1:18491 2>/dev/null &

# The caller half, as a program can use it: a local port, one fresh leg per
# connection. Anything that speaks TCP now reaches the device.
"$DRT" tunnel "$RV/s/fp?k=caller-key-for-the-example-only" --local 127.0.0.1:18492 2>/dev/null &
for _ in $(seq 1 50); do curl -s -o /dev/null "http://127.0.0.1:18492/ready" && break; sleep 0.1; done

curl -s "http://127.0.0.1:18492/hello"
curl -s "http://127.0.0.1:18492/again"

kill %1 %2 %3 %4 2>/dev/null
wait 2>/dev/null
