#!/usr/bin/env bash
# The parts of the WireGuard block that run without a privilege: a key pair,
# and the refusals a config earns before anything is bound.
#
# Bringing the interface up is the other half, and it needs CAP_NET_ADMIN --
# see README.md. meta.json marks this example `needs_privilege`, so the gate
# skips the privileged half rather than pretending it passed.
set -u
DRT="${DRT:-drt}"

echo '$ drt wg keygen'
"$DRT" wg keygen

echo
echo '$ drt wg --config wrong.host.lua'
"$DRT" wg --config wrong.host.lua
echo "exit $?"
