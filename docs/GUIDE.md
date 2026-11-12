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
| **repo** | One repository of one format (`apt`, `pypi`, `npm`, `nuget`, `oci`, `raw`). |
| **snapshot** | An immutable state of a repo, numbered per repo. Clients only ever see one whole snapshot. |
| **bundle** | A signed file set holding what changed in all repos since the previous bundle. Numbered per channel. |
| **channel** | One collector-to-vault stream of bundles. The default channel is enough unless one collector feeds several vaults. |
| **wishlist** | What clients asked the vault for and did not get. |

Both sides run the same binary, `mer`, and keep everything in one data directory (`/data` in the container).

## Install

The container image is the supported way to run it.

```bash
docker pull ghcr.io/binchitects/mirror-every-repo:1.1.0
```

The image's entry point is `mer`. It runs as user id 10001 and expects two volumes: `/data` for the store and database, `/keys` for key files.

A long-running container per side, with a shell alias, keeps the rest of this guide short:

```bash
docker run -d --name mer --restart unless-stopped \
  -v mer-data:/data -v mer-keys:/keys -v /mnt/usb:/media \
  -p 8080:8080 -p 9000:9000/udp \
  --entrypoint sleep ghcr.io/binchitects/mirror-every-repo:1.1.0 infinity
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

### NuGet (.NET)

```text
mer repo add nuget --format nuget
mer nuget sync nuget --framework net8.0 -p Serilog.Sinks.Console -p Newtonsoft.Json@13.0.3
```

- `--framework` is the target framework of your projects (`net8.0`, `netstandard2.0`, `net48`); repeat it for several.
- A package without a version means the newest listed stable one. `Id@1.2.3` means that version or the next higher one that exists, which is how NuGet reads a version. `Id@[1.2.3]` means exactly that version.
- Packages a build pulls in implicitly (runtime packs for self-contained publish, analyzers) have to be named.

### Container images

```text
mer repo add hub --format oci
mer oci sync hub -i library/alpine:3.20 -i library/nginx:1.27 --platform linux/amd64
```

- Use `name@sha256:...` to pin an image by digest.
- `--registry` selects another registry (default Docker Hub). Only images that can be pulled anonymously are supported.
- `--platform` is repeatable. Other platforms of a multi-platform image are not downloaded.

### RPM (dnf, yum)

```text
mer repo add rocky --format rpm
mer rpm sync rocky \
    --url https://dl.rockylinux.org/pub/rocky/9/BaseOS/x86_64/os \
    --url https://dl.rockylinux.org/pub/rocky/9/AppStream/x86_64/os \
    --keyring /keys/RPM-GPG-KEY-Rocky-9 --sign-home /keys/gnupg \
    -p nginx -p git
```

- `--url` is the directory that holds `repodata/`. Give every repository the packages may depend on (a distribution usually splits into two or three); together they become one repository on the vault.
- `-p` is a package name, or anything a package provides: `/usr/bin/git`, `libfoo.so.1()(64bit)`. What the package needs is added. `--recommends` also follows weak dependencies, which `dnf` installs by default.
- `--arch` (default `x86_64`) is repeatable; `noarch` packages are always included.
- `--keyring` is the distribution's public key file (on a machine of that distribution: `/etc/pki/rpm-gpg/`). With it, the signature on the upstream metadata is required and verified. Without it the sync still works but says that signatures were not checked.
- `--sign-home` signs the metadata of the mirrored repository with the site key (`mer apt keygen` creates it), for clients that set `repo_gpgcheck=1`.
- Packages are stored unchanged, so clients keep `gpgcheck=1` with the distribution's own key.
- Module streams are not mirrored; packages that belong to one are left out, and the sync says how many.

### Maven

```text
mer repo add maven --format maven
mer maven sync maven \
    -a org.apache.commons:commons-lang3:3.14.0 -a com.google.guava:guava \
    -a org.apache.maven.plugins:maven-compiler-plugin:3.13.0 \
    --bom org.springframework.boot:spring-boot-dependencies:3.3.0
```

- `-a` is `group:artifact:version`. Without a version it is the one a `--bom` manages, else the latest release.
- `--bom` names a BOM your projects import. The versions it manages are then used throughout, as they are in those projects.
- **Build plugins are artifacts too.** A `mvn package` needs the resources, compiler, surefire and jar plugins at the versions your Maven or your POMs name; mirror them with `-a`. Pin plugin versions in your projects so that the list is known. Test runners fetch more at run time (the surefire provider for your test framework); name those as well, or build with `-DskipTests` until they are mirrored.
- `--classifier sources` (repeatable) also mirrors those files where they exist.
- `--url` selects another repository (default Maven Central).
- Every version of an artifact that is reached is mirrored, so the one Maven settles on is there.

### Cargo (Rust)

```text
mer repo add crates --format cargo
mer cargo sync crates -p serde -p "tokio@1.38" --lockfile /work/myproject/Cargo.lock
```

- `-p` is written as in `Cargo.toml`: `tokio@1.38` means 1.38 or a compatible later version, `rand@=0.8.5` exactly that one.
- Named crates are mirrored with every feature switched on, so that any build can be served. With `--default-features-only` only their default features are followed; add single ones with `--feature tokio/full`.
- `--lockfile` (repeatable) mirrors exactly the versions a `Cargo.lock` names, so that `cargo build --locked` works inside the air gap.
- `--index` selects another registry (default crates.io).

### Go modules

```text
mer repo add go --format go
mer go sync go -m github.com/gin-gonic/gin@v1.10.0 -m golang.org/x/sync --gomod /work/myservice/go.mod
```

- `-m` is a module path, with `@version` or without (the latest release).
- `--gomod` (repeatable) is the `go.mod` of a project: what it requires is mirrored at exactly the versions it names.
- Every file is compared with the Go checksum database (`--sumdb`, default `sum.golang.org`). For a private proxy whose modules that database does not know, use `--no-sumdb`.
- Clients set `GOSUMDB=off`: the check they cannot make from inside the air gap was made on the collector.

### Helm charts

```text
mer repo add charts --format helm
mer helm sync charts --url https://charts.jetstack.io -c cert-manager -c "cert-manager@~1.16"
```

- `-c` is a chart name, with `@version` or `@range` or without (the newest stable version).
- One chart repository per repo. If the repository keeps its archives on another host (GitHub releases, for example), allow it: `--allow-host github.com`.
- Charts kept in an OCI registry are container content: `mer repo add oci-charts --format oci`, then `mer oci sync oci-charts --registry https://ghcr.io -i <path>:<version>`, and clients use `helm pull oci://vault:8080/oci-charts/<path> --version <version>`.

### Plain files

```text
mer repo add tools --format raw
mer repo sync-dir tools /path/to/directory
```

The repo becomes a copy of the directory tree.

### Contacting other hosts

The collector only connects to the upstream host you name, plus the hosts that upstream is known to use (`files.pythonhosted.org` for PyPI, `auth.docker.io` for Docker Hub, `static.crates.io` for crates.io, `sum.golang.org` for Go). If a private upstream keeps files or tokens on another host, allow it with `--allow-host`.

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

Imported snapshots are sealed but not visible to clients. In the web panel, the Overview lists what is waiting; "Review changes" shows what publishing would add, change and remove, first as packages (added, updated from one version to another, removed) and then file by file; then approve and publish. From the command line:

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
  ghcr.io/binchitects/mirror-every-repo:1.1.0 \
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
| pip | `pip install --index-url https://vault:8080/pypi/simple/ <package>`, or `index-url` in `pip.conf`. |
| npm | `npm config set registry https://vault:8080/npm/`. Use `npm install --no-audit`: the vault has no audit service. |
| dotnet | `dotnet nuget add source https://vault:8080/nuget/v3/index.json --name vault`, and remove the default source. Restore works; searching for packages does not. |
| docker | `docker pull vault:8080/hub/library/alpine:3.20`. The image name is `<vault>/<repo>/<upstream name>`. |
| dnf | A file in `/etc/yum.repos.d/` with `baseurl=https://vault:8080/rocky/`, `gpgcheck=1` and the distribution's `gpgkey`. Install with the distribution's own repositories switched off: `dnf --disablerepo='*' --enablerepo=vault install <package>`. |
| mvn | `~/.m2/settings.xml` with a mirror of `*` at `https://vault:8080/maven`. |
| cargo | `.cargo/config.toml`: `[source.crates-io] replace-with = "vault"` and `[registries.vault] index = "sparse+https://vault:8080/crates/index/"`. |
| go | `go env -w GOPROXY=https://vault:8080/go GOSUMDB=off` |
| helm | `helm repo add vault https://vault:8080/charts` |
| files | `curl -O https://vault:8080/tools/<path>` |

On plain HTTP, clients need to be told to accept it: `--trusted-host vault` for pip, `--allow-insecure-connections` for the dotnet source, `insecure-registries` in Docker's `daemon.json`, `--plain-http` for `helm pull oci://`. apt, dnf, npm, mvn (with a mirror you configure yourself), cargo, go and helm accept `http://` URLs as they are.

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

### Sign-in through LDAP

People can sign in to the panel with their directory account (OpenLDAP, Active Directory, FreeIPA). Give the server the directory and say which groups get which role:

```text
mer serve ... \
    --ldap-url ldap://directory.example.org:389 --ldap-starttls \
    --ldap-bind-dn "cn=mirror,ou=services,dc=example,dc=org" --ldap-bind-password-file /keys/ldap-password \
    --ldap-user-base "ou=people,dc=example,dc=org" \
    --ldap-group-base "ou=groups,dc=example,dc=org" \
    --ldap-admin-group mirror-admins --ldap-operator-group mirror-operators --ldap-viewer-group staff
```

- The user is looked up with `--ldap-user-filter` (default `(uid={user})`; for Active Directory `(sAMAccountName={user})`), then the password is checked by signing in to the directory as that user. The password is not stored and not logged.
- A group is named by its `cn` or by its full DN; each option can be given several times. The highest role wins. Someone whose password is right but who is in none of the groups is refused.
- Groups come from the user's `memberOf` attribute and, with `--ldap-group-base`, from a search with `--ldap-group-filter` (default: `member`, `uniqueMember` or `memberUid`).
- The role is read from the directory at every sign-in. A session that is already open lasts up to 12 hours; to shut someone out at once, disable the user in the panel (Access page).
- Local users are checked first and keep working when the directory cannot be reached. Keep one local administrator for that case.
- Use `ldaps://` or `--ldap-starttls`; with plain `ldap://` the server says at start-up that passwords cross the network in clear. For a directory certificate from your own CA, start the server with the environment variable `SSL_CERT_FILE` pointing at that CA.
- Every option can also be set through the environment: `MER_LDAP_URL`, `MER_LDAP_BIND_DN`, and so on (`mer serve --help` lists them).
- User names are lower-case letters, digits, `.`, `_` and `-`.

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
| pip | `index-url = https://token:<token>@vault:8080/pypi/simple/` |
| npm | `.npmrc`: `//vault:8080/npm/:_authToken=<token>` |
| dotnet | `dotnet nuget add source ... --username any --password <token> --store-password-in-clear-text` |
| docker | `docker login vault:8080` with any user name and the token as password |
| dnf | `username=token` and `password=<token>` in the `.repo` file |
| mvn | `<server>` in `settings.xml` with the id of the mirror, any user name, and the token as password |
| cargo | `credential-provider = ["cargo:token"]` under `[registries.vault]`, and `token = "Bearer <token>"` under `[registries.vault]` in `~/.cargo/credentials.toml` |
| go | `~/.netrc`: `machine vault login go password <token>` |
| helm | `helm repo add vault https://vault:8080/charts --username any --password <token>` |

The apt and cargo flows are tested end to end with real clients. The others rely on the same Basic and Bearer handling, which is covered by the server's tests.

## Day-to-day operation

```text
mer status                       # one-screen summary of both transfer and repo state
mer wishlist                     # what clients asked for and did not get
mer audit                        # recent audit entries, and a check of the whole chain
mer fsck --deep                  # re-hash every stored file against its digest
```

**Wishlist.** Requests for packages, projects or images that are not mirrored are counted. The Wishlist page of the panel shows, for each item, what to add to the sync command on the collector (`-r flask`, `-a org.slf4j:slf4j-api:2.0.13`), and offers the list as a text file to carry out. Operators can dismiss items that are not going to be mirrored, or clear the list. (apt and dnf clients do not ask the server for packages missing from the index, so their wishes do not appear.)

**How things are going.** The Overview and Traffic pages chart downloads, data sent, requests for missing items and storage used over the last 24 hours, 7 days, 30 days or year. Each repository shows how old its published content is. To be warned when a repository has not been updated for too long:

```text
mer repo stale debian 14         # report it as out of date when the content is older than 14 days ("off" to stop)
```

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
| pip: `is not a trusted or secure host` | pip refuses plain HTTP indexes | Serve with TLS, or pass `--trusted-host` |
| mvn: `was not found in ... during a previous attempt` | Maven remembers that an artifact was missing | After the missing artifact has been mirrored and published, run `mvn -U ...` once |
| mvn: a plugin `or one of its dependencies could not be resolved` | A build plugin, or something it needs, is not mirrored | Add the plugin with `-a group:artifact:version`; the Wishlist page names what was asked for |
| cargo: `no matching package named ...` | The crate, or the version a `Cargo.lock` pins, is not mirrored | Add it with `-p`, or give the lock file with `--lockfile` |
| go: `verifying module: ... sum.golang.org` | The client is trying to reach the checksum database | `go env -w GOSUMDB=off` on the client |
| dnf: `No match for argument` | The package is not in the mirrored repository | Add it with `-p` on the collector |
| dnf: `GPG check FAILED` | The client does not have the distribution's key, or the package is from elsewhere | Point `gpgkey` at the distribution's key file |
| Sign-in answers "the directory cannot be asked right now" | The LDAP server cannot be reached, or its certificate is not trusted | The reason is in the audit log (`mer audit`). Local users can still sign in |
| Sync warns `depends on X, which upstream does not provide` | A dependency could not be satisfied in that upstream | Usually harmless for optional items; otherwise add the upstream or component that has it |
| The panel says no user exists yet | A fresh vault has no accounts | `mer user add admin --role admin` on the vault host |
