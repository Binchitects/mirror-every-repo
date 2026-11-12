#!/usr/bin/env bash
# End-to-end demo of the formats added in 1.1: mirror real packages on the
# collector, carry them across, and use them with stock clients that have no
# internet access.
set -euo pipefail
# If a step stops the script, say which command it was.
trap 'echo "STOPPED at line $LINENO: $BASH_COMMAND" >&2' ERR
export MSYS_NO_PATHCONV=1
export COMPOSE_PROFILES=more
cd "$(dirname "$0")/.."

C="docker compose exec -T collector"
V="docker compose exec -T vault"
HELM="docker compose exec -T helmclient"
RUST="docker compose exec -T cargoclient"
GO="docker compose exec -T goclient"
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
$HELM timeout 5 nc -z 1.1.1.1 443 2>/dev/null && fail "helm client can reach the internet" || ok "helm client cannot reach the internet"

step "3. pairing"
$C mer init
$V mer init
PUB=$($C mer key gen /keys/bundle.key | tr -d '\r\n')
$V mer key trust /data/trust.txt "$PUB"
docker compose exec -T -d vault sh -c 'mer serve --listen 0.0.0.0:8080 > /tmp/http.log 2>&1'

step "4. Helm: mirror 'ingress-nginx' and 'cert-manager' from their chart repositories"
$C mer repo add nginx-charts --format helm
# This repository keeps its chart archives on GitHub releases.
$C mer helm sync nginx-charts --url https://kubernetes.github.io/ingress-nginx -c ingress-nginx --allow-host github.com
$C mer repo add jetstack --format helm
$C mer helm sync jetstack --url https://charts.jetstack.io -c cert-manager -c 'cert-manager@~1.16'
carry

step "5. Helm: search, pull and render with a stock helm that can only reach the vault"
$HELM helm repo add nginx http://vault:8080/nginx-charts | sed 's/^/  | /'
$HELM helm repo add jetstack http://vault:8080/jetstack | sed 's/^/  | /'
$HELM helm search repo --versions | tr -d '\r' | cut -c1-70 | sed 's/^/  | /'
KINDS=$($HELM helm template web nginx/ingress-nginx | tr -d '\r' | grep -c '^kind: ' || true)
[ "$KINDS" -gt 5 ] && ok "ingress-nginx was pulled from the vault and renders $KINDS Kubernetes objects" || fail "ingress-nginx did not render"
OUT=$($HELM sh -c 'cd /tmp && helm pull jetstack/cert-manager --version "~1.16" --untar && grep "^version:" cert-manager/Chart.yaml' | tr -d '\r')
case "$OUT" in "version: v1.16."*) ok "cert-manager pulled by range: $OUT" ;; *) fail "cert-manager was not pulled: $OUT" ;; esac
if $HELM helm pull jetstack/cert-manager --version 1.12.0 >/dev/null 2>&1; then fail "a version that is not mirrored was pulled"
else ok "a version that is not mirrored cannot be pulled"; fi
$HELM sh -c 'wget -q -O /dev/null http://vault:8080/jetstack/charts/cert-manager-1.12.0.tgz' 2>/dev/null || true
case "$($V mer wishlist)" in *"charts/cert-manager-1.12.0"*) ok "a direct request for that chart is on the wishlist" ;; *) $V mer wishlist; fail "the chart is not on the wishlist" ;; esac

step "6. Helm: a chart kept in an OCI registry, mirrored as a container repo"
$C mer repo add oci-charts --format oci
SYNC=$($C mer oci sync oci-charts --registry https://ghcr.io -i stefanprodan/charts/podinfo:6.7.1 | tr -d '\r')
echo "$SYNC"
UPSTREAM_DIGEST=$(echo "$SYNC" | awk '/->/ {print $NF}')
carry
OUT=$($HELM sh -c 'cd "$(mktemp -d)" && helm pull oci://vault:8080/oci-charts/stefanprodan/charts/podinfo --version 6.7.1 --plain-http --untar 2>&1 && grep "^version:" podinfo/Chart.yaml' | tr -d '\r')
case "$OUT" in *"Digest: $UPSTREAM_DIGEST"*"version: 6.7.1"*) ok "helm pull oci:// from the vault gives the upstream digest ($UPSTREAM_DIGEST)" ;; *) echo "$OUT"; fail "the OCI chart was not pulled" ;; esac

step "7. Cargo: mirror 'serde' and 'serde_json' with their dependencies from crates.io"
$C mer repo add crates --format cargo
$C mer cargo sync crates -p serde -p serde_json | grep -v WARNING
carry

step "8. Cargo: build and run with a stock cargo that can only reach the vault"
$RUST timeout 5 bash -c '</dev/tcp/1.1.1.1/443' 2>/dev/null && fail "rust client can reach the internet" || ok "rust client cannot reach the internet"
$RUST sh -c 'mkdir -p /tmp/app/src /tmp/app/.cargo'
$RUST sh -c 'cat > /tmp/app/.cargo/config.toml' <<'TOML'
[source.crates-io]
replace-with = "vault"

[registries.vault]
index = "sparse+http://vault:8080/crates/index/"
TOML
$RUST sh -c 'cat > /tmp/app/Cargo.toml' <<'TOML'
[package]
name = "app"
version = "0.1.0"
edition = "2021"

[dependencies]
serde = { version = "1", features = ["derive"] }
serde_json = "1"
TOML
$RUST sh -c 'cat > /tmp/app/src/main.rs' <<'RS'
#[derive(serde::Serialize)]
struct Hello {
    from: &'static str,
}
fn main() {
    println!("{}", serde_json::to_string(&Hello { from: "the vault" }).unwrap());
}
RS
OUT=$($RUST sh -c 'cd /tmp/app && cargo run 2>&1' | tr -d '
')
echo "$OUT" | grep -E 'Downloaded [0-9]+ crates|Finished' | sed 's/^/  | /'
case "$OUT" in *'{"from":"the vault"}'*) ok "resolved, downloaded, compiled (with a derive macro) and ran from the vault" ;; *) echo "$OUT" | tail -15; fail "the program did not run" ;; esac
LOCKED=$($RUST sh -c 'grep -c "^checksum = " /tmp/app/Cargo.lock' | tr -d '
')
ok "$LOCKED crates in Cargo.lock, each checked by cargo against the checksum in the index"
if $RUST sh -c 'cd /tmp/app && cargo add rand' >/dev/null 2>&1; then fail "rand was usable although it is not mirrored"
else ok "a crate that is not mirrored cannot be added"; fi
case "$($V mer wishlist)" in *"crates  rand"*) ok "the request for rand is on the wishlist" ;; *) $V mer wishlist; fail "rand is not on the wishlist" ;; esac

step "9. Go: mirror 'github.com/gin-gonic/gin' v1.10.0 from proxy.golang.org, checked against sum.golang.org"
$C mer repo add go --format go
$C mer go sync go -m github.com/gin-gonic/gin@v1.10.0
carry

step "10. Go: get, build and run with a stock go that can only reach the vault"
$GO timeout 5 bash -c '</dev/tcp/1.1.1.1/443' 2>/dev/null && fail "go client can reach the internet" || ok "go client cannot reach the internet"
$GO sh -c 'mkdir -p /tmp/app && cd /tmp/app && go mod init app >/dev/null 2>&1'
$GO sh -c 'cat > /tmp/app/main.go' <<'GOSRC'
package main

import (
	"fmt"
	"net/http/httptest"

	"github.com/gin-gonic/gin"
)

func main() {
	gin.SetMode(gin.ReleaseMode)
	r := gin.New()
	r.GET("/hello", func(c *gin.Context) { c.JSON(200, gin.H{"from": "the vault"}) })
	w := httptest.NewRecorder()
	r.ServeHTTP(w, httptest.NewRequest("GET", "/hello", nil))
	fmt.Println(w.Body.String(), gin.Version)
}
GOSRC
$GO sh -c 'cd /tmp/app && go get github.com/gin-gonic/gin@v1.10.0 2>&1' | tail -3 | sed 's/^/  | /'
OUT=$($GO sh -c 'cd /tmp/app && go run . 2>&1' | tr -d '')
case "$OUT" in '{"from":"the vault"} v1.10.0') ok "gin was fetched from the vault, compiled and ran: $OUT" ;; *) echo "$OUT" | tail -15; fail "the program did not run" ;; esac
SUMS=$($GO sh -c 'grep -c "" /tmp/app/go.sum' | tr -d '')
ok "$SUMS lines in go.sum"
OUT=$($GO sh -c 'cd /tmp/app && go mod verify 2>&1' | tr -d '')
case "$OUT" in "all modules verified") ok "go mod verify: $OUT" ;; *) echo "$OUT"; fail "go mod verify failed" ;; esac
if $GO sh -c 'cd /tmp/app && go get github.com/google/uuid' >/dev/null 2>&1; then fail "uuid was usable although it is not mirrored"
else ok "a module that is not mirrored cannot be fetched"; fi
case "$($V mer wishlist)" in *"github.com/google/uuid"*) ok "the request for github.com/google/uuid is on the wishlist" ;; *) $V mer wishlist; fail "uuid is not on the wishlist" ;; esac

step "vault: state and integrity"
$V mer status
$V mer fsck --deep
$V sh -c 'grep -c " 200 " /tmp/http.log' | sed 's/^/  requests answered with 200: /'

step "DONE: all checks passed"
