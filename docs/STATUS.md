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
| `mer-fetch` | Upstream access for the collector: HTTP(S) mirror or a local directory |
| `mer-format-apt` | Debian/Ubuntu repos: upstream signature check, index parsing, dependency closure, verified download, generation and site-signing of the subset repository |
| `mer-auth` | Users with roles (viewer, operator, admin), Argon2id passwords, sessions, login lockout, read tokens for package clients |
| `mer-http` | HTTP(S) server: serves the published snapshot of each repo (range and conditional requests, token check for private repos), the web panel under `/ui/` with its JSON API under `/api/`, download statistics, `/metrics`, `/healthz` |
| `mer` | Command-line binary for both roles |

Run all tests with `bash scripts/dcargo.sh test --workspace`. Two end-to-end demos run in containers:

- `bash deploy/demo.sh`: raw files over removable media and the diode link.
- `bash deploy/demo-apt.sh`: packages mirrored from deb.debian.org, carried across, and installed with a stock `apt-get` on a machine with no internet access.

## The web panel

`mer serve` offers the panel at `/ui/`. It is three static files compiled into the binary and loads nothing from anywhere else.

- Overview: repositories, store size, snapshots awaiting review, repositories that are stuck and why.
- Repositories: snapshots with a review of what publishing or rolling back would change, then approve, reject, publish, roll back; per-repo token requirement and size limit.
- Transfers: imported bundles and how far each repository has been applied.
- Wishlist, Traffic (per repository, per file, per client), Audit log with its integrity check.
- Access: users and roles, client tokens, own password.

Create the first administrator on the server with `mer user add admin --role admin`. Use `--tls-cert` and `--tls-key` for HTTPS, and `--admin-listen` to keep the panel on a different address from package clients.

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

- Package formats: PyPI, npm, OCI.
- LDAP/AD and OIDC sign-in, and per-repository permissions.
- Running a sync or an export from the panel. The panel reviews and decides; mirroring, sealing and importing are command-line operations.
- The signed report file carrying the wishlist and missing blobs back to the collector, and wish ingest on the collector. Misses are recorded and listed (`mer wishlist`), not exported.
- A wishlist for apt: apt never asks the server for a package that is not in the index, so "package not mirrored" does not show up as an HTTP miss. It needs a different signal.
- Denylist and taint, S3 backend, Postgres, self-update and vulnerability database extras.
