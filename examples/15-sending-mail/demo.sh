#!/usr/bin/env bash
# The relay and the program, orchestrated so the gate can run them
# unattended. A person runs the two commands in README.md instead — this
# file exists because a README command block should not contain job control.
set -u
DRT="${DRT:-drt}"
rm -f wire.txt
python3 relay.py 3 >relay.log 2>&1 &
until grep -q ready relay.log 2>/dev/null; do sleep 0.1; done
"$DRT" run --config deploy.json
wait %1 2>/dev/null
echo ""
echo "what reached the relay:"
sed 's/^/  /' wire.txt
