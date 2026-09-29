#!/usr/bin/env bash
# The room, a caller and an answerer, orchestrated so the gate can run them
# unattended: curl plays both, as README.md's commands do by hand.
set -u
DRT="${DRT:-drt}"
"$DRT" start --config app.json &
for _ in $(seq 1 50); do curl -s -o /dev/null http://127.0.0.1:18495/ && break; sleep 0.1; done
curl -s --data-binary '{"caller":"record"}' http://127.0.0.1:18495/call > answered.txt &
sleep 0.5
echo "calls:  $(curl -s http://127.0.0.1:18495/calls)"
curl -s -o /dev/null -w 'answer: %{http_code}\n' --data-binary '{"answerer":"record"}' http://127.0.0.1:18495/answer/1
wait %2
echo "caller: $(cat answered.txt)"
echo "calls:  $(curl -s http://127.0.0.1:18495/calls)"
kill %1 2>/dev/null
wait 2>/dev/null
rm -f answered.txt
