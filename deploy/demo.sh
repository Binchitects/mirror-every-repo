#!/usr/bin/env bash
# End-to-end demo and smoke test of both transports between two containers.
# Every step checks its own result; the script stops at the first failure.
set -euo pipefail
# If a step stops the script, say which command it was.
trap 'echo "STOPPED at line $LINENO: $BASH_COMMAND" >&2' ERR
export MSYS_NO_PATHCONV=1
cd "$(dirname "$0")/.."

C="docker compose exec -T collector"
V="docker compose exec -T vault"
step() { printf '\n=== %s ===\n' "$*"; }
ok()   { printf '  OK: %s\n' "$*"; }
fail() { printf '  FAILED: %s\n' "$*"; exit 1; }

step "1. build the image and start both nodes"
docker compose down -v --remove-orphans >/dev/null 2>&1 || true
docker compose up -d --build 2>&1 | tail -4
# The shared "USB" volume belongs to whichever container touched it first.
docker compose exec -T -u root collector chown mer:mer /media

step "2. network isolation"
if $C timeout 5 bash -c '</dev/tcp/1.1.1.1/443' 2>/dev/null; then ok "collector can reach the internet"
else echo "  note: collector has no internet either (host offline?)"; fi
if $V timeout 5 bash -c '</dev/tcp/1.1.1.1/443' 2>/dev/null; then fail "vault can reach the internet"
else ok "vault cannot reach the internet"; fi

step "3. pairing, done once by hand"
$C mer init
$V mer init
PUB=$($C mer key gen /keys/bundle.key | tr -d '\r\n')
echo "  collector bundle key: $PUB"
$C mer key link /keys/link.key
$C cp /keys/link.key /media/link.key
$V cp /media/link.key /keys/link.key
$V rm /media/link.key
$V mer key trust /data/trust.txt "$PUB"

step "4. collector: mirror content and seal bundle 1"
$C sh -c 'mkdir -p /tmp/up/pool /tmp/up/dists \
  && head -c 3000000 /dev/urandom > /tmp/up/pool/tool_1.0.deb \
  && echo "release 1" > /tmp/up/dists/Release'
$C mer repo add tools
$C mer repo sync-dir tools /tmp/up
$C mer export seal --key /keys/bundle.key --segment-size 256K
$C mer export media --seq 1 --out /media/b1 --volume-size 1M

step "5. vault: import bundle 1 from removable media"
$V mer import media /media/b1
$V mer status

step "6. quarantine gate"
if $V mer get tools dists/Release >/dev/null 2>&1; then fail "content was served before approval"
else ok "imported content is not served before approval"; fi
$V mer approve tools 1
$V mer publish tools 1
[ "$($V mer get tools dists/Release | tr -d '\r')" = "release 1" ] && ok "Release is served after publish" || fail "Release content"
WANT=$($C sha256sum /tmp/up/pool/tool_1.0.deb | cut -d' ' -f1)
GOT=$($V sh -c 'mer get tools pool/tool_1.0.deb | sha256sum' | cut -d' ' -f1)
[ "$WANT" = "$GOT" ] && ok "3 MB package is byte-identical on the vault ($GOT)" || fail "package differs"

step "7. collector: change content, seal bundle 2 (a delta)"
$C sh -c 'echo "release 2" > /tmp/up/dists/Release \
  && head -c 1500000 /dev/urandom > /tmp/up/pool/tool_1.1.deb \
  && rm /tmp/up/pool/tool_1.0.deb'
$C mer repo sync-dir tools /tmp/up
$C mer export seal --key /keys/bundle.key --segment-size 256K

step "8. bundle 2 over the data diode (one-way UDP, two passes)"
docker compose exec -T -d vault sh -c \
  'mer import diode --listen 0.0.0.0:9000 --link-key /keys/link.key --auto-publish --idle-exit 6 > /tmp/rx.log 2>&1'
sleep 2
$C mer export diode --seq 2 --to vault:9000 --link-key /keys/link.key --rate-mbit 50 --passes 2
sleep 9
$V cat /tmp/rx.log
[ "$($V mer get tools dists/Release | tr -d '\r')" = "release 2" ] && ok "vault serves release 2 (auto-published)" || fail "release 2 not live"
WANT=$($C sha256sum /tmp/up/pool/tool_1.1.deb | cut -d' ' -f1)
GOT=$($V sh -c 'mer get tools pool/tool_1.1.deb | sha256sum' | cut -d' ' -f1)
[ "$WANT" = "$GOT" ] && ok "new package arrived intact over the diode" || fail "package differs"
if $V mer get tools pool/tool_1.0.deb >/dev/null 2>&1; then fail "deleted package still served"
else ok "package deleted upstream is gone from the published view"; fi

step "9. rollback and roll forward"
$V mer rollback tools 1
[ "$($V mer get tools dists/Release | tr -d '\r')" = "release 1" ] && ok "rollback serves release 1 again" || fail "rollback"
$V mer publish tools 2
[ "$($V mer get tools dists/Release | tr -d '\r')" = "release 2" ] && ok "back on release 2" || fail "roll forward"

step "10. wishlist: what clients asked for and did not get"
$V mer get tools pool/tool_2.0.deb >/dev/null 2>&1 || true
$V mer get tools pool/tool_2.0.deb >/dev/null 2>&1 || true
$V mer wishlist

step "11. retention, space reclaim, integrity, audit"
$V mer repo snapshots tools
$V mer compact tools 2
$V mer gc --grace-hours 0
$V mer fsck --deep
$V mer status
$V mer audit --limit 30

step "DONE: all checks passed"
