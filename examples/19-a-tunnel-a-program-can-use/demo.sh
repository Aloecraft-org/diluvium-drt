#!/usr/bin/env bash
# The three processes a tunnel needs, on one machine, so the example runs
# unattended. A person types them in three terminals; this file exists
# because a README command block should not contain job control.
set -u
DRT="${DRT:-drt}"

# `>/dev/null` as well as `2>`: this is a program now, and a program's
# `print` goes to stdout. The `drt relay` verb it replaced logged to stderr.
"$DRT" start --config rendezvous.json >/dev/null 2>&1 &
"$DRT" start --config device.json 2>/dev/null &
for _ in $(seq 1 50); do curl -s -o /dev/null "http://127.0.0.1:18491/" && break; sleep 0.1; done

# The device holds an outbound leg open, ready to be claimed. From a file,
# so the park key is in park.json and not on this command line: the flag
# form is `drt tunnel --park "$RV/park/fp?k=…" --to 127.0.0.1:18491`.
"$DRT" --config park.json tunnel 2>/dev/null &

# The caller half, as a program can use it: a local port, one fresh leg per
# connection. Anything that speaks TCP now reaches the device. The flag
# form is `drt tunnel "$RV/s/fp?k=…" --local 127.0.0.1:18492`.
"$DRT" --config claim.json tunnel 2>/dev/null &
for _ in $(seq 1 50); do curl -s -o /dev/null "http://127.0.0.1:18492/ready" && break; sleep 0.1; done

curl -s "http://127.0.0.1:18492/hello"
curl -s "http://127.0.0.1:18492/again"

kill %1 %2 %3 %4 2>/dev/null
wait 2>/dev/null
