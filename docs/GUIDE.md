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
docker pull ghcr.io/binchitects/mirror-every-repo:1.0.0
```

The image's entry point is `mer`. It runs as user id 10001 and expects two volumes: `/data` for the store and database, `/keys` for key files.

A long-running container per side, with a shell alias, keeps the rest of this guide short:

```bash
docker run -d --name mer --restart unless-stopped \
  -v mer-data:/data -v mer-keys:/keys -v /mnt/usb:/media \
  -p 8080:8080 -p 9000:9000/udp \
  --entrypoint sleep ghcr.io/binchitects/mirror-every-repo:1.0.0 infinity
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

### Plain files

```text
mer repo add tools --format raw
mer repo sync-dir tools /path/to/directory
```

The repo becomes a copy of the directory tree.

### Contacting other hosts

The collector only connects to the upstream host you name, plus the hosts that upstream is known to use (`files.pythonhosted.org` for PyPI, `auth.docker.io` for Docker Hub). If a private upstream keeps files or tokens on another host, allow it with `--allow-host`.

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

Imported snapshots are sealed but not visible to clients. In the web panel, the Overview lists what is waiting; "Review changes" shows what publishing would add, change and remove; then approve and publish. From the command line:

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
  ghcr.io/binchitects/mirror-every-repo:1.0.0 \
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
| files | `curl -O https://vault:8080/tools/<path>` |

On plain HTTP, clients need to be told to accept it: `--trusted-host vault` for pip, `--allow-insecure-connections` for the dotnet source, `insecure-registries` in Docker's `daemon.json`. apt and npm accept `http://` URLs as they are.

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

The apt flow is tested end to end with a real client. The others rely on the same Basic and Bearer handling, which is covered by the server's tests.

## Day-to-day operation

```text
mer status                       # one-screen summary of both transfer and repo state
mer wishlist                     # what clients asked for and did not get
mer audit                        # recent audit entries, and a check of the whole chain
mer fsck --deep                  # re-hash every stored file against its digest
```

**Wishlist.** Requests for packages, projects or images that are not mirrored are counted. Use the list to decide what to add to the next sync. (apt clients do not ask the server for packages missing from the index, so apt wishes do not appear.)

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
| Sync warns `depends on X, which upstream does not provide` | A dependency could not be satisfied in that upstream | Usually harmless for optional items; otherwise add the upstream or component that has it |
| The panel says no user exists yet | A fresh vault has no accounts | `mer user add admin --role admin` on the vault host |
