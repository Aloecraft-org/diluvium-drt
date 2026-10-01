#!/usr/bin/env bash
# The server, a caller and an answerer, orchestrated so the gate can run them
# unattended: curl plays both, as README.md's commands do by hand.
set -u
DRT="${DRT:-drt}"
BASE=http://127.0.0.1:18495/v1/page
A=answerer-token-for-the-example-only
C=caller-token-for-the-example-only
"$DRT" start --config app.json &
for _ in $(seq 1 50); do curl -s -o /dev/null http://127.0.0.1:18495/ && break; sleep 0.1; done

echo "call:    $(curl -s -w ' %{http_code}' --data-binary '{"caller":"record"}' "$BASE/calls?k=$C")"
echo "poll:    $(curl -s "$BASE/calls?k=$A")"
curl -sN "$BASE/events?k=$A" > events.txt &
sleep 0.3
curl -s -i --data-binary '{"caller":"record"}' "$BASE/calls?k=$C" > called.txt &
sleep 0.5
echo "poll:    $(curl -s "$BASE/calls?since=0&k=$A")"
echo "answer:  $(curl -s -w '%{http_code}' --data-binary '{"answerer":"record"}' "$BASE/calls/c1/answer?k=$A")"
wait %3
echo "caller:  $(tr -d '\r' < called.txt | grep -E '^(HTTP|location|\{)' | paste -sd ' ')"
echo "poll:    $(curl -s "$BASE/calls?since=1&k=$A")"
echo "again:   $(curl -s -w ' %{http_code}' -X DELETE "$BASE/calls/c1?k=$C")"
echo "wrong:   $(curl -s -w ' %{http_code}' "$BASE/calls?k=$C")"
echo "events:"
sed 's/^/  /' events.txt
kill %2 %1 2>/dev/null
wait 2>/dev/null
rm -f events.txt called.txt
