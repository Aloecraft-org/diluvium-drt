#!/usr/bin/env bash
# The relay in one terminal and the deployment in another, orchestrated so
# the gate can run them unattended. A person types the commands in README.md.
#
# Nothing here polls the relay: it speaks UDP and STUN, so there is no
# curl-shaped way to ask whether it is up. Its startup line is the answer,
# and the sleep is long enough to see it.
set -u
DRT="${DRT:-drt}"

echo '$ drt turn --config turn.host.lua'
"$DRT" turn --config turn.host.lua &
sleep 1

echo
echo '$ drt run --config app.host.lua'
"$DRT" run --config app.host.lua

echo
echo '$ drt turn --config open.host.lua'
"$DRT" turn --config open.host.lua
echo "exit $?"

kill %1 2>/dev/null
wait 2>/dev/null
