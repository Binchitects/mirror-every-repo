#!/usr/bin/env bash
# Install mirror-every-repo on this machine, as a vault (inside the air gap)
# or a collector (on the internet side). Run as root:
#
#   curl -fsSL https://github.com/Binchitects/mirror-every-repo/releases/latest/download/install.sh | bash -s -- vault
#   bash install.sh collector
#
# Running it again upgrades the binary and keeps all data and settings.
#
# Options:
#   --version vX.Y.Z    release to install (default: the latest)
#   --binary PATH       install this mer binary instead of downloading one
#                       (for machines without internet access: copy the
#                       release archive across and pass the extracted mer)
#   --data DIR          data directory (default /var/lib/mer)
#   --listen ADDR       vault: address for package clients and the panel (default 0.0.0.0:8443)
#   --admin-listen ADDR vault: put the panel on its own address instead; the
#                       panel can then also import bundles (from $DATA/files)
#   --panel ADDR        collector: address of its web panel (default 127.0.0.1:8081;
#                       reach it with an SSH tunnel)
#   --hostname NAME     vault: name in the generated TLS certificate (default: this host's name)
#   --tls-cert FILE --tls-key FILE
#                       vault: use this certificate instead of generating one
#   --no-tls            vault: plain HTTP (clients then need extra settings; see the guide)
#   --trust PUBKEY      vault: trust bundles signed by this collector key (repeatable)
#   --no-start          install everything but do not start the service
#   --uninstall         remove the service and the program (data is kept)
#   --purge             with --uninstall: also delete the data directory and keys
set -euo pipefail

REPO="Binchitects/mirror-every-repo"
ROLE="" VERSION="" BINARY="" DATA="/var/lib/mer" LISTEN="0.0.0.0:8443" ADMIN_LISTEN="" PANEL="127.0.0.1:8081" HOSTNAME_="" TLS_CERT="" TLS_KEY=""
NO_TLS=0 NO_START=0 UNINSTALL=0 PURGE=0
TRUST=()
LIB=/usr/local/lib/mer CONF=/etc/mer

say() { printf '\033[1m==>\033[0m %s\n' "$*"; }
warn() { printf '\033[33mwarning:\033[0m %s\n' "$*" >&2; }
die() { printf '\033[31merror:\033[0m %s\n' "$*" >&2; exit 1; }
host_name() { hostname -f 2>/dev/null || hostname 2>/dev/null || cat /proc/sys/kernel/hostname; }
host_ips() { hostname -I 2>/dev/null || ip -4 -o addr show scope global 2>/dev/null | awk '{sub("/.*","",$4); print $4}'; }
usage() { sed -n '2,/^set -euo/p' "$0" 2>/dev/null | sed '$d; s/^# \{0,1\}//'; exit "${1:-0}"; }

while [ $# -gt 0 ]; do
  case "$1" in
    vault|collector) ROLE="$1" ;;
    --version) VERSION="${2:?}"; shift ;;
    --binary) BINARY="${2:?}"; shift ;;
    --data) DATA="${2:?}"; shift ;;
    --listen) LISTEN="${2:?}"; shift ;;
    --admin-listen) ADMIN_LISTEN="${2:?}"; shift ;;
    --panel) PANEL="${2:?}"; shift ;;
    --hostname) HOSTNAME_="${2:?}"; shift ;;
    --tls-cert) TLS_CERT="${2:?}"; shift ;;
    --tls-key) TLS_KEY="${2:?}"; shift ;;
    --no-tls) NO_TLS=1 ;;
    --trust) TRUST+=("${2:?}"); shift ;;
    --no-start) NO_START=1 ;;
    --uninstall) UNINSTALL=1 ;;
    --purge) PURGE=1 ;;
    -h|--help) usage 0 ;;
    *) die "unknown argument: $1 (try --help)" ;;
  esac
  shift
done

[ "$(id -u)" = 0 ] || die "run this as root (sudo bash install.sh ...)"
[ "$(uname -s)" = Linux ] || die "only Linux is supported"
command -v useradd >/dev/null 2>&1 && command -v runuser >/dev/null 2>&1 || die "this installer needs useradd and runuser (Debian, Ubuntu, RHEL, Rocky, AlmaLinux, Fedora, SUSE)"
case "$(uname -m)" in x86_64|amd64) ;; *) die "only x86-64 builds are published; build from source on $(uname -m)" ;; esac
HAVE_SYSTEMD=0
if command -v systemctl >/dev/null 2>&1 && [ -d /run/systemd/system ]; then HAVE_SYSTEMD=1; fi

# ------------------------------------------------------------- uninstall
if [ "$UNINSTALL" = 1 ]; then
  if [ "$HAVE_SYSTEMD" = 1 ]; then
    for u in mer-vault.service mer-panel.service mer-sync.timer mer-sync.service; do
      systemctl disable --now "$u" >/dev/null 2>&1 || true
      rm -f "/etc/systemd/system/$u"
    done
    systemctl daemon-reload
  fi
  rm -rf "$LIB" /usr/local/bin/mer /etc/profile.d/mer.sh
  if [ "$PURGE" = 1 ]; then
    rm -rf "$DATA" "$CONF"
    userdel mer >/dev/null 2>&1 || true
    say "removed mirror-every-repo, its data ($DATA) and settings ($CONF)"
  else
    say "removed the program and services; data in $DATA and settings in $CONF are kept (--purge deletes them)"
  fi
  exit 0
fi

[ -n "$ROLE" ] || usage 1

# ------------------------------------------------------------- packages
pkg_install() {
  if command -v apt-get >/dev/null 2>&1; then
    DEBIAN_FRONTEND=noninteractive apt-get update -qq && DEBIAN_FRONTEND=noninteractive apt-get install -y -qq "$@" >/dev/null
  elif command -v dnf >/dev/null 2>&1; then dnf install -y -q "$@" >/dev/null
  elif command -v yum >/dev/null 2>&1; then yum install -y -q "$@" >/dev/null
  elif command -v zypper >/dev/null 2>&1; then zypper -q -n install "$@" >/dev/null
  else die "please install: $*"
  fi
}
need=()
command -v tar >/dev/null || need+=(tar)
command -v gpg >/dev/null || need+=(gnupg2)
command -v gpgv >/dev/null || need+=(gpgv)
command -v sha256sum >/dev/null || need+=(coreutils)
# curl downloads the release, and checks that the vault answers.
command -v curl >/dev/null || need+=(curl ca-certificates)
[ "$ROLE" = vault ] && [ "$NO_TLS" = 0 ] && [ -z "$TLS_CERT" ] && ! command -v openssl >/dev/null && need+=(openssl)
if [ ${#need[@]} -gt 0 ]; then
  # Package names differ a little between distributions.
  if command -v apt-get >/dev/null 2>&1; then need=("${need[@]/gnupg2/gnupg}"); fi
  if ! command -v apt-get >/dev/null 2>&1; then need=("${need[@]/gpgv/gnupg2}"); fi
  say "installing ${need[*]}"
  pkg_install "${need[@]}"
fi

# ------------------------------------------------------------- the program
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT
if [ -n "$BINARY" ]; then
  [ -f "$BINARY" ] || die "$BINARY does not exist"
  cp "$BINARY" "$TMP/mer"
else
  if [ -z "$VERSION" ]; then
    VERSION="$(curl -fsSL "https://api.github.com/repos/$REPO/releases/latest" | sed -n 's/.*"tag_name": *"\([^"]*\)".*/\1/p' | head -1)"
    [ -n "$VERSION" ] || die "cannot find the latest release of $REPO (pass --version, or --binary on machines without internet)"
  fi
  NAME="mer-${VERSION}-linux-x86_64"
  URL="https://github.com/$REPO/releases/download/$VERSION/$NAME.tar.gz"
  say "downloading $NAME"
  curl -fsSL -o "$TMP/$NAME.tar.gz" "$URL" || die "cannot download $URL"
  curl -fsSL -o "$TMP/$NAME.tar.gz.sha256" "$URL.sha256" || die "cannot download the checksum of $URL"
  (cd "$TMP" && sha256sum -c --quiet "$NAME.tar.gz.sha256") || die "the download does not match its checksum"
  tar -xzf "$TMP/$NAME.tar.gz" -C "$TMP"
  cp "$TMP/$NAME/mer" "$TMP/mer"
fi
chmod 755 "$TMP/mer"
"$TMP/mer" --version >/dev/null 2>&1 || die "the mer binary does not run on this machine"
INSTALLED="$("$TMP/mer" --version)"

id mer >/dev/null 2>&1 || useradd --system --user-group --home-dir "$DATA" --no-create-home --shell /usr/sbin/nologin mer
install -d -m 755 "$LIB"
install -m 755 "$TMP/mer" "$LIB/mer"
# `mer` for administrators: runs as the service user, on this machine's data.
cat > /usr/local/bin/mer <<EOF
#!/bin/sh
# Installed by mirror-every-repo's install.sh. Runs mer as the mer user.
export MER_DATA="\${MER_DATA:-$DATA}"
if [ "\$(id -u)" = 0 ]; then exec runuser -u mer -- $LIB/mer "\$@"; fi
exec $LIB/mer "\$@"
EOF
chmod 755 /usr/local/bin/mer
echo "export MER_DATA=$DATA" > /etc/profile.d/mer.sh
install -d -m 750 -o mer -g mer "$DATA"
install -d -m 750 -o root -g mer "$CONF"
MER() { runuser -u mer -- "$LIB/mer" --data "$DATA" "$@"; }
MER init >/dev/null
# The one directory the web panel may read and write files in (exports,
# imports, lockfiles), and where command-line exports go too.
install -d -m 750 -o mer -g mer "$DATA/files"
say "installed $INSTALLED in $LIB, data in $DATA"
# Sign-in through a company directory: settings the services read, kept
# across upgrades (the installer never rewrites this file).
if [ ! -f "$CONF/ldap.env" ]; then
  cat > "$CONF/ldap.env" <<'LDAPEOF'
# Sign-in to the web panel through an LDAP directory (Active Directory,
# OpenLDAP, FreeIPA). Local users such as admin keep working and are checked
# first. Uncomment and fill in, then restart the service:
#   sudo systemctl restart mer-vault     (or mer-panel on a collector)
# Groups map to roles; a user in none of them cannot sign in.
#MER_LDAP_URL=ldaps://dc1.example.org:636
# For an ldap:// URL, upgrade to TLS before anything is sent:
#MER_LDAP_STARTTLS=true
# The CA that signed the directory's certificate (PEM), if it is a company CA:
#MER_LDAP_CA_FILE=/etc/mer/ldap-ca.pem
# The account that looks users up, and a file (mode 640, group mer) with its password:
#MER_LDAP_BIND_DN=cn=mirror,ou=service,dc=example,dc=org
#MER_LDAP_BIND_PASSWORD_FILE=/etc/mer/ldap-bind-password
#MER_LDAP_USER_BASE=ou=people,dc=example,dc=org
# How a user is found. Active Directory: (sAMAccountName={user})
#MER_LDAP_USER_FILTER=(uid={user})
# Where groups are; without it, the memberOf attribute of the user is read:
#MER_LDAP_GROUP_BASE=ou=groups,dc=example,dc=org
#MER_LDAP_ADMIN_GROUP=mirror-admins
#MER_LDAP_OPERATOR_GROUP=mirror-operators
#MER_LDAP_VIEWER_GROUP=mirror-viewers
LDAPEOF
  chgrp mer "$CONF/ldap.env" && chmod 640 "$CONF/ldap.env"
fi

# The first administrator of the web panel, with a generated password.
first_admin() {
  PASSWORD=""
  if [ -z "$(MER user list 2>/dev/null)" ]; then
    PASSWORD="$(MER user add admin --role admin | sed -n 's/^password (shown once): //p')"
    umask 077 && printf '%s\n' "$PASSWORD" > "$CONF/admin-password" && umask 022
  fi
}
show_admin() {
  if [ -n "$PASSWORD" ]; then
    echo "  Sign in as    admin"
    echo "  Password      $PASSWORD"
    echo "                (also in $CONF/admin-password; delete that file after you change the password)"
  elif [ -f "$CONF/admin-password" ]; then
    echo "  Sign in as    admin (first password in $CONF/admin-password, unless you changed it)"
  fi
}

# ------------------------------------------------------------- vault
if [ "$ROLE" = vault ]; then
  SERVE=(serve --listen "$LISTEN" --quiet)
  # With the panel on its own address it can also import bundles.
  [ -n "$ADMIN_LISTEN" ] && SERVE+=(--admin-listen "$ADMIN_LISTEN" --jobs --files "$DATA/files")
  SCHEME=https
  if [ "$NO_TLS" = 1 ]; then
    SCHEME=http
  else
    install -d -m 750 -o root -g mer "$CONF/tls"
    if [ -n "$TLS_CERT" ]; then
      [ -n "$TLS_KEY" ] || die "--tls-cert needs --tls-key"
      install -m 644 "$TLS_CERT" "$CONF/tls/cert.pem"
      install -m 640 -g mer "$TLS_KEY" "$CONF/tls/key.pem"
    elif [ ! -f "$CONF/tls/cert.pem" ]; then
      NAME_="${HOSTNAME_:-$(host_name)}"
      SAN="DNS:$NAME_,DNS:localhost,IP:127.0.0.1"
      for ip in $(host_ips); do case "$ip" in *:*) ;; *) SAN="$SAN,IP:$ip" ;; esac; done
      openssl req -x509 -newkey rsa:3072 -nodes -days 3650 -subj "/CN=$NAME_" -addext "subjectAltName=$SAN" \
        -keyout "$CONF/tls/key.pem" -out "$CONF/tls/cert.pem" >/dev/null 2>&1 || die "openssl could not create a certificate"
      chgrp mer "$CONF/tls/key.pem" && chmod 640 "$CONF/tls/key.pem" && chmod 644 "$CONF/tls/cert.pem"
      say "generated a self-signed certificate for $NAME_ ($CONF/tls/cert.pem); clients must trust it, or replace it with --tls-cert/--tls-key"
    fi
    SERVE+=(--tls-cert "$CONF/tls/cert.pem" --tls-key "$CONF/tls/key.pem")
  fi
  if [ ${#TRUST[@]} -gt 0 ]; then
    MER key trust "$DATA/trust.txt" "${TRUST[@]}" >/dev/null
    say "trusting bundles from ${#TRUST[@]} collector key(s)"
  fi
  first_admin
  if [ "$HAVE_SYSTEMD" = 1 ]; then
    cat > /etc/systemd/system/mer-vault.service <<EOF
[Unit]
Description=mirror-every-repo vault (package server and web panel)
Documentation=https://github.com/$REPO/blob/main/docs/GUIDE.md
After=network-online.target
Wants=network-online.target

[Service]
User=mer
Group=mer
Environment=MER_DATA=$DATA
EnvironmentFile=-$CONF/ldap.env
ExecStart=$LIB/mer ${SERVE[*]}
Restart=on-failure
RestartSec=2
NoNewPrivileges=yes
ProtectSystem=strict
ProtectHome=read-only
ReadWritePaths=$DATA
PrivateTmp=yes
PrivateDevices=yes
ProtectKernelTunables=yes
ProtectKernelModules=yes
ProtectControlGroups=yes
RestrictSUIDSGID=yes
LockPersonality=yes

[Install]
WantedBy=multi-user.target
EOF
    systemctl daemon-reload
    if [ "$NO_START" = 0 ]; then
      systemctl enable -q mer-vault.service
      systemctl restart mer-vault.service
      PORT="${LISTEN##*:}"
      for _ in $(seq 1 30); do
        curl -fsk "$SCHEME://127.0.0.1:$PORT/healthz" >/dev/null 2>&1 && break
        sleep 0.5
      done
      curl -fsk "$SCHEME://127.0.0.1:$PORT/healthz" >/dev/null 2>&1 || die "the vault did not come up; see: journalctl -u mer-vault"
      say "the vault is running (systemctl status mer-vault)"
    fi
  else
    warn "no systemd here; start the vault yourself: runuser -u mer -- $LIB/mer --data $DATA ${SERVE[*]}"
  fi
  HOST="${HOSTNAME_:-$(host_name)}"
  PANEL_PORT="${ADMIN_LISTEN:-$LISTEN}"
  cat <<EOF

mirror-every-repo vault is ready.

  Web panel     $SCHEME://$HOST:${PANEL_PORT##*:}/ui/
EOF
  show_admin
  cat <<EOF

Next steps
  1. Trust your collector, unless you passed --trust (the collector prints its key when you install it):
       sudo mer key trust $DATA/trust.txt <collector public key>
  2. Import what the collector exported, from a mounted disk:
       sudo mer import media /mnt/usb
     then review and publish in the web panel.
  3. Point clients at $SCHEME://$HOST:${LISTEN##*:}/<repo>/ ("Client setup" in the panel shows the exact lines).
EOF
fi

# ------------------------------------------------------------- collector
if [ "$ROLE" = collector ]; then
  install -d -m 700 -o mer -g mer "$DATA/keys"

  if [ ! -f "$DATA/keys/bundle.key" ]; then
    MER key gen "$DATA/keys/bundle.key" >/dev/null
    say "generated the bundle signing key ($DATA/keys/bundle.key)"
  fi
  if [ ! -d "$DATA/keys/gnupg" ]; then
    MER apt keygen --gnupghome "$DATA/keys/gnupg" >/dev/null
    say "generated the apt site key ($DATA/keys/gnupg)"
  fi
  PUB="$(MER key pub "$DATA/keys/bundle.key")"
  if [ ! -f "$CONF/sync.sh" ]; then
    cat > "$CONF/sync.sh" <<'EOF'
#!/bin/sh
# What this collector mirrors. Run by mer-sync.service (see `systemctl list-timers`),
# or by hand: sudo sh /etc/mer/sync.sh
# Add one repository per line, then the export at the end carries every change.
set -e
# mer repo add pypi --format pypi 2>/dev/null || true
# mer pypi sync pypi --python 3.12 -r requests -r flask
# mer repo add npm --format npm 2>/dev/null || true
# mer npm sync npm -p express
# mer repo add rocky --format rpm 2>/dev/null || true
# mer rpm sync rocky -p nginx --upstream https://dl.rockylinux.org/pub/rocky/9/BaseOS/x86_64/os/ --upstream https://dl.rockylinux.org/pub/rocky/9/AppStream/x86_64/os/

# Seal one bundle with everything that changed (nothing happens when nothing did).
mer export seal --key /var/lib/mer/keys/bundle.key
EOF
    chmod 750 "$CONF/sync.sh" && chgrp mer "$CONF/sync.sh"
  fi
  first_admin
  if [ "$HAVE_SYSTEMD" = 1 ]; then
    # The collector's web panel: repositories, sources, syncs, seal and export.
    # It reaches the internet and the files directory, so it listens on
    # loopback by default.
    cat > /etc/systemd/system/mer-panel.service <<EOF
[Unit]
Description=mirror-every-repo collector web panel and job runner
After=network-online.target
Wants=network-online.target

[Service]
User=mer
Group=mer
Environment=MER_DATA=$DATA
EnvironmentFile=-$CONF/ldap.env
ExecStart=$LIB/mer serve --listen $PANEL --jobs --files $DATA/files --bundle-key $DATA/keys/bundle.key --apt-gnupghome $DATA/keys/gnupg --quiet
Restart=on-failure
RestartSec=2
NoNewPrivileges=yes
ProtectSystem=strict
ProtectHome=read-only
ReadWritePaths=$DATA
PrivateTmp=yes

[Install]
WantedBy=multi-user.target
EOF
    cat > /etc/systemd/system/mer-sync.service <<EOF
[Unit]
Description=mirror-every-repo collector: sync upstreams and seal a bundle
After=network-online.target
Wants=network-online.target

[Service]
Type=oneshot
User=mer
Group=mer
Environment=MER_DATA=$DATA
Environment=PATH=$LIB:/usr/bin:/bin
ExecStart=/bin/sh $CONF/sync.sh
NoNewPrivileges=yes
ProtectSystem=strict
ReadWritePaths=$DATA
PrivateTmp=yes
EOF
    cat > /etc/systemd/system/mer-sync.timer <<EOF
[Unit]
Description=Run the mirror-every-repo collector sync daily

[Timer]
OnCalendar=daily
RandomizedDelaySec=1h
Persistent=true

[Install]
WantedBy=timers.target
EOF
    systemctl daemon-reload
    if [ "$NO_START" = 0 ]; then
      systemctl enable -q --now mer-sync.timer
      systemctl enable -q mer-panel.service
      systemctl restart mer-panel.service
      for _ in $(seq 1 30); do curl -fs "http://$PANEL/healthz" >/dev/null 2>&1 && break; sleep 0.5; done
      curl -fs "http://$PANEL/healthz" >/dev/null 2>&1 || die "the panel did not come up; see: journalctl -u mer-panel"
    fi
  fi
  cat <<EOF

mirror-every-repo collector is ready.

  Public key of this collector (give it to the vault):
    $PUB

  Web panel     http://$PANEL/ui/  (from another machine: ssh -L ${PANEL##*:}:$PANEL <this host>)
EOF
  show_admin
  cat <<EOF

Next steps
  1. On the vault:  sudo mer key trust $DATA/trust.txt $PUB
     (or install the vault with: --trust $PUB)
  2. In the panel's Jobs tab: create repositories, set their sources, sync, seal, export,
     and add a schedule to sync and seal on its own. Or on the command line: say what to mirror in $CONF/sync.sh and run it
     (sudo sh $CONF/sync.sh); it also runs daily (systemctl list-timers mer-sync.timer).
  3. Exports land in $DATA/files; copy them to a disk to carry across:
       sudo mer export media --seq <n> --out $DATA/files/bundle-<n>
       sudo cp -r $DATA/files/bundle-<n> /mnt/usb/
EOF
fi
