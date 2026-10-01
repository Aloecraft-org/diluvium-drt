#!/usr/bin/env bash
# The deployment, one client that reads the whole stream and one that
# leaves after the first event, orchestrated so the gate can run them
# unattended. README.md has the commands a person types.
set -u
DRT="${DRT:-drt}"
"$DRT" start --config app.json &
for _ in $(seq 1 50); do curl -s -o /dev/null http://127.0.0.1:18496/ && break; sleep 0.1; done
curl -sN http://127.0.0.1:18496/events
curl -sN --max-time 0.3 http://127.0.0.1:18496/events > /dev/null
sleep 0.5
kill %1 2>/dev/null
wait 2>/dev/null
