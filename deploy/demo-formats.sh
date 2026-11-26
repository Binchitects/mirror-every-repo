#!/usr/bin/env bash
# End-to-end demo of the language formats: mirror real packages on the
# collector, carry them across, and install them with stock clients that have
# no internet access.
set -euo pipefail
# If a step stops the script, say which command it was.
trap 'echo "STOPPED at line $LINENO: $BASH_COMMAND" >&2' ERR
export MSYS_NO_PATHCONV=1
export COMPOSE_PROFILES=formats
cd "$(dirname "$0")/.."

C="docker compose exec -T collector"
V="docker compose exec -T vault"
PY="docker compose exec -T pyclient"
NODE="docker compose exec -T nodeclient"
NET="docker compose exec -T dotnetclient"
RS="docker compose exec -T rustclient"
GO="docker compose exec -T goclient"
HELM="docker compose exec -T helmclient"
RPM="docker compose exec -T rpmclient"
APK="docker compose exec -T apkclient"
RB="docker compose exec -T rubyclient"
RBB="docker compose exec -T rubybuild"
PHP="docker compose exec -T phpclient"
PHPB="docker compose exec -T phpbuild"
MB="docker compose exec -T mavenbuild"
MVN="docker compose exec -T mavenclient"
step() { printf '\n=== %s ===\n' "$*"; }
ok()   { printf '  OK: %s\n' "$*"; }
fail() { printf '  FAILED: %s\n' "$*"; exit 1; }
SEQ=0
# Seal whatever changed on the collector and import it on the vault.
carry() {
  SEQ=$((SEQ + 1))
  $C mer export seal --key /keys/bundle.key | tail -1
  $C mer export media --seq "$SEQ" --out "/media/b$SEQ" --volume-size 512M >/dev/null
  $V mer import media "/media/b$SEQ" --auto-publish | grep -v "already up to date"
}

step "1. build the image and start the nodes"
docker compose down -v --remove-orphans >/dev/null 2>&1 || true
docker compose up -d --build 2>&1 | tail -3
docker compose exec -T -u root collector chown mer:mer /media

step "2. network isolation"
$C timeout 5 bash -c '</dev/tcp/1.1.1.1/443' 2>/dev/null && ok "collector can reach the internet" || fail "collector needs internet for this demo"
$V timeout 5 bash -c '</dev/tcp/1.1.1.1/443' 2>/dev/null && fail "vault can reach the internet" || ok "vault cannot reach the internet"
$PY timeout 5 bash -c '</dev/tcp/1.1.1.1/443' 2>/dev/null && fail "python client can reach the internet" || ok "python client cannot reach the internet"

step "3. pairing"
$C mer init
$V mer init
PUB=$($C mer key gen /keys/bundle.key | tr -d '\r\n')
$V mer key trust /data/trust.txt "$PUB"
docker compose exec -T -d vault sh -c 'mer serve --listen 0.0.0.0:8080 > /tmp/http.log 2>&1'

step "4. PyPI: mirror 'requests' for CPython 3.12 from pypi.org"
$C mer repo add pypi --format pypi
$C mer pypi sync pypi --python 3.12 -r requests
carry

step "5. PyPI: install with a stock pip that can only reach the vault"
PIP="pip install --disable-pip-version-check --no-cache-dir --index-url http://vault:8080/pypi/simple/ --trusted-host vault"
$PY sh -c 'python -c "import requests" 2>/dev/null' && fail "requests is already installed" || ok "requests is not installed yet"
$PY sh -c "$PIP requests" 2>&1 | tail -3 | sed 's/^/  | /'
OUT=$($PY python -c 'import requests, urllib3, idna, certifi, charset_normalizer; print("requests", requests.__version__)' | tr -d '\r')
case "$OUT" in "requests "*) ok "installed from the vault and imports with all its dependencies: $OUT" ;; *) fail "import failed: $OUT" ;; esac
if $PY sh -c "$PIP flask" >/dev/null 2>&1; then fail "flask was installable although it is not mirrored"
else ok "a project that is not mirrored cannot be installed"; fi
$V mer wishlist | sed 's/^/  | /'
case "$($V mer wishlist)" in *simple/flask*) ok "the request for flask is on the wishlist" ;; *) fail "flask is not on the wishlist" ;; esac

step "6. PyPI: act on the wishlist; only the new files cross"
$C mer pypi sync pypi --python 3.12 -r requests -r flask | tail -3
carry
$PY sh -c "$PIP flask" 2>&1 | tail -1 | sed 's/^/  | /'
OUT=$($PY python -c 'import flask; print(flask.Flask("x").name)' | tr -d '\r')
[ "$OUT" = "x" ] && ok "flask installs and runs after the next bundle" || fail "flask did not run"

step "7. npm: mirror 'express' from registry.npmjs.org"
$C mer repo add npm --format npm
$C mer npm sync npm -p express
carry

step "8. npm: install with a stock npm that can only reach the vault"
$NODE timeout 5 bash -c '</dev/tcp/1.1.1.1/443' 2>/dev/null && fail "node client can reach the internet" || ok "node client cannot reach the internet"
NPM="npm install --registry http://vault:8080/npm/ --no-audit --no-fund --loglevel=error"
$NODE sh -c "mkdir -p /tmp/app && cd /tmp/app && npm init -y >/dev/null && $NPM express" 2>&1 | tail -2 | sed 's/^/  | /'
OUT=$($NODE sh -c 'cd /tmp/app && node -e "const e = require(\"express\"); const a = e(); console.log(typeof a.listen, require(\"express/package.json\").version)"' | tr -d '\r')
case "$OUT" in "function "*) ok "express installed from the vault and loads: version ${OUT#function }" ;; *) fail "express did not load: $OUT" ;; esac
COUNT=$($NODE sh -c 'ls /tmp/app/node_modules | wc -l' | tr -d '\r ')
ok "$COUNT packages in node_modules, all from the vault"
$NODE sh -c 'cd /tmp/app && grep -c "\"resolved\": \"http://vault:8080/npm/" package-lock.json' | sed 's/^/  lockfile entries resolved to the vault: /'
if $NODE sh -c 'cd /tmp/app && grep -q "registry.npmjs.org" package-lock.json'; then fail "the lockfile mentions the upstream registry"
else ok "nothing in the lockfile points at the upstream registry"; fi
if $NODE sh -c "cd /tmp/app && $NPM left-pad" >/dev/null 2>&1; then fail "left-pad was installable although it is not mirrored"
else ok "a package that is not mirrored cannot be installed"; fi
case "$($V mer wishlist)" in *"npm  left-pad"*) ok "the request for left-pad is on the wishlist" ;; *) $V mer wishlist; fail "left-pad is not on the wishlist" ;; esac

step "9. NuGet: mirror 'Serilog.Sinks.Console' for net8.0 from nuget.org"
$C mer repo add nuget --format nuget
$C mer nuget sync nuget --framework net8.0 -p Serilog.Sinks.Console
carry

step "10. NuGet: restore, build and run with a stock dotnet SDK that can only reach the vault"
$NET timeout 5 bash -c '</dev/tcp/1.1.1.1/443' 2>/dev/null && fail "dotnet client can reach the internet" || ok "dotnet client cannot reach the internet"
$NET sh -c 'rm -rf /tmp/app && dotnet new console -n app -o /tmp/app --no-restore >/dev/null'
# The vault is the only package source; a version of "*" means "newest the feed has".
$NET sh -c 'cat > /tmp/app/nuget.config' <<'XML'
<configuration>
  <packageSources>
    <clear />
    <add key="vault" value="http://vault:8080/nuget/v3/index.json" allowInsecureConnections="true" />
  </packageSources>
</configuration>
XML
$NET sh -c 'cat > /tmp/app/app.csproj' <<'XML'
<Project Sdk="Microsoft.NET.Sdk">
  <PropertyGroup>
    <OutputType>Exe</OutputType>
    <TargetFramework>net8.0</TargetFramework>
    <ImplicitUsings>enable</ImplicitUsings>
  </PropertyGroup>
  <ItemGroup>
    <PackageReference Include="Serilog.Sinks.Console" Version="*" />
  </ItemGroup>
</Project>
XML
$NET sh -c 'cat > /tmp/app/Program.cs' <<'CS'
using Serilog;
Log.Logger = new LoggerConfiguration().WriteTo.Console().CreateLogger();
Log.Information("hello from the vault");
Log.CloseAndFlush();
CS
$NET sh -c 'cd /tmp/app && dotnet restore' 2>&1 | tail -2 | sed 's/^/  | /'
OUT=$($NET sh -c 'cd /tmp/app && dotnet run --no-restore' 2>&1 | tr -d '\r')
case "$OUT" in *"hello from the vault"*) ok "restored from the vault, built and ran: ${OUT##*] }" ;; *) echo "$OUT"; fail "the program did not run" ;; esac
$NET sh -c 'ls /root/.nuget/packages' | tr -d '\r' | tr '\n' ' ' | sed 's/^/  packages restored: /'; echo
if $NET sh -c 'cd /tmp/app && dotnet add package Dapper --version 2.1.35' >/dev/null 2>&1; then fail "Dapper was restorable although it is not mirrored"
else ok "a package that is not mirrored cannot be restored"; fi
case "$($V mer wishlist)" in *"nuget  dapper"*) ok "the request for Dapper is on the wishlist" ;; *) $V mer wishlist; fail "Dapper is not on the wishlist" ;; esac

step "11. Cargo: mirror 'serde' (with derive), 'serde_json' and 'anyhow' from crates.io"
$C mer repo add cargo --format cargo
$C mer cargo sync cargo -c 'serde@1:derive' -c serde_json -c anyhow
carry

step "12. Cargo: build and run with a stock cargo that can only reach the vault"
$RS timeout 5 bash -c '</dev/tcp/1.1.1.1/443' 2>/dev/null && fail "rust client can reach the internet" || ok "rust client cannot reach the internet"
$RS sh -c 'rm -rf /tmp/app && cargo new --quiet /tmp/app && mkdir -p /tmp/app/.cargo'
# Source replacement: everything that would come from crates.io comes from the vault.
$RS sh -c 'cat > /tmp/app/.cargo/config.toml' <<'TOML'
[source.crates-io]
replace-with = "vault"

[source.vault]
registry = "sparse+http://vault:8080/cargo/"
TOML
$RS sh -c 'cat >> /tmp/app/Cargo.toml' <<'TOML'
serde = { version = "1", features = ["derive"] }
serde_json = "1"
anyhow = "1"
TOML
$RS sh -c 'cat > /tmp/app/src/main.rs' <<'RS'
#[derive(serde::Serialize, serde::Deserialize)]
struct Greeting { text: String }

fn main() -> anyhow::Result<()> {
    let g: Greeting = serde_json::from_str(r#"{"text":"hello from the vault"}"#)?;
    println!("{}", serde_json::to_string(&Greeting { text: g.text })?);
    Ok(())
}
RS
$RS sh -c 'cd /tmp/app && cargo build --quiet' 2>&1 | tail -3 | sed 's/^/  | /'
OUT=$($RS sh -c 'cd /tmp/app && ./target/debug/app' 2>&1 | tr -d '\r')
case "$OUT" in *"hello from the vault"*) ok "fetched from the vault, built and ran: $OUT" ;; *) echo "$OUT"; fail "the program did not run" ;; esac
$RS sh -c 'cd /tmp/app && grep -c "^name = " Cargo.lock' | tr -d '\r' | sed 's/^/  packages in Cargo.lock: /'
if $RS sh -c 'cd /tmp/app && cargo add --quiet rand@0.8 && cargo build --quiet' >/dev/null 2>&1; then fail "rand was buildable although it is not mirrored"
else ok "a crate that is not mirrored cannot be used"; fi
case "$($V mer wishlist)" in *"cargo  rand"*) ok "the request for rand is on the wishlist" ;; *) $V mer wishlist; fail "rand is not on the wishlist" ;; esac

step "13. Go: mirror 'cobra' and 'BurntSushi/toml' from proxy.golang.org, checked against sum.golang.org"
$C mer repo add go --format go
$C mer go sync go -m github.com/spf13/cobra@v1.8.1 -m github.com/BurntSushi/toml@v1.4.0
carry

step "14. Go: tidy, build and run with a stock go that can only reach the vault"
$GO timeout 5 bash -c '</dev/tcp/1.1.1.1/443' 2>/dev/null && fail "go client can reach the internet" || ok "go client cannot reach the internet"
$GO sh -c 'rm -rf /tmp/app && mkdir /tmp/app && cd /tmp/app && go mod init example.com/app >/dev/null 2>&1'
$GO sh -c 'cat > /tmp/app/main.go' <<'GOSRC'
package main

import (
	"fmt"

	"github.com/BurntSushi/toml"
	"github.com/spf13/cobra"
)

func main() {
	var doc struct{ Text string }
	toml.Decode(`text = "hello from the vault"`, &doc)
	cmd := &cobra.Command{Use: "app", Run: func(*cobra.Command, []string) { fmt.Println(doc.Text) }}
	cmd.Execute()
}
GOSRC
$GO sh -c 'cd /tmp/app && go get github.com/spf13/cobra@v1.8.1 github.com/BurntSushi/toml@v1.4.0 && go mod tidy' 2>&1 | tail -3 | sed 's/^/  | /'
OUT=$($GO sh -c 'cd /tmp/app && go build -o app . && ./app' 2>&1 | tr -d '\r')
case "$OUT" in *"hello from the vault"*) ok "fetched from the vault, built and ran: $OUT" ;; *) echo "$OUT"; fail "the program did not run" ;; esac
$GO sh -c 'cd /tmp/app && grep -c . go.sum' | tr -d '\r' | sed 's/^/  lines in go.sum: /'
if $GO sh -c 'cd /tmp/app && go get github.com/google/uuid@v1.6.0' >/dev/null 2>&1; then fail "uuid was fetchable although it is not mirrored"
else ok "a module that is not mirrored cannot be fetched"; fi
case "$($V mer wishlist)" in *"go  github.com/google/uuid"*) ok "the request for google/uuid is on the wishlist" ;; *) $V mer wishlist; fail "google/uuid is not on the wishlist" ;; esac

step "15. Helm: mirror the 'cert-manager' chart from charts.jetstack.io"
$C mer repo add helm --format helm
$C mer helm sync helm --upstream https://charts.jetstack.io -c cert-manager
carry

step "16. Helm: add the vault as a chart repository and render the chart with a stock helm"
$HELM timeout 5 sh -c 'nc -z 1.1.1.1 443' 2>/dev/null && fail "helm client can reach the internet" || ok "helm client cannot reach the internet"
$HELM helm repo add vault http://vault:8080/helm/ 2>&1 | sed 's/^/  | /'
$HELM helm search repo vault/ 2>&1 | sed 's/^/  | /'
$HELM sh -c 'cd /tmp && rm -f cert-manager-*.tgz && helm pull vault/cert-manager && ls cert-manager-*.tgz' | tr -d '\r' | sed 's/^/  pulled: /'
COUNT=$($HELM sh -c 'helm template cm vault/cert-manager --namespace cert-manager | grep -c "^kind: Deployment"' | tr -d '\r')
[ "$COUNT" -ge 3 ] && ok "chart from the vault renders: $COUNT Deployments" || fail "chart did not render ($COUNT)"
if $HELM helm pull vault/trust-manager >/dev/null 2>&1; then fail "trust-manager was pullable although it is not mirrored"
else ok "a chart that is not mirrored cannot be pulled"; fi

step "17. RPM: mirror 'tmux' and 'jq' from Rocky Linux 9 BaseOS and AppStream, metadata checked against the Rocky key"
$C mer repo add rocky --format rpm
# The collector checks repomd.xml against the same key the clients trust.
docker compose cp rpmclient:/etc/pki/rpm-gpg/RPM-GPG-KEY-Rocky-9 /tmp/mer-rocky-key.asc >/dev/null
docker compose cp /tmp/mer-rocky-key.asc collector:/tmp/rocky.asc >/dev/null
rm -f /tmp/mer-rocky-key.asc
$C mer rpm sync rocky --keyring /tmp/rocky.asc \
  --upstream https://dl.rockylinux.org/pub/rocky/9/BaseOS/x86_64/os/ \
  --upstream https://dl.rockylinux.org/pub/rocky/9/AppStream/x86_64/os/ \
  -p tmux -p jq | grep -v "^  WARNING" | tail -3
carry

step "18. RPM: install with a stock dnf that can only reach the vault, package signatures checked"
$RPM timeout 5 bash -c '</dev/tcp/1.1.1.1/443' 2>/dev/null && fail "rpm client can reach the internet" || ok "rpm client cannot reach the internet"
$RPM sh -c 'cat > /etc/yum.repos.d/vault.repo' <<'INI'
[vault]
name=vault
baseurl=http://vault:8080/rocky/
gpgcheck=1
gpgkey=file:///etc/pki/rpm-gpg/RPM-GPG-KEY-Rocky-9
INI
$RPM sh -c 'command -v tmux jq' >/dev/null 2>&1 && fail "tmux or jq is already installed" || ok "tmux and jq are not installed yet"
$RPM dnf -y -q --disablerepo='*' --enablerepo=vault install tmux jq 2>&1 | tail -3 | sed 's/^/  | /'
OUT=$($RPM sh -c 'tmux -V; echo "{\"text\":\"hello from the vault\"}" | jq -r .text' 2>&1 | tr -d '\r')
case "$OUT" in *"hello from the vault"*) ok "installed from the vault with signatures checked: $(echo "$OUT" | tr '\n' ' ')" ;; *) echo "$OUT"; fail "tmux or jq did not run" ;; esac
if $RPM dnf -y -q --disablerepo='*' --enablerepo=vault install vim-enhanced >/dev/null 2>&1; then fail "vim was installable although it is not mirrored"
else ok "a package that is not mirrored cannot be installed"; fi

step "19. Alpine: mirror 'curl', 'fzf' (community) and what a stock Alpine image has installed, from Alpine 3.24 main and community, checked against Alpine's keys"
$C mer repo add alpine --format apk
# The collector trusts the keys an Alpine machine has, and mirrors what the
# client image has installed too, so that apk upgrade works from the vault.
rm -rf /tmp/mer-apk-keys
docker compose cp apkclient:/etc/apk/keys /tmp/mer-apk-keys >/dev/null
docker compose cp /tmp/mer-apk-keys collector:/tmp/apk-keys >/dev/null
rm -rf /tmp/mer-apk-keys
$APK apk info 2>/dev/null | $C sh -c 'cat > /tmp/apk-installed'
$C mer apk sync alpine --keys /tmp/apk-keys --upstream https://dl-cdn.alpinelinux.org/alpine/v3.24/main \
  --upstream https://dl-cdn.alpinelinux.org/alpine/v3.24/community \
  -p curl -p fzf --packages-from /tmp/apk-installed | grep -v "^  WARNING" | tail -3
carry

step "20. Alpine: install and upgrade with a stock apk that can only reach the vault, with the keys it already has"
$APK timeout 5 wget -q -O /dev/null https://dl-cdn.alpinelinux.org/ 2>/dev/null && fail "apk client can reach the internet" || ok "apk client cannot reach the internet"
$APK sh -c 'printf "http://vault:8080/alpine/main\nhttp://vault:8080/alpine/community\n" > /etc/apk/repositories'
$APK sh -c 'command -v curl' >/dev/null 2>&1 && fail "curl is already installed" || ok "curl is not installed yet"
$APK apk update 2>&1 | tail -1 | sed 's/^/  | /'
$APK apk add curl 2>&1 | tail -2 | sed 's/^/  | /'
OUT=$($APK curl --version 2>&1 | tr -d '\r')
OUT=${OUT%%$'\n'*}
case "$OUT" in curl\ *) ok "installed from the vault, index signed by Alpine: $OUT" ;; *) echo "$OUT"; fail "curl did not run" ;; esac
$APK apk add fzf 2>&1 | tail -1 | sed 's/^/  | /'
$APK fzf --version >/dev/null 2>&1 && ok "fzf installed from the vault's copy of community" || fail "fzf did not install from community"
$APK apk upgrade 2>&1 | tail -1 | sed 's/^/  | /'
$APK apk upgrade --simulate 2>&1 | grep -q "Upgrading" && fail "apk upgrade left packages behind" || ok "apk upgrade brought the image up to date from the vault"
if $APK apk add jq >/dev/null 2>&1; then fail "jq was installable although it is not mirrored"
else ok "a package that is not mirrored cannot be installed"; fi
$APK apk del jq >/dev/null 2>&1 || true
$V mer wishlist | grep -q "alpine  jq-" && ok "jq is on the vault's wishlist" || { $V mer wishlist; fail "jq is not on the wishlist"; }

step "21. Maven: build once on a connected machine, mirror exactly what it used, plus guava with its dependencies"
# The project, written on both machines: commons-lang3 on the connected one,
# commons-lang3 and guava inside the air gap.
project() {
  $1 sh -c 'rm -rf /tmp/app && mkdir -p /tmp/app/src/main/java/demo'
  $1 sh -c 'cat > /tmp/app/pom.xml' <<XML
<project xmlns="http://maven.apache.org/POM/4.0.0">
  <modelVersion>4.0.0</modelVersion>
  <groupId>demo</groupId><artifactId>app</artifactId><version>1.0</version>
  <properties><maven.compiler.release>21</maven.compiler.release><project.build.sourceEncoding>UTF-8</project.build.sourceEncoding></properties>
  <dependencies>
    <dependency><groupId>org.apache.commons</groupId><artifactId>commons-lang3</artifactId><version>3.17.0</version></dependency>
    $2
  </dependencies>
</project>
XML
  $1 sh -c 'cat > /tmp/app/src/main/java/demo/Main.java' <<JAVA
package demo;
public class Main {
  public static void main(String[] a) {
    System.out.println(org.apache.commons.lang3.StringUtils.capitalize("hello from the vault") $3);
  }
}
JAVA
}
GUAVA='<dependency><groupId>com.google.guava</groupId><artifactId>guava</artifactId><version>33.3.1-jre</version></dependency>'
project "$MB" "" ""
$MB sh -c 'cd /tmp/app && mvn -B -q package -Dmaven.repo.local=/tmp/m2' 2>&1 | tail -3 | sed 's/^/  | /'
$MB sh -c 'find /tmp/m2 -name "*.jar" | wc -l' | tr -d '\r' | sed 's/^/  jars the connected build used: /'
rm -rf /tmp/mer-demo-m2 && docker compose cp mavenbuild:/tmp/m2 /tmp/mer-demo-m2 >/dev/null
docker compose cp /tmp/mer-demo-m2 collector:/tmp/m2 >/dev/null && rm -rf /tmp/mer-demo-m2
$C mer repo add maven --format maven
$C mer maven sync maven --from-m2 /tmp/m2 -a com.google.guava:guava:33.3.1-jre | grep -v "^  WARNING" | tail -3
carry

step "22. Maven: build and run with a stock mvn that can only reach the vault"
$MVN timeout 5 bash -c '</dev/tcp/1.1.1.1/443' 2>/dev/null && fail "maven client can reach the internet" || ok "maven client cannot reach the internet"
$MVN sh -c 'mkdir -p /root/.m2 && cat > /root/.m2/settings.xml' <<'XML'
<settings>
  <mirrors>
    <mirror><id>vault</id><mirrorOf>*</mirrorOf><url>http://vault:8080/maven/</url><blocked>false</blocked></mirror>
  </mirrors>
</settings>
XML
project "$MVN" "$GUAVA" '+ " " + com.google.common.collect.ImmutableList.of(1, 2, 3)'
if ! $MVN sh -c 'cd /tmp/app && mvn -B -q package' > /tmp/mer-demo-mvn.log 2>&1; then
  grep -E "ERROR|Could not|Failed" /tmp/mer-demo-mvn.log | head -20 | sed 's/^/  | /'
  $V sh -c 'grep "/maven/" /tmp/http.log | grep -v " 200 " | tail -20' | sed 's/^/  vault | /'
  $V mer status | sed 's/^/  vault | /'
  fail "mvn package failed"
fi
rm -f /tmp/mer-demo-mvn.log
OUT=$($MVN sh -c 'cd /tmp/app && java -cp "target/classes:$(find /root/.m2 -name "*.jar" | tr "\n" ":")" demo.Main' 2>&1 | tr -d '\r')
case "$OUT" in *"Hello from the vault [1, 2, 3]"*) ok "built from the vault and ran: $OUT" ;; *) echo "$OUT"; fail "the program did not run" ;; esac
$V sh -c 'grep "/maven/" /tmp/http.log | grep -c " 404 " || true' | tr -d '\r' | sed 's/^/  maven requests answered 404 (checksum and metadata probes): /'
$MVN sh -c "cd /tmp/app && sed -i 's#</dependencies>#<dependency><groupId>org.slf4j</groupId><artifactId>slf4j-api</artifactId><version>2.0.16</version></dependency></dependencies>#' pom.xml"
if $MVN sh -c 'cd /tmp/app && mvn -B -q package' >/dev/null 2>&1; then fail "slf4j was usable although it is not mirrored"
else ok "an artifact that is not mirrored cannot be used"; fi
case "$($V mer wishlist)" in *"org.slf4j:slf4j-api:2.0.16"*) ok "the request for slf4j-api is on the wishlist" ;; *) $V mer wishlist; fail "slf4j-api is not on the wishlist" ;; esac

step "23. RubyGems: mirror 'sinatra' and 'puma', and exactly what a connected machine's Gemfile.lock names"
# A project locked on a connected machine: nokogiri, with its build for this platform.
$RBB sh -c 'mkdir -p /app && cd /app && printf "source \"https://rubygems.org\"\ngem \"nokogiri\"\ngem \"rack\"\n" > Gemfile && bundle lock >/dev/null 2>&1 \
  && others=$(sed -n "/^PLATFORMS/,/^$/p" Gemfile.lock | sed "1d;/^$/d;s/ //g" | grep -vx "x86_64-linux-gnu\|ruby" | tr "\n" " ") \
  && { [ -z "$others" ] || bundle lock --remove-platform $others >/dev/null; }' || fail "the connected machine could not lock the project"
docker compose cp rubybuild:/app/Gemfile.lock /tmp/mer-demo-Gemfile.lock >/dev/null
docker compose cp /tmp/mer-demo-Gemfile.lock collector:/tmp/Gemfile.lock >/dev/null
rm -f /tmp/mer-demo-Gemfile.lock
RUBYV=$($RB ruby -e 'print RUBY_VERSION' | tr -d '\r')
$C mer repo add gems --format gem
$C mer gem sync gems --ruby "$RUBYV" -g sinatra -g puma --lockfile /tmp/Gemfile.lock | grep -v "^  WARNING" | tail -3
carry

step "24. RubyGems: gem install and bundle install with stock Ruby tools that can only reach the vault"
$RB timeout 5 ruby -rsocket -e 'TCPSocket.new("1.1.1.1", 443)' 2>/dev/null && fail "ruby client can reach the internet" || ok "ruby client cannot reach the internet"
$RB gem install --no-document --clear-sources --source http://vault:8080/gems/ sinatra 2>&1 | tail -1 | sed 's/^/  | /'
OUT=$($RB ruby -e 'require "sinatra/base"; puts Sinatra::VERSION' 2>&1 | tr -d '\r')
case "$OUT" in [0-9]*) ok "gem install from the vault: sinatra $OUT" ;; *) echo "$OUT"; fail "sinatra did not load" ;; esac
$RB sh -c 'mkdir -p /app && cat > /app/Gemfile' <<'RUBY'
source "http://vault:8080/gems/"
gem "nokogiri"
gem "rack"
RUBY
docker compose cp rubybuild:/app/Gemfile.lock /tmp/mer-demo-Gemfile.lock >/dev/null
docker compose cp /tmp/mer-demo-Gemfile.lock rubyclient:/app/Gemfile.lock >/dev/null
rm -f /tmp/mer-demo-Gemfile.lock
$RB sh -c 'cd /app && sed -i "s|remote: https://rubygems.org/|remote: http://vault:8080/gems/|" Gemfile.lock && bundle install 2>&1 | tail -1' | sed 's/^/  | /'
OUT=$($RB sh -c 'cd /app && bundle exec ruby -e "require \"nokogiri\"; puts Nokogiri::VERSION; puts Nokogiri::HTML(\"<p>hello from the vault</p>\").text"' 2>&1 | tr -d '\r')
case "$OUT" in *"hello from the vault"*) ok "bundle install of the locked project, precompiled nokogiri $(echo "$OUT" | head -1)" ;; *) echo "$OUT"; fail "the locked project did not install" ;; esac
# Bundler keeps a copy of the vault's index; a new snapshot changes it.
$C mer gem sync gems --ruby "$RUBYV" -g sinatra -g puma -g rake --lockfile /tmp/Gemfile.lock | grep -v "^  WARNING" | tail -1
carry
$RB sh -c 'mkdir -p /app2 && cd /app2 && printf "source \"http://vault:8080/gems/\"\ngem \"rake\"\ngem \"rack\"\n" > Gemfile && bundle install 2>&1 | tail -1' | sed 's/^/  | /'
$RB sh -c 'cd /app2 && bundle exec rake --version' >/dev/null 2>&1 && ok "after a new snapshot, Bundler's kept index is replaced, not patched: rake installs" || fail "bundle install failed after the index changed"
if $RB gem install --no-document --clear-sources --source http://vault:8080/gems/ rails >/dev/null 2>&1; then fail "rails was installable although it is not mirrored"
else ok "a gem that is not mirrored cannot be installed"; fi
case "$($V mer wishlist)" in *"gems  rails"*) ok "rails is on the wishlist" ;; *) $V mer wishlist; fail "rails is not on the wishlist" ;; esac

step "25. Composer: mirror 'monolog/monolog', and exactly what a connected machine's composer.lock names"
$PHPB sh -c 'mkdir -p /proj && cd /proj && composer require --no-interaction --no-install --no-audit symfony/console:^7 >/dev/null 2>&1' || fail "the connected machine could not lock the project"
docker compose cp phpbuild:/proj/composer.lock /tmp/mer-demo-composer.lock >/dev/null
docker compose cp /tmp/mer-demo-composer.lock collector:/tmp/composer.lock >/dev/null
PHPV=$($PHP php -r 'echo PHP_VERSION;' | tr -d '\r')
$C mer repo add php --format composer
$C mer composer sync php --php "$PHPV" -p monolog/monolog --lockfile /tmp/composer.lock | grep -v "^  WARNING" | tail -3
carry

step "26. Composer: require and install with a stock composer that can only reach the vault"
$PHP timeout 5 php -r 'exit(@fsockopen("1.1.1.1", 443) ? 0 : 1);' 2>/dev/null && fail "php client can reach the internet" || ok "php client cannot reach the internet"
$PHP composer config -g repo.packagist composer http://vault:8080/php/
$PHP composer config -g secure-http false
$PHP sh -c 'mkdir -p /app1 && cd /app1 && composer require --no-interaction monolog/monolog 2>&1 | grep -i "installing\|error" | tail -2' | sed 's/^/  | /'
OUT=$($PHP sh -c 'cd /app1 && php -r '"'"'require "vendor/autoload.php"; $l = new Monolog\Logger("demo"); $l->pushHandler(new Monolog\Handler\StreamHandler("php://stdout")); $l->info("hello from the vault");'"'"'' 2>&1 | tr -d '\r')
case "$OUT" in *"hello from the vault"*) ok "composer require from the vault: monolog logs" ;; *) echo "$OUT"; fail "monolog did not load" ;; esac
# A lock made upstream names GitHub's archives; composer update against the
# vault keeps the locked versions and points the lock at the vault.
$PHP mkdir -p /app2
docker compose cp phpbuild:/proj/composer.json /tmp/mer-demo-composer.json >/dev/null
docker compose cp /tmp/mer-demo-composer.json phpclient:/app2/composer.json >/dev/null
docker compose cp /tmp/mer-demo-composer.lock phpclient:/app2/composer.lock >/dev/null
rm -f /tmp/mer-demo-composer.lock /tmp/mer-demo-composer.json
LOCKED=$($PHPB sh -c 'cd /proj && composer show --locked symfony/console 2>/dev/null | sed -n "s/^versions : \* //p"' | tr -d '\r')
$PHP sh -c 'cd /app2 && composer update --no-interaction 2>&1 | grep -i "installing\|error" | tail -2' | sed 's/^/  | /'
GOT=$($PHP sh -c 'cd /app2 && composer show symfony/console 2>/dev/null | sed -n "s/^versions : \* //p"' | tr -d '\r')
OUT=$($PHP sh -c 'cd /app2 && php -r '"'"'require "vendor/autoload.php"; echo (new Symfony\Component\Console\Application("demo", "1.0"))->getLongVersion();'"'"'' 2>&1 | tr -d '\r')
[ -n "$LOCKED" ] && [ "$GOT" = "$LOCKED" ] && case "$OUT" in *demo*) true ;; *) false ;; esac \
  && ok "the locked project installs at the locked version (symfony/console $GOT)" || { echo "locked $LOCKED, got $GOT: $OUT"; fail "the locked project did not install as locked"; }
if $PHP sh -c 'mkdir -p /app3 && cd /app3 && composer require --no-interaction guzzlehttp/guzzle' >/dev/null 2>&1; then fail "guzzle was installable although it is not mirrored"
else ok "a package that is not mirrored cannot be installed"; fi
case "$($V mer wishlist)" in *"php  guzzlehttp/guzzle"*) ok "guzzle is on the wishlist" ;; *) $V mer wishlist; fail "guzzle is not on the wishlist" ;; esac

step "27. Containers: mirror 'alpine:3.20' from Docker Hub"
$C mer repo add hub --format oci
SYNC=$($C mer oci sync hub -i library/alpine:3.20 | tr -d '\r')
echo "$SYNC"
UPSTREAM_DIGEST=$(echo "$SYNC" | awk '/->/ {print $NF}')
carry

step "28. Containers: pull and run with a real Docker engine, from the vault only"
# The vault itself has no published ports (its network is internal), so a
# second server process on the same vault data is exposed on localhost for
# the Docker engine of this machine. The image is pulled under a name that
# exists nowhere else, so it can only have come from the vault.
docker rm -f mer-registry >/dev/null 2>&1 || true
docker run -d --name mer-registry -p 127.0.0.1:5005:8080 -v mer-demo_vault-data:/data mer:dev serve --quiet --no-admin >/dev/null
sleep 2
IMG=localhost:5005/hub/library/alpine:3.20
docker rmi -f "$IMG" >/dev/null 2>&1 || true
docker pull "$IMG" 2>&1 | tail -3 | sed 's/^/  | /'
PULLED=$(docker image inspect "$IMG" --format '{{index .RepoDigests 0}}' | tr -d '\r')
[ "${PULLED##*@}" = "$UPSTREAM_DIGEST" ] && ok "pulled digest equals the upstream digest ($UPSTREAM_DIGEST)" || fail "digest differs: $PULLED vs $UPSTREAM_DIGEST"
OUT=$(docker run --rm --network none "$IMG" cat /etc/alpine-release | tr -d '\r')
case "$OUT" in 3.20*) ok "the image runs: Alpine $OUT" ;; *) fail "image did not run: $OUT" ;; esac
if docker pull localhost:5005/hub/library/busybox:latest >/dev/null 2>&1; then fail "busybox was pullable although it is not mirrored"
else ok "an image that is not mirrored cannot be pulled"; fi
case "$($V mer wishlist)" in *"images/library/busybox:latest"*) ok "the request for busybox is on the wishlist" ;; *) $V mer wishlist; fail "busybox is not on the wishlist" ;; esac
docker rmi -f "$IMG" >/dev/null 2>&1 || true
docker rm -f mer-registry >/dev/null 2>&1 || true

step "29. vault: state and integrity"
$V mer status
$V mer fsck --deep
$V sh -c 'grep -c " 200 " /tmp/http.log || true' | sed 's/^/  requests answered with 200: /'

step "DONE: all checks passed"
