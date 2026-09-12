#!/usr/bin/env bash
# Two WireGuard peers on one machine, both in userspace, and a request
# across the tunnel between them. No interface is created, so no privilege
# is asked for, and the gate runs this without --privileged -- which is the
# whole demonstration. A person types these in three terminals; this file
# exists because a README command block should not contain job control.
set -u
DRT="${DRT:-drt}"

echo '$ drt wg check --config laptop.json'
"$DRT" wg check --config laptop.json
echo "exit $?"

echo
echo '$ drt wg check --config fetchpoint.json'
"$DRT" wg check --config fetchpoint.json
echo "exit $?"

# The fetchpoint's program, the fetchpoint's end of the tunnel, and the
# laptop's. Two userspace stacks can cross a tunnel on one host where two
# kernel interfaces cannot (22-wireguard-interface says why).
"$DRT" start --config device.json 2>/dev/null &
for _ in $(seq 1 50); do curl -s -o /dev/null "http://127.0.0.1:18522/" && break; sleep 0.1; done
# stdout to /dev/null on both ends: `stdlib:wg` prints what the block
# reports -- the forward it bound, then its peers on a timer -- and two
# processes interleaving timed reports is not a transcript. The one line
# the laptop says on stderr is the one the transcript shows.
"$DRT" start --config fetchpoint.json >/dev/null 2>&1 &
"$DRT" start --config laptop.json >/dev/null 2>laptop.log &
for _ in $(seq 1 50); do curl -s -o /dev/null --max-time 5 "http://127.0.0.1:18523/ready" && break; sleep 0.1; done

echo
echo '$ drt start --config laptop.json'
cat laptop.log

echo
echo '$ curl http://127.0.0.1:18523/hello'
curl -s --max-time 5 "http://127.0.0.1:18523/hello"
curl -s --max-time 5 "http://127.0.0.1:18523/again"

kill %1 %2 %3 2>/dev/null
wait 2>/dev/null
rm -f laptop.log
