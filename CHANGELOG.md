# Changelog

All notable changes to this project are recorded here. Versions follow [Semantic Versioning](https://semver.org).

## [1.4.0] - 2026-11-27

### Formats

- **Composer** (PHP): packages mirrored from packagist.org with what they require, for the PHP inside the air gap, or exactly what a `composer.lock` names. The vault serves metadata of exactly what is mirrored, pointing at its own archives with the SHA-1 Composer checks. Stock `composer require` and `composer update` use the vault; packages that are not mirrored land on the wishlist. `mer composer sync`.
- **RubyGems**: gems mirrored from rubygems.org with their dependencies, for the Ruby and platforms inside the air gap, or exactly what a `Gemfile.lock` names; every `.gem` checked against the SHA-256 in the compact index. Stock `gem install` and `bundle install` use the vault, precompiled builds (nokogiri) included. Gems that are not mirrored land on the wishlist. `mer gem sync`.
- **Alpine (apk)**: packages mirrored from Alpine's main and community repositories with what they need, through names, `so:`, `cmd:` and `pc:` capabilities, apk's provider choice and install-if. Each `APKINDEX.tar.gz` is checked against Alpine's keys and served unchanged, so stock clients use the keys they have; every package is checked against it. A package that is not mirrored lands on the wishlist. `--packages-from` takes `apk info` output, so `apk upgrade` works from the vault. `mer apk sync`.

### Serving

- Every file is sent with `Repr-Digest` (its SHA-256), which clients can check what they assemble against.

### Upgrading

No database or bundle format change: the vault and the collector can be upgraded in either order.

## [1.3.0] - 2026-11-26

### Web panel

- **Dashboards**: the Overview and Traffic pages chart downloads, data sent, requests for missing items and storage used over 24 hours, 7 days, 30 days or a year, with a hover readout, keyboard access and a table view. Values are kept per hour in the database.
- **Content age**: each repository shows how old its published content is, and can be given an age limit (panel setting or `mer repo stale`) after which it is reported as out of date.
- **Browse and search** a repository's contents as packages with their versions, or as files.
- **Review by package**: the review of a snapshot lists packages added, updated and removed before the files.
- **Wishlist actions**: each item says what to add on the collector, in the words of the Jobs tab's source form and the sync command (`--requirement=django`, `--crate=rand`, `--artifact=g:a:v`, ...). The list downloads as text; operators can dismiss items or clear it.
- **Schedules** in the Jobs tab: sync chosen repositories (or every one with a source) daily at a UTC time on chosen days, or every few hours, then seal a bundle of what changed. Runs that came due while the server was down happen once when it is back; a sync still waiting is not queued twice; "Run now" runs a schedule straight away. Scheduled jobs are audited as `schedule <n>`.
- **LDAP sign-in**: people sign in to the panel with their directory account (OpenLDAP, Active Directory, FreeIPA), and groups decide their role. Local users are checked first and keep working when the directory is down. Settings go in `/etc/mer/ldap.env` with the installer, or are `mer serve --ldap-*` options; `--ldap-ca-file` trusts a company CA for the directory only.

### Upgrading

The database schema moves to version 5 on first start. The binary is about 1.3 MB larger, from the LDAP client.

## [1.2.0] - 2026-11-26

### Formats

- **Helm charts**: charts mirrored from a chart repository at their newest stable or an exact version, each checked against the digest in the upstream `index.yaml`. Stock `helm` adds the vault as a repository, pulls and installs from it. `mer helm sync`.

- **RPM** (yum and dnf): packages mirrored from one or more repositories (BaseOS and AppStream together) with what they require, through provides, files and rich dependencies. `repomd.xml` is checked against the distribution key, everything else against it; stock `dnf` installs from the vault with package signatures checked. `mer rpm sync`.

- **Maven**: exactly what a local repository holds after a build on a connected machine (plugins included), or artifacts with their dependencies through POM resolution (parents, properties, dependency management, BOMs, scopes, exclusions, ranges, relocations). Files are checked against the strongest upstream checksum; stock `mvn` builds against the vault as a mirror. `mer maven sync`.

### Web panel

- A **Jobs** tab, with `mer serve --jobs`: create repositories, edit each one's source (the same options as `mer <format> sync`, checked by the server), sync, seal, write a bundle to disk, and import from disk on the vault. Jobs run `mer` itself, one at a time, with live logs and cancel. File paths are confined to the `--files` directory. Admins edit sources, operators run jobs, and both are audited. Jobs are refused when the panel shares a public listener with package clients.
- The database moves to schema 3 (sync sources and jobs). It is upgraded in place on first start; older versions of `mer` cannot open it afterwards.

### Installation and releases

- `install.sh`: one command installs a vault or a collector as a hardened systemd service, with keys, a TLS certificate and the first administrator, and prints the next steps. It upgrades in place and uninstalls. It is attached to every release.
- Release binaries are static (musl): they run on any x86-64 Linux, RHEL 8 and 9 and Ubuntu 20.04 included.
- `scripts/release.sh` builds, checks and publishes a release from a tag on any machine with Docker.

### Collector

- Requests that fail in transit, or that the server turns away for now (429, 502, 503, 504), are retried three times with growing waits; and a download that breaks off or stalls (less than 16 KiB in 30 seconds) picks up where it stopped with a `Range` request, or skips what it already has when the server ignores ranges. Every format's content checks still apply.
- Fixed: a sync could hang forever when an upstream (or a proxy) stopped answering on a reused connection, because the HTTP library drops a connection's timeouts when it reuses it. Every request now uses a fresh connection with its timeouts.

- Proxies: the collector honours `HTTPS_PROXY`, `HTTP_PROXY` and `NO_PROXY`, and says when it uses one.
- `mer apt sync --allow-host` for mirrors that redirect downloads to another host.

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
