#!/usr/bin/env bash
# Run cargo inside the official Rust container (no local toolchain needed).
# Usage: scripts/dcargo.sh test -p mer-core
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && (pwd -W 2>/dev/null || pwd))"
MSYS_NO_PATHCONV=1 docker run --rm \
  -v "$ROOT:/work" \
  -v mer-target:/target \
  -v mer-cargo-registry:/usr/local/cargo/registry \
  -e CARGO_TARGET_DIR=/target -e CARGO_TERM_COLOR=never \
  -w /work rust:1-bookworm cargo "$@"
