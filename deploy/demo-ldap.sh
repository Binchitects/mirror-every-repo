#!/usr/bin/env bash
# End-to-end demo of sign-in through an LDAP directory: a real OpenLDAP
# server inside the air gap, users in different groups, and the web panel's
# sign-in API asked the way a browser asks it.
set -euo pipefail
trap 'echo "STOPPED at line $LINENO: $BASH_COMMAND" >&2' ERR
export MSYS_NO_PATHCONV=1
export COMPOSE_PROFILES=ldap
cd "$(dirname "$0")/.."

V="docker compose exec -T vault"
D="docker compose exec -T directory"
PY="docker compose exec -T pyclient"
step() { printf '\n=== %s ===\n' "$*"; }
ok()   { printf '  OK: %s\n' "$*"; }
fail() { printf '  FAILED: %s\n' "$*"; exit 1; }
# Passwords for this run only. They are never printed.
secret() { head -c 18 /dev/urandom | base64 | tr -dc 'A-Za-z0-9' | head -c 20; }
ADMIN_PW=$(secret); DANA_PW=$(secret); OMAR_PW=$(secret); VERA_PW=$(secret); NORA_PW=$(secret); ROOT_PW=$(secret)
BASE="dc=example,dc=org"

step "1. start the vault, a directory server and a client"
docker compose down -v --remove-orphans >/dev/null 2>&1 || true
LDAP_ADMIN_PASSWORD="$ADMIN_PW" docker compose up -d --build 2>&1 | tail -3
$V timeout 5 bash -c '</dev/tcp/1.1.1.1/443' 2>/dev/null && fail "vault can reach the internet" || ok "vault cannot reach the internet"
for _ in $(seq 1 30); do
  $D ldapsearch -x -H ldap://localhost -b "$BASE" -D "cn=admin,$BASE" -w "$ADMIN_PW" -s base >/dev/null 2>&1 && break
  sleep 1
done
$D ldapsearch -x -H ldap://localhost -b "$BASE" -D "cn=admin,$BASE" -w "$ADMIN_PW" -s base >/dev/null || fail "the directory server did not start"
ok "the directory server answers"

step "2. people and groups in the directory"
person() { printf 'dn: uid=%s,ou=people,%s\nobjectClass: inetOrgPerson\ncn: %s\nsn: %s\nuid: %s\nuserPassword: %s\n\n' "$1" "$BASE" "$1" "$1" "$1" "$2"; }
group() { printf 'dn: cn=%s,ou=groups,%s\nobjectClass: groupOfNames\ncn: %s\nmember: uid=%s,ou=people,%s\n\n' "$1" "$BASE" "$1" "$2" "$BASE"; }
{
  printf 'dn: ou=people,%s\nobjectClass: organizationalUnit\nou: people\n\ndn: ou=groups,%s\nobjectClass: organizationalUnit\nou: groups\n\n' "$BASE" "$BASE"
  person dana "$DANA_PW"; person omar "$OMAR_PW"; person vera "$VERA_PW"; person nora "$NORA_PW"
  group mirror-admins dana; group mirror-operators omar; group mirror-viewers vera; group canteen nora
} | $D ldapadd -x -H ldap://localhost -D "cn=admin,$BASE" -w "$ADMIN_PW" >/dev/null
ok "dana (mirror-admins), omar (mirror-operators), vera (mirror-viewers), nora (in no group that grants a role)"

step "3. the vault, with one local administrator and LDAP sign-in"
$V mer init
printf '%s' "$ROOT_PW" | $V mer user add root --role admin --password-stdin >/dev/null
printf '%s' "$ADMIN_PW" | $V sh -c 'umask 077 && cat > /tmp/ldap-bind-password'
# The directory has a certificate from a private CA, as directories inside a
# company do. The vault is told to trust that CA, and upgrades the
# connection to TLS before any name or password is sent.
$D cat /container/service/slapd/assets/certs/ca.crt | $V sh -c 'cat > /tmp/ldap-ca.crt'
LDAP_ARGS="--ldap-url ldap://directory:389 --ldap-starttls --ldap-bind-dn 'cn=admin,$BASE' --ldap-bind-password-file /tmp/ldap-bind-password \
  --ldap-user-base 'ou=people,$BASE' --ldap-group-base 'ou=groups,$BASE' \
  --ldap-admin-group mirror-admins --ldap-operator-group mirror-operators --ldap-viewer-group mirror-viewers"
serve() { # serve <environment for the server>
  # Stop the server that is running, if any, and wait until it is gone.
  $V sh -c 'if [ -f /tmp/mer.pid ]; then kill "$(cat /tmp/mer.pid)" 2>/dev/null; while kill -0 "$(cat /tmp/mer.pid)" 2>/dev/null; do sleep 0.2; done; rm -f /tmp/mer.pid; fi'
  docker compose exec -T -d vault sh -c "$1 mer serve --listen 0.0.0.0:8080 --quiet $LDAP_ARGS > /tmp/http.log 2>&1 & echo \$! > /tmp/mer.pid; wait"
  sleep 2
  $V sh -c 'kill -0 "$(cat /tmp/mer.pid)"' || fail "the server did not start: $($V cat /tmp/http.log)"
}
serve "SSL_CERT_FILE=/tmp/ldap-ca.crt"
$V sh -c 'grep "sign-in also through LDAP" /tmp/http.log' | sed 's/^/  | /'

# Sign in the way the panel does and print "<status> <role>".
$PY sh -c 'cat > /tmp/signin.py' <<'PYEOF'
import json, sys, urllib.request, urllib.error
req = urllib.request.Request("http://vault:8080/api/login", data=json.dumps({"user": sys.argv[1], "password": sys.argv[2]}).encode(), headers={"Content-Type": "application/json"})
try:
    with urllib.request.urlopen(req, timeout=30) as r:
        print(r.status, json.load(r).get("role", ""))
except urllib.error.HTTPError as e:
    print(e.code, "")
PYEOF
signin() { $PY python /tmp/signin.py "$1" "$2" | tr -d '\r'; }
expect() { # expect <what> <user> <password> <answer>
  local got; got=$(signin "$2" "$3")
  [ "$got" = "$4" ] && ok "$1: $got" || fail "$1: expected '$4', got '$got'"
}

step "4. sign-in"
expect "dana, member of mirror-admins" dana "$DANA_PW" "200 admin"
expect "omar, member of mirror-operators" omar "$OMAR_PW" "200 operator"
expect "vera, member of mirror-viewers" vera "$VERA_PW" "200 viewer"
expect "nora, right password but no group that grants a role" nora "$NORA_PW" "401 "
expect "dana with a wrong password" dana "not-the-password" "401 "
expect "dana with the password of somebody else" dana "$OMAR_PW" "401 "
expect "a name that is not in the directory" ghost "whatever-it-is" "401 "
expect "a name written to change the search" 'dana)(uid=*' "$DANA_PW" "401 "
expect "the local administrator" root "$ROOT_PW" "200 admin"
$V mer user list | sed 's/^/  | /'

step "5. the role follows the directory"
printf 'dn: cn=mirror-operators,ou=groups,%s\nchangetype: modify\nadd: member\nmember: uid=vera,ou=people,%s\n' "$BASE" "$BASE" | $D ldapmodify -x -H ldap://localhost -D "cn=admin,$BASE" -w "$ADMIN_PW" >/dev/null
expect "vera after being added to mirror-operators" vera "$VERA_PW" "200 operator"
printf 'dn: cn=mirror-admins,ou=groups,%s\nchangetype: modify\nreplace: member\nmember: uid=omar,ou=people,%s\n' "$BASE" "$BASE" | $D ldapmodify -x -H ldap://localhost -D "cn=admin,$BASE" -w "$ADMIN_PW" >/dev/null
expect "dana after being removed from mirror-admins" dana "$DANA_PW" "401 "

step "6. the directory goes away"
docker compose stop -t 2 directory >/dev/null 2>&1
expect "a directory user while the directory is down" omar "$OMAR_PW" "503 "
expect "the local administrator while the directory is down" root "$ROOT_PW" "200 admin"
docker compose start directory >/dev/null 2>&1
for _ in $(seq 1 30); do
  [ "$(signin omar "$OMAR_PW")" = "200 admin" ] && break
  sleep 1
done
expect "omar once the directory is back (now in mirror-admins too)" omar "$OMAR_PW" "200 admin"
AUDIT=$($V mer audit | tr -d '\r')
case "$AUDIT" in *"login.directory_error"*) ok "the outage is in the audit log" ;; *) fail "the outage is not in the audit log" ;; esac
case "$AUDIT" in *"$DANA_PW"*|*"$OMAR_PW"*|*"$ADMIN_PW"*) fail "a password is in the audit log" ;; *) ok "no password is in the audit log" ;; esac
case "$($V cat /tmp/http.log)" in *"$DANA_PW"*|*"$ADMIN_PW"*) fail "a password is in the server output" ;; *) ok "no password is in the server output" ;; esac

step "7. a directory whose certificate is not trusted is not asked"
serve ""
expect "a directory user when the vault does not trust the CA of the directory" omar "$OMAR_PW" "503 "
expect "the local administrator" root "$ROOT_PW" "200 admin"

step "DONE: all checks passed"
