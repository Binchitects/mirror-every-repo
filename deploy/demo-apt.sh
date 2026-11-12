#!/usr/bin/env bash
# End-to-end apt demo: mirror real packages from Debian on the collector,
# carry them across, and install them with a stock apt-get on a machine that
# has no internet access.
set -euo pipefail
# If a step stops the script, say which command it was.
trap 'echo "STOPPED at line $LINENO: $BASH_COMMAND" >&2' ERR
export MSYS_NO_PATHCONV=1
cd "$(dirname "$0")/.."

C="docker compose exec -T collector"
V="docker compose exec -T vault"
K="docker compose exec -T -u root client"
step() { printf '\n=== %s ===\n' "$*"; }
ok()   { printf '  OK: %s\n' "$*"; }
fail() { printf '  FAILED: %s\n' "$*"; exit 1; }

step "1. build the image and start collector, vault and client"
docker compose down -v --remove-orphans >/dev/null 2>&1 || true
docker compose up -d --build 2>&1 | tail -6
# The shared "USB" volume belongs to whichever container touched it first.
docker compose exec -T -u root collector chown mer:mer /media

step "2. network isolation"
$C timeout 5 bash -c '</dev/tcp/1.1.1.1/443' 2>/dev/null && ok "collector can reach the internet" || fail "collector needs internet for this demo"
$V timeout 5 bash -c '</dev/tcp/1.1.1.1/443' 2>/dev/null && fail "vault can reach the internet" || ok "vault cannot reach the internet"
$K timeout 5 bash -c '</dev/tcp/1.1.1.1/443' 2>/dev/null && fail "client can reach the internet" || ok "client cannot reach the internet"

step "3. pairing and keys"
$C mer init
$V mer init
PUB=$($C mer key gen /keys/bundle.key | tr -d '\r\n')
$C mer key link /keys/link.key
$C cp /keys/link.key /media/link.key
$V cp /media/link.key /keys/link.key
$V rm /media/link.key
$V mer key trust /data/trust.txt "$PUB"
$C mer apt keygen --name "Demo Site Key" | head -1

step "4. collector: mirror 'hello' and its dependencies from deb.debian.org"
$C mer repo add debian --format apt
$C mer apt sync debian --upstream http://deb.debian.org/debian --suite bookworm --package hello
$C mer export seal --key /keys/bundle.key
$C mer export media --seq 1 --out /media/b1 --volume-size 64M

step "5. vault: import from media, approve, publish, serve over HTTP"
$V mer import media /media/b1
$V mer approve debian 1
$V mer publish debian 1
docker compose exec -T -d vault sh -c 'mer serve --listen 0.0.0.0:8080 > /tmp/http.log 2>&1'
sleep 2
# The site public key reaches clients out of band (here: the shared volume).
$V mer get debian keys/site.asc --out /media/site.asc

step "6. client: a stock Debian with only the vault as its package source"
$K sh -c 'rm -f /etc/apt/sources.list.d/* /etc/apt/sources.list \
  && echo "deb [signed-by=/media/site.asc] http://vault:8080/debian bookworm main" > /etc/apt/sources.list'
$K sh -c 'command -v hello >/dev/null' && fail "hello is already installed" || ok "hello is not installed yet"
$K apt-get update 2>&1 | sed 's/^/  | /'
$K sh -c 'DEBIAN_FRONTEND=noninteractive apt-get install -y hello' 2>&1 | tail -6 | sed 's/^/  | /'
OUT=$($K hello | tr -d '\r')
[ "$OUT" = "Hello, world!" ] && ok "installed from the vault and runs: $OUT" || fail "hello did not run"

step "7. tampering: a client that trusts a different key refuses the vault"
$K sh -c 'cp /usr/share/keyrings/debian-archive-keyring.gpg /tmp/other.gpg \
  && echo "deb [signed-by=/tmp/other.gpg] http://vault:8080/debian bookworm main" > /etc/apt/sources.list'
LOG=$($K apt-get update 2>&1 || true)
case "$LOG" in
  *NO_PUBKEY*|*"not signed"*) ok "apt rejects the repository without the site key (NO_PUBKEY)" ;;
  *) echo "$LOG"; fail "apt accepted a repository signed by an untrusted key" ;;
esac
$K sh -c 'echo "deb [signed-by=/media/site.asc] http://vault:8080/debian bookworm main" > /etc/apt/sources.list'

step "8. collector: add 'jq' to the mirror; bundle 2 crosses the diode"
$K sh -c 'apt-get update >/dev/null 2>&1; apt-get install -y jq >/dev/null 2>&1' && fail "jq was installable before it was mirrored" || ok "jq is not available yet"
$C mer apt sync debian --upstream http://deb.debian.org/debian --suite bookworm --package hello --package jq
$C mer export seal --key /keys/bundle.key
docker compose exec -T -d vault sh -c \
  'mer import diode --listen 0.0.0.0:9000 --link-key /keys/link.key --auto-publish --idle-exit 6 > /tmp/rx.log 2>&1'
sleep 2
$C mer export diode --seq 2 --to vault:9000 --link-key /keys/link.key --rate-mbit 50 --passes 2
sleep 9
$V cat /tmp/rx.log

step "9. client: the new package is installable, with no change on the client"
$K apt-get update 2>&1 | sed 's/^/  | /'
$K sh -c 'DEBIAN_FRONTEND=noninteractive apt-get install -y jq' 2>&1 | tail -4 | sed 's/^/  | /'
OUT=$($K sh -c 'echo "{\"mirror\":\"works\"}" | jq -r .mirror' | tr -d '\r')
[ "$OUT" = "works" ] && ok "jq installed from the vault and runs" || fail "jq did not run"

step "10. access control: the repository now needs a token"
printf 'demo-admin-password\n' | $V mer user add admin --role admin --password-stdin
$V mer repo private debian on
LOG=$($K apt-get update 2>&1 || true)
case "$LOG" in
  *401*) ok "apt without a token is refused (401)" ;;
  *) echo "$LOG"; fail "a private repository was served without a token" ;;
esac
TOKEN=$($V mer token add client1 --repo debian | tr -d '\r' | awk '/secret/ {print $NF}')
$K sh -c "printf 'machine http://vault:8080 login apt password %s\n' '$TOKEN' > /etc/apt/auth.conf.d/vault.conf && chmod 600 /etc/apt/auth.conf.d/vault.conf"
LOG=$($K apt-get update 2>&1 || true)
case "$LOG" in
  *401*|*"Err:"*) echo "$LOG"; fail "apt with a valid token was refused" ;;
  *) ok "apt with the token updates normally" ;;
esac
$K sh -c 'DEBIAN_FRONTEND=noninteractive apt-get install -y --reinstall hello >/dev/null 2>&1' && ok "packages download with the token" || fail "download with token"
$V mer token revoke client1
LOG=$($K apt-get update 2>&1 || true)
case "$LOG" in
  *401*) ok "a revoked token is refused at once" ;;
  *) echo "$LOG"; fail "revoked token still works" ;;
esac
$V mer repo private debian off

step "11. vault: state, traffic and integrity"
$V mer status
echo "  requests served by the vault:"
$V sh -c 'grep -c " 200 " /tmp/http.log' | sed 's/^/    200: /'
$V sh -c 'grep " 404 " /tmp/http.log | head -5' | sed 's/^/    /' || true
$V mer fsck --deep
$V mer audit --limit 12

step "DONE: all checks passed"
