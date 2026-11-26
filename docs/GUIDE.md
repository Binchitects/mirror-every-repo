# Operator guide

This guide takes you from nothing to a working mirror across an air gap, then covers day-to-day operation. For what is and is not built, see [STATUS.md](STATUS.md).

- [Concepts](#concepts)
- [Install](#install)
- [Pair the two sides](#pair-the-two-sides)
- [Mirror content on the collector](#mirror-content-on-the-collector)
- [Carry bundles across](#carry-bundles-across)
- [Review, approve, publish](#review-approve-publish)
- [Serve clients](#serve-clients)
- [Access control](#access-control)
- [Day-to-day operation](#day-to-day-operation)
- [Troubleshooting](#troubleshooting)

## Concepts

| Term | Meaning |
|---|---|
| **collector** | The host on the internet side. Mirrors upstreams and produces bundles. |
| **vault** | The host inside the air gap. Imports bundles and serves clients. |
| **repo** | One repository of one format (`apt`, `rpm`, `pypi`, `npm`, `nuget`, `maven`, `cargo`, `go`, `helm`, `oci`, `raw`). |
| **snapshot** | An immutable state of a repo, numbered per repo. Clients only ever see one whole snapshot. |
| **bundle** | A signed file set holding what changed in all repos since the previous bundle. Numbered per channel. |
| **channel** | One collector-to-vault stream of bundles. The default channel is enough unless one collector feeds several vaults. |
| **wishlist** | What clients asked the vault for and did not get. |

Both sides run the same binary, `mer`, and keep everything in one data directory (`/data` in the container).

## Install

### With the installer

On each machine, as root:

```bash
# collector (internet side)
curl -fsSL https://github.com/Binchitects/mirror-every-repo/releases/latest/download/install.sh | bash -s -- collector

# vault (inside the air gap): carry install.sh and the release archive across first
tar -xzf mer-v1.3.0-linux-x86_64.tar.gz
bash install.sh vault --binary mer-v1.3.0-linux-x86_64/mer --trust <collector public key>
```

What it sets up:

| | Collector | Vault |
|---|---|---|
| Program | `/usr/local/lib/mer/mer`, and `/usr/local/bin/mer`, which runs it as the `mer` user (`sudo mer status`) | the same |
| Data | `/var/lib/mer` | `/var/lib/mer` |
| Keys | bundle signing key and apt site key in `/var/lib/mer/keys` (its public key is printed) | trust file `/var/lib/mer/trust.txt` from `--trust` |
| TLS | | a self-signed certificate in `/etc/mer/tls` (or yours, with `--tls-cert` and `--tls-key`) |
| Service | `mer-sync.timer` runs `/etc/mer/sync.sh` daily: edit it to say what to mirror | `mer-vault.service` (`mer serve` on port 8443) |
| First login | | user `admin`, password printed once and kept in `/etc/mer/admin-password` until you delete it |

Options: `--version` picks a release, `--listen` and `--admin-listen` set the vault's addresses, `--no-tls` serves plain HTTP, `--data` moves the data directory, `--no-start` installs without starting. Running the installer again upgrades the program and keeps everything else. `--uninstall` removes the program and services; add `--purge` to delete the data and keys too.

Exports are written to `/var/lib/mer/outbox`, because the service user cannot write to a disk mounted by root: `sudo mer export media --seq 1 --out /var/lib/mer/outbox/b1`, then copy `b1` to the disk.

The collector honours `HTTPS_PROXY` and `NO_PROXY`; for the timer, add them to `mer-sync.service` with `systemctl edit mer-sync`.

### With the container image

```bash
docker pull ghcr.io/binchitects/mirror-every-repo:1.3.0
```

The image's entry point is `mer`. It runs as user id 10001 and expects two volumes: `/data` for the store and database, `/keys` for key files.

A long-running container per side, with a shell alias, keeps the rest of this guide short:

```bash
docker run -d --name mer --restart unless-stopped \
  -v mer-data:/data -v mer-keys:/keys -v /mnt/usb:/media \
  -p 8080:8080 -p 9000:9000/udp \
  --entrypoint sleep ghcr.io/binchitects/mirror-every-repo:1.3.0 infinity
```

```bash
alias mer='docker exec -i mer mer'
```

On the vault you will later replace `sleep infinity` with `mer serve` (see [Serve clients](#serve-clients)). If you bind-mount host directories instead of named volumes, make them writable by user id 10001.

The image for the vault has to reach the vault somehow: export it on the collector side with `docker save` and carry it across with your first media transfer.

## Pair the two sides

Pairing is done once, by hand. It gives the vault the collector's public key, and both ends of a diode a shared link key.

On the collector:

```text
mer init
mer key gen /keys/bundle.key        # prints the public key, a 64-character hex string
```

On the vault:

```text
mer init
mer key trust /data/trust.txt <public key>
```

Read the public key over a second channel (a phone call, a printout) and compare it on both sides. Everything the vault accepts later rests on this one key.

To require two collectors to sign (for example a second person's key), list both keys and set a threshold:

```text
mer key trust /data/trust.txt <key one> <key two> --threshold 2
```

If you use a data diode, create the link key on the collector and carry the file to the vault once:

```text
collector$ mer key link /keys/link.key
```

The link key lets the vault ignore every packet that did not come from your collector. Treat it as a secret.

## Mirror content on the collector

Each `sync` command makes the repo equal to what you name, plus dependencies, in a new snapshot. Running it again with the same arguments picks up upstream changes and downloads only what is new. Remove a package from the list and the next snapshot no longer has it.

### apt (Debian, Ubuntu)

```text
mer repo add debian --format apt
mer apt keygen                       # once: the site key your clients will trust
mer apt sync debian --upstream http://deb.debian.org/debian --suite bookworm \
    --package nginx --package postgresql-15
```

- The upstream release signature is checked against `--keyring` (default: the Debian archive keyring in the image). For Ubuntu or another archive, pass its keyring file.
- `--component` and `--arch` are repeatable (defaults `main`, `amd64`). `--recommends` also follows recommended packages.
- The collector generates the indices for the subset and signs them with the site key. Clients trust the site key, not Debian's.
- Use one repo per suite.

### RPM (yum and dnf)

```text
mer repo add rocky --format rpm
mer rpm sync rocky --keyring RPM-GPG-KEY-Rocky-9 \
    --upstream https://dl.rockylinux.org/pub/rocky/9/BaseOS/x86_64/os/ \
    --upstream https://dl.rockylinux.org/pub/rocky/9/AppStream/x86_64/os/ \
    -p tmux -p nginx
```

- `--upstream` is a repository base URL, the directory that holds `repodata/`. Repeat it to merge several repositories into one vault repo, as Enterprise Linux needs for BaseOS and AppStream.
- Requirements are followed through package names, sonames and other provides, file paths (from `filelists`), and rich dependencies (`(a or b)`, `(a if b)`). Weak dependencies (Recommends) are not followed; dnf installs without them when they are missing.
- `--keyring` is the distribution's signing key, armored or binary. `repomd.xml.asc` must verify against it; everything else is checked against `repomd.xml`, so the whole repository traces back to that key. Without `--keyring` the metadata is trusted on TLS alone.
- `--arch` is repeatable (default `x86_64` and `noarch`).
- Clients keep `gpgcheck=1` with the distribution key: packages are upstream's own, signatures included.

### Alpine (apk)

```text
mer repo add alpine --format apk
mer apk sync alpine --keys /etc/apk/keys \
    --upstream https://dl-cdn.alpinelinux.org/alpine/v3.24/main \
    --upstream https://dl-cdn.alpinelinux.org/alpine/v3.24/community \
    -p curl -p nginx --packages-from installed.txt
```

- `--upstream` is a repository as `/etc/apk/repositories` names it. Repeat it for main and community: community packages need main. Each keeps its own path on the vault (`alpine/main`, `alpine/community`).
- `--keys` is a directory of Alpine's keys, as `/etc/apk/keys` on any Alpine machine. Each `APKINDEX.tar.gz` must be signed by one of them, and every package is checked against it, so everything traces back to Alpine's key. Without `--keys`, indexes are trusted on TLS alone.
- What packages need is followed through names, `so:`, `cmd:` and `pc:` capabilities, as apk picks among providers. Packages apk installs by itself (install-if: docs, shell completions, OpenRC services) come along once what they wait for is mirrored.
- The index is Alpine's own, unchanged, still signed by Alpine. Clients need no key from you, and `apk search` shows the whole repository. Installing a package that is not mirrored fails with 404, and the package lands on the vault's wishlist.
- `--packages-from` reads a list of package names, as `apk info` prints them on a client image (or `/etc/apk/world`). Mirror what your images already have, and `apk upgrade` works from the vault too.
- `--arch` is repeatable (default `x86_64`). Each index is a new snapshot when Alpine republishes it, which happens several times a day.

### PyPI

```text
mer repo add pypi --format pypi
mer pypi sync pypi --python 3.12 --python 3.11 -r requests -r "django>=4.2,<5" -r "uvicorn[standard]"
```

- `--python` is the CPython version your machines run; repeat it for several.
- `--arch` (default `x86_64`) and `--libc` (default `manylinux`; add `--libc musllinux` for Alpine) decide which binary wheels are kept.
- `--sdists if-no-wheel` (default) keeps a source archive only when a version has no usable wheel. `always` and `never` are the alternatives.
- One version per project is mirrored: the newest that satisfies everything that asked for it.

### npm

```text
mer repo add npm --format npm
mer npm sync npm -p express -p "lodash@^4.17" -p "@types/node@20"
```

- Dependencies, optional dependencies and non-optional peer dependencies are followed; development dependencies are not.
- Packages built for another operating system or CPU are left out. `--os`, `--cpu` and `--libc` set the target (defaults `linux`, `x64`, `glibc`).

### Composer (PHP)

```text
mer repo add php --format composer
mer composer sync php --php 8.3.14 -p monolog/monolog -p "symfony/console:^7.1" --lockfile composer.lock
```

- `-p` names a package: `monolog/monolog` (newest stable), `"symfony/console:^7.1"` (a constraint, as in `composer.json`), `psr/log@3.0.2` (exactly). Each constraint, the package's own `require` included, resolves to its newest matching stable version on its own; this is not Composer's solver. `--lockfile` mirrors exactly what a `composer.lock` names, dev packages included.
- `--php` is the PHP inside the air gap. Versions whose `require.php` rules it out are left out. Requirements on PHP extensions (`ext-*`) and libraries are not packages and are not mirrored.
- Packagist keeps most archives on GitHub and states no checksum for them, so they are trusted on TLS. The vault's metadata states the SHA-1 of every archive it serves, which Composer checks on download.
- A project locked on a connected machine: copy its `composer.json` and `composer.lock` across and run `composer update` against the vault, not `composer install`. A lock made upstream names GitHub's archives, which `composer install` would try to reach. `composer update` with only the locked versions on the vault keeps them, and writes the vault's addresses into the lock.
- Branches (`dev-main`) are mirrored only from a `composer.lock`. Requirements on virtual packages (`psr/log-implementation`) are reported; name a package that provides them.

### RubyGems

```text
mer repo add gems --format gem
mer gem sync gems --ruby 3.4.1 -g sinatra -g "rails:~> 8.0" --lockfile Gemfile.lock
```

- `-g` names a gem: `rails` (newest release), `"rails:~> 8.0"` (a requirement), `rack@3.2.9` (exactly). Each requirement, the gem's own and those of its dependencies, resolves to its newest matching version on its own; this is not Bundler's solver, so a combination only an older version satisfies is not found. `--lockfile` sidesteps that: exactly what a `Gemfile.lock` names is mirrored.
- `--ruby` is the Ruby inside the air gap. Versions that need another Ruby are left out, as Bundler would leave them out. Without it, any Ruby.
- `--platform` (repeatable) is the platforms to keep besides pure Ruby, as Bundler names them (default `x86_64-linux` and `x86_64-linux-gnu`). Gems with precompiled builds (nokogiri, grpc, google-protobuf) are mirrored for those platforms and in their source form.
- The vault serves the compact index (`/versions`, `/info/<gem>`) of exactly what is mirrored, with upstream's lines unchanged, so `gem install` and Bundler both see only gems they can get. `gem search` and the older Marshal indexes are not offered.
- To make a `Gemfile.lock` on a connected machine for exactly the platforms inside, remove the others: `bundle lock --remove-platform aarch64-linux-gnu ...`.

### NuGet (.NET)

```text
mer repo add nuget --format nuget
mer nuget sync nuget --framework net8.0 -p Serilog.Sinks.Console -p Newtonsoft.Json@13.0.3
```

- `--framework` is the target framework of your projects (`net8.0`, `netstandard2.0`, `net48`); repeat it for several.
- A package without a version means the newest listed stable one. `Id@1.2.3` means that version or the next higher one that exists, which is how NuGet reads a version. `Id@[1.2.3]` means exactly that version.
- Packages a build pulls in implicitly (runtime packs for self-contained publish, analyzers) have to be named.

### Maven (Java, Kotlin, Scala)

```text
mer repo add maven --format maven
# on a connected machine, build your projects once: mvn package -Dmaven.repo.local=/tmp/m2
mer maven sync maven --from-m2 /tmp/m2
mer maven sync maven --from-m2 /tmp/m2 -a com.google.guava:guava:33.3.1-jre
```

- `--from-m2` mirrors exactly what a local repository holds after a build. A Maven build needs plugins (compiler, surefire, jar and their dependencies) as much as libraries, and which ones depends on the Maven version and the project, so this is the most reliable way to make projects build inside the air gap. Build with the same Maven version the air-gapped machines use.
- `-a group:artifact:version` mirrors an artifact with its compile and runtime dependencies, following POMs through parents, properties, dependency management, imported BOMs, exclusions, version ranges and relocations. Test, provided and optional dependencies are left out. `group:artifact` alone means the newest release. Use `group:artifact:ext:classifier:version` for other files.
- Every file is checked against the strongest checksum the upstream repository serves (SHA-512, SHA-256 or SHA-1). The vault adds `.sha1` files and version metadata for what it holds.

### Cargo (Rust)

```text
mer repo add cargo --format cargo
mer cargo sync cargo -c 'serde@1:derive' -c 'tokio@1:full' -c anyhow
mer cargo sync cargo --lockfile path/to/Cargo.lock
```

- A crate without a version means the newest one. `name@1.2` is a Cargo requirement (`^1.2`); `name@=1.2.3` is exactly that version. Features go after a colon: `tokio@1:full,macros`. Default features are always on.
- Each requirement resolves to the newest matching version that is not yanked, as Cargo does. Optional dependencies are mirrored only when an enabled feature turns them on. Build dependencies are followed; development dependencies are not.
- Dependencies for every target platform are mirrored (Windows and macOS ones too), because Cargo resolves for all platforms even when it builds for one, exactly like `cargo vendor`.
- `--lockfile` mirrors the registry packages of a `Cargo.lock` at exactly the versions it names, and checks them against the checksums the lockfile states. Repeat it for several projects. This is the most reliable way to make an existing project build inside the air gap.
- `--rust-version 1.80` passes over versions that declare a newer `rust-version` than the toolchain your machines have, when an older version fits.
- `--index` selects another sparse registry (default crates.io). Crates from crates.io are downloaded from `static.crates.io`, which is allowed automatically.

### Go modules

```text
mer repo add go --format go
mer go sync go -m github.com/spf13/cobra@v1.8.1 -m golang.org/x/text
mer go sync go --gosum path/to/go.sum
mer go sync go --gomod path/to/go.mod
```

- A module without a version means its newest release that is not retracted, as `go get module@latest` picks it.
- Requirements in `go.mod` files are followed through the whole module graph. The `go.mod` of every version in the graph is mirrored (they are small, and `go` reads many of them). Zips are mirrored for the versions a build can select: the newest in the full graph, the newest in the pruned graph that Go 1.17 and later use, and every version a root names.
- `--gosum` mirrors exactly what a project's `go.sum` lists and checks every download against its hashes. This is the most reliable way to make an existing project build inside the air gap. `--gomod` takes a project's `go.mod` and mirrors its requirements with what they need; `replace` directives in it are not followed.
- Every `.zip` and `.mod` is checked against the hash in the Go checksum database (`--sumdb`, default `sum.golang.org`) unless a given `go.sum` has it. For private modules that the checksum database cannot know, pass their `go.sum` and `--no-sumdb`.
- `--proxy` selects another module proxy (default `proxy.golang.org`).

### Helm charts

```text
mer repo add helm --format helm
mer helm sync helm --upstream https://charts.jetstack.io -c cert-manager -c cert-manager@v1.15.3
```

- `--upstream` is the chart repository URL, the one you would give `helm repo add`.
- A chart without a version means its newest stable version. `name@version` is exactly that version; repeat a chart to keep several.
- Charts include their dependencies (`charts/` inside the archive), so nothing else needs mirroring.
- Each chart is checked against the SHA-256 digest in the upstream index. If a repository keeps its chart files on another host (GitHub releases, for example), allow it with `--allow-host github.com`; GitHub's asset download hosts are then allowed too.
- Charts published to OCI registries (`oci://...`) are not covered by this format.

### Container images

```text
mer repo add hub --format oci
mer oci sync hub -i library/alpine:3.20 -i library/nginx:1.27 --platform linux/amd64
```

- Use `name@sha256:...` to pin an image by digest.
- `--registry` selects another registry (default Docker Hub). Only images that can be pulled anonymously are supported.
- `--platform` is repeatable. Other platforms of a multi-platform image are not downloaded.

### Plain files

```text
mer repo add tools --format raw
mer repo sync-dir tools /path/to/directory
```

The repo becomes a copy of the directory tree.

### Contacting other hosts

The collector only connects to the upstream host you name, plus the hosts that upstream is known to use: `files.pythonhosted.org` for PyPI, `static.crates.io` for crates.io, `auth.docker.io` and its CDN hosts (`production.cloudfront.docker.com`, `production.cloudflare.docker.com`) for Docker Hub, `cdn01.quay.io` to `cdn03.quay.io` for quay.io, `pkg-containers.githubusercontent.com` for ghcr.io, and GitHub's release asset hosts whenever `github.com` is allowed. It honours `HTTPS_PROXY`, `HTTP_PROXY` and `NO_PROXY` when the collector reaches the internet through a proxy; the allowlist still applies to every destination. This includes redirects: a redirect to any other host is refused, never followed, and the error names the host. If that host is expected (a private registry's storage, a CDN), allow it with `--allow-host`. Redirects from https to http are always refused, and credentials are never sent on to another host.

## Run it from the web panel

Everything in the two sections above and below can also be done in the web panel's **Jobs** tab: create repositories, say what each mirrors (its *source*), sync, seal a bundle, write it to disk, and on the vault import it.

Start the server with jobs enabled. The installer does this for you: the collector's panel listens on `127.0.0.1:8081`, and a vault installed with `--admin-listen` can import.

```text
# collector
mer serve --listen 127.0.0.1:8081 --jobs --files /var/lib/mer/files \
    --bundle-key /var/lib/mer/keys/bundle.key --apt-gnupghome /var/lib/mer/keys/gnupg
# vault: packages for everyone, the panel on its own address
mer serve --listen 0.0.0.0:8443 --admin-listen 127.0.0.1:8444 --jobs --files /var/lib/mer/files --tls-cert ... --tls-key ...
```

- A job runs `mer` itself with the same options the command line takes, one job at a time. Its output is shown live and kept (the last 256 KiB). A job can be cancelled. Jobs that were running when the server stopped are marked *interrupted*.
- The source form for each format lists the same options as `mer <format> sync`; the server checks every value before saving it.
- `--files` is the only directory panel users can name files in: lockfiles, `go.sum` files, local Maven repositories and keyrings for sources, the target of an export, the source of an import. Paths are relative to it and cannot lead outside, symbolic links included. Without `--files` those fields are refused.
- Who can do what: admins create repositories and edit sources; operators run syncs, seals, exports and imports, and cancel jobs; viewers see everything. Every change and every job is in the audit log with the user's name.
- **Schedules** run syncs without anyone pressing a button: daily at a time on chosen days, or every so many hours. Each run queues a sync job for each chosen repository (or for every repository with a source), then, if asked, a seal, so the bundle carries what the syncs brought. A repository whose sync is still waiting or running is skipped rather than queued twice. Times are UTC. A run that came due while the server was down happens once when it is back. "Run now" runs a schedule straight away and keeps its next time. Operators manage schedules; scheduled jobs are in the audit log as `schedule <n>`. Writing a bundle to disk stays a step a person takes, since someone has to carry the disk.
- The installer also sets up `mer-sync.timer`, which runs `/etc/mer/sync.sh` once a day: the command-line way to say what to mirror. As installed, that script only seals. If you schedule from the panel instead, you can turn the timer off with `sudo systemctl disable --now mer-sync.timer`. Both can run side by side: a seal takes only snapshots that are complete, so it never picks up half of a sync that is still running.
- The newest 500 finished jobs are kept, with their logs; older ones are forgotten. The audit log keeps the record of every job.
- A source decides which hosts the collector contacts, so jobs are refused unless the panel is on a listener package clients cannot reach: `--admin-listen`, or `--listen` on loopback. From another machine, use an SSH tunnel: `ssh -L 8081:127.0.0.1:8081 collector`.

## Carry bundles across

### Seal

```text
mer export seal --key /keys/bundle.key
```

This seals bundle N: everything that changed in any repo since bundle N-1. If nothing changed, it says so and creates nothing. Sealed bundles stay on the collector, so any of them can be exported again later.

### Removable media

```text
collector$ mer export media --seq 7 --out /media/bundle-7 --volume-size fat32
vault$     mer import media /media/bundle-7
```

- `--volume-size` is `fat32`, `dvd`, `bd25`, or a size such as `32G`. The bundle is split into volume files of at most that size.
- Volumes can be imported one medium at a time, in any order. After each one the vault prints which segments are still missing.
- A damaged volume costs only the segments it damaged. Import a second copy and only those are taken from it.
- The media is never written to.

### Data diode

On the vault, a receiver that keeps running:

```text
mer import diode --listen 0.0.0.0:9000 --link-key /keys/link.key
```

On the collector, for each bundle:

```text
mer export diode --seq 7 --to <vault address>:9000 --link-key /keys/link.key --rate-mbit 80 --passes 3
```

- `--rate-mbit` must not exceed what the diode carries. Anything above is lost without notice.
- Each pass sends new repair data, so more passes tolerate more loss. Two is a sensible minimum.
- The vault imports a bundle as soon as all its segments are complete. What the diode did not deliver can be completed from media, and the other way round: both transports feed the same bundle.
- A hardware diode does not answer ARP. Configure a static neighbour entry for the vault address on the sending host.

### When a bundle is lost

Bundles do not have to arrive in order. If bundle 7 never arrives and bundle 8 does, the repos that changed in 7 wait (state `incomplete`); all other repos move on. Export 7 again and import it, and the waiting repos catch up. Each bundle repeats the change lists of the previous 8, so only the content itself has to be re-sent.

## Review, approve, publish

Imported snapshots are sealed but not visible to clients. In the web panel, the Overview lists what is waiting; "Review changes" shows what publishing would add, update and remove, package by package and then file by file; then approve and publish. "Browse" on the Repositories page lists what a snapshot holds, as packages with their versions or as files, with search. From the command line:

```text
mer status                    # repos, snapshots, bundles
mer repo snapshots debian
mer approve debian 12
mer publish debian 12
mer rollback debian 11        # back to an older approved snapshot, immediately
mer reject debian 13          # a rejected snapshot can never be published
```

If you trust the collector fully and do not want a manual step, import with `--auto-publish`.

## Serve clients

Run the server on the vault:

```bash
docker run -d --name mer --restart unless-stopped \
  -v mer-data:/data -v mer-keys:/keys -p 8080:8080 -p 8443:8443 \
  ghcr.io/binchitects/mirror-every-repo:1.3.0 \
  serve --listen 0.0.0.0:8080 --admin-listen 0.0.0.0:8443 \
        --tls-cert /keys/tls.crt --tls-key /keys/tls.key
```

- `--listen` serves package clients. `--admin-listen` puts the web panel on a separate address so that client machines cannot reach it; without it the panel shares the first listener. `--no-admin` turns the panel off.
- `--tls-cert` and `--tls-key` are PEM files from your internal certificate authority. Without them the server speaks plain HTTP, which also sends panel passwords in the clear.
- Imports and other `mer` commands can run next to the server on the same data directory (`docker exec mer mer import media ...`).

Client configuration, with `vault:8080` as the package listener:

| Client | Configuration |
|---|---|
| apt | `deb [signed-by=/etc/apt/keyrings/site.asc] https://vault:8080/debian bookworm main`. Get the key once from `https://vault:8080/debian/keys/site.asc` or by hand. |
| dnf, yum | A `.repo` file with `baseurl=https://vault:8080/rocky/`, `gpgcheck=1` and the distribution key. Install with `--disablerepo='*' --enablerepo=vault`, or disable the other repositories. |
| composer | `composer config -g repo.packagist composer https://vault:8443/php/` (with `composer config -g secure-http false` if the vault is on plain HTTP). |
| gem | `gem install --clear-sources --source https://vault:8443/gems/ <gem>`, or for good `gem sources --remove https://rubygems.org/ --add https://vault:8443/gems/` |
| bundle | `source "https://vault:8443/gems/"` in the Gemfile, or without changing Gemfiles `bundle config set --global mirror.https://rubygems.org https://vault:8443/gems/` |
| apk | `/etc/apk/repositories` with `https://vault:8080/alpine/main` (and `.../community`) and nothing else. The keys the client has already check the index. |
| pip | `pip install --index-url https://vault:8080/pypi/simple/ <package>`, or `index-url` in `pip.conf`. |
| npm | `npm config set registry https://vault:8080/npm/`. Use `npm install --no-audit`: the vault has no audit service. |
| dotnet | `dotnet nuget add source https://vault:8080/nuget/v3/index.json --name vault`, and remove the default source. Restore works; searching for packages does not. |
| mvn | In `~/.m2/settings.xml`, a mirror `<mirrorOf>*</mirrorOf>` with `<url>https://vault:8080/maven/</url>`. Gradle: a `maven { url = uri("https://vault:8080/maven/") }` repository. |
| cargo | In `.cargo/config.toml`: `[source.crates-io]` `replace-with = "vault"` and `[source.vault]` `registry = "sparse+https://vault:8080/cargo/"`. Build works; `cargo search` and `cargo publish` do not. |
| go | `go env -w GOPROXY=https://vault:8080/go/ GOSUMDB=off GOTOOLCHAIN=local`. Downloads, `go mod tidy` and `go build` work; `go` still checks everything against the project's `go.sum`. |
| helm | `helm repo add vault https://vault:8080/helm/`, then `helm install` or `helm pull vault/<chart>` as usual. |
| docker | `docker pull vault:8080/hub/library/alpine:3.20`. The image name is `<vault>/<repo>/<upstream name>`. |
| files | `curl -O https://vault:8080/tools/<path>` |

On plain HTTP, clients need to be told to accept it: `--trusted-host vault` for pip, `--allow-insecure-connections` for the dotnet source, `insecure-registries` in Docker's `daemon.json`. apt, npm, cargo, go and helm accept `http://` URLs as they are (go then cannot send a token; see below).

The panel's "Client setup" button shows these lines for each repo with your address filled in.

`/healthz` answers `ok` for load balancers and monitors. `/metrics` exposes Prometheus metrics.

## Access control

### Panel users

```text
mer user add admin --role admin       # the first user must be an admin; prints a password once
mer user add olga --role operator
mer user list
mer user passwd olga                  # new generated password; ends her sessions
mer user disable olga
```

| Role | Can |
|---|---|
| viewer | See everything in the panel |
| operator | Also approve, reject, publish, roll back, run garbage collection |
| admin | Also manage users, tokens and repository settings |

Add `--password-stdin` to supply a password instead of having one generated. Five wrong passwords lock a user name for a minute. Every sign-in and every change is in the audit log with the user's name.

### Sign in through a company directory (LDAP)

People can sign in to the panel with their directory account (OpenLDAP, Active Directory, FreeIPA). The panel finds the user with a search, checks the password by binding as that user, and reads the user's groups; the groups you name decide the role. Someone in none of them cannot sign in.

With the installer, fill in `/etc/mer/ldap.env` (it lists every setting, commented out) and restart the service (`sudo systemctl restart mer-vault`, or `mer-panel` on a collector). Upgrades never overwrite that file. By hand, the same settings are options of `mer serve`:

```text
mer serve ... \
  --ldap-url ldaps://dc1.example.org:636 \
  --ldap-ca-file /etc/mer/ldap-ca.pem \
  --ldap-bind-dn 'cn=mirror,ou=service,dc=example,dc=org' --ldap-bind-password-file /etc/mer/ldap-bind-password \
  --ldap-user-base 'ou=people,dc=example,dc=org' \
  --ldap-group-base 'ou=groups,dc=example,dc=org' \
  --ldap-admin-group mirror-admins --ldap-operator-group mirror-operators --ldap-viewer-group mirror-viewers
```

| Setting | Meaning |
|---|---|
| `--ldap-url` | `ldaps://host:636`, or `ldap://host:389` with `--ldap-starttls` so nothing is sent unencrypted |
| `--ldap-ca-file` | The CA that signed the directory's certificate. Only it is trusted for the directory; without it, the system's CAs are |
| `--ldap-bind-dn`, `--ldap-bind-password-file` | The account that looks users up. The password is read from a file, never from the command line. Without them, the search is anonymous |
| `--ldap-user-base`, `--ldap-user-filter` | Where users are and how one is found. The filter defaults to `(uid={user})`; for Active Directory use `(sAMAccountName={user})` |
| `--ldap-group-base`, `--ldap-group-filter` | Where groups are. Without a base, the user's `memberOf` attribute is read instead |
| `--ldap-admin-group`, `--ldap-operator-group`, `--ldap-viewer-group` | Groups, by name (`cn`) or full DN, that grant each role. Repeatable on the command line; one each in the settings file |

- Local users (such as `admin` from the installer) are checked first, and still work when the directory is down or not answering. A directory user who cannot be checked gets "the directory cannot be asked right now"; the reason is in the audit log.
- The role is read from the directory at every sign-in, so moving someone between groups takes effect the next time they sign in. Changing a directory user's role in the panel lasts only until then.
- A directory user has no password here and cannot be given one. An administrator can still disable a directory user in the panel, which shuts them out whatever the directory says.
- Nothing a user types can change the search: names are escaped, names that could not be local user names are not sent at all, and an empty password is refused before anything reaches the directory.
- Each step waits at most 10 seconds for the directory.

### Private repositories

By default a published repo can be read by any machine that reaches the vault. To require a token:

```text
mer repo private debian on
mer token add build-farm --repo debian     # prints the secret once
mer token list
mer token revoke build-farm                # takes effect immediately
```

A token without `--repo` is valid for every repo. Clients send the token as a password:

| Client | Where the token goes |
|---|---|
| apt | `/etc/apt/auth.conf.d/vault.conf`: `machine https://vault:8080 login apt password <token>` |
| dnf, yum | `username=any` and `password=<token>` in the `.repo` file |
| apk | In the repository line: `https://any:<token>@vault:8080/alpine/main` |
| bundle | `bundle config set --global https://vault:8443/gems/ any:<token>` |
| composer | `composer config -g http-basic.vault any <token>` |
| gem | The token in the source URL: `--source https://any:<token>@vault:8443/gems/` |
| pip | `index-url = https://token:<token>@vault:8080/pypi/simple/` |
| npm | `.npmrc`: `//vault:8080/npm/:_authToken=<token>` |
| dotnet | `dotnet nuget add source ... --username any --password <token> --store-password-in-clear-text` |
| mvn | A `<server>` with the mirror's `<id>`, any `<username>` and the token as `<password>` in `settings.xml` |
| cargo | In `.cargo/config.toml`, define the vault as `[registries.vault]` `index = "sparse+https://vault:8080/cargo/"` (instead of `[source.vault]`), keep `replace-with = "vault"`, and add `[registry]` `global-credential-providers = ["cargo:token"]`. Put the token in `~/.cargo/credentials.toml` as `[registries.vault]` `token = "<token>"`, or in `CARGO_REGISTRIES_VAULT_TOKEN`. `cargo login` does not work, because the vault has no registry API. |
| go | `~/.netrc`: `machine vault:8080 login go password <token>` (the machine name includes the port). Go sends credentials only over HTTPS, so a private Go repo needs the vault on TLS. |
| helm | `helm repo add vault https://vault:8080/helm/ --username any --password <token>` |
| docker | `docker login vault:8080` with any user name and the token as password |

The apt, Cargo, Go and Helm flows are tested end to end with real clients, as are open Maven and RPM repositories. The others rely on the same Basic and Bearer handling, which is covered by the server's tests. Cargo sends the token on its own, without `Bearer`, which the vault also accepts.

## Day-to-day operation

```text
mer status                       # one-screen summary of both transfer and repo state
mer wishlist                     # what clients asked for and did not get
mer audit                        # recent audit entries, and a check of the whole chain
mer fsck --deep                  # re-hash every stored file against its digest
```

**Wishlist.** Requests for packages, projects or images that are not mirrored are counted. Use the list to decide what to add to the next sync: the panel's Wishlist page shows, for each item, what to add on the collector (for example `--requirement=django`), in the same words as the repository's source in the Jobs tab and the sync command. "Download as text" gives the whole list as a file to carry across. (apt clients do not ask the server for packages missing from the index, so apt wishes do not appear.)

**Dashboards.** The Overview and Traffic pages chart downloads, data sent, requests for missing items and storage over 24 hours, 7 days, 30 days or a year. The values are kept per hour in the database.

**Freshness.** Each repository shows how old its published content is. Give it a limit, and it is reported as out of date once what is published is older:

```text
mer repo stale debian 14         # report debian when its content is more than 14 days old
mer repo stale debian off
```

The same setting is under "Settings" on the Repositories page.

**Space.** Content stays as long as a retained snapshot refers to it.

```text
mer repo quota debian 200G       # refuse snapshots that would exceed this (0 removes the limit)
mer compact debian 12            # forget snapshots older than 12; they can no longer be rolled back to
mer gc --grace-hours 24          # delete content nothing refers to any more
```

Garbage collection only removes content that has been unreferenced for the grace period. Keep the default unless you are sure no import or sync is running.

**Backups.** Everything is in the data directory. Stop the container, copy the volume, start it again. The key files in `/keys` are small and should be backed up separately and kept offline.

**Upgrades.** Pull the new image and restart. The database is upgraded in place on first start. Upgrade the vault before the collector if a release changes the bundle format; the changelog says when that is the case. Back up first.

## Troubleshooting

| What you see | What it means | What to do |
|---|---|---|
| Repo state `incomplete` | Its changes arrived but some content did not (an earlier bundle is missing) | Re-export and import the bundle named in the detail |
| Repo state `waiting_base` | The vault is more than 8 bundles behind for this repo | Import the missing bundles, oldest first |
| Repo state `quota_blocked` | The snapshot would exceed the repo's quota | Raise the quota or compact, then `mer import retry` |
| Repo state `diverged` | The vault's history for this repo differs from the collector's | Do not force it. Check whether the repo was changed on the vault, or whether two collectors use one channel |
| `a different bundle is already recorded for seq N` | Two different signed bundles claim the same number | Stop. One collector data directory was restored from an old backup or duplicated |
| `signature check failed` on import | The bundle was not signed by a key in the trust file | Check `trust.txt` against the collector's `mer key pub /keys/bundle.key` |
| apt: `NO_PUBKEY` | The client does not have the site key | Install `keys/site.asc` and reference it with `signed-by=` |
| `401` from a client | The repo is private and the token is missing, wrong or revoked | See [Private repositories](#private-repositories) |
| docker: `server gave HTTP response to HTTPS client` | Docker expects TLS | Serve with TLS, or list the vault under `insecure-registries` |
| dotnet: `NU1302` | NuGet refuses plain HTTP sources | Serve with TLS, or add `allowInsecureConnections="true"` to the source |
| go: `refusing to pass credentials to insecure URL` | Go does not send tokens over plain HTTP | Serve with TLS and put the token in `~/.netrc` |
| go: an error naming `sum.golang.org` | The client tries to reach the checksum database | `go env -w GOSUMDB=off`; the collector already checked every hash against it |
| go: an error about a newer Go toolchain | A module asks for a newer Go than the client has, and go tries to download one | `go env -w GOTOOLCHAIN=local`, and use an older version of that module or a newer Go |
| dnf: `GPG check FAILED` or `public key ... is not installed` | The client does not have the distribution key | Point `gpgkey=` at the distribution's key file, which the distribution's own images ship in `/etc/pki/rpm-gpg/` |
| `repomd.xml does not verify` on the collector | The keyring is not the key that signed the repository | Use the distribution's key for that release (for example `RPM-GPG-KEY-Rocky-9`) |
| apk: `UNTRUSTED signature` on a client | The client does not have the key that signed the index | Use an Alpine image of the same release, or install `alpine-keys` |
| `not signed by a trusted key` on the collector | `--keys` does not hold the key that signed the index | Copy `/etc/apk/keys` from an Alpine machine of that release |
| apk: `ERROR: <package>: HTTP 404` | The package is not mirrored | It is on the wishlist; add it to the sync with `-p` |
| composer: `Failed to download ... from dist: ... api.github.com` | The lock file was made upstream and names GitHub's archives | Run `composer update` against the vault instead of `composer install`; it keeps the locked versions and points the lock at the vault |
| composer: `Filter list data could not be fetched ... repo.packagist.org` | Composer looks for packagist's malware filter list | Harmless; Composer ignores the failure |
| bundle: `Could not find gem 'x' in rubygems repository` | The gem, or a version that fits, is not mirrored | It is on the wishlist; add it with `-g`, or sync the project's `Gemfile.lock` with `--lockfile` |
| bundle: a gem needs a native build that fails | The precompiled build for that platform is not mirrored | Add the platform with `--platform` (as `bundle platform` names it) |
| mvn: `Could not resolve dependencies` or `Plugin ... could not be resolved` | The artifact or plugin is not mirrored | It is on the wishlist; build the project once on a connected machine and sync with `--from-m2` |
| pip: `is not a trusted or secure host` | pip refuses plain HTTP indexes | Serve with TLS, or pass `--trusted-host` |
| Sync warns `depends on X, which upstream does not provide` | A dependency could not be satisfied in that upstream | Usually harmless for optional items; otherwise add the upstream or component that has it |
| The panel says no user exists yet | A fresh vault has no accounts | `mer user add admin --role admin` on the vault host |
