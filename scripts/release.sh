#!/usr/bin/env bash
# Build and publish a release from a tag, on this machine. It produces what
# .github/workflows/release.yml produces, for when the hosted runners are
# not available:
#
#   scripts/release.sh v1.2.0              # build and check only
#   scripts/release.sh v1.2.0 --publish    # also create the GitHub Release
#   scripts/release.sh v1.2.0 --publish --image   # and push the container image
#
# Gates, in order: the tag exists and matches the version in Cargo.toml;
# lint and the full test suite pass at that tag; the binary builds and runs
# on a clean Debian. Publishing needs `gh` signed in with the repo scope;
# --image needs `docker login ghcr.io` (a token with write:packages).
set -euo pipefail
trap 'echo "STOPPED at line $LINENO: $BASH_COMMAND" >&2' ERR
export MSYS_NO_PATHCONV=1

TAG="${1:?usage: scripts/release.sh vX.Y.Z [--publish] [--image]}"
shift
PUBLISH=0 IMAGE=0
for a in "$@"; do
  case "$a" in
    --publish) PUBLISH=1 ;;
    --image) IMAGE=1 ;;
    *) echo "unknown option $a" >&2; exit 2 ;;
  esac
done
case "$TAG" in v[0-9]*.[0-9]*.[0-9]*) ;; *) echo "tag must look like v1.2.3" >&2; exit 2 ;; esac
VERSION="${TAG#v}"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
REPO="$(git -C "$ROOT" remote get-url origin | sed -E 's#(git@github.com:|https://github.com/)##; s#\.git$##')"
IMAGE_NAME="ghcr.io/$(echo "$REPO" | tr '[:upper:]' '[:lower:]')"
step() { printf '\n=== %s ===\n' "$*"; }

step "1. the tag"
git -C "$ROOT" rev-parse -q --verify "refs/tags/$TAG" >/dev/null || { echo "tag $TAG does not exist locally" >&2; exit 1; }
if [ "$PUBLISH" = 1 ]; then
  git -C "$ROOT" ls-remote --exit-code --tags origin "refs/tags/$TAG" >/dev/null || { echo "push the tag first: git push origin $TAG" >&2; exit 1; }
fi
WORK="$(mktemp -d)"
cleanup() { git -C "$ROOT" worktree remove --force "$WORK/src" >/dev/null 2>&1 || true; rm -rf "$WORK"; }
trap cleanup EXIT
git -C "$ROOT" worktree add -q --detach "$WORK/src" "$TAG"
CARGO_VERSION="$(grep -m1 '^version = ' "$WORK/src/Cargo.toml" | cut -d'"' -f2)"
[ "$CARGO_VERSION" = "$VERSION" ] || { echo "Cargo.toml says $CARGO_VERSION, the tag says $VERSION" >&2; exit 1; }
echo "$TAG is $(git -C "$ROOT" rev-parse --short "$TAG^{commit}"), version $VERSION"

step "2. lint, test and build at the tag (rust:1.99-bookworm, as CI)"
# Its own target volume, so a release never reuses development build output.
docker run --rm -v "$WORK/src:/work" -v mer-release-target:/target -v mer-cargo-registry:/usr/local/cargo/registry \
  -e CARGO_TARGET_DIR=/target -e CARGO_TERM_COLOR=never -w /work rust:1.99-bookworm sh -c '
    set -e
    rustup component add clippy >/dev/null 2>&1
    cargo clippy --workspace --all-targets --locked -q -- -D warnings
    cargo test --workspace --locked -q 2>&1 | grep -E "^test result" | awk "{p+=\$4; f+=\$6} END {print \"tests passed:\", p, \"failed:\", f; exit f>0}"
    # Static (musl): runs on any x86-64 Linux, whatever its glibc.
    apt-get update -qq >/dev/null && apt-get install -y -qq musl-tools >/dev/null 2>&1
    rustup target add x86_64-unknown-linux-musl >/dev/null 2>&1
    cargo build --release --locked -q -p mer --target x86_64-unknown-linux-musl
    cp /target/x86_64-unknown-linux-musl/release/mer /work/mer.release'

step "3. package"
NAME="mer-${TAG}-linux-x86_64"
mkdir -p "$WORK/out/$NAME"
cp "$WORK/src/mer.release" "$WORK/out/$NAME/mer"
cp "$WORK/src/LICENSE" "$WORK/src/README.md" "$WORK/src/CHANGELOG.md" "$WORK/out/$NAME/"
cp "$ROOT/deploy/install.sh" "$WORK/out/install.sh"   # the current installer works with any release
(cd "$WORK/out" && tar -czf "$NAME.tar.gz" "$NAME" && sha256sum "$NAME.tar.gz" > "$NAME.tar.gz.sha256" && sha256sum install.sh > install.sh.sha256)
ls -l "$WORK/out" | sed 's/^/  /'

step "4. the binary runs on old and new distributions"
for img in debian:bookworm-slim rockylinux:8 ubuntu:20.04 alpine:3.20; do
  printf '  %-22s ' "$img"
  docker run --rm -v "$WORK/out:/out:ro" "$img" sh -c "cd /tmp && tar -xzf /out/$NAME.tar.gz && ./$NAME/mer --version && ./$NAME/mer --data /tmp/d init >/dev/null"
done

step "5. release notes from CHANGELOG.md"
awk -v v="$VERSION" '/^## / { if (found) exit; if (index($0, "[" v "]")) { found = 1; next } } found { print }' "$WORK/src/CHANGELOG.md" > "$WORK/notes.md"
[ -s "$WORK/notes.md" ] || { echo "CHANGELOG.md has no section for [$VERSION]" >&2; exit 1; }
cat >> "$WORK/notes.md" <<EOF

## Get it

Install a vault or a collector with one command (as root):

\`\`\`bash
curl -fsSL https://github.com/$REPO/releases/download/$TAG/install.sh | bash -s -- vault
\`\`\`

Or download the Linux binary below (static, for any x86-64 Linux) and check it against the \`.sha256\` file.
EOF
if [ "$IMAGE" = 1 ]; then
  printf '\nThe container image:\n\n```bash\ndocker pull %s:%s\n```\n' "$IMAGE_NAME" "$VERSION" >> "$WORK/notes.md"
fi
sed 's/^/  /' "$WORK/notes.md" | head -40

if [ "$IMAGE" = 1 ]; then
  step "6. container image"
  docker build -q -t "$IMAGE_NAME:$VERSION" -t "$IMAGE_NAME:latest" "$WORK/src" >/dev/null
  docker run --rm "$IMAGE_NAME:$VERSION" --version
  # Each push must succeed, or the release stops here (a failure on the left of
  # && would not stop the script). Uploads to ghcr.io sometimes time out, so
  # each is tried a few times; a retry resumes with the layers already there.
  push() {
    for try in 1 2 3; do
      docker push -q "$1" && return 0
      echo "push of $1 failed (try $try of 3)" >&2
      sleep 10
    done
    # The Docker daemon may upload through a proxy that times out on large
    # layers. skopeo, in a container, connects directly and uses the same
    # login (docker login writes it to ~/.docker/config.json).
    echo "pushing $1 with skopeo instead" >&2
    docker save "$1" -o "$WORK/image.tar" &&
      docker run --rm -v "$WORK:/img:ro" -v "${DOCKER_CONFIG:-$HOME/.docker}/config.json:/auth.json:ro" quay.io/skopeo/stable:v1.16.1 \
        copy -q --retry-times 3 --authfile /auth.json "docker-archive:/img/image.tar" "docker://$1"
  }
  push "$IMAGE_NAME:$VERSION" || { echo "the image was not pushed; nothing was published" >&2; exit 1; }
  push "$IMAGE_NAME:latest" || { echo "the latest tag was not pushed; nothing was published" >&2; exit 1; }
  # Prove it is there: read it back from the registry.
  docker manifest inspect "$IMAGE_NAME:$VERSION" >/dev/null || { echo "the registry does not have $IMAGE_NAME:$VERSION" >&2; exit 1; }
  echo "pushed $IMAGE_NAME:$VERSION and :latest"
fi

if [ "$PUBLISH" = 1 ]; then
  step "7. GitHub Release"
  FILES=("$WORK/out/$NAME.tar.gz" "$WORK/out/$NAME.tar.gz.sha256" "$WORK/out/install.sh" "$WORK/out/install.sh.sha256")
  if gh release view "$TAG" --repo "$REPO" >/dev/null 2>&1; then
    gh release upload "$TAG" --repo "$REPO" --clobber "${FILES[@]}"
    gh release edit "$TAG" --repo "$REPO" --notes-file "$WORK/notes.md" >/dev/null
  else
    gh release create "$TAG" --repo "$REPO" --title "$TAG" --notes-file "$WORK/notes.md" --verify-tag "${FILES[@]}"
  fi
  # Download what was published and check it, as a user would.
  mkdir "$WORK/check"
  gh release download "$TAG" --repo "$REPO" --dir "$WORK/check" --pattern "$NAME.tar.gz*"
  (cd "$WORK/check" && sha256sum -c "$NAME.tar.gz.sha256")
  echo "published: https://github.com/$REPO/releases/tag/$TAG"
fi
echo
echo "DONE: $TAG"
