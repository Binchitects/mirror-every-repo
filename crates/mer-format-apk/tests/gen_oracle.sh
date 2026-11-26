#!/bin/sh
# Regenerate tests/oracle: what apk itself installs into an empty root for
# real packages of Alpine's main repository, and the part of that index the
# answers depend on (the packages, every other provider of what they need,
# and the install-if packages that mention them). Run inside Alpine, from
# the repository root:
#
#   docker run --rm -v "$PWD:/w" -w /w alpine:3.24 sh crates/mer-format-apk/tests/gen_oracle.sh
set -eu
M=https://dl-cdn.alpinelinux.org/alpine/v3.24/main
OUT=crates/mer-format-apk/tests/oracle
T=$(mktemp -d)
apk add -q python3
wget -q -O "$T/index.tgz" "$M/x86_64/APKINDEX.tar.gz"
tar -xzf "$T/index.tgz" -C "$T" APKINDEX 2>/dev/null
mkdir -p "$OUT"
: > "$T/answers"
for p in curl git openssh nginx python3 bash vim openssl tzdata sqlite rsync make perl sudo shadow \
         iproute2 tar xz coreutils findutils grep sed gawk procps-ng util-linux e2fsprogs dnsmasq haproxy \
         busybox-extras ca-certificates openrc chrony nftables wireguard-tools gcc musl-dev build-base \
         nodejs npm py3-pip lua5.4 ruby; do
  grep -qx "P:$p" "$T/APKINDEX" || continue
  rm -rf "$T/root" && mkdir -p "$T/root"
  got=$(apk --root "$T/root" --initdb --keys-dir /etc/apk/keys --repositories-file /dev/null -X "$M" --arch x86_64 \
          --no-scripts --no-cache add "$p" 2>/dev/null | sed -n 's/^( *[0-9]*\/[0-9]*) Installing \([^ ]*\) .*/\1/p' | sort | tr '\n' ' ')
  [ -n "$got" ] && printf '%s %s\n' "$p" "$got" >> "$T/answers"
done
python3 -I - "$T/APKINDEX" "$T/answers" "$OUT" <<'EOF'
import gzip, json, sys
index, answers, out = sys.argv[1:4]
blocks = [b for b in open(index).read().split("\n\n") if b.strip()]
def fields(b):
    f = {}
    for line in b.splitlines():
        k, _, v = line.partition(":")
        f.setdefault(k, v)
    return f
def bare(w):
    w = w.lstrip("!")
    for c in "=<>~":
        w = w.split(c)[0]
    return w
by_name = {fields(b)["P"]: b for b in blocks}
provides = {}
for b in blocks:
    f = fields(b)
    for w in [f["P"]] + f.get("p", "").split():
        provides.setdefault(bare(w), set()).add(f["P"])
want = {}
for line in open(answers):
    p, *names = line.split()
    want[p] = sorted(names)
used = set(n for v in want.values() for n in v)
keep = set(used)
for n in used:
    for d in fields(by_name[n]).get("D", "").split():
        if not d.startswith("!"):
            keep |= provides.get(bare(d), set())
for b in blocks:
    f = fields(b)
    if any(bare(w) in used for w in f.get("i", "").split()):
        keep.add(f["P"])
text = "\n\n".join(by_name[n] for n in sorted(keep)) + "\n\n"
with gzip.open(f"{out}/APKINDEX.gz", "wt", compresslevel=9) as g:
    g.write(text)
json.dump(want, open(f"{out}/installs.json", "w"), indent=0, sort_keys=True)
print(f"{len(want)} packages, {len(keep)} index entries kept", file=sys.stderr)
EOF
