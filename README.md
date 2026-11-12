# mirror-every-repo

A package repository mirror for air-gapped networks, in Rust. One binary (`mer`), two roles:

- **collector** (internet side): mirrors content, seals signed delta bundles.
- **vault** (air-gapped side): verifies and imports bundles, quarantines them until approved, serves the published view.

Bundles cross the gap on **removable media** (USB, disk, DVD) or through a **data diode** (one-way UDP with error correction). The collector never needs an answer from the vault.

Status: the storage and transfer core, the apt format, HTTP(S) serving, access control and a web panel are built and tested: a stock `apt-get` on a machine with no internet access installs packages from the vault, and snapshots are reviewed and published from the browser. PyPI, npm and OCI are not built yet. See [docs/STATUS.md](docs/STATUS.md) and the target design in [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md).

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

The first demo starts a collector and a vault in two containers (the vault on a network with no route out), moves one bundle by media and one through the diode link, and checks the result at every step. The second mirrors real packages from deb.debian.org and installs them with `apt-get` on a third container that can reach only the vault.

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
