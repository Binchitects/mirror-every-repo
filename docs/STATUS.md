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
| `mer-fetch` | Upstream access for the collector: HTTP(S) mirror or a local directory, and an HTTP client that only contacts allowlisted hosts |
| `mer-format-apt` | Debian/Ubuntu repos: upstream signature check, index parsing, dependency closure, verified download, generation and site-signing of the subset repository |
| `mer-format-pypi` | Python indexes: PEP 440 versions and specifiers, PEP 508 requirements and markers, wheel tags, dependency following, verified download, static PEP 503 index |
| `mer-format-npm` | npm registries: semver ranges, resolution of dependency ranges to versions, platform filtering, tarballs verified against the registry integrity hash, filtered package documents |
| `mer-format-nuget` | NuGet feeds for .NET: versions and ranges, target-framework selection of dependency groups, lowest-applicable version resolution as `dotnet restore` does it, packages verified against the feed catalog hash, static V3 feed |
| `mer-format-oci` | Container images: tag resolution, platform selection with attestations, every manifest, config and layer fetched and verified by digest |
| `mer-auth` | Users with roles (viewer, operator, admin), Argon2id passwords, sessions, login lockout, read tokens for package clients |
| `mer-http` | HTTP(S) server: serves the published snapshot of each repo (range and conditional requests, token check for private repos), the client conventions of pip, npm, dotnet and container engines (including the pull side of the registry API under `/v2/`), the web panel under `/ui/` with its JSON API under `/api/`, download statistics, `/metrics`, `/healthz` |
| `mer` | Command-line binary for both roles |

Run all tests with `bash scripts/dcargo.sh test --workspace`. Three end-to-end demos run in containers:

- `bash deploy/demo.sh`: raw files over removable media and the diode link.
- `bash deploy/demo-apt.sh`: packages mirrored from deb.debian.org, carried across, and installed with a stock `apt-get` on a machine with no internet access.
- `bash deploy/demo-formats.sh`: `requests` and `flask` installed with a stock `pip`, `express` with a stock `npm`, a .NET program using `Serilog` restored, built and run with a stock `dotnet` SDK, all on machines with no internet access; `alpine` pulled and run with a real Docker engine from the vault.

Version and range logic is checked against the reference implementations: apt version ordering against `dpkg`, Python versions, specifiers and markers against Python's `packaging` library, npm ranges against the `semver` library npm uses, NuGet versions, ranges and framework selection against `NuGet.Versioning` and `NuGet.Frameworks`. The truth tables are in the repository (`crates/*/tests/`) with the scripts that generate them.

## The web panel

`mer serve` offers the panel at `/ui/`. It is three static files compiled into the binary and loads nothing from anywhere else.

- Overview: repositories, store size, snapshots awaiting review, repositories that are stuck and why.
- Repositories: snapshots with a review of what publishing or rolling back would change, then approve, reject, publish, roll back; per-repo token requirement and size limit.
- Transfers: imported bundles and how far each repository has been applied.
- Wishlist, Traffic (per repository, per file, per client), Audit log with its integrity check.
- Access: users and roles, client tokens, own password.

Create the first administrator on the server with `mer user add admin --role admin`. Use `--tls-cert` and `--tls-key` for HTTPS, and `--admin-listen` to keep the panel on a different address from package clients.

## Where trust comes from, per format

| Format | What the collector trusts | What it verifies |
|---|---|---|
| apt | The upstream archive signing key | Signature on `InRelease`; every index and package against the signed hashes |
| PyPI | The TLS connection to the index | Every file against the SHA-256 the index states |
| npm | The TLS connection to the registry | Every tarball against the integrity hash the registry states |
| NuGet | The TLS connection to the feed | Every package against the SHA-512 in the feed catalog, when the feed has one |
| Containers | The TLS connection for the tag-to-digest mapping | Every manifest, config and layer against its digest |
| raw | The operator who supplies the files | Nothing upstream |

Only apt has an upstream signature that is checked. PyPI attestations, npm registry signatures, NuGet package signatures and container image signatures (cosign, Notary) are not verified. Whatever the format, content is checked again on the vault against the signed bundle before it is stored.

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
- Diode: no heartbeats and no signed staleness detection; no spill-to-disk for partly received segments; plain socket calls with the default receive buffer (the demos show a quarter or more of packets dropped on a burst, which error correction and the second pass absorb).
- Media: no parity volumes.

## Not built yet

- LDAP/AD and OIDC sign-in, and per-repository permissions.
- Running a sync or an export from the panel. The panel reviews and decides; mirroring, sealing and importing are command-line operations.
- The signed report file carrying the wishlist and missing blobs back to the collector, and wish ingest on the collector. Misses are recorded and listed (`mer wishlist`), not exported.
- A wishlist for apt: apt never asks the server for a package that is not in the index, so "package not mirrored" does not show up as an HTTP miss. It needs a different signal. (pip, npm and container pulls do land on the wishlist.)
- More formats: yum/dnf, Go modules, Cargo, Maven, Helm, NuGet symbol packages.
- Pushing or publishing packages to the vault from inside the air gap. Every format is read-only for clients.
- Denylist and taint, S3 backend, Postgres, self-update and vulnerability database extras.
