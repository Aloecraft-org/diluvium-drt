#!/usr/bin/env bash
# The two gates and the question, orchestrated so the gate can run them
# unattended. A person types the commands in README.md instead -- this file
# exists because a README command block should not contain job control.
set -u
DRT="${DRT:-drt}"
export REFLECT_KEY=demo
"$DRT" p2p --reflect 34790 --reflect-peer 127.0.0.1:34791 --reflect-key env:REFLECT_KEY >gate1.log 2>&1 &
"$DRT" p2p --reflect 34791 --reflect-peer 127.0.0.1:34790 --reflect-key env:REFLECT_KEY >gate2.log 2>&1 &
sleep 1
"$DRT" netcheck 127.0.0.1:34790
kill %1 %2 2>/dev/null
wait 2>/dev/null
