# Implementation status

`docs/ARCHITECTURE.md` is the target design. This file says what exists today.

## Built and tested

| Crate | What it does |
|---|---|
| `mer-core` | Digests, snapshot chain roots, entry model, wire structs, path validation |
| `mer-blob` | Content-addressed blob store on a filesystem: verified atomic writes, dedupe, range reads |
| `mer-meta` | SQLite metadata: interval-encoded snapshots, O(delta) diff, publish pointer, GC, retention, misses, hash-chained audit log, channel ledgers, hourly history for the charts |
| `mer-storage` | Facade over blob store and metadata: ingest, snapshot writer, approval gate, publish and rollback, lookup with miss recording, GC with crash recovery, quota, fsck |
| `mer-crypto` | Ed25519 signed envelopes with a k-of-n trust store, diode link MAC |
| `mer-fec` | RaptorQ block encoder and decoder |
| `mer-bundle-format` | CBOR codecs with limits, blob index, Merkle tree, block and slot codec, signed descriptor |
| `mer-bundle` | Collector: delta computation without feedback, seal, sign. Vault: staging, resumable assembly, import, chained per-repo apply |
| `mer-transport-media` | `.mab` volume writer and reader for removable media |
| `mer-transport-diode` | One-way UDP sender (passes, interleaving, pacing) and receiver |
| `mer-fetch` | Upstream access for the collector: HTTP(S) mirror or a local directory, and an HTTP client that only contacts allowlisted hosts |
| `mer-format-apt` | Debian/Ubuntu repos: upstream signature check, index parsing, dependency closure, verified download, generation and site-signing of the subset repository |
| `mer-format-pypi` | Python indexes: PEP 440 versions and specifiers, PEP 508 requirements and markers, wheel tags, dependency following, verified download, static PEP 503 index |
| `mer-format-npm` | npm registries: semver ranges, resolution of dependency ranges to versions, platform filtering, tarballs verified against the registry integrity hash, filtered package documents |
| `mer-format-nuget` | NuGet feeds for .NET: versions and ranges, target-framework selection of dependency groups, lowest-applicable version resolution as `dotnet restore` does it, packages verified against the feed catalog hash, static V3 feed |
| `mer-format-oci` | Container images: tag resolution, platform selection with attestations, every manifest, config and layer fetched and verified by digest |
| `mer-format-helm` | Helm chart repositories: charts chosen by name, version or range, verified against the SHA-256 in the upstream index, an index of what was mirrored. (Charts in OCI registries go through `mer-format-oci`.) |
| `mer-format-cargo` | Cargo registries (sparse index): Cargo's own version rules, normal and build dependencies, optional dependencies only where a feature turns them on, exact versions from `Cargo.lock` files, crates verified against the index checksum |
| `mer-format-go` | Go modules (GOPROXY protocol): the module graph the `go` command would load (with graph pruning), minimal version selection, every `go.mod` and source archive compared with the Go checksum database |
| `mer-format-maven` | Maven repositories: POM inheritance, properties, dependency management, imported BOMs, exclusions, relocations, version ranges; compile and runtime dependencies; build plugins; artifacts verified against the upstream SHA-1; checksum files and `maven-metadata.xml` generated |
| `mer-format-rpm` | RPM repositories for dnf and yum: package lists read as a stream, dependency closure over one or more upstream repositories (provides with versions, file and boolean dependencies), packages verified against the package list, metadata of the subset generated, upstream metadata signature verified, optional site signature |
| `mer-auth` | Users with roles (viewer, operator, admin), Argon2id passwords, sessions, login lockout, read tokens for package clients, sign-in through a directory |
| `mer-ldap` | LDAP directory client for sign-in: search, bind as the user, groups to roles |
| `mer-http` | HTTP(S) server: serves the published snapshot of each repo (range and conditional requests, token check for private repos), the client conventions of pip, npm, dotnet, cargo, go, helm, mvn, dnf and container engines (including the pull side of the registry API under `/v2/`), the web panel under `/ui/` with its JSON API under `/api/`, download statistics and history, `/metrics`, `/healthz` |
| `mer` | Command-line binary for both roles |

Run all tests with `bash scripts/dcargo.sh test --workspace`. Five end-to-end demos run in containers:

- `bash deploy/demo.sh`: raw files over removable media and the diode link.
- `bash deploy/demo-apt.sh`: packages mirrored from deb.debian.org, carried across, and installed with a stock `apt-get` on a machine with no internet access.
- `bash deploy/demo-formats.sh`: `requests` and `flask` installed with a stock `pip`, `express` with a stock `npm`, a .NET program using `Serilog` restored, built and run with a stock `dotnet` SDK, all on machines with no internet access; `alpine` pulled and run with a real Docker engine from the vault.
- `bash deploy/demo-more.sh [helm cargo go maven rpm]`: charts pulled and rendered with a stock `helm` (from a chart repository and from an OCI registry); a Rust program with a derive macro built with a stock `cargo`; a Go program using `gin` built with a stock `go`, then `go mod verify`; a Java program built with a stock `mvn package`, libraries and build plugins all from the vault; `jq` installed on Rocky Linux with a stock `dnf`, package and metadata signatures checked. Each section can be run on its own.
- `bash deploy/demo-ldap.sh`: panel sign-in against a real OpenLDAP server over StartTLS with a private CA: roles from groups, refusals, a role change, the directory going down.

Version and range logic is checked against the reference implementations: apt version ordering against `dpkg`, Python versions, specifiers and markers against Python's `packaging` library, npm ranges against the `semver` library npm uses, NuGet versions, ranges and framework selection against `NuGet.Versioning` and `NuGet.Frameworks`, Maven version ordering against Maven's `ComparableVersion` (5,041 pairs), RPM version comparison against `rpm` (4,624 pairs). Go's `h1:` checksums are checked against real files and the values the Go checksum database publishes for them. Cargo versions and requirements use the `semver` crate, which is the one Cargo uses. The truth tables are in the repository (`crates/*/tests/`) with the scripts that generate them.

## The web panel

`mer serve` offers the panel at `/ui/`. It is three static files compiled into the binary and loads nothing from anywhere else.

- Overview: repositories, store size, snapshots awaiting review, repositories that are stuck and why; charts of downloads, data sent, requests for missing items and storage used over 24 hours, 7 days, 30 days or a year; how old the published content of each repository is, with a warning past a limit you set per repository.
- Repositories: snapshots with a review of what publishing or rolling back would change, summarised by package (added, updated, removed) and listed by file, then approve, reject, publish, roll back; browsing and searching the contents as packages with their versions or as files; client setup lines; per-repo token requirement, size limit and age limit.
- Transfers: imported bundles and how far each repository has been applied.
- Wishlist: what was asked for and is missing, with what to add to the sync command on the collector; dismiss, clear, download as text.
- Traffic: charts over the chosen period, per repository with a trend line, most downloaded files, clients.
- Audit log with its integrity check.
- Access: users and roles (local and from the directory), client tokens, own password.

The charts are drawn by the panel itself (no chart library). Each can be read with the keyboard and opened as a table.

Create the first administrator on the server with `mer user add admin --role admin`. Use `--tls-cert` and `--tls-key` for HTTPS, and `--admin-listen` to keep the panel on a different address from package clients.

## Where trust comes from, per format

| Format | What the collector trusts | What it verifies |
|---|---|---|
| apt | The upstream archive signing key | Signature on `InRelease`; every index and package against the signed hashes |
| PyPI | The TLS connection to the index | Every file against the SHA-256 the index states |
| npm | The TLS connection to the registry | Every tarball against the integrity hash the registry states |
| NuGet | The TLS connection to the feed | Every package against the SHA-512 in the feed catalog, when the feed has one |
| Containers | The TLS connection for the tag-to-digest mapping | Every manifest, config and layer against its digest |
| Helm | The TLS connection to the chart repository | Every chart against the SHA-256 in the repository index |
| Cargo | The TLS connection to the index | Every crate against the SHA-256 in the index (what Cargo checks) |
| Go | The TLS connections to the proxy and to the checksum database | Every `go.mod` and source archive against the hash the checksum database gives |
| Maven | The TLS connection to the repository | Every file against the SHA-1 published next to it (what Maven checks) |
| RPM | With `--keyring`: the distribution's signing key. Without: the TLS connection | Signature on `repomd.xml` (with `--keyring`); the package list against it; every package against the package list. Packages keep their own signatures, which clients check |
| raw | The operator who supplies the files | Nothing upstream |

Upstream signatures are checked for apt and, with `--keyring`, for RPM metadata. For Go, the answers of the checksum database are used as they come over TLS; its transparency-log proofs are not verified. PyPI attestations, npm registry signatures, NuGet package signatures, Maven `.asc` signatures, Helm provenance files and container image signatures (cosign, Notary) are not verified. Whatever the format, content is checked again on the vault against the signed bundle before it is stored.

## How apt works here

1. The collector fetches `InRelease` and verifies it with `gpgv` against the upstream archive keyring. Only text that `gpgv` reports as signed is parsed.
2. Each `Packages.gz` must match the size and SHA-256 in that signed file.
3. The wanted packages plus their `Depends` and `Pre-Depends` (optionally `Recommends`) are selected. Each `.deb` must match the size and SHA-256 in the verified index. Packages already in the store are not downloaded again.
4. The collector writes a complete repository of that subset into one snapshot: `pool/`, `Packages`, `Packages.gz`, by-hash copies, `Release`, `InRelease`, `Release.gpg`, and `keys/site.asc`. The Release is signed with the site key and has no `Valid-Until`.
5. The snapshot crosses the gap in a bundle like any other content. The vault serves the files as they are.
6. Clients use `deb [signed-by=<site key>] http://<vault>:8080/<repo> <suite> <components>`.

## Simpler than the design, on purpose

- All APIs are synchronous (no tokio). The HTTP server is a small thread-per-worker server, and every request does one database lookup under a single lock. Fine for tens of clients, not tuned for hundreds.
- Sign-in is local users, and optionally an LDAP directory (also Active Directory through LDAP). There is no OIDC. Permissions are three global roles, not per repository or path.
- LDAP: a user must be in a group that grants a role; nested groups are not followed unless the directory resolves them itself (`memberOf`). The role is read at sign-in, so a change in the directory takes effect at the next sign-in; a session that is already open lasts up to 12 hours (disable the user in the panel to end it at once). User names are lower-case letters, digits, `.`, `_` and `-`. A private CA is given through the `SSL_CERT_FILE` environment variable; there are no client certificates.
- Dashboards: history is kept per hour for 400 days, in UTC. Storage sizes are sampled every ten minutes while the server runs, so a period in which no server ran shows the last known size. Per-client figures are still per server process.
- Client tokens grant read access to one repository or to all; there are no upload or per-path scopes.
- Request counters on the overview and the per-client table are per server process and reset on restart. Per-file and per-repository download totals are stored.
- TLS is offered by the server itself with a certificate you supply; there is no certificate renewal or client-certificate support.
- apt metadata is generated and signed on the collector and travels as ordinary snapshot content. The design generates and signs it on the vault instead (so the site key never leaves the air gap). Consequence today: the site signing key lives on the internet-side machine.
- Dependency resolution is not a solver in any format. PyPI combines the constraints of all parents and picks one version per project but never backtracks; npm resolves each range on its own, like npm does; apt is described next. When something cannot be satisfied it is reported as a warning and left out.
- PyPI: the upstream index must speak the JSON Simple API (PEP 691). Dependencies come from the metadata the index publishes (PEP 658); a project that offers only a source archive is mirrored without its dependencies, with a warning. Wheel selection matches Python version, architecture and libc family but does not check the glibc version in `manylinux` tags.
- npm: the vault does not answer search or audit requests (`npm install --no-audit`).
- NuGet: the vault offers package download only (the flat container). There is no search and no metadata service, so `dotnet restore` and `dotnet add package X --version V` work, but `dotnet add package X` without a version and IDE package browsing do not. Every (package, range) is resolved to its lowest fitting version and all of them are mirrored, which is a superset of what a restore picks. Framework selection covers modern .NET, .NET Core, .NET Standard and .NET Framework; other frameworks (Xamarin, UAP, portable profiles) are treated as not compatible. Tooling packages a build may pull implicitly (runtime packs for self-contained or cross-platform publish, analyzers, workloads) must be named as packages to mirror.
- Containers: pull only, anonymous upstream access only (public images), no referrers API and no `_catalog`. Docker needs the vault on HTTPS or listed under `insecure-registries`.
- Helm: one version per chart name and requirement is mirrored. Provenance (`.prov`) files are not mirrored. `helm` works out from the index what exists, so a chart version that is not mirrored shows up on the wishlist only when its archive is asked for directly.
- Cargo: the crates you name are mirrored with all their features on (or only the default ones, with `--default-features-only` and `--feature`); their dependencies with the features asked of them. Dependencies for every target platform are followed. Each requirement resolves to its highest matching version, which is what a fresh `cargo` resolve picks; a project with an older `Cargo.lock` needs that file given to the sync (`--lockfile`). Publishing, `cargo search` and the web API are not offered. Git dependencies and other registries are not followed.
- Go: versions are selected for a project that requires the named modules (each alone, and all together), or for the `go.mod` files you give. A project that combines them with other requirements can end up needing other versions; those show on the wishlist. Clients run with `GOSUMDB=off`, because the collector made that check. `replace` and `exclude` directives of dependencies are ignored, as Go does. No `sumdb` proxying, no `vcs` fallback.
- Maven: every version of an artifact that is reached is mirrored, so the one Maven settles on is there; this is more than one build needs. Scopes followed are compile and runtime; optional dependencies are not. Each named artifact is walked both as a dependency and as the root of its own resolution (which is how build plugins are resolved). Build plugins, and what test runners fetch at run time (the surefire providers, for example), have to be named as artifacts. Profiles in POMs are not evaluated, and properties that come from the environment are not known. Snapshots (`-SNAPSHOT` versions with timestamped files) are not supported. Plugin-prefix metadata is not generated, so plugins need to be named with a version in the project. Gradle works for libraries published with a POM; Gradle module metadata is mirrored where the POM announces it, but Gradle resolution is not tested, and the Gradle plugin portal is not a Maven repository.
- RPM: the newest package of each name is taken (for a versioned requirement, the newest that satisfies it). Module streams (`modules.yaml`) are not mirrored and modular packages are left out. Groups (`comps`), update information (`updateinfo`) and delta RPMs are not mirrored. `dnf` learns from the metadata what exists, so RPM requests never land on the wishlist. Package signatures are not checked on the collector; the client does that (`gpgcheck=1`). Metadata compressed with gzip or zstd is read; xz is not. The site key that signs the metadata lives on the collector, as with apt.
- apt dependency resolution ignores version constraints and picks the first alternative of an `a | b` dependency. Within a single suite this is almost always right, but it is not a solver. Unsatisfiable hard dependencies are reported as warnings.
- Only `Packages.gz` is read from upstream (not `.xz`), and no source packages, translations or Contents files are mirrored.
- OpenPGP is done by running `gpgv` and `gpg`, so the image contains GnuPG.
- One process per data directory is assumed for writes; the design's split into `serve`, `worker`, `export` and `keyd` processes is not done. `mer serve` and `mer import` can run side by side on one data directory (SQLite arbitrates), as the demos do.
- Blobs are always files; small blobs are not stored inline in SQLite.
- No ingest sessions or leases. An ingested blob that is not yet in a snapshot is protected from GC by the grace period only.
- Bundle trust is a static file of public keys with a threshold. No root keys, no rotation chain, no epochs beyond a fixed epoch 1.
- Bundle payload is not compressed and not encrypted.
- A repo's changes always travel as chained steps. There is no "full state" bundle, so a vault that is further behind than the step window needs the missing bundles re-exported.
- Diode: no heartbeats and no signed staleness detection; no spill-to-disk for partly received segments; plain socket calls with the default receive buffer (the demos show a quarter or more of packets dropped on a burst, which error correction and the second pass absorb).
- Media: no parity volumes.

## Not built yet

- OIDC sign-in, and per-repository permissions.
- Running a sync or an export from the panel. The panel reviews and decides; mirroring, sealing and importing are command-line operations.
- The signed report file carrying the wishlist and missing blobs back to the collector, and wish ingest on the collector. The wishlist can be downloaded from the panel as text that says what to add to each sync command; applying it is a manual step.
- A wishlist for apt and RPM: these clients never ask the server for a package that is not in the index, so "package not mirrored" does not show up as an HTTP miss. It needs a different signal. (pip, npm, dotnet, cargo, go, mvn and container pulls do land on the wishlist.)
- More formats: RubyGems, Composer, Conda, Terraform providers, RPM modules, NuGet symbol packages.
- Pushing or publishing packages to the vault from inside the air gap. Every format is read-only for clients.
- Denylist and taint, S3 backend, Postgres, self-update and vulnerability database extras.
