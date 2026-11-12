# Changelog

All notable changes to this project are recorded here. Versions follow [Semantic Versioning](https://semver.org).

## [1.1.0] - unreleased

Five more formats, dashboards, a more capable panel, and sign-in through LDAP. Upgrading from 1.0.0 needs no manual step: the database is upgraded in place on first start (back it up first). The bundle format is unchanged, so a 1.0 vault still imports bundles from a 1.1 collector; serving the new formats needs 1.1 on the vault.

### New formats

Each is tested end to end with its stock client on a machine that can reach only the vault (`deploy/demo-more.sh`).

- **Helm**: chart repositories, charts chosen by name, version or range and verified against the index digest. Charts kept in OCI registries are mirrored as container repositories and pulled with `helm pull oci://`.
- **Cargo** (Rust): the sparse index protocol. Dependencies are followed the way Cargo resolves them, including which optional dependencies each feature turns on; `Cargo.lock` files can be given to mirror exact versions.
- **Go modules**: the GOPROXY protocol. The module graph is built as the `go` command builds it, and every `go.mod` and source archive is compared with the Go checksum database, so clients can run with `GOSUMDB=off`.
- **Maven** (Java and other JVM languages): POM inheritance, properties, dependency management, BOMs, exclusions and version ranges; build plugins are mirrored like any artifact; a real `mvn package` runs from the vault alone.
- **RPM** (dnf, yum): dependency closure across several upstream repositories (for example BaseOS and AppStream), upstream metadata signature verified with the distribution key, packages stored unchanged so their signatures verify on clients, optional site signature on the mirrored metadata.

### Dashboards and panel

- Charts of downloads, data sent, requests for missing items and storage used, over 24 hours, 7 days, 30 days or a year, on the Overview and Traffic pages. History is kept per hour for 400 days.
- Each repository shows how old its published content is, and can be given an age limit after which it is reported as out of date (`mer repo stale <repo> <days>`, or in the panel).
- Browse and search what a repository holds, as packages with their versions or as files.
- The review of a snapshot starts with a summary by package: added, updated, removed.
- The wishlist says what to add to the sync command on the collector for each item, can be downloaded as text, and operators can dismiss items or clear it.

### Access

- **Sign-in through LDAP** (OpenLDAP, Active Directory, FreeIPA): `mer serve --ldap-url ...`. Groups map to the admin, operator and viewer roles. Local users keep working and are checked first, so there is still a way in when the directory is down. Tested against a real directory over StartTLS (`deploy/demo-ldap.sh`).

### Known limits

- RPM module streams are not mirrored; modular packages are left out.
- Maven snapshots and POM profiles are not supported; Gradle resolution is not tested.
- Cargo and Go mirror what a fresh resolve of the named packages needs; projects pinned to older versions need their `Cargo.lock` or `go.mod` given to the sync.
- There is no OIDC sign-in.
- See [docs/STATUS.md](docs/STATUS.md) for the complete list.

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
