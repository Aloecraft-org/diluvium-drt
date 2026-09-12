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
# `check` rather than bare `drt wg`, which no longer exists: serving is
# `drt start` with a `wireguard` block now, and the three things that need
# no privilege are what `wg` kept. The refusal is the same one, from the
# same `validate`.
echo '$ drt wg check --config wrong.json'
"$DRT" wg check --config wrong.json
echo "exit $?"


# example: omits the `here:` lines `check` also prints -- whether this machine
# has /dev/net/tun and CAP_NET_ADMIN. They are true and they are useful, and
# they say something different on every machine, so meta.json normalises them
# away; crates/drt/tests/wireguard.rs is where they are held.
echo
echo '$ drt wg check --config hub-unroutable.json'
"$DRT" wg check --config hub-unroutable.json
echo "exit $?"

echo
echo '$ drt wg check --config rendezvous.json'
"$DRT" wg check --config rendezvous.json
echo "exit $?"
