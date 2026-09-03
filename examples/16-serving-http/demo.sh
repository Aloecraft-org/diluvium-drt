#!/usr/bin/env bash
# The deployment and three requests, orchestrated so the gate can run them
# unattended. A person types the commands in README.md instead — this file
# exists because a README command block should not contain job control.
set -u
DRT="${DRT:-drt}"
"$DRT" start --config app.json &
for _ in $(seq 1 50); do curl -s -o /dev/null http://127.0.0.1:18475/ && break; sleep 0.1; done
curl -s http://127.0.0.1:18475/hello
curl -s -H 'X-Name: curl' http://127.0.0.1:18475/hello
curl -s -d 'a body' http://127.0.0.1:18475/echo
kill %1 2>/dev/null
wait 2>/dev/null
