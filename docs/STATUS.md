# Implementation status

`docs/ARCHITECTURE.md` is the target design. This file says what exists today.

## Built and tested

| Crate | What it does |
|---|---|
| `mer-core` | Digests, snapshot chain roots, entry model, wire structs, path validation |
| `mer-blob` | Content-addressed blob store on a filesystem: verified atomic writes, dedupe, range reads |
| `mer-meta` | SQLite metadata: interval-encoded snapshots, O(delta) diff, publish pointer, GC, retention, misses, hash-chained audit log, channel ledgers |
| `mer-storage` | Facade over blob store and metadata: ingest, snapshot writer, approval gate, publish and rollback, lookup with miss recording, GC with crash recovery, quota, fsck |
| `mer-crypto` | Ed25519 signed envelopes with a k-of-n trust store, diode link MAC |
| `mer-fec` | RaptorQ block encoder and decoder |
| `mer-bundle-format` | CBOR codecs with limits, blob index, Merkle tree, block and slot codec, signed descriptor |
| `mer-bundle` | Collector: delta computation without feedback, seal, sign. Vault: staging, resumable assembly, import, chained per-repo apply |
| `mer-transport-media` | `.mab` volume writer and reader for removable media |
| `mer-transport-diode` | One-way UDP sender (passes, interleaving, pacing) and receiver |
| `mer-fetch` | Upstream access for the collector: HTTP(S) mirror or a local directory, and an HTTP client that only contacts allowlisted hosts, redirects included (followed one by one, never to another host unless it is allowed, never from https to http, without carrying credentials to another host) |
| `mer-format-apt` | Debian/Ubuntu repos: upstream signature check, index parsing, dependency closure, verified download, generation and site-signing of the subset repository |
| `mer-format-rpm` | RPM repositories for yum and dnf: `repomd.xml` checked with `gpgv` against the distribution key, metadata checked against it (gzip, zstd, xz), requirements followed through provides, files and rich dependencies across several repositories, packages verified by SHA-256, upstream's package entries kept byte for byte in the generated subset repository |
| `mer-format-pypi` | Python indexes: PEP 440 versions and specifiers, PEP 508 requirements and markers, wheel tags, dependency following, verified download, static PEP 503 index |
| `mer-format-npm` | npm registries: semver ranges, resolution of dependency ranges to versions, platform filtering, tarballs verified against the registry integrity hash, filtered package documents |
| `mer-format-nuget` | NuGet feeds for .NET: versions and ranges, target-framework selection of dependency groups, lowest-applicable version resolution as `dotnet restore` does it, packages verified against the feed catalog hash, static V3 feed |
| `mer-format-maven` | Maven repositories: exact mirroring of a local repository filled by a build (plugins included), or POM resolution with parents, properties, dependency management, imported BOMs, scopes, optional dependencies, exclusions, ranges and relocations; files checked against the strongest upstream checksum; version and plugin-prefix metadata and `.sha1` files generated for the vault |
| `mer-format-cargo` | Cargo registries for Rust: sparse index, version requirements matched with the `semver` crate Cargo uses, feature-aware following of optional dependencies, exact mirroring of a `Cargo.lock`, crates verified against the index checksum, static sparse index |
| `mer-format-go` | Go modules: module proxy protocol with case escaping, Go version ordering, the module graph followed as minimal version selection sees it (full and pruned), exact mirroring of a `go.sum`, retractions honoured for newest-release lookups, every zip and `go.mod` checked against its `h1:` hash from the checksum database or a `go.sum`, static proxy tree |
| `mer-format-helm` | Helm chart repositories: `index.yaml` read, newest stable or exact chart versions picked, charts verified against the index digest, an index of the subset with chart URLs relative to the vault |
| `mer-format-oci` | Container images: tag resolution, platform selection with attestations, every manifest, config and layer fetched and verified by digest |
| `mer-auth` | Users with roles (viewer, operator, admin), Argon2id passwords, sessions, login lockout, read tokens for package clients |
| `mer-http` | HTTP(S) server: serves the published snapshot of each repo (range and conditional requests, token check for private repos), the client conventions of pip, npm, dotnet, cargo, go, helm and container engines (including the pull side of the registry API under `/v2/`), the web panel under `/ui/` with its JSON API under `/api/`, download statistics, `/metrics`, `/healthz` |
| `mer` | Command-line binary for both roles |

Run all tests with `bash scripts/dcargo.sh test --workspace`. `deploy/install.sh` installs a vault or a collector as a systemd service; it is tested on Debian 12 and Rocky Linux 9. `bash deploy/demo-install.sh` checks it end to end: it installs both sides in two systemd machines, runs the whole pipeline through the two web panels (repository, source, sync, seal, export, import, publish), installs a package with a stock pip over the vault's generated certificate, then upgrades and uninstalls. `--release vX.Y.Z` runs it against a published release instead of this tree. `scripts/release.sh` builds and publishes a release from a tag without the hosted CI runners. Release binaries are static and run on any x86-64 Linux. Three end-to-end demos run in containers:

- `bash deploy/demo.sh`: raw files over removable media and the diode link.
- `bash deploy/demo-apt.sh`: packages mirrored from deb.debian.org, carried across, and installed with a stock `apt-get` on a machine with no internet access.
- `bash deploy/demo-formats.sh` (set `MER_HTTPS_PROXY` if the collector needs a proxy to reach the internet): `requests` and `flask` installed with a stock `pip`, `express` with a stock `npm`, a .NET program using `Serilog` restored, built and run with a stock `dotnet` SDK, a Rust program using `serde` (with derive), `serde_json` and `anyhow` built and run with a stock `cargo`, a Go program using `cobra` and `BurntSushi/toml` tidied, built and run with a stock `go`, the `cert-manager` chart added, pulled and rendered with a stock `helm`, a Java program using `commons-lang3` and `guava` built with a stock `mvn` from what a connected build used plus POM resolution, `tmux` and `jq` installed with a stock `dnf` on Rocky Linux 9 (dependencies from BaseOS and AppStream, signatures checked), all on machines with no internet access; `alpine` pulled and run with a real Docker engine from the vault.

Version and range logic is checked against the reference implementations: apt version ordering against `dpkg`, RPM version ordering against `rpm` itself, Maven version ordering against Maven's `ComparableVersion`, Python versions, specifiers and markers against Python's `packaging` library, npm ranges against the `semver` library npm uses, NuGet versions, ranges and framework selection against `NuGet.Versioning` and `NuGet.Frameworks`, Go version ordering against `golang.org/x/mod/semver` and Go `h1:` hashes against what the `go` command records for real modules. The truth tables are in the repository (`crates/*/tests/`) with the scripts that generate them.

## The web panel

`mer serve` offers the panel at `/ui/`. It is three static files compiled into the binary and loads nothing from anywhere else.

- Overview: repositories, store size, snapshots awaiting review, repositories that are stuck and why, repositories whose content is older than their limit; charts of downloads, data sent, missing items and storage over 24 hours to a year.
- Repositories: contents as packages or files, with search; snapshots with a review of what publishing or rolling back would change (by package, then by file), then approve, reject, publish, roll back; per-repo token requirement, size limit and age limit.
- Transfers: imported bundles and how far each repository has been applied.
- Wishlist with what to add on the collector for each item, as text to download; dismiss and clear. Traffic (charts, per repository, per file, per client), Audit log with its integrity check.
- Access: users and roles, client tokens, own password.
- Jobs (with `mer serve --jobs`): repositories and their sources, sync, seal, export to disk, import from disk, with live job logs and cancel; schedules that sync and seal at set times. Jobs run `mer` itself, one at a time; files are confined to the `--files` directory; editing sources is for admins, running jobs for operators, and both are audited.

Create the first administrator on the server with `mer user add admin --role admin`. Use `--tls-cert` and `--tls-key` for HTTPS, and `--admin-listen` to keep the panel on a different address from package clients.

## Where trust comes from, per format

| Format | What the collector trusts | What it verifies |
|---|---|---|
| apt | The upstream archive signing key | Signature on `InRelease`; every index and package against the signed hashes |
| RPM | The distribution key, through `repomd.xml.asc`, or TLS when no key is given | Every metadata file against `repomd.xml`; every package against its SHA-256 in `primary.xml`. Clients check package signatures with the distribution key |
| PyPI | The TLS connection to the index | Every file against the SHA-256 the index states |
| npm | The TLS connection to the registry | Every tarball against the integrity hash the registry states |
| NuGet | The TLS connection to the feed | Every package against the SHA-512 in the feed catalog, when the feed has one |
| Go | The TLS connections to the module proxy and to the checksum database, or a given `go.sum` | Every zip and `go.mod` against its `h1:` hash. The checksum database's signed tree head and inclusion proofs are not checked |
| Maven | The TLS connection to the repository | Every file against the strongest checksum it serves (SHA-512, SHA-256, else SHA-1); PGP signatures (`.asc`) are not verified |
| Helm | The TLS connection to the chart repository | Every chart against the SHA-256 digest in `index.yaml`, when the index states one |
| Cargo | The TLS connection to the index | Every `.crate` against the SHA-256 in the index, and against the checksum in `Cargo.lock` when one is given |
| Containers | The TLS connection for the tag-to-digest mapping | Every manifest, config and layer against its digest |
| raw | The operator who supplies the files | Nothing upstream |

Upstream signatures are checked for apt and, when a key is given, for RPM repository metadata. Go checksum database proofs, Helm provenance files (`.prov`), PyPI attestations, npm registry signatures, NuGet package signatures and container image signatures (cosign, Notary) are not verified. Whatever the format, content is checked again on the vault against the signed bundle before it is stored.

## How apt works here

1. The collector fetches `InRelease` and verifies it with `gpgv` against the upstream archive keyring. Only text that `gpgv` reports as signed is parsed.
2. Each `Packages.gz` must match the size and SHA-256 in that signed file.
3. The wanted packages plus their `Depends` and `Pre-Depends` (optionally `Recommends`) are selected. Each `.deb` must match the size and SHA-256 in the verified index. Packages already in the store are not downloaded again.
4. The collector writes a complete repository of that subset into one snapshot: `pool/`, `Packages`, `Packages.gz`, by-hash copies, `Release`, `InRelease`, `Release.gpg`, and `keys/site.asc`. The Release is signed with the site key and has no `Valid-Until`.
5. The snapshot crosses the gap in a bundle like any other content. The vault serves the files as they are.
6. Clients use `deb [signed-by=<site key>] http://<vault>:8080/<repo> <suite> <components>`.

## Simpler than the design, on purpose

- All APIs are synchronous (no tokio). The HTTP server is a small thread-per-worker server, and every request does one database lookup under a single lock. Fine for tens of clients, not tuned for hundreds.
- Sign-in is local users only: no LDAP/AD and no OIDC. Permissions are three global roles, not per repository or path.
- Client tokens grant read access to one repository or to all; there are no upload or per-path scopes.
- Request counters on the overview and the per-client table are per server process and reset on restart. Per-file and per-repository download totals are stored.
- TLS is offered by the server itself with a certificate you supply; there is no certificate renewal or client-certificate support.
- apt metadata is generated and signed on the collector and travels as ordinary snapshot content. The design generates and signs it on the vault instead (so the site key never leaves the air gap). Consequence today: the site signing key lives on the internet-side machine.
- Dependency resolution is not a solver in any format. PyPI combines the constraints of all parents and picks one version per project but never backtracks; npm resolves each range on its own, like npm does; apt is described next. When something cannot be satisfied it is reported as a warning and left out.
- PyPI: the upstream index must speak the JSON Simple API (PEP 691). Dependencies come from the metadata the index publishes (PEP 658); a project that offers only a source archive is mirrored without its dependencies, with a warning. Wheel selection matches Python version, architecture and libc family but does not check the glibc version in `manylinux` tags.
- npm: the vault does not answer search or audit requests (`npm install --no-audit`).
- NuGet: the vault offers package download only (the flat container). There is no search and no metadata service, so `dotnet restore` and `dotnet add package X --version V` work, but `dotnet add package X` without a version and IDE package browsing do not. Every (package, range) is resolved to its lowest fitting version and all of them are mirrored, which is a superset of what a restore picks. Framework selection covers modern .NET, .NET Core, .NET Standard and .NET Framework; other frameworks (Xamarin, UAP, portable profiles) are treated as not compatible. Tooling packages a build may pull implicitly (runtime packs for self-contained or cross-platform publish, analyzers, workloads) must be named as packages to mirror.
- Cargo: sparse indexes only (not the git index protocol). The vault offers download only: no `cargo search`, no `cargo publish`, no web API. Dependencies are followed for every target platform, as `cargo vendor` does, so Windows- and macOS-only crates are mirrored too. Each requirement resolves to its newest match on its own, and the features asked of a crate are applied to every mirrored version of it, so whichever version Cargo settles on has its optional dependencies. This is not a solver: a combination of requirements that only an older version satisfies is not found. `--lockfile` sidesteps resolution entirely. Dependencies on other registries are reported and left out.
- Go: the vault serves the module proxy protocol only; it is not a checksum database, so clients set `GOSUMDB=off` (they still check `go.sum`). Zips are mirrored for the versions a build can select (newest in the full graph, newest in the pruned graph, versions roots name), which is an estimate, not a solve; `--gosum` is exact. `replace` directives are not followed. Toolchain downloads (`golang.org/toolchain`) are not mirrored; clients set `GOTOOLCHAIN=local`. Private Go repos need TLS, because `go` sends credentials only over HTTPS. Requests go makes while looking for the module that holds a package (`github.com/owner`, or a package path inside a mirrored module) are not recorded as wishes; other probes for paths that are not modules can still appear on the wishlist.
- RPM: requirements are followed by capability, not solved: version constraints are ignored and the newest package of each name and architecture is kept, one provider per capability. Weak dependencies, modularity metadata (`modules.yaml`), comps groups, updateinfo and delta RPMs are not mirrored. Package entries that point at another server (`xml:base`) are refused. Like apt, dnf resolves against the metadata locally, so a package that is not mirrored never reaches the vault and is not on the wishlist. The generated repository metadata is not signed; clients keep `gpgcheck=1` for package signatures.
- Maven: POM resolution keeps every version it meets rather than mediating to one per artifact (nearest wins), a superset of what a build picks; a consuming project's own dependency management can still choose versions that were not met, which `--from-m2` avoids. Build plugins are not derived from POMs: use `--from-m2`. SNAPSHOT versions, Gradle module metadata (`.module`), profiles and `${env.*}` or system properties in POMs are not handled. Requests for missing POMs and artifacts are wishes; checksum and metadata probes are not.
- Helm: classic chart repositories (`index.yaml`) only; charts in OCI registries are not covered. Charts are picked by exact version or newest stable, not by range. Provenance files are not mirrored, so `helm install --verify` does not work. Helm looks charts up in its local copy of the index, so a chart that is not mirrored never reaches the vault and does not show up on the wishlist; a missing chart archive does. The vault writes its `index.yaml` as JSON, which is valid YAML.
- Containers: pull only, anonymous upstream access only (public images), no referrers API and no `_catalog`. Docker needs the vault on HTTPS or listed under `insecure-registries`.
- apt dependency resolution ignores version constraints and picks the first alternative of an `a | b` dependency. Within a single suite this is almost always right, but it is not a solver. Unsatisfiable hard dependencies are reported as warnings.
- Only `Packages.gz` is read from upstream (not `.xz`), and no source packages, translations or Contents files are mirrored.
- OpenPGP is done by running `gpgv` and `gpg`, so the image contains GnuPG.
- One process per data directory is assumed for writes; the design's split into `serve`, `worker`, `export` and `keyd` processes is not done. `mer serve` and `mer import` can run side by side on one data directory (SQLite arbitrates), as the demos do.
- Blobs are always files; small blobs are not stored inline in SQLite.
- No ingest sessions or leases. An ingested blob that is not yet in a snapshot is protected from GC by the grace period only.
- Bundle trust is a static file of public keys with a threshold. No root keys, no rotation chain, no epochs beyond a fixed epoch 1.
- Bundle payload is not compressed and not encrypted.
- A repo's changes always travel as chained steps. There is no "full state" bundle, so a vault that is further behind than the step window needs the missing bundles re-exported.
- Jobs run one at a time per server. Schedules are daily at a UTC time on chosen days, or every N hours; there are no cron expressions or time zones, and a scheduled run does not write bundles to disk. Diode export and import are command-line only.
- Diode: no heartbeats and no signed staleness detection; no spill-to-disk for partly received segments; plain socket calls with the default receive buffer (the demos show a quarter or more of packets dropped on a burst, which error correction and the second pass absorb).
- Media: no parity volumes.

## Not built yet

- LDAP/AD and OIDC sign-in, and per-repository permissions.
- The signed report file carrying the wishlist and missing blobs back to the collector, and wish ingest on the collector. Misses are recorded and listed (`mer wishlist`), not exported.
- A wishlist for apt: apt never asks the server for a package that is not in the index, so "package not mirrored" does not show up as an HTTP miss. It needs a different signal. (pip, npm and container pulls do land on the wishlist.)
- Verifying the Go checksum database's signed tree and inclusion proofs on the collector, and serving a checksum database from the vault.
- More formats: NuGet symbol packages, Gradle module metadata, Conda, Alpine (apk), Ruby gems, Composer, Hex.
- Pushing or publishing packages to the vault from inside the air gap. Every format is read-only for clients.
- Denylist and taint, S3 backend, Postgres, self-update and vulnerability database extras.
