#!/usr/bin/env python3
"""Regenerate rpm_truth.json from rpm's own version comparison.

    python crates/mer-format-rpm/tests/gen_fixture.py

Runs rpm.vercmp (the rpmvercmp function, through rpm's built-in Lua) in a
Rocky Linux container on every ordered pair of the strings below.
"""
import json, os, subprocess

VERSIONS = [
    "1.0", "1.0.0", "1.1", "1.10", "1.2", "2", "0.9", "1", "01", "1.01", "1.001", "10", "9",
    "1.0a", "1.0b", "1.0.a", "1.a", "a", "b", "A", "ab", "abc", "1a", "a1", "1.0a1", "1.0a2", "1.0b1",
    "1.0~rc1", "1.0~rc2", "1.0~", "1.0~~", "1.0~beta", "1~", "1.0^", "1.0^git1", "1.0^git2", "1.0^20240101",
    "1.0~rc1^git1", "1.0.1^git1", "1.0.1", "5.1.8", "5.1.16", "9.el9", "9.el9_3", "9.el9_3.1", "10.el9", "9.el8",
    "2.34", "2.34.1", "20240101", "2024.01.01", "1_0", "1-0", "1+0", "1..0", "1.0.", ".1", "1.0-1", "3.2.2",
    "1.fc40", "1.fc41", "0.1.rc1", "0.1.20240101git", "1.0.0alpha", "1.0.0.alpha", "1.0x", "1.0X", "1.0post1",
]

lua = "local v = {" + ",".join('"%s"' % v for v in VERSIONS) + "} for i = 1, #v do for j = 1, #v do print(rpm.vercmp(v[i], v[j]) .. \",\") end end"
out = subprocess.run(
    ["docker", "run", "--rm", "-i", "rockylinux:9", "sh", "-c", 'rpm --version >&2; rpm --eval "%{lua: $(cat)}"'],
    input=lua, check=True, capture_output=True, text=True, env={**os.environ, "MSYS_NO_PATHCONV": "1"},
)
values = [int(x) for x in out.stdout.split(",") if x.strip()]
n = len(VERSIONS)
assert len(values) == n * n, (len(values), n * n, out.stderr[-500:])
matrix = [values[i * n:(i + 1) * n] for i in range(n)]
path = os.path.join(os.path.dirname(__file__), "rpm_truth.json")
with open(path, "w", newline="\n") as f:
    json.dump({"reference": out.stderr.strip().splitlines()[-1], "versions": VERSIONS, "cmp": matrix}, f, separators=(",", ":"))
    f.write("\n")
print(f"{n} versions, {n * n} comparisons ({out.stderr.strip().splitlines()[-1]}) -> {path}")
