#!/usr/bin/env python3
"""Regenerate maven_truth.json from Maven's own version comparison.

    python crates/mer-format-maven/tests/gen_fixture.py

Runs org.apache.maven.artifact.versioning.ComparableVersion from a Maven
container on every ordered pair of the versions below and records the
result, so the Rust implementation can be checked against the reference.
"""
import json, os, subprocess

VERSIONS = [
    "1", "1.0", "1.0.0", "1.1", "1.0.1", "1.10", "1.2", "2", "0.9", "1.0.0.0.1",
    "1.0-alpha", "1.0-alpha-1", "1.0-alpha1", "1.0a1", "1.0-a1", "1.0-beta", "1.0-b2", "1.0-beta-2",
    "1.0-milestone-1", "1.0-m1", "1.0-M1", "1.0-rc", "1.0-rc1", "1.0-RC1", "1.0-cr1", "1.0-rc-2",
    "1.0-SNAPSHOT", "1.0-snapshot", "1.0-ga", "1.0-final", "1.0.Final", "1.0.RELEASE", "1.0-release",
    "1.0-sp", "1.0-sp1", "1.0-sp-1", "1.0-xyz", "1.0-abc", "1.0.xyz", "1.0-1", "1.0-2", "1.0.0-1",
    "1.0-jre", "31.1-jre", "31.1-android", "2.0.0.RELEASE", "2.0.0.M1", "2.0.0.RC1", "2.0.0.BUILD-SNAPSHOT",
    "5.3.31", "5.3.9", "6.0.0-M2", "6.0.0-RC1", "6.0.0", "20040616", "1.0.0-20240101.123456-7",
    "1.0-alpha-1-SNAPSHOT", "1.0.1-SNAPSHOT", "1-1.foo-bar1baz-.1", "1.0.0-rc1+build5", "2.13.4.2", "2.13.4-1",
    "1.0-0", "1.0.0-0", "1-0", "1.a", "1.0.b", "0.0.1", "v1.0", "1.0-", "1..0",
]

args = []
for a in VERSIONS:
    for b in VERSIONS:
        args += [a, b]
# The pairs go in on standard input: there are too many for a command line on every system.
out = subprocess.run(
    ["docker", "run", "--rm", "-i", "maven:3.9-eclipse-temurin-17", "sh", "-c",
     'java -cp "/usr/share/maven/lib/*" org.apache.maven.artifact.versioning.ComparableVersion $(cat)'],
    input=" ".join(args), check=True, capture_output=True, text=True, env={**os.environ, "MSYS_NO_PATHCONV": "1"},
).stdout
# One comparison line follows each argument but the last: "   a < b".
lines = [l for l in out.splitlines() if l.startswith("   ")]
assert len(lines) == len(args) - 1, (len(lines), len(args))
sign = {"<": -1, "==": 0, ">": 1}
n = len(VERSIONS)
matrix = []
for i in range(n):
    row = []
    for j in range(n):
        line = lines[2 * (i * n + j)]
        a, op, b = line.strip().rsplit(" ", 2)
        assert (a, b) == (VERSIONS[i], VERSIONS[j]), line
        row.append(sign[op])
    matrix.append(row)
path = os.path.join(os.path.dirname(__file__), "maven_truth.json")
with open(path, "w", newline="\n") as f:
    json.dump({"reference": "Apache Maven 3.9 ComparableVersion", "versions": VERSIONS, "cmp": matrix}, f, separators=(",", ":"))
    f.write("\n")
print(f"{n} versions, {n * n} comparisons -> {path}")
