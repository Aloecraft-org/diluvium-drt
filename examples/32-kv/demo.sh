#!/usr/bin/env bash
# The server, the CLI and the program, orchestrated so the gate can run
# them unattended. README.md has the commands a person types.
set -u
DRT="${DRT:-drt}"
data=$(mktemp -d)
"$DRT" p2p --match 18497 --kv "$data" > /dev/null 2>&1 &
for _ in $(seq 1 50); do curl -s -o /dev/null http://127.0.0.1:18497/v1/home/kv && break; sleep 0.1; done
K=drt://127.0.0.1:18497/v1/home/kv
echo '$ drt kv put $K/bpi/info "fingerprint=SHA256:..."'
"$DRT" kv put $K/bpi/info "fingerprint=SHA256:..."
echo '$ drt kv put --if-version 7 $K/bpi/info "someone else"'
"$DRT" kv put --if-version 7 $K/bpi/info "someone else"
echo '$ curl -s $K/bpi/info'
curl -s http://127.0.0.1:18497/v1/home/kv/bpi/info; echo
echo '$ drt run --config app.json'
"$DRT" run --config app.json
echo '$ drt kv ls $K'
"$DRT" kv ls $K
kill %1 2>/dev/null
wait 2>/dev/null
rm -rf "$data"
