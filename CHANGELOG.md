# Changelog

All notable changes to this project are recorded here. Versions follow [Semantic Versioning](https://semver.org).

## [Unreleased]

### Formats

- **Helm charts**: charts mirrored from a chart repository at their newest stable or an exact version, each checked against the digest in the upstream `index.yaml`. Stock `helm` adds the vault as a repository, pulls and installs from it. `mer helm sync`.

### Collector

- Redirects from upstreams are now held to the host allowlist. Before, the HTTP library followed them to any host. Each hop is checked; a redirect to a host that is not allowed fails with its name and the `--allow-host` to add, https-to-http redirects are refused, and credentials are not sent on to another host. The blob hosts of Docker Hub, quay.io and ghcr.io, and GitHub's release asset hosts, are allowed by default. An apt upstream follows redirects within its own host only.

## [1.1.0] - 2026-11-26

### Formats

- **Cargo** (Rust): crates mirrored from a sparse index (crates.io by default) with their dependencies, optional dependencies followed only when a feature turns them on, or exactly the versions a `Cargo.lock` names. Every `.crate` is checked against the index checksum. Stock `cargo` builds against the vault with source replacement. `mer cargo sync`.
- **Go modules**: modules mirrored from a module proxy (proxy.golang.org by default) with what minimal version selection can choose, or exactly what a `go.sum` lists. Every zip and `go.mod` is checked against its `h1:` hash from sum.golang.org or the `go.sum`. Stock `go` tidies and builds against the vault as `GOPROXY`. `mer go sync`.

### Vault

- Go clients probing package path prefixes (`github.com/owner`) are not recorded on the wishlist.
- Client tokens are also accepted on their own in the `Authorization` header, which is how Cargo sends them.

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
