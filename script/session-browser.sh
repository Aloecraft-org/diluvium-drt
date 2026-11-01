#!/usr/bin/env bash
# stdlib:session in a real browser behind a real reverse proxy: Chromium
# signs in through Caddy's forward_auth to a page on another subdomain, so
# the cookie's Domain, the 302 to the login with `rd`, the Remote-User and
# Remote-Groups the proxy copies, a group-gated path and sign-out are all
# what a browser does, not what a test client imitates. The directory is a
# real OpenLDAP slapd and the sessions live in a real `drt p2p --match --kv
# --topics`.
#
#   script/session-browser.sh
#
# Needs slapd, caddy, node 22 and Playwright with Chromium (PLAYWRIGHT names
# the package directory, as for script/browser-access-client.sh; CHROMIUM
# an executable to launch instead of Playwright's own). DRT names the
# binary; the default builds one with --features full.
set -eu
cd "$(dirname "$0")/.."
ROOT=$PWD

: "${PLAYWRIGHT:=$ROOT/crates/drt-web/browser-test/node_modules/playwright}"
export PLAYWRIGHT
if [ -z "${DRT:-}" ]; then
    cargo build -p drt --features full
    DRT=$ROOT/target/debug/drt
fi
for tool in /usr/sbin/slapd caddy node; do
    command -v "$tool" >/dev/null || { echo "$0: needs $tool" >&2; exit 2; }
done

# Fixed ports: the browser's host-resolver rule and Caddy's sites name them.
LDAP=18930 KV=18931 AUTH=18932 PROXY=18933 APP=18934
W=$(mktemp -d)
PIDS=
cleanup() {
    for p in $PIDS; do kill "$p" 2>/dev/null || true; done
    wait 2>/dev/null || true
    [ "${KEEP:-}" ] && echo "kept $W" || rm -rf "$W"
}
trap cleanup EXIT INT TERM

wait_port() {
    i=0
    until (exec 3<>"/dev/tcp/127.0.0.1/$1") 2>/dev/null; do
        i=$((i + 1)); [ $i -lt 200 ] || { echo "$0: nothing listened on $1; $2" >&2; cat "$W"/*.log >&2; exit 1; }
        sleep 0.05
    done
}

# depth: the directory
mkdir -p "$W/db"
cat > "$W/slapd.conf" <<CONF
include /etc/ldap/schema/core.schema
include /etc/ldap/schema/cosine.schema
include /etc/ldap/schema/inetorgperson.schema
pidfile $W/slapd.pid
modulepath /usr/lib/ldap
moduleload back_mdb
database mdb
maxsize 10485760
suffix "dc=example,dc=com"
rootdn "cn=admin,dc=example,dc=com"
rootpw admin-pw
directory $W/db
access to attrs=userPassword by self read by anonymous auth by * none
access to * by * read
CONF
/usr/sbin/slapadd -f "$W/slapd.conf" -l crates/drt/tests/session-browser/directory.ldif
# A copy, because Ubuntu's AppArmor profile for /usr/sbin/slapd confines
# that path to its own directories and so refuses this config.
cp /usr/sbin/slapd "$W/slapd"
"$W/slapd" -f "$W/slapd.conf" -h "ldap://127.0.0.1:$LDAP/" -d 0 >"$W/slapd.log" 2>&1 &
PIDS="$PIDS $!"
wait_port $LDAP slapd

# depth: the kv server and the session program
echo '{"topics":{"auth":{"tokens":[{"token":"browser-kv-token","roles":["read","write"]}]}}}' > "$W/topics.json"
"$DRT" p2p --match $KV --kv "$W/kv" --topics "$W/topics.json" >"$W/match.log" 2>&1 &
PIDS="$PIDS $!"
wait_port $KV "drt p2p --match"
sed -e "s/@LDAP@/$LDAP/g" -e "s/@KV@/$KV/g" -e "s/@AUTH@/$AUTH/g" -e "s/@PROXY@/$PROXY/g" \
    crates/drt/tests/session-browser/session.json > "$W/session.json"
"$DRT" start --config "$W/session.json" >"$W/session.log" 2>&1 &
PIDS="$PIDS $!"
wait_port $AUTH "drt start (stdlib:session)"

# depth: the proxy and the app behind it
sed -e "s/@AUTH@/$AUTH/g" -e "s/@PROXY@/$PROXY/g" -e "s/@APP@/$APP/g" \
    crates/drt/tests/session-browser/Caddyfile > "$W/Caddyfile"
caddy run --adapter caddyfile --config "$W/Caddyfile" >"$W/caddy.log" 2>&1 &
PIDS="$PIDS $!"
wait_port $PROXY caddy

if ! PROXY=$PROXY node crates/drt/tests/session-browser/check.mjs; then
    echo "--- session.log"; cat "$W/session.log"; echo "--- caddy.log"; tail -20 "$W/caddy.log"
    exit 1
fi
