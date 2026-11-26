#!/usr/bin/env python3
"""Regenerate tests/fixture: a tiny Alpine repository built the way abuild
builds one, signed with a throwaway key (only its public half is kept).

    python3 -I crates/mer-format-apk/tests/gen_fixture.py

Needs openssl. Packages are three gzip members (signature, control with
.PKGINFO and its datahash, data); the index is a signature member and a
member with DESCRIPTION and APKINDEX, signed over the raw second member.
"""
import base64, gzip, hashlib, io, os, subprocess, sys, tarfile, tempfile

HERE = os.path.dirname(os.path.abspath(__file__))
OUT = os.path.join(HERE, "fixture")
KEY_NAME = "test@example.org-1.rsa.pub"


def gz(b):
    return gzip.compress(b, mtime=0)


def tar(files, cut, checksums=False):
    buf = io.BytesIO()
    fmt = tarfile.PAX_FORMAT if checksums else tarfile.GNU_FORMAT
    with tarfile.open(fileobj=buf, mode="w", format=fmt) as t:
        for name, data in files:
            if data is None:  # a directory
                info = tarfile.TarInfo(name)
                info.type, info.mode, info.mtime = tarfile.DIRTYPE, 0o755, 0
                t.addfile(info)
                continue
            info = tarfile.TarInfo(name)
            info.size, info.mtime, info.mode = len(data), 0, 0o644
            if checksums:  # as abuild-tar adds them, and apk requires
                info.pax_headers = {"APK-TOOLS.checksum.SHA1": hashlib.sha1(data).hexdigest()}
            t.addfile(info, io.BytesIO(data))
    raw = buf.getvalue()
    if cut:  # abuild cuts the end-of-archive blocks so members concatenate
        raw = raw.rstrip(b"\0")
        raw += b"\0" * (-len(raw) % 512)
    return raw


def sign(key, data, digest):
    with tempfile.NamedTemporaryFile() as f:
        f.write(data)
        f.flush()
        return subprocess.run(["openssl", "dgst", f"-{digest}", "-sign", key, f.name], check=True, capture_output=True).stdout


def package(key, name, version, files):
    data = gz(tar(files, cut=False, checksums=True))
    info = f"pkgname = {name}\npkgver = {version}\narch = x86_64\nsize = 1\ndatahash = {hashlib.sha256(data).hexdigest()}\n"
    control = gz(tar([(".PKGINFO", info.encode())], cut=True))
    sig = gz(tar([(f".SIGN.RSA.{KEY_NAME}", sign(key, control, "sha1"))], cut=True))
    apk = sig + control + data
    return apk, "Q1" + base64.b64encode(hashlib.sha1(control).digest()).decode()


def index(key, entries, kind, digest):
    text = "\n\n".join(entries) + "\n\n"
    body = gz(tar([("DESCRIPTION", b"fixture"), ("APKINDEX", text.encode())], cut=False))
    sig = gz(tar([(f".SIGN.{kind}.{KEY_NAME}", sign(key, body, digest))], cut=True))
    return sig + body


def main():
    tmp = tempfile.mkdtemp()
    key, other = os.path.join(tmp, "key.pem"), os.path.join(tmp, "other.pem")
    for k in (key, other):
        subprocess.run(["openssl", "genrsa", "-out", k, "2048"], check=True, capture_output=True)
    os.makedirs(os.path.join(OUT, "keys"), exist_ok=True)
    os.makedirs(os.path.join(OUT, "untrusted"), exist_ok=True)
    subprocess.run(["openssl", "rsa", "-in", key, "-pubout", "-out", os.path.join(OUT, "keys", KEY_NAME)], check=True, capture_output=True)
    # A key of the same name that did not sign anything here.
    subprocess.run(["openssl", "rsa", "-in", other, "-pubout", "-out", os.path.join(OUT, "untrusted", KEY_NAME)], check=True, capture_output=True)

    # name, version, index fields beyond the basics
    pkgs = [
        ("hello", "1.0-r0", "D:so:libhello.so.1 cmd:sh !oldhello\n"),
        ("libhello", "1.0-r0", "p:so:libhello.so.1=1\n"),
        ("busybox", "1.37.0-r1", "p:cmd:sh=1.37.0-r1 /bin/sh\nk:100\n"),
        ("toybox", "0.8-r0", "p:cmd:sh=0.8-r0\n"),
        ("hello-doc", "1.0-r0", "i:docs hello=1.0-r0\n"),
        ("hello-bash-completion", "1.0-r0", "i:hello=1.0-r0 bash-completion\n"),
        ("bash-completion", "2.16-r0", ""),
        ("unwanted", "9.9-r0", "D:needs-nothing-here\n"),
    ]
    entries = []
    repo = os.path.join(OUT, "main", "x86_64")
    os.makedirs(repo, exist_ok=True)
    for name, version, extra in pkgs:
        files = [("usr", None), ("usr/share", None), (f"usr/share/{name}", None), (f"usr/share/{name}/README", f"{name} {version}\n".encode())]
        apk, csum = package(key, name, version, files)
        with open(os.path.join(repo, f"{name}-{version}.apk"), "wb") as f:
            f.write(apk)
        entries.append(f"C:{csum}\nP:{name}\nV:{version}\nA:x86_64\nS:{len(apk)}\nI:1\nT:{name}\nU:https://example.org\nL:MIT\n{extra}".rstrip("\n"))
    with open(os.path.join(repo, "APKINDEX.tar.gz"), "wb") as f:
        f.write(index(key, entries, "RSA", "sha1"))
    with open(os.path.join(OUT, "APKINDEX-rsa256.tar.gz"), "wb") as f:
        f.write(index(key, entries[:2], "RSA256", "sha256"))
    print(f"wrote {len(pkgs)} packages and the indexes to {OUT}", file=sys.stderr)


main()
