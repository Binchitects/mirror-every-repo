"""Generate tests/packaging_truth.json from Python's reference implementation.

Run inside an official Python image (pip vendors the `packaging` library):

    docker run --rm -v "$PWD/crates/mer-format-pypi/tests:/t" python:3.12-slim python /t/gen_fixture.py

The Rust test `agrees_with_python_packaging` must give the same answers.
"""
import json
import pathlib

from pip._vendor.packaging.markers import Marker
from pip._vendor.packaging.specifiers import SpecifierSet
from pip._vendor.packaging.version import InvalidVersion, Version

VERSIONS = [
    "0", "0.1", "1", "1.0", "1.0.0", "1.0.1", "1.1", "1.2.3", "1.10", "1.9", "2", "2.0", "2.0.0.0", "3.0.0",
    "1.0a1", "1.0a2", "1.0b1", "1.0rc1", "1.0rc2", "1.0.dev1", "1.0.dev2", "1.0a1.dev1", "1.0.post1",
    "1.0.post1.dev2", "1.0.post2", "1.0a1.post1", "1!0.5", "1!1.0", "1.0+local", "1.0+local.2", "1.0+abc",
    "1.0+1", "1.0+10", "1.0+2.abc", "v1.5", "1.5alpha3", "1.5-beta.2", "1.5c1", "1.5.rev4", "1.5-7", "1.5pre1",
    "1.5.preview2", "2.0.0a0", "2.0.0.dev0", "2.0.0rc1.post1", "2024.1.1", "2024.10.2", "0.0.1.dev0",
    "1.0-dev", "1.0.post", "1.0a", "1.4", "1.4.2", "1.4.5", "1.4.9", "1.5.0", "2.2", "2.9", "2.1", "1.7",
    "1.7.1", "1.7.post1", "1.7.post3", "1.7+local", "3.11", "3.9", "3.10",
    # not versions
    "", "abc", "1.0.", "1..0", "1.0+", "1.0 beta", "!1", "1.0-", "1.x", "1.0+a b", "1.0.*",
]

SPECIFIERS = [
    "", ">=1.0", ">1.0", "<1.0", "<=1.0", "==1.0", "!=1.0", "==1.0.*", "!=1.0.*", "==1.*", "~=1.0", "~=1.4.5",
    "~=2.2", ">=1.0,<2", ">=1.0,<2.0.0", ">1.7", ">1.7.post2", "<2", "<2.0.0a0", "<1.0rc1", ">=1.0a1",
    "==1.0+local", "!=1.0+local", "===1.0", ">=1!0", "<=1.5", ">=1.4,!=1.4.5,<1.5", "==2.0", "<=2.0.0rc1",
    ">1.0.post1", ">=1.0.post1", "<1.0.post2", "==1.0.post1", ">=2024.1", "~=2024.1.0", ">=0", "<1!0",
    "==1.4.*,!=1.4.2", ">=1.0.dev1", "<1.0.dev2", ">1.0a1", "<1.0a2", "==1.0a1", "~=1.0a1.post0",
]

ENV = {
    "os_name": "posix", "sys_platform": "linux", "platform_system": "Linux", "platform_machine": "x86_64",
    "platform_python_implementation": "CPython", "implementation_name": "cpython", "platform_release": "",
    "platform_version": "", "python_version": "3.11", "python_full_version": "3.11.4",
    "implementation_version": "3.11.4", "extra": "",
}

MARKERS = [
    "python_version >= '3.8'", "python_version < '3.8'", "python_version == '3.11'", "python_version != '3.11'",
    "python_version > '3.9'", "python_version >= '3.10'", "python_version < '3.10'", "python_version ~= '3.11'",
    "python_version ~= '3.9'", "python_full_version >= '3.11.0'", "python_full_version < '3.11.4'",
    "python_full_version <= '3.11.4'", "python_full_version > '3.11.4rc1'", "python_full_version == '3.11.*'",
    "sys_platform == 'linux'", "sys_platform == 'win32'", "sys_platform != 'win32'", "sys_platform == 'darwin'",
    "os_name == 'posix'", "os_name == 'nt'", "platform_system == 'Linux'", "platform_system == 'Windows'",
    "platform_machine == 'x86_64'", "platform_machine != 'aarch64'", "platform_machine == 'arm64'",
    "implementation_name == 'cpython'", "implementation_name == 'pypy'",
    "platform_python_implementation == 'CPython'", "platform_python_implementation != 'PyPy'",
    "'linux' in sys_platform", "'win' in sys_platform", "sys_platform not in 'win32 cygwin'",
    "sys_platform in 'linux darwin'", "'x86' in platform_machine", "platform_machine in 'x86_64 amd64 AMD64'",
    "extra == 'test'", "extra == ''", "extra != 'test'",
    "python_version >= '3.8' and sys_platform == 'linux'", "python_version < '3.8' and sys_platform == 'linux'",
    "python_version < '3.8' or sys_platform == 'linux'", "python_version < '3.8' or sys_platform == 'win32'",
    "(python_version < '3' or os_name == 'posix') and platform_machine == 'x86_64'",
    "python_version < '3' or (os_name == 'posix' and platform_machine == 'aarch64')",
    "python_version >= '3.10' and python_version < '4'", "python_version >= '3.6' and python_version < '3.10'",
    "os_name == 'nt' and python_version >= '3' or sys_platform == 'linux'",
    "os_name == 'nt' and (python_version >= '3' or sys_platform == 'linux')",
    "python_version >= '3' and extra == 'socks'", "extra == 'socks' or extra == 'security'",
    "python_version in '3.10 3.11 3.12'", "python_version not in '3.7 3.8'",
    "implementation_version >= '3.11'", "platform_release == ''", "os.name == 'posix'",
    "sys_platform == 'linux' and python_version >= '3.7' and platform_machine != 'i686'",
    "sys_platform == \"linux\" or sys_platform == \"darwin\"",
    "python_version >= \"3.8\" and python_version < \"3.13\"",
    "platform_system != \"Darwin\" and platform_system != \"Windows\"",
    "sys_platform >= 'linux'", "sys_platform < 'win32'", "python_version <= '3.11'", "python_version === '3.11'",
]


def parse(text):
    try:
        return Version(text)
    except InvalidVersion:
        return None


def main():
    versions = {t: parse(t) for t in VERSIONS}
    valid = [t for t, v in versions.items() if v is not None]
    order = {}
    for a in valid:
        for b in valid:
            va, vb = versions[a], versions[b]
            order[f"{a} {b}"] = -1 if va < vb else (1 if va > vb else 0)
    matches = {}
    for spec in SPECIFIERS:
        s = SpecifierSet(spec)
        matches[spec] = [t for t in valid if s.contains(versions[t], prereleases=True)]
    markers = {}
    for m in MARKERS:
        try:
            markers[m] = bool(Marker(m).evaluate(environment=dict(ENV)))
        except Exception as e:  # noqa: BLE001 - recorded, the Rust side may also refuse it
            markers[m] = None
    out = {
        "valid": valid,
        "invalid": [t for t, v in versions.items() if v is None],
        "prerelease": [t for t in valid if versions[t].is_prerelease],
        "order": order,
        "matches": matches,
        "env": ENV,
        "markers": markers,
    }
    path = pathlib.Path(__file__).with_name("packaging_truth.json")
    path.write_text(json.dumps(out, indent=1, sort_keys=True) + "\n")
    import pip._vendor.packaging as p
    print(f"packaging {p.__version__}: {len(valid)} versions, {len(order)} pairs, "
          f"{sum(len(v) for v in matches.values())} matches over {len(SPECIFIERS)} specifiers, {len(markers)} markers")


main()
