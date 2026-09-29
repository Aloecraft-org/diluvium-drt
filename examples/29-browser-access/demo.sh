#!/usr/bin/env bash
# The deployment and two POSTs, orchestrated so the gate can run them
# unattended. A person types the commands in README.md instead -- this file
# exists because a README command block should not contain job control.
set -u
DRT="${DRT:-drt}"
"$DRT" start --config app.json &
for _ in $(seq 1 50); do curl -s -o /dev/null http://127.0.0.1:18490/ && break; sleep 0.1; done
BROWSER='{"v":1,"u":"Xk3fQ9aBc2Dd7eFg","p":"8bqS0lK1vT6YpR2eWm4nHc","f":"EBESExQVFhcYGRobHB0eHyAhIiMkJSYnKCkqKywtLi8=","c":[]}'
curl -s -i --data-binary "$BROWSER" http://127.0.0.1:18490/session | tr -d '\r' | grep -E '^(HTTP|access-control|\{)'
echo
curl -s --data-binary 'not a record' http://127.0.0.1:18490/session >/dev/null
sleep 0.5
kill %1 2>/dev/null
wait 2>/dev/null
rm -f webrtc-identity.json
