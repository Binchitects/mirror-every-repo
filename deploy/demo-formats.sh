#!/usr/bin/env bash
# End-to-end demo of the language formats: mirror real packages on the
# collector, carry them across, and install them with stock clients that have
# no internet access.
set -euo pipefail
# If a step stops the script, say which command it was.
trap 'echo "STOPPED at line $LINENO: $BASH_COMMAND" >&2' ERR
export MSYS_NO_PATHCONV=1
export COMPOSE_PROFILES=formats
cd "$(dirname "$0")/.."

C="docker compose exec -T collector"
V="docker compose exec -T vault"
PY="docker compose exec -T pyclient"
step() { printf '\n=== %s ===\n' "$*"; }
ok()   { printf '  OK: %s\n' "$*"; }
fail() { printf '  FAILED: %s\n' "$*"; exit 1; }
SEQ=0
# Seal whatever changed on the collector and import it on the vault.
carry() {
  SEQ=$((SEQ + 1))
  $C mer export seal --key /keys/bundle.key | tail -1
  $C mer export media --seq "$SEQ" --out "/media/b$SEQ" --volume-size 512M >/dev/null
  $V mer import media "/media/b$SEQ" --auto-publish | tail -2
}

step "1. build the image and start the nodes"
docker compose down -v --remove-orphans >/dev/null 2>&1 || true
docker compose up -d --build 2>&1 | tail -3
docker compose exec -T -u root collector chown mer:mer /media

step "2. network isolation"
$C timeout 5 bash -c '</dev/tcp/1.1.1.1/443' 2>/dev/null && ok "collector can reach the internet" || fail "collector needs internet for this demo"
$V timeout 5 bash -c '</dev/tcp/1.1.1.1/443' 2>/dev/null && fail "vault can reach the internet" || ok "vault cannot reach the internet"
$PY timeout 5 bash -c '</dev/tcp/1.1.1.1/443' 2>/dev/null && fail "python client can reach the internet" || ok "python client cannot reach the internet"

step "3. pairing"
$C mer init
$V mer init
PUB=$($C mer key gen /keys/bundle.key | tr -d '\r\n')
$V mer key trust /data/trust.txt "$PUB"
docker compose exec -T -d vault sh -c 'mer serve --listen 0.0.0.0:8080 > /tmp/http.log 2>&1'

step "4. PyPI: mirror 'requests' for CPython 3.12 from pypi.org"
$C mer repo add pypi --format pypi
$C mer pypi sync pypi --python 3.12 -r requests
carry

step "5. PyPI: install with a stock pip that can only reach the vault"
PIP="pip install --disable-pip-version-check --no-cache-dir --index-url http://vault:8080/pypi/simple/ --trusted-host vault"
$PY sh -c 'python -c "import requests" 2>/dev/null' && fail "requests is already installed" || ok "requests is not installed yet"
$PY sh -c "$PIP requests" 2>&1 | tail -3 | sed 's/^/  | /'
OUT=$($PY python -c 'import requests, urllib3, idna, certifi, charset_normalizer; print("requests", requests.__version__)' | tr -d '\r')
case "$OUT" in "requests "*) ok "installed from the vault and imports with all its dependencies: $OUT" ;; *) fail "import failed: $OUT" ;; esac
if $PY sh -c "$PIP flask" >/dev/null 2>&1; then fail "flask was installable although it is not mirrored"
else ok "a project that is not mirrored cannot be installed"; fi
$V mer wishlist | sed 's/^/  | /'
case "$($V mer wishlist)" in *simple/flask*) ok "the request for flask is on the wishlist" ;; *) fail "flask is not on the wishlist" ;; esac

step "6. PyPI: act on the wishlist; only the new files cross"
$C mer pypi sync pypi --python 3.12 -r requests -r flask | tail -3
carry
$PY sh -c "$PIP flask" 2>&1 | tail -1 | sed 's/^/  | /'
OUT=$($PY python -c 'import flask; print(flask.Flask("x").name)' | tr -d '\r')
[ "$OUT" = "x" ] && ok "flask installs and runs after the next bundle" || fail "flask did not run"

step "7. vault: state and integrity"
$V mer status
$V mer fsck --deep
$V sh -c 'grep -c " 200 " /tmp/http.log' | sed 's/^/  requests answered with 200: /'

step "DONE: all checks passed"
