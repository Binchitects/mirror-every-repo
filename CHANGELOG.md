# Changelog

All notable changes to this project are recorded here. Versions follow [Semantic Versioning](https://semver.org).

## [1.0.0] - 2026-11-12

First release.

### Formats

- **apt** (Debian, Ubuntu): upstream archive signature verified, packages and dependencies selected, a site-signed repository of the subset generated.
- **PyPI**: version, specifier and marker rules as in Python's `packaging`; wheels selected for the configured Python versions and platform.
- **npm**: range resolution as in npm's `semver`, several versions of a package side by side, platform-specific optional packages filtered.
- **NuGet** (.NET): lowest-applicable version resolution and dependency groups per target framework, as `dotnet restore` does it.
- **Container images** (OCI, Docker): chosen platforms of multi-platform images with their attestations; pull side of the registry API.
- **Plain files**: any directory tree.

### Transfer across the gap

- Signed delta bundles computed without any feedback from the air-gapped side.
- Removable media: volumes sized to the medium, readable in any order, resumable, tolerant of damaged sectors.
- Data diode: one-way UDP with RaptorQ error correction and repeat passes, packets authenticated with a link key.
- A lost or late bundle holds back only the repositories it affects; everything catches up when it arrives.

### Vault

- Content-addressed store shared by all formats; immutable snapshots; quarantine, approve or reject, atomic publish, rollback.
- HTTP and HTTPS serving to stock `apt-get`, `pip`, `npm`, `dotnet` and container clients, with resume and conditional requests.
- Web panel: overview, review of what a snapshot would change, approve, publish, roll back, transfers, wishlist, traffic, audit log, users, tokens, client setup lines.
- Access control: users with viewer, operator and admin roles; client tokens for private repositories.
- Quotas, retention, garbage collection, integrity check, hash-chained audit log, Prometheus metrics.

### Known limits

- Upstream signatures are verified for apt only.
- Clients cannot publish to the vault.
- Sign-in is local users only.
- See [docs/STATUS.md](docs/STATUS.md) for the complete list.
