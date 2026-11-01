#!/usr/bin/env bash
# The server, the JWKS and the issuer, orchestrated so the gate can run
# them unattended. README.md has the commands a person types.
set -u
DRT="${DRT:-drt}"
data=$(mktemp -d)
"$DRT" p2p --match 18498 --kv "$data" > /dev/null 2>&1 &
for _ in $(seq 1 50); do curl -s -o /dev/null http://127.0.0.1:18498/v1/auth/kv && break; sleep 0.1; done
"$DRT" start --config app.json > /dev/null 2>&1 &
for _ in $(seq 1 100); do curl -sf -o /dev/null http://127.0.0.1:18499/.well-known/jwks.json && break; sleep 0.1; done
echo '$ curl -s http://127.0.0.1:18499/.well-known/jwks.json | python3 -m json.tool --sort-keys'
curl -s http://127.0.0.1:18499/.well-known/jwks.json | python3 -m json.tool --sort-keys
echo '$ drt kv ls drt://127.0.0.1:18498/v1/auth/kv'
"$DRT" kv ls drt://127.0.0.1:18498/v1/auth/kv
echo '$ drt run --config sign.json > token'
"$DRT" run --config sign.json > "$data/token"
echo '$ python3 verify.py token'
python3 verify.py "$data/token"
kill %2 %1 2>/dev/null
wait 2>/dev/null
rm -rf "$data"
