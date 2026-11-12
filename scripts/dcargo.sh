#!/usr/bin/env bash
# Run cargo inside the official Rust container (no local toolchain needed).
#
#   scripts/dcargo.sh test --workspace
#   scripts/dcargo.sh clippy --workspace --all-targets -- -D warnings
#
# The image is the one CI and the release build use, so results match.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && (pwd -W 2>/dev/null || pwd))"
# The image ships without clippy; add it when it is asked for.
SETUP=':'
if [ "${1:-}" = "clippy" ]; then SETUP='rustup component add clippy >/dev/null 2>&1'; fi
MSYS_NO_PATHCONV=1 docker run --rm \
  -v "$ROOT:/work" \
  -v mer-target:/target \
  -v mer-cargo-registry:/usr/local/cargo/registry \
  -e CARGO_TARGET_DIR=/target -e CARGO_TERM_COLOR=never \
  -w /work rust:1.99-bookworm sh -c "$SETUP"' && exec cargo "$@"' cargo "$@"
