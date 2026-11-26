#!/usr/bin/env bash
# The installer and the web panel, end to end, on two systemd machines:
#
#   bash deploy/demo-install.sh            # builds mer from this tree
#   bash deploy/demo-install.sh --release v1.4.0   # installs a published release
#
# 1. install.sh sets up a collector and a vault (keys, TLS, first admin,
#    services) the way an operator would.
# 2. Everything else goes through the two web panels' API, as a browser
#    would: create a repository, set its source, sync, seal, export; on the
#    vault, import and publish.
# 3. A stock pip on a third machine installs from the vault over its
#    generated TLS certificate; the installer then upgrades and uninstalls.
set -euo pipefail
trap 'echo "STOPPED at line $LINENO: $BASH_COMMAND" >&2' ERR
export MSYS_NO_PATHCONV=1
cd "$(dirname "$0")/.."
RELEASE=""
[ "${1:-}" = "--release" ] && RELEASE="${2:?release tag}"

step() { printf '\n=== %s ===\n' "$*"; }
ok()   { printf '  OK: %s\n' "$*"; }
fail() { printf '  FAILED: %s\n' "$*"; exit 1; }
NET=mer-install-demo
WORK="$(mktemp -d)"
cleanup() {
  docker rm -f mer-i-collector mer-i-vault mer-i-client >/dev/null 2>&1 || true
  docker network rm "$NET" >/dev/null 2>&1 || true
  rm -rf "$WORK"
}
trap cleanup EXIT

step "1. two machines with systemd"
cleanup; WORK="$(mktemp -d)"
docker network create "$NET" >/dev/null
cp deploy/install.sh "$WORK/"
if [ -z "$RELEASE" ]; then
  # A static build of this tree, as releases are built.
  docker run --rm -v "$PWD:/work" -v mer-musl-target:/target -v mer-cargo-registry:/usr/local/cargo/registry \
    -e CARGO_TARGET_DIR=/target -w /work rust:1.99-bookworm sh -c '
      apt-get update -qq >/dev/null && apt-get install -y -qq musl-tools >/dev/null 2>&1
      rustup target add x86_64-unknown-linux-musl >/dev/null 2>&1
      cargo build --release --locked -q -p mer --target x86_64-unknown-linux-musl'
  docker run --rm -v mer-musl-target:/target -v "$WORK:/o" rust:1.99-bookworm cp /target/x86_64-unknown-linux-musl/release/mer /o/mer
fi
for m in collector vault; do
  docker run -d --name "mer-i-$m" --hostname "$m" --network "$NET" --privileged --cgroupns=host \
    -v /sys/fs/cgroup:/sys/fs/cgroup:rw -v "$WORK:/inst:ro" jrei/systemd-debian:12 >/dev/null
done
sleep 3
C="docker exec mer-i-collector"
V="docker exec mer-i-vault"
$V systemctl is-system-running --wait >/dev/null 2>&1 || true
ok "collector and vault are up"

step "2. install the collector"
if [ -n "$RELEASE" ]; then FROM=(--version "$RELEASE"); else FROM=(--binary /inst/mer); fi
$C bash /inst/install.sh collector "${FROM[@]}" > "$WORK/collector.txt" 2>&1 || { cat "$WORK/collector.txt"; fail "the collector installer failed"; }
sed -n '/is ready/,$p' "$WORK/collector.txt" | sed 's/^/  | /' | head -8
PUB=$($C mer key pub /var/lib/mer/keys/bundle.key | tr -d '\r')
for u in mer-panel.service mer-sync.timer; do [ "$($C systemctl is-active $u | tr -d '\r')" = active ] && ok "$u is active" || fail "$u is not active"; done

step "3. install the vault, trusting the collector, with its panel on its own address"
$V bash /inst/install.sh vault "${FROM[@]}" --trust "$PUB" --admin-listen 127.0.0.1:8444 > "$WORK/vault.txt" 2>&1 || { cat "$WORK/vault.txt"; fail "the vault installer failed"; }
sed -n '/is ready/,/Next steps/p' "$WORK/vault.txt" | sed 's/^/  | /' | head -6
[ "$($V systemctl is-active mer-vault | tr -d '\r')" = active ] && ok "mer-vault is active" || fail "mer-vault is not active"

# The panel API, as the browser uses it: sign in, then the CSRF token on every change.
cat > "$WORK/api.sh" <<'EOF'
#!/bin/sh
# api.sh <base> login | <base> get <path> | <base> post <path> <json> | <base> wait <job>
B=$1; shift; J=/tmp/mer-cookies; T=/tmp/mer-csrf
case "$1" in
  login) curl -sk -c $J -H 'Content-Type: application/json' -d "{\"user\":\"admin\",\"password\":\"$(cat /etc/mer/admin-password)\"}" "$B/api/login" | sed -n 's/.*"csrf":"\([^"]*\)".*/\1/p' > $T; [ -s $T ] ;;
  get) curl -sk -b $J "$B/api/$2" ;;
  post) curl -sk -b $J -H "X-CSRF-Token: $(cat $T)" -H 'Content-Type: application/json' -d "$3" "$B/api/$2" ;;
  wait) for i in $(seq 1 600); do
          s=$(curl -sk -b $J "$B/api/jobs/$2" | sed -n 's/.*"state":"\([a-z]*\)".*/\1/p')
          case "$s" in queued|running) sleep 1 ;; *) echo "$s"; exit 0 ;; esac
        done; echo timeout ;;
esac
EOF
chmod +x "$WORK/api.sh"
CAPI() { $C /inst/api.sh http://127.0.0.1:8081 "$@"; }
VAPI() { $V /inst/api.sh https://127.0.0.1:8444 "$@"; }
job_of() { sed -n 's/.*"job":\([0-9]*\).*/\1/p'; }

step "4. collector panel: repository, source, sync, seal, export"
CAPI login && ok "signed in to the collector panel as admin"
CAPI post repos '{"name":"pypi","format":"pypi"}' >/dev/null
CAPI post repos/pypi/source '{"spec":{"requirement":["requests"],"python":["3.12"]}}' | grep -q requests && ok "source saved"
J=$(CAPI post repos/pypi/sync '{}' | job_of); [ "$(CAPI wait "$J" | tr -d '\r')" = succeeded ] && ok "sync job $J succeeded" || { CAPI get "jobs/$J"; fail "sync failed"; }
CAPI get "jobs/$J" | grep -o 'requests [0-9.]*' | head -1 | sed 's/^/  | /'
J=$(CAPI post bundles/seal '{}' | job_of); [ "$(CAPI wait "$J" | tr -d '\r')" = succeeded ] && ok "seal job $J succeeded" || fail "seal failed"
J=$(CAPI post bundles/export '{"seq":1,"out":"usb/bundle-1"}' | job_of); [ "$(CAPI wait "$J" | tr -d '\r')" = succeeded ] && ok "export job $J succeeded" || fail "export failed"
S=$(CAPI post schedules '{"repos":["pypi"],"rule":{"kind":"daily","at":"03:00"},"seal":true}' | sed -n 's/.*"id":\([0-9]*\).*/\1/p')
[ -n "$S" ] && ok "schedule $S: daily at 03:00 UTC, sync pypi then seal" || fail "the schedule was not saved"
RUN=$(CAPI post "schedules/$S/run" '{}')
for J in $(echo "$RUN" | sed -n 's/.*"jobs":\[\([0-9,]*\)\].*/\1/p' | tr ',' ' '); do
  [ "$(CAPI wait "$J" | tr -d '\r')" = succeeded ] || { CAPI get "jobs/$J"; fail "scheduled job $J failed"; }
done
[ "$(echo "$RUN" | grep -o '"jobs":\[[0-9,]*\]' | tr -cd ',' | wc -c)" = 1 ] && ok "running it queued a sync and a seal, and both succeeded" || fail "the schedule ran: $RUN"
CAPI get audit | grep -q '"job.sync"' && CAPI get audit | grep -q "schedule $S" && ok "the jobs and the schedule's own are in the audit log"

step "5. carry the bundle across"
docker cp -q mer-i-collector:/var/lib/mer/files/usb "$WORK/usb"
docker cp -q "$WORK/usb" mer-i-vault:/var/lib/mer/files/usb
$V chown -R mer:mer /var/lib/mer/files/usb
ok "$(ls "$WORK/usb/bundle-1" | wc -l) files on the 'disk'"

step "6. vault panel: import and publish"
VAPI login && ok "signed in to the vault panel as admin"
J=$(VAPI post bundles/import '{"dir":"usb/bundle-1","auto_publish":true}' | job_of); [ "$(VAPI wait "$J" | tr -d '\r')" = succeeded ] && ok "import job $J succeeded" || { VAPI get "jobs/$J"; fail "import failed"; }
VAPI get overview | grep -q '"live":1' && ok "the repository is published"
$V sh -c 'curl -sk -o /dev/null -w "%{http_code}" https://127.0.0.1:8443/ui/' | grep -q 404 && ok "the panel is not on the package listener"

step "7. a stock pip installs from the vault, trusting its generated certificate"
docker cp mer-i-vault:/etc/mer/tls/cert.pem "$WORK/vault.pem"
docker run -d --name mer-i-client --network "$NET" -v "$WORK/vault.pem:/vault.pem:ro" python:3.12-slim sleep infinity >/dev/null
docker exec mer-i-client sh -c 'pip install -q --disable-pip-version-check --root-user-action=ignore --cert /vault.pem --index-url https://vault:8443/pypi/simple/ requests' 2>&1 | tail -2 | sed 's/^/  | /'
OUT=$(docker exec mer-i-client python -c 'import requests; print(requests.__version__)' | tr -d '\r')
[ -n "$OUT" ] && ok "requests $OUT installed from the vault" || fail "pip install failed"

step "8. upgrade in place, then uninstall"
$V bash /inst/install.sh vault "${FROM[@]}" --admin-listen 127.0.0.1:8444 >/dev/null 2>&1 && ok "re-running the installer upgrades"
docker exec mer-i-client sh -c 'pip download -q --no-deps --cert /vault.pem --index-url https://vault:8443/pypi/simple/ -d /tmp/x requests' && ok "the vault still serves after the upgrade"
$V bash /inst/install.sh --uninstall >/dev/null && ! $V sh -c 'command -v mer' >/dev/null && $V test -d /var/lib/mer && ok "uninstalled; the data is kept"

step "DONE: all checks passed"
