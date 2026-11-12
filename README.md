# mirror-every-repo

A package repository mirror for air-gapped networks, in Rust. One binary (`mer`), two roles:

- **collector** (internet side): mirrors content, seals signed delta bundles.
- **vault** (air-gapped side): verifies and imports bundles, quarantines them until approved, serves the published view.

Bundles cross the gap on **removable media** (USB, disk, DVD) or through a **data diode** (one-way UDP with error correction). The collector never needs an answer from the vault.

Status: built and tested end to end for six formats: apt, PyPI, npm, NuGet (.NET), container images and raw files. Stock `apt-get`, `pip`, `npm`, `dotnet` and `docker` install from the vault on machines with no internet access, and snapshots are reviewed and published from a web panel. See [docs/STATUS.md](docs/STATUS.md) and the target design in [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md).

## Build and test

No local Rust toolchain is needed, only Docker.

```bash
bash scripts/dcargo.sh test --workspace
```

```bash
bash deploy/demo.sh
```

```bash
bash deploy/demo-apt.sh
```

```bash
bash deploy/demo-formats.sh
```

The first demo starts a collector and a vault in two containers (the vault on a network with no route out), moves one bundle by media and one through the diode link, and checks the result at every step. The second mirrors real packages from deb.debian.org and installs them with `apt-get` on a third container that can reach only the vault. The third does the same for Python (`pip`), JavaScript (`npm`), .NET (`dotnet restore`) and container images (`docker pull`).

## Mirror a Debian repository

```text
# collector, once
mer apt keygen                                   # site key; prints the public key
mer repo add debian --format apt

# collector, each time
mer apt sync debian --upstream http://deb.debian.org/debian \
    --suite bookworm --package nginx --package postgresql-15
mer export seal --key /keys/bundle.key
mer export media --seq <n> --out /mnt/usb        # or: mer export diode ...

# vault
mer import media /mnt/usb
mer approve debian <snapshot> && mer publish debian <snapshot>
mer serve --listen 0.0.0.0:8080

# clients inside the air gap (site key copied to them once)
deb [signed-by=/etc/apt/keyrings/site.asc] http://vault:8080/debian bookworm main
```

## Mirror Python, npm, .NET and container images

```text
# collector
mer repo add pypi --format pypi
mer pypi sync pypi --python 3.12 -r requests -r "django>=4.2,<5"

mer repo add npm --format npm
mer npm sync npm -p express -p "@types/node@20"

mer repo add nuget --format nuget
mer nuget sync nuget --framework net8.0 -p Serilog.Sinks.Console -p Newtonsoft.Json@13.0.3

mer repo add hub --format oci
mer oci sync hub -i library/alpine:3.20 -i library/nginx:1.27 --platform linux/amd64

mer export seal --key /keys/bundle.key          # one bundle carries all repos

# clients inside the air gap
pip install --index-url http://vault:8080/pypi/simple/ --trusted-host vault requests
npm install --registry http://vault:8080/npm/ --no-audit express
dotnet nuget add source http://vault:8080/nuget/v3/index.json --name vault --allow-insecure-connections
docker pull vault:8080/hub/library/alpine:3.20
```

The panel shows the exact client lines for each repository under "Client setup". What clients ask for and the vault does not have appears on the wishlist (`mer wishlist`, or the Wishlist page).

## Web panel and access control

```text
mer user add admin --role admin                  # first administrator; prints a generated password once
mer serve --listen 0.0.0.0:8080 --admin-listen 0.0.0.0:8443 \
    --tls-cert /keys/tls.crt --tls-key /keys/tls.key
# panel:    https://vault:8443/ui/
# packages: https://vault:8080/<repo>/

mer repo private debian on                       # downloads now need a token
mer token add build-farm --repo debian           # prints the secret once
# on an apt client, in /etc/apt/auth.conf.d/vault.conf:
#   machine https://vault:8080 login apt password <token>
```

Roles: a viewer can look; an operator can also approve, reject, publish and roll back; an admin can also manage users, tokens and repository settings.

## Use

```text
# both sides, once
mer init
collector$ mer key gen /keys/bundle.key          # prints the public key
collector$ mer key link /keys/link.key           # copy this file to the vault by hand
vault$     mer key trust /data/trust.txt <public key>

# collector
mer repo add tools
mer repo sync-dir tools /path/to/files
mer export seal  --key /keys/bundle.key
mer export media --seq 1 --out /mnt/usb --volume-size fat32
mer export diode --seq 1 --to vault:9000 --link-key /keys/link.key --rate-mbit 100

# vault
mer import media /mnt/usb
mer import diode --listen 0.0.0.0:9000 --link-key /keys/link.key
mer status
mer approve tools 1 && mer publish tools 1
mer get tools path/in/repo
mer rollback tools 1
mer wishlist
mer compact tools 2 && mer gc
mer fsck --deep
mer audit
```
