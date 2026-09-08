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


# example: omits the `here:` lines `check` also prints -- whether this machine
# has /dev/net/tun and CAP_NET_ADMIN. They are true and they are useful, and
# they say something different on every machine, so meta.json normalises them
# away; crates/drt/tests/wireguard.rs is where they are held.
echo
echo '$ drt wg check --config hub-unroutable.host.lua'
"$DRT" wg check --config hub-unroutable.host.lua
echo "exit $?"

echo
echo '$ drt wg check --config rendezvous.host.lua'
"$DRT" wg check --config rendezvous.host.lua
echo "exit $?"
