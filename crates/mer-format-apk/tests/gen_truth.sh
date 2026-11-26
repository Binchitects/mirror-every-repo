#!/bin/sh
# Regenerate tests/apk_truth.json: version pairs and how apk itself orders
# them. Run inside Alpine, from the repository root:
#
#   docker run --rm -v "$PWD:/w" -w /w alpine:3.24 sh crates/mer-format-apk/tests/gen_truth.sh
#
# Pairs come from the real versions in main and community, plus edge cases
# for every token kind (suffixes, letters, leading zeros, hashes, revisions).
set -eu
T=$(mktemp -d)
for r in main community; do
  wget -q -O "$T/$r.tgz" "https://dl-cdn.alpinelinux.org/alpine/v3.24/$r/x86_64/APKINDEX.tar.gz"
  tar -xzf "$T/$r.tgz" -C "$T" APKINDEX 2>/dev/null
  sed -n 's/^V://p' "$T/APKINDEX" >> "$T/versions"
done
sort -u "$T/versions" > "$T/all"
# Neighbours in a plain sort share prefixes, which is where ordering is subtle.
awk 'NR > 1 { print prev, $0 } { prev = $0 }' "$T/all" | awk 'NR % 7 == 0' > "$T/pairs"
# Pairs far apart.
awk '{ a[NR] = $0 } END { srand(42); for (i = 0; i < 600; i++) print a[int(rand() * NR) + 1], a[int(rand() * NR) + 1] }' "$T/all" >> "$T/pairs"
cat >> "$T/pairs" <<'EOF'
1.0 1.0
1.0 1.0-r0
1.0-r1 1.0-r0
1.0-r10 1.0-r9
1.0 1.0.0
1.0.1 1.0
1.10 1.9
1.01 1.1
1.001 1.01
1.0a 1.0
1.0a 1.0b
1.0z 1.1
1.0_alpha 1.0
1.0_alpha 1.0_beta
1.0_beta 1.0_pre
1.0_pre 1.0_rc
1.0_rc 1.0
1.0_rc1 1.0_rc2
1.0_rc10 1.0_rc9
1.0 1.0_cvs
1.0_cvs 1.0_svn
1.0_svn 1.0_git
1.0_git 1.0_hg
1.0_hg 1.0_p
1.0_p1 1.0_p2
1.0_p 1.0.1
1.0_alpha1_p2 1.0_alpha1
1.0_rc1-r1 1.0-r0
1.0~abc123 1.0
1.0~abc123 1.0~abd
1.0~abc-r1 1.0~abc-r0
20240101 20231231
0.0.1 0.0.01
1.2.3_rc1-r0 1.2.3-r0
1.2a 1.2_p1
2.0_beta20210101 2.0_beta3
18446744073709551616 1
1.0_pre1_rc2 1.0_pre1
1.0.0.0.1 1.0.0.0
3a 3.1
EOF
{
  printf '[\n'
  first=1
  while read -r a b; do
    r=$(apk version -t "$a" "$b" 2>/dev/null || true)
    case "$r" in "<"|">"|"=") ;; *) continue ;; esac
    [ "$first" = 1 ] || printf ',\n'
    first=0
    printf '["%s","%s","%s"]' "$a" "$b" "$r"
  done < "$T/pairs"
  printf '\n]\n'
} > crates/mer-format-apk/tests/apk_truth.json
echo "$(grep -c '^\[' crates/mer-format-apk/tests/apk_truth.json) pairs" >&2
