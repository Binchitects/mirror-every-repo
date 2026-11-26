"""Generates rpm_truth.json with rpm's own version comparison.

    docker run --rm -v "$PWD":/t -w /t rockylinux:9 python3 gen_truth.py
"""
import itertools, json, rpm

versions = [
    "1.0", "1.0.0", "1.0a", "1.0.a", "1.0~rc1", "1.0~rc2", "1.0~", "1.0^", "1.0^git1", "1.0^git1~pre",
    "1.01", "1.1", "1.10", "1.9", "2", "2.0", "a", "b", "1a", "1.a", "1_0", "1..0",
    "5.2", "5.10", "0001", "1", "1.0.1", "1.0+1", "10.0", "2.0a1", "2.0b", "1.0z", "0.9.9",
]
evrs = [
    ("0", "1.0", "1"), ("1", "0.5", "1"), ("0", "1.0", "1.el9"), ("0", "1.0", "1.el9_2"), ("0", "1.0", "2.el9"),
    ("0", "1.0", "1.el9.1"), ("", "1.0", "1"), ("2", "1.0", "1"), ("0", "1.0~rc1", "1"), ("0", "1.0", "1~"),
]

out = {"vercmp": [], "evrcmp": []}
for a, b in itertools.product(versions, repeat=2):
    out["vercmp"].append([a, b, rpm.labelCompare(("0", a, "1"), ("0", b, "1"))])
for a, b in itertools.product(evrs, repeat=2):
    ea = (a[0] or None, a[1], a[2])
    eb = (b[0] or None, b[1], b[2])
    out["evrcmp"].append([list(a), list(b), rpm.labelCompare(ea, eb)])
json.dump(out, open("rpm_truth.json", "w"), indent=0)
print(len(out["vercmp"]), "version pairs,", len(out["evrcmp"]), "evr pairs")
