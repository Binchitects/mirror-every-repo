# Security policy

`mirror-every-repo` sits on a trust boundary: it decides what software enters an isolated network. Reports of security problems are taken seriously.

## Reporting a vulnerability

Please do not open a public issue for a security problem.

Use GitHub's private reporting instead: on the repository page, go to **Security**, then **Report a vulnerability**
([direct link](https://github.com/Binchitects/mirror-every-repo/security/advisories/new)). Include what you found, how to reproduce it, and which version you tested.

You should receive an acknowledgement within a few working days. Once a fix is available it is released, and the advisory is published with credit to the reporter unless you prefer otherwise.

## Supported versions

Security fixes are made for the latest released version.

## What is in scope

Especially interesting are ways to:

- get content onto the vault that the collector did not sign, or that differs from what it signed;
- have content served to clients before it was approved and published, or after it was rolled back;
- make the vault accept a replayed, reordered or forked bundle;
- read or write outside the data directory through a crafted path, package name or archive;
- bypass sign-in, roles, the CSRF check or the token check on private repositories;
- alter the audit log without the integrity check noticing;
- crash or exhaust the vault with crafted bundles, media volumes or diode packets.

## Known limits that are not vulnerabilities

These are documented design limits (see [docs/STATUS.md](docs/STATUS.md)):

- Upstream signatures are verified for apt only. For PyPI, npm, NuGet and container images, the collector trusts its TLS connection to the upstream and the hashes that upstream states.
- The apt site signing key lives on the collector.
- Anyone with shell access to a host running `mer` has full control of that side.
- The diode link key authenticates packets but does not encrypt them, and bundles are not encrypted.
