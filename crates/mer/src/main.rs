//! `mer`: one binary for both sides of the air gap.
use anyhow::{anyhow, bail, Context, Result};
use clap::{Parser, Subcommand};
use mer_blob::Expect;
use mer_bundle::{seal_bundle, ApplyState, ImportPolicy, ImportReport, SealOptions, SealedBundle, Vault};
use mer_bundle_format::{SegmentSink, SegmentSource};
use mer_core::{ChannelId, EntryRole};
use mer_crypto::{KeyPair, LinkKey, TrustStore};
use mer_storage::{Lookup, Storage};
use mer_transport_diode::{Receiver, ReceiverConfig, RxEvent, Sender, SenderConfig};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

#[derive(Parser)]
#[command(name = "mer", version, about = "Package repository mirror for air-gapped networks")]
struct Cli {
    /// Data directory (blob store, database, staging).
    #[arg(long, env = "MER_DATA", default_value = "/data", global = true)]
    data: PathBuf,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Create the data directory.
    Init,
    /// Signing keys and the diode link key.
    Key {
        #[command(subcommand)]
        cmd: KeyCmd,
    },
    /// Repositories.
    Repo {
        #[command(subcommand)]
        cmd: RepoCmd,
    },
    /// Approve a quarantined snapshot.
    Approve { repo: String, seq: u64 },
    /// Reject a quarantined snapshot. It can never be published.
    Reject { repo: String, seq: u64 },
    /// Make an approved snapshot the one clients see.
    Publish { repo: String, seq: u64 },
    /// Go back to an older approved snapshot.
    Rollback { repo: String, seq: u64 },
    /// Read a file from the published view of a repo.
    Get {
        repo: String,
        path: String,
        /// Write to this file instead of standard output.
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// Collector side: seal bundles and send them.
    Export {
        #[command(subcommand)]
        cmd: ExportCmd,
    },
    /// Vault side: receive bundles and import them.
    Import {
        #[command(subcommand)]
        cmd: ImportCmd,
    },
    /// Debian/Ubuntu repositories (collector side).
    Apt {
        #[command(subcommand)]
        cmd: AptCmd,
    },
    /// Python package indexes (collector side).
    Pypi {
        #[command(subcommand)]
        cmd: PypiCmd,
    },
    /// npm registries (collector side).
    Npm {
        #[command(subcommand)]
        cmd: NpmCmd,
    },
    /// Container images (collector side).
    Oci {
        #[command(subcommand)]
        cmd: OciCmd,
    },
    /// NuGet feeds for .NET (collector side).
    Nuget {
        #[command(subcommand)]
        cmd: NugetCmd,
    },
    /// Helm chart repositories (collector side).
    Helm {
        #[command(subcommand)]
        cmd: HelmCmd,
    },
    /// Cargo registries for Rust (collector side).
    Cargo {
        #[command(subcommand)]
        cmd: CargoCmd,
    },
    /// Go modules (collector side).
    Go {
        #[command(subcommand)]
        cmd: GoCmd,
    },
    /// Serve published snapshots to package clients, and the web panel.
    Serve {
        /// Listener for package clients (and the panel, unless --admin-listen is given).
        #[arg(long, default_value = "0.0.0.0:8080")]
        listen: String,
        /// Put the web panel on its own listener, away from package clients.
        #[arg(long)]
        admin_listen: Option<String>,
        /// Do not serve the web panel at all.
        #[arg(long, conflicts_with = "admin_listen")]
        no_admin: bool,
        /// PEM certificate chain. With --tls-key, every listener speaks HTTPS.
        #[arg(long, requires = "tls_key")]
        tls_cert: Option<PathBuf>,
        #[arg(long, requires = "tls_cert")]
        tls_key: Option<PathBuf>,
        #[arg(long, default_value_t = 8)]
        threads: usize,
        /// Do not print one line per request.
        #[arg(long)]
        quiet: bool,
    },
    /// Users of the web panel.
    User {
        #[command(subcommand)]
        cmd: UserCmd,
    },
    /// Access tokens for package clients of private repositories.
    Token {
        #[command(subcommand)]
        cmd: TokenCmd,
    },
    /// Repos, channels, bundles and disk use.
    Status,
    /// What clients asked for that the vault does not have.
    Wishlist {
        #[arg(long, default_value_t = 50)]
        limit: u32,
    },
    /// Delete content no retained snapshot refers to.
    Gc {
        /// Only delete content that has been unreferenced for this long.
        #[arg(long, default_value_t = 24)]
        grace_hours: i64,
    },
    /// Retention: forget history of a repo older than snapshot `keep_from`.
    Compact { repo: String, keep_from: u64 },
    /// Check the store against the database.
    Fsck {
        /// Re-hash every blob.
        #[arg(long)]
        deep: bool,
    },
    /// Show the audit log and verify its hash chain.
    Audit {
        #[arg(long, default_value_t = 20)]
        limit: u32,
    },
}

#[derive(Subcommand)]
enum KeyCmd {
    /// Generate a bundle signing key. Prints the public key.
    Gen { file: PathBuf },
    /// Print the public key of a signing key file.
    Pub { file: PathBuf },
    /// Generate a diode link key (shared secret for both ends).
    Link { file: PathBuf },
    /// Write a trust file for the vault from public keys.
    Trust {
        file: PathBuf,
        /// Hex public keys of the collectors to trust.
        #[arg(required = true)]
        pubkeys: Vec<String>,
        #[arg(long, default_value_t = 1)]
        threshold: usize,
    },
}

#[derive(Subcommand)]
enum PypiCmd {
    /// Mirror Python packages and their dependencies into a new snapshot of a pypi repo.
    Sync {
        repo: String,
        /// Simple API of the upstream index (it must answer in JSON, PEP 691).
        #[arg(long, default_value = "https://pypi.org/simple")]
        index_url: String,
        /// What to mirror, as pip would write it: requests, "django>=4.2,<5", "uvicorn[standard]".
        #[arg(long = "requirement", short = 'r', required = true)]
        requirements: Vec<String>,
        /// CPython version used inside the air gap (repeatable), for example 3.11.
        #[arg(long = "python", required = true)]
        pythons: Vec<String>,
        /// CPU architecture as wheel tags spell it.
        #[arg(long, default_value = "x86_64")]
        arch: String,
        /// Accepted Linux wheel families (repeatable): manylinux, musllinux.
        #[arg(long = "libc", default_value = "manylinux")]
        libcs: Vec<String>,
        /// Source archives: never, if-no-wheel or always.
        #[arg(long, default_value = "if-no-wheel")]
        sdists: String,
        /// Allow pre-release versions.
        #[arg(long)]
        pre: bool,
        /// Additional host the collector may download files from (repeatable).
        #[arg(long = "allow-host")]
        allow_hosts: Vec<String>,
    },
}

#[derive(Subcommand)]
enum NpmCmd {
    /// Mirror npm packages and their dependencies into a new snapshot of an npm repo.
    Sync {
        repo: String,
        #[arg(long, default_value = "https://registry.npmjs.org")]
        registry: String,
        /// What to mirror: express, "lodash@^4.17", "@types/node@20", typescript@latest.
        #[arg(long = "package", short = 'p', required = true)]
        packages: Vec<String>,
        /// Platform of the machines inside the air gap, in Node terms.
        #[arg(long, default_value = "linux")]
        os: String,
        #[arg(long, default_value = "x64")]
        cpu: String,
        #[arg(long, default_value = "glibc")]
        libc: String,
        /// Additional host the collector may download tarballs from (repeatable).
        #[arg(long = "allow-host")]
        allow_hosts: Vec<String>,
    },
}

#[derive(Subcommand)]
enum OciCmd {
    /// Mirror container images into a new snapshot of an oci repo.
    Sync {
        repo: String,
        #[arg(long, default_value = "https://registry-1.docker.io")]
        registry: String,
        /// Image to mirror (repeatable): library/alpine:3.20, or name@sha256:... to pin.
        #[arg(long = "image", short = 'i', required = true)]
        images: Vec<String>,
        /// Platform to keep from multi-platform images (repeatable).
        #[arg(long = "platform", default_value = "linux/amd64")]
        platforms: Vec<String>,
        /// Additional host the collector may contact (repeatable), for
        /// example the token service of a private registry.
        #[arg(long = "allow-host")]
        allow_hosts: Vec<String>,
    },
}

#[derive(Subcommand)]
enum HelmCmd {
    /// Mirror charts from a chart repository into a new snapshot of a helm repo.
    /// (Charts kept in an OCI registry are mirrored with `mer oci sync`.)
    Sync {
        repo: String,
        /// The chart repository, as given to `helm repo add`.
        #[arg(long)]
        url: String,
        /// What to mirror (repeatable): ingress-nginx (newest stable),
        /// ingress-nginx@4.15.1, "cert-manager@^1.16".
        #[arg(long = "chart", short = 'c', required = true)]
        charts: Vec<String>,
        /// Let a chart named without a version resolve to a pre-release.
        #[arg(long)]
        pre: bool,
        /// Additional host the collector may contact (repeatable), if the
        /// repository keeps its chart archives on another host.
        #[arg(long = "allow-host")]
        allow_hosts: Vec<String>,
    },
}

#[derive(Subcommand)]
enum CargoCmd {
    /// Mirror crates and their dependencies into a new snapshot of a cargo repo.
    Sync {
        repo: String,
        /// Sparse index of the upstream registry.
        #[arg(long, default_value = "https://index.crates.io")]
        index: String,
        /// What to mirror (repeatable), written as in Cargo.toml: serde
        /// (newest stable), tokio@1.38 (1.38 or compatible), "rand@=0.8.5".
        #[arg(long = "crate", short = 'p')]
        crates: Vec<String>,
        /// A Cargo.lock whose exact versions are mirrored too (repeatable),
        /// so that `cargo build --locked` works inside the air gap.
        #[arg(long = "lockfile")]
        lockfiles: Vec<PathBuf>,
        /// Named crates are mirrored with every feature switched on, so that
        /// any build can be served. With this, only their default features
        /// (and those given with --feature) are followed.
        #[arg(long)]
        default_features_only: bool,
        /// A feature of a named crate to follow (repeatable): tokio/full.
        #[arg(long = "feature", short = 'F')]
        features: Vec<String>,
        /// Additional host the collector may contact (repeatable), if the
        /// registry keeps crate files on another host.
        #[arg(long = "allow-host")]
        allow_hosts: Vec<String>,
    },
}

#[derive(Subcommand)]
enum GoCmd {
    /// Mirror Go modules and what they require into a new snapshot of a go repo.
    Sync {
        repo: String,
        /// The upstream module proxy.
        #[arg(long, default_value = "https://proxy.golang.org")]
        proxy: String,
        /// What to mirror (repeatable): github.com/gin-gonic/gin (latest
        /// release) or github.com/gin-gonic/gin@v1.10.0.
        #[arg(long = "module", short = 'm')]
        modules: Vec<String>,
        /// The go.mod of a project inside the air gap (repeatable): what it
        /// requires is mirrored at exactly the versions it names.
        #[arg(long = "gomod")]
        gomods: Vec<PathBuf>,
        /// The checksum database every file is compared with.
        #[arg(long, default_value = "https://sum.golang.org")]
        sumdb: String,
        /// Keep files as the proxy sends them, without the checksum database
        /// (for a private proxy whose modules the database does not know).
        #[arg(long)]
        no_sumdb: bool,
        /// Additional host the collector may contact (repeatable).
        #[arg(long = "allow-host")]
        allow_hosts: Vec<String>,
    },
}

#[derive(Subcommand)]
enum NugetCmd {
    /// Mirror NuGet packages and their dependencies into a new snapshot of a nuget repo.
    Sync {
        repo: String,
        /// Service index of the upstream feed.
        #[arg(long, default_value = "https://api.nuget.org/v3/index.json")]
        feed: String,
        /// What to mirror (repeatable): Serilog (newest stable),
        /// Newtonsoft.Json@13.0.3, "Foo@[1.0,2.0)".
        #[arg(long = "package", short = 'p', required = true)]
        packages: Vec<String>,
        /// Target framework of the projects inside the air gap (repeatable):
        /// net8.0, netstandard2.0, net48.
        #[arg(long = "framework", required = true)]
        frameworks: Vec<String>,
        /// Let a package named without a version resolve to a pre-release.
        #[arg(long)]
        pre: bool,
        /// Additional host the collector may contact (repeatable), if the
        /// feed keeps packages or metadata on another host.
        #[arg(long = "allow-host")]
        allow_hosts: Vec<String>,
    },
}

#[derive(Subcommand)]
enum UserCmd {
    /// Create a user. Without --password-stdin a password is generated and shown once.
    Add {
        name: String,
        /// viewer, operator or admin. The first user must be an admin.
        #[arg(long, default_value = "viewer")]
        role: String,
        /// Read the password from standard input instead of generating one.
        #[arg(long)]
        password_stdin: bool,
    },
    List,
    /// Set a new password (ends the sessions of that user).
    Passwd {
        name: String,
        #[arg(long)]
        password_stdin: bool,
    },
    Role { name: String, role: String },
    Disable { name: String },
    Enable { name: String },
}

#[derive(Subcommand)]
enum TokenCmd {
    /// Create a token. The secret is shown once.
    Add {
        name: String,
        /// Limit the token to one repository (default: every repository).
        #[arg(long)]
        repo: Option<String>,
    },
    List,
    Revoke { name: String },
}

#[derive(Subcommand)]
enum AptCmd {
    /// Create the site signing key that clients will trust.
    Keygen {
        /// GnuPG home directory to create the key in.
        #[arg(long, default_value = "/keys/gnupg")]
        gnupghome: PathBuf,
        #[arg(long, default_value = "mirror-every-repo site key")]
        name: String,
    },
    /// Mirror packages and their dependencies from an upstream archive into a
    /// new snapshot of an apt repo.
    Sync {
        repo: String,
        /// Upstream archive: an http(s) URL or a directory holding a mirror.
        #[arg(long)]
        upstream: String,
        /// Suite or codename under dists/, for example bookworm.
        #[arg(long)]
        suite: String,
        #[arg(long = "component", default_value = "main")]
        components: Vec<String>,
        #[arg(long = "arch", default_value = "amd64")]
        architectures: Vec<String>,
        /// Package to mirror (repeatable). Dependencies are added automatically.
        #[arg(long = "package", required = true)]
        packages: Vec<String>,
        /// Also follow Recommends.
        #[arg(long)]
        recommends: bool,
        /// Public keys of the upstream archive.
        #[arg(long, default_value = "/usr/share/keyrings/debian-archive-keyring.gpg")]
        keyring: PathBuf,
        #[arg(long, default_value = "/keys/gnupg")]
        gnupghome: PathBuf,
        /// Accept an upstream Release whose Valid-Until has passed.
        #[arg(long)]
        ignore_valid_until: bool,
        #[arg(long, default_value = "mirror-every-repo")]
        origin: String,
    },
}

#[derive(Subcommand)]
enum RepoCmd {
    Add {
        name: String,
        #[arg(long, default_value = "raw")]
        format: String,
    },
    List,
    /// Snapshots of a repo.
    Snapshots { repo: String },
    /// Make a raw repo equal to a directory tree and seal a snapshot.
    SyncDir {
        repo: String,
        dir: PathBuf,
        /// Also approve and publish the new snapshot locally.
        #[arg(long)]
        publish: bool,
    },
    /// Limit the size of a repo (0 removes the limit).
    Quota { repo: String, bytes: String },
    /// Require an access token to download from a repo (on) or not (off).
    Private { repo: String, state: String },
    /// Report a repository as out of date when what is published is older
    /// than this many days ("off" to stop reporting).
    Stale { repo: String, days: String },
}

#[derive(Subcommand)]
enum ExportCmd {
    /// Seal everything that changed since the last bundle.
    Seal {
        #[arg(long, default_value = "0000000000000001")]
        channel: String,
        /// Bundle signing key file.
        #[arg(long)]
        key: PathBuf,
        #[arg(long, default_value = "4M")]
        segment_size: String,
        #[arg(long, default_value_t = 8)]
        step_window: u64,
    },
    /// Write a sealed bundle as volume files for removable media.
    Media {
        #[arg(long, default_value = "0000000000000001")]
        channel: String,
        #[arg(long)]
        seq: u64,
        #[arg(long)]
        out: PathBuf,
        /// Size of one medium: a number with K/M/G suffix, or fat32, dvd, bd25.
        #[arg(long, default_value = "fat32")]
        volume_size: String,
    },
    /// Send a sealed bundle through a data diode (one-way UDP).
    Diode {
        #[arg(long, default_value = "0000000000000001")]
        channel: String,
        #[arg(long)]
        seq: u64,
        /// Destination host:port.
        #[arg(long)]
        to: String,
        #[arg(long)]
        link_key: PathBuf,
        /// Must not exceed what the diode can carry: the excess is lost silently.
        #[arg(long, default_value_t = 100.0)]
        rate_mbit: f64,
        /// How many times to send the bundle (each pass adds new repair data).
        #[arg(long, default_value_t = 2)]
        passes: u16,
        /// Extra repair data per pass, as a fraction.
        #[arg(long, default_value_t = 0.10)]
        overhead: f64,
    },
}

#[derive(Subcommand)]
enum ImportCmd {
    /// Read volume files from a mounted medium.
    Media {
        dir: PathBuf,
        /// Trust file (defaults to <data>/trust.txt).
        #[arg(long)]
        trust: Option<PathBuf>,
        /// Publish imported snapshots without manual approval.
        #[arg(long)]
        auto_publish: bool,
    },
    /// Listen on the receiving end of a data diode.
    Diode {
        #[arg(long, default_value = "0.0.0.0:9000")]
        listen: String,
        #[arg(long)]
        link_key: PathBuf,
        #[arg(long)]
        trust: Option<PathBuf>,
        #[arg(long)]
        auto_publish: bool,
        /// Exit after this many seconds without any packet (default: run forever).
        #[arg(long)]
        idle_exit: Option<u64>,
    },
    /// Retry repos that were waiting (after raising a quota, for example).
    Retry {
        #[arg(long, default_value = "0000000000000001")]
        channel: String,
        #[arg(long)]
        trust: Option<PathBuf>,
        #[arg(long)]
        auto_publish: bool,
    },
}

fn parse_size(s: &str) -> Result<u64> {
    let s = s.trim();
    match s.to_ascii_lowercase().as_str() {
        "fat32" => return Ok(4 * (1 << 30) - 4096),
        "dvd" => return Ok(4_700_000_000),
        "bd25" => return Ok(25_000_000_000),
        _ => {}
    }
    let (num, mult) = match s.chars().last().map(|c| c.to_ascii_uppercase()) {
        Some('K') => (&s[..s.len() - 1], 1u64 << 10),
        Some('M') => (&s[..s.len() - 1], 1 << 20),
        Some('G') => (&s[..s.len() - 1], 1 << 30),
        Some('T') => (&s[..s.len() - 1], 1 << 40),
        _ => (s, 1),
    };
    let n: u64 = num.trim().parse().map_err(|_| anyhow!("cannot read size {s:?}"))?;
    n.checked_mul(mult).ok_or_else(|| anyhow!("size {s:?} is too large"))
}

/// A password from standard input, or a freshly generated one (true = generated).
fn read_or_generate_password(from_stdin: bool) -> Result<(String, bool)> {
    if from_stdin {
        let mut line = String::new();
        std::io::stdin().read_line(&mut line)?;
        let password = line.trim_end_matches(['\r', '\n']).to_string();
        if password.is_empty() {
            bail!("no password on standard input");
        }
        return Ok((password, false));
    }
    use rand::RngCore;
    let mut raw = [0u8; 15];
    rand::rngs::OsRng.fill_bytes(&mut raw);
    Ok((hex::encode(raw), true))
}

fn human(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = bytes as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{bytes} B")
    } else {
        format!("{v:.1} {}", UNITS[u])
    }
}

fn channel(hex: &str) -> Result<ChannelId> {
    ChannelId::from_hex(hex).map_err(|_| anyhow!("channel id must be 16 hex characters, got {hex:?}"))
}

fn open(data: &Path) -> Result<Arc<Storage>> {
    if !data.join("meta.db").exists() {
        bail!("{} is not initialised; run `mer --data {} init` first", data.display(), data.display());
    }
    Ok(Arc::new(Storage::open(data).with_context(|| format!("opening {}", data.display()))?))
}

fn open_vault(data: &Path, trust: Option<PathBuf>, auto_publish: bool) -> Result<Arc<Vault>> {
    let trust_path = trust.unwrap_or_else(|| data.join("trust.txt"));
    let trust = TrustStore::load(&trust_path)
        .with_context(|| format!("loading trust file {} (create it with `mer key trust`)", trust_path.display()))?;
    Ok(Arc::new(Vault::new(open(data)?, trust, ImportPolicy { auto_publish })?))
}

fn print_import(report: &ImportReport) {
    println!(
        "imported bundle {} (id {}): {} blobs, {} new ({})",
        report.seq,
        &report.bundle_id.to_hex()[..16],
        report.blobs_total,
        report.blobs_new,
        human(report.bytes_new)
    );
    for r in &report.repos {
        let what = match r.state {
            ApplyState::Applied => match r.snapshot {
                Some(s) => format!("applied, snapshot {s}"),
                None => "applied, no change".to_string(),
            },
            ApplyState::UpToDate => "already up to date".to_string(),
            ApplyState::Incomplete => format!("INCOMPLETE: {}", r.detail),
            ApplyState::WaitingBase => format!("WAITING for an earlier bundle: {}", r.detail),
            ApplyState::Diverged => format!("DIVERGED: {}", r.detail),
            ApplyState::QuotaBlocked => format!("QUOTA: {}", r.detail),
        };
        println!("  repo {:<20} at bundle {:<4} {what}", r.repo, r.applied_seq);
    }
}

/// Every regular file under `root`, as (repo path, file path). Symlinks are not followed.
fn walk(root: &Path) -> Result<Vec<(String, PathBuf)>> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).with_context(|| format!("reading {}", dir.display()))? {
            let entry = entry?;
            let ty = entry.file_type()?;
            let path = entry.path();
            if ty.is_dir() {
                stack.push(path);
            } else if ty.is_file() {
                let rel = path.strip_prefix(root)?;
                let parts: Vec<&str> = rel
                    .components()
                    .map(|c| c.as_os_str().to_str().ok_or_else(|| anyhow!("non UTF-8 name under {}", dir.display())))
                    .collect::<Result<_>>()?;
                out.push((parts.join("/"), path));
            }
        }
    }
    out.sort();
    Ok(out)
}

fn sync_dir(st: &Storage, repo: &str, dir: &Path, publish: bool) -> Result<()> {
    let files = walk(dir)?;
    let repo_row = st.repo(repo)?;
    let before: BTreeSet<String> = match st.head(repo)? {
        Some(head) => st.meta().read(|t| t.entries_at(repo_row.id, head.seq))?.into_iter().map(|e| e.path).collect(),
        None => BTreeSet::new(),
    };
    let mut writer = st.begin(repo)?;
    let mut bytes = 0u64;
    let mut now: BTreeSet<&str> = BTreeSet::new();
    for (rel, path) in &files {
        let mut f = std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
        let (digest, size) = st.ingest(&mut f, Expect::default())?;
        writer.put(rel, EntryRole::Artifact, &digest)?;
        bytes += size;
        now.insert(rel);
    }
    let mut removed = 0;
    for gone in before.iter().filter(|p| !now.contains(p.as_str())) {
        writer.remove(gone)?;
        removed += 1;
    }
    match writer.seal(None)? {
        None => println!("repo {repo}: no change"),
        Some(snap) => {
            println!(
                "repo {repo}: snapshot {} sealed, {} files ({}), {removed} removed",
                snap.seq,
                files.len(),
                human(bytes)
            );
            if publish {
                st.decide(repo, snap.seq, true, "cli")?;
                st.publish(repo, snap.seq, "cli")?;
                println!("repo {repo}: snapshot {} published", snap.seq);
            }
        }
    }
    Ok(())
}

fn status(st: &Storage) -> Result<()> {
    let (blobs, bytes) = st.meta().read(|t| t.blob_count())?;
    println!("store: {blobs} blobs, {}", human(bytes));
    println!("repos:");
    for repo in st.meta().read(|t| t.repo_list())? {
        let (head, live, usage) =
            st.meta().read(|t| Ok((t.snapshot_head(repo.id)?, t.live(repo.id)?, t.repo_usage(repo.id)?)))?;
        let head = head.map_or("none".to_string(), |h| format!("{} ({})", h.seq, h.state.as_str()));
        let live = live.map_or("nothing published".to_string(), |l| format!("snapshot {}", l.seq));
        let quota = repo.quota_bytes.map_or(String::new(), |q| format!(" of {}", human(q)));
        println!(
            "  {:<20} {:<6} newest {:<14} live: {:<18} {}{quota}",
            repo.name,
            repo.format,
            head,
            live,
            human(usage)
        );
    }
    for ch in st.meta().read(|t| t.channel_list())? {
        println!("channel {}: sealed up to {}, imported up to {}", ch.id, ch.head_seq, ch.imported_seq);
        for b in st.meta().read(|t| t.bundle_import_list(&ch.id))? {
            println!("  bundle {:<4} {:<10} id {}", b.seq, b.state, &b.bundle_id.to_hex()[..16]);
        }
        for repo in st.meta().read(|t| t.repo_list())? {
            if let Some(r) = st.meta().read(|t| t.repo_import_get(&ch.id, repo.id))? {
                let detail = if r.detail.is_empty() { String::new() } else { format!(" ({})", r.detail) };
                println!("  repo {:<20} at bundle {:<4} {}{detail}", repo.name, r.applied_seq, r.state);
            }
        }
    }
    Ok(())
}

/// Let the process end quietly when its output is piped into something that
/// stops reading (`mer status | head`, a pager that is closed early).
///
/// Rust ignores SIGPIPE by default, which turns a closed pipe into a panic in
/// `println!`. Restoring the default action is right for one-shot commands.
/// It must not be done for the server: there, a client that disconnects in
/// the middle of a download must end that one response, not the process.
fn quiet_on_closed_pipe() {
    #[cfg(unix)]
    // SAFETY: called once at startup, before any other thread exists;
    // SIGPIPE and SIG_DFL are valid arguments for signal().
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    if !matches!(cli.cmd, Cmd::Serve { .. } | Cmd::Import { .. }) {
        quiet_on_closed_pipe();
    }
    let data = cli.data;
    match cli.cmd {
        Cmd::Init => {
            Storage::open(&data).with_context(|| format!("creating {}", data.display()))?;
            println!("initialised {}", data.display());
        }
        Cmd::Key { cmd } => match cmd {
            KeyCmd::Gen { file } => {
                let k = KeyPair::generate();
                k.save(&file)?;
                println!("{}", k.public());
            }
            KeyCmd::Pub { file } => println!("{}", KeyPair::load(&file)?.public()),
            KeyCmd::Link { file } => {
                LinkKey::generate().save(&file)?;
                println!("link key written to {}", file.display());
            }
            KeyCmd::Trust { file, pubkeys, threshold } => {
                let keys = pubkeys
                    .iter()
                    .map(|p| mer_core::PubKey::from_hex(p).map_err(|_| anyhow!("not a public key: {p:?}")))
                    .collect::<Result<Vec<_>>>()?;
                if threshold > keys.len() {
                    bail!("threshold {threshold} is higher than the number of keys ({})", keys.len());
                }
                TrustStore::new(keys, threshold).save(&file)?;
                println!("trust file written to {}", file.display());
            }
        },
        Cmd::Repo { cmd } => {
            let st = open(&data)?;
            match cmd {
                RepoCmd::Add { name, format } => {
                    st.create_repo(&name, &format)?;
                    println!("repo {name} created");
                }
                RepoCmd::List => {
                    for r in st.meta().read(|t| t.repo_list())? {
                        println!("{:<20} {}", r.name, r.format);
                    }
                }
                RepoCmd::Snapshots { repo } => {
                    let r = st.repo(&repo)?;
                    let live = st.live(&repo)?.map(|l| l.seq);
                    for s in st.meta().read(|t| t.snapshot_list(r.id))? {
                        let mark = if Some(s.seq) == live { "  <- live" } else { "" };
                        println!(
                            "{:<5} {:<9} {:>8} files {:>10}{mark}",
                            s.seq,
                            s.state.as_str(),
                            s.entry_count,
                            human(s.total_bytes)
                        );
                    }
                }
                RepoCmd::SyncDir { repo, dir, publish } => sync_dir(&st, &repo, &dir, publish)?,
                RepoCmd::Private { repo, state } => {
                    let private = match state.as_str() {
                        "on" => true,
                        "off" => false,
                        other => bail!("expected on or off, got {other:?}"),
                    };
                    let r = st.repo(&repo)?;
                    st.meta().write(|t| {
                        t.repo_set_private(r.id, private)?;
                        t.audit(mer_storage::now_ms(), "cli", "repo.settings", &format!("{repo} private={private}"))?;
                        Ok(())
                    })?;
                    println!("repo {repo}: {}", if private { "an access token is now required" } else { "open to every client" });
                }
                RepoCmd::Stale { repo, days } => {
                    let limit = match days.as_str() {
                        "off" => None,
                        d => Some(d.parse::<u32>().ok().filter(|d| (1..=3650).contains(d)).ok_or_else(|| anyhow!("expected a number of days (1 to 3650) or off, got {d:?}"))?),
                    };
                    let r = st.repo(&repo)?;
                    st.meta().write(|t| {
                        t.repo_set_stale_days(r.id, limit)?;
                        t.audit(mer_storage::now_ms(), "cli", "repo.settings", &format!("{repo} stale_after_days={limit:?}"))?;
                        Ok(())
                    })?;
                    match limit {
                        Some(d) => println!("repo {repo}: reported as out of date when what is published is older than {d} days"),
                        None => println!("repo {repo}: never reported as out of date"),
                    }
                }
                RepoCmd::Quota { repo, bytes } => {
                    let r = st.repo(&repo)?;
                    let q = parse_size(&bytes)?;
                    st.meta().write(|t| t.repo_set_quota(r.id, (q > 0).then_some(q)))?;
                    println!("repo {repo}: quota {}", if q > 0 { human(q) } else { "removed".into() });
                }
            }
        }
        Cmd::Approve { repo, seq } => {
            open(&data)?.decide(&repo, seq, true, "cli")?;
            println!("{repo}@{seq} approved");
        }
        Cmd::Reject { repo, seq } => {
            open(&data)?.decide(&repo, seq, false, "cli")?;
            println!("{repo}@{seq} rejected");
        }
        Cmd::Publish { repo, seq } => {
            open(&data)?.publish(&repo, seq, "cli")?;
            println!("{repo}@{seq} is live");
        }
        Cmd::Rollback { repo, seq } => {
            open(&data)?.rollback(&repo, seq, "cli")?;
            println!("{repo} rolled back to {seq}");
        }
        Cmd::Get { repo, path, out } => {
            let st = open(&data)?;
            match st.get(&repo, &path)? {
                Lookup::Found(e) => {
                    let mut src = st.open_blob(&e.digest, None)?;
                    match out {
                        Some(file) => {
                            std::io::copy(&mut src, &mut std::fs::File::create(&file)?)?;
                            eprintln!("{} bytes, sha256 {}", e.size, e.digest);
                        }
                        None => {
                            std::io::copy(&mut src, &mut std::io::stdout().lock())?;
                        }
                    }
                }
                Lookup::Miss => bail!("{repo}: {path} not found (recorded on the wishlist)"),
                Lookup::NotPublished => bail!("{repo}: nothing has been published yet"),
            }
        }
        Cmd::Export { cmd } => {
            let st = open(&data)?;
            match cmd {
                ExportCmd::Seal { channel: ch, key, segment_size, step_window } => {
                    let key = KeyPair::load(&key)?;
                    let opts = SealOptions {
                        segment_size: u32::try_from(parse_size(&segment_size)?)
                            .map_err(|_| anyhow!("segment size too large"))?,
                        step_window,
                        collector: "mer".into(),
                    };
                    match seal_bundle(&st, channel(&ch)?, &[&key], &opts)? {
                        None => println!("nothing changed since the last bundle"),
                        Some(b) => println!(
                            "sealed bundle {} (id {}): {} blobs, {} in {} segments",
                            b.seq(),
                            &b.id().to_hex()[..16],
                            b.blob_count(),
                            human(b.stream_len()),
                            b.params().segment_count
                        ),
                    }
                }
                ExportCmd::Media { channel: ch, seq, out, volume_size } => {
                    let b = SealedBundle::open(&st, channel(&ch)?, seq)?;
                    let vols = mer_transport_media::write_volumes(&b, &out, parse_size(&volume_size)?)?;
                    println!("bundle {seq}: {} volume(s) written to {}", vols.len(), out.display());
                    for v in vols {
                        println!("  {}  {}", v.display(), human(std::fs::metadata(&v)?.len()));
                    }
                }
                ExportCmd::Diode { channel: ch, seq, to, link_key, rate_mbit, passes, overhead } => {
                    let b = SealedBundle::open(&st, channel(&ch)?, seq)?;
                    let sock = std::net::UdpSocket::bind("0.0.0.0:0")?;
                    sock.connect(&to).with_context(|| format!("resolving {to}"))?;
                    let cfg = SenderConfig {
                        rate_bps: (rate_mbit * 1_000_000.0) as u64,
                        overhead,
                        ..Default::default()
                    };
                    let session = (mer_storage::now_ms() / 1000) as u32;
                    let mut tx = Sender::new(sock, LinkKey::load(&link_key)?, session, cfg);
                    for pass in 0..passes {
                        let s = tx.send_pass(&b, pass)?;
                        println!("bundle {seq} pass {pass}: {} packets, {}", s.packets, human(s.bytes));
                    }
                }
            }
        }
        Cmd::Import { cmd } => match cmd {
            ImportCmd::Media { dir, trust, auto_publish } => {
                let vault = open_vault(&data, trust, auto_publish)?;
                let rep = mer_transport_media::import_dir(&dir, vault.as_ref())?;
                println!(
                    "{} volume(s) read: {} segments new, {} already held, {} damaged",
                    rep.volumes_read, rep.slots_accepted, rep.slots_already_had, rep.slots_damaged
                );
                for (path, why) in &rep.skipped {
                    println!("  skipped {}: {why}", path.display());
                }
                for (id, missing) in &rep.bundles {
                    if !missing.is_empty() {
                        let ranges: Vec<String> = missing.iter().map(|r| format!("{}..{}", r.start, r.end)).collect();
                        println!(
                            "  bundle {} still needs segments {} (insert the next medium)",
                            &id.to_hex()[..16],
                            ranges.join(", ")
                        );
                    }
                }
                for report in vault.import_ready()? {
                    print_import(&report);
                }
            }
            ImportCmd::Diode { listen, link_key, trust, auto_publish, idle_exit } => {
                let vault = open_vault(&data, trust, auto_publish)?;
                let sock = std::net::UdpSocket::bind(&listen).with_context(|| format!("binding {listen}"))?;
                println!("listening on {listen}");
                let mut rx = Receiver::new(vault.clone(), LinkKey::load(&link_key)?, ReceiverConfig::default());
                let mut failure = None;
                rx.run(&sock, idle_exit.map(Duration::from_secs), &mut |ev| {
                    match ev {
                        RxEvent::SegmentStored { bundle, .. } if vault.missing(&bundle).is_empty() => {
                            match vault.import_ready() {
                                Ok(reports) => reports.iter().for_each(print_import),
                                Err(e) => {
                                    failure = Some(e);
                                    return false;
                                }
                            }
                        }
                        RxEvent::PassEnded { seq, pass } => {
                            for s in vault.staged().iter().filter(|s| s.seq == seq) {
                                println!(
                                    "bundle {seq} pass {pass} ended: {} of {} segments",
                                    s.segments_have, s.segments_total
                                );
                            }
                        }
                        _ => {}
                    }
                    true
                })?;
                let s = rx.stats();
                println!(
                    "link: {} packets, {} lost, {} bad MAC, {} malformed, {} segments rebuilt",
                    s.packets, s.lost, s.bad_mac, s.malformed, s.segments_completed
                );
                if let Some(e) = failure {
                    return Err(e.into());
                }
            }
            ImportCmd::Retry { channel: ch, trust, auto_publish } => {
                let vault = open_vault(&data, trust, auto_publish)?;
                for r in vault.apply_pending(&channel(&ch)?)? {
                    println!("repo {:<20} at bundle {:<4} {:?} {}", r.repo, r.applied_seq, r.state, r.detail);
                }
            }
        },
        Cmd::Apt { cmd } => match cmd {
            AptCmd::Keygen { gnupghome, name } => {
                let key = mer_format_apt::SiteKey::generate(&gnupghome, &name)?;
                println!("site key created in {}", gnupghome.display());
                print!("{}", String::from_utf8_lossy(&key.public_armored()?));
            }
            AptCmd::Sync {
                repo,
                upstream,
                suite,
                components,
                architectures,
                packages,
                recommends,
                keyring,
                gnupghome,
                ignore_valid_until,
                origin,
            } => {
                let st = open(&data)?;
                let up = mer_fetch::upstream_from(&upstream)?;
                let site = mer_format_apt::SiteKey::open(&gnupghome)?;
                let cfg = mer_format_apt::AptConfig {
                    suite,
                    components,
                    architectures,
                    packages,
                    recommends,
                    keyring,
                    ignore_valid_until,
                    origin,
                };
                println!("syncing {repo} from {} (suite {})", up.describe(), cfg.suite);
                let rep = mer_format_apt::sync(&st, &repo, up.as_ref(), &cfg, &site, mer_storage::now_ms() / 1000)?;
                println!(
                    "upstream signature verified; {} packages upstream, {} selected ({})",
                    rep.upstream_packages,
                    rep.selected,
                    human(rep.total_bytes)
                );
                println!(
                    "downloaded {} ({}), {} already in the store",
                    rep.downloaded,
                    human(rep.downloaded_bytes),
                    rep.reused
                );
                for (pkg, dep) in &rep.unresolved {
                    println!("  WARNING: {pkg} depends on {dep}, which upstream does not provide");
                }
                match rep.snapshot {
                    Some(seq) => println!("repo {repo}: snapshot {seq} sealed"),
                    None => println!("repo {repo}: no change"),
                }
            }
        },
        Cmd::Pypi { cmd } => match cmd {
            PypiCmd::Sync { repo, index_url, requirements, pythons, arch, libcs, sdists, pre, allow_hosts } => {
                let st = open(&data)?;
                let sdists = match sdists.as_str() {
                    "never" => mer_format_pypi::Sdists::Never,
                    "if-no-wheel" => mer_format_pypi::Sdists::IfNoWheel,
                    "always" => mer_format_pypi::Sdists::Always,
                    other => bail!("--sdists must be never, if-no-wheel or always, got {other:?}"),
                };
                // The collector contacts the index host, the host PyPI serves
                // files from, and whatever else was explicitly allowed.
                let mut hosts = allow_hosts;
                let index_host = mer_fetch::host_of(&index_url).ok_or_else(|| anyhow!("{index_url:?} is not an http(s) URL"))?;
                if index_host == "pypi.org" {
                    hosts.push("files.pythonhosted.org".into());
                }
                hosts.push(index_host);
                let client = mer_fetch::HttpClient::new(&hosts);
                let cfg = mer_format_pypi::PyConfig { index_url, requirements, pythons, arch, libcs, sdists, prereleases: pre };
                println!("syncing {repo} from {} (allowed hosts: {})", cfg.index_url, hosts.join(", "));
                let rep = mer_format_pypi::sync(&st, &repo, &client, &cfg)?;
                for (name, version) in &rep.projects {
                    println!("  {name} {version}");
                }
                println!(
                    "{} projects, {} files; downloaded {} ({}), {} already in the store",
                    rep.projects.len(),
                    rep.files,
                    rep.downloaded,
                    human(rep.downloaded_bytes),
                    rep.reused
                );
                for w in &rep.warnings {
                    println!("  WARNING: {w}");
                }
                match rep.snapshot {
                    Some(seq) => println!("repo {repo}: snapshot {seq} sealed"),
                    None => println!("repo {repo}: no change"),
                }
            }
        },
        Cmd::Npm { cmd } => match cmd {
            NpmCmd::Sync { repo, registry, packages, os, cpu, libc, allow_hosts } => {
                let st = open(&data)?;
                let mut hosts = allow_hosts;
                hosts.push(mer_fetch::host_of(&registry).ok_or_else(|| anyhow!("{registry:?} is not an http(s) URL"))?);
                let client = mer_fetch::HttpClient::new(&hosts);
                let cfg = mer_format_npm::NpmConfig { registry, packages, os, cpu, libc };
                println!("syncing {repo} from {} (allowed hosts: {})", cfg.registry, hosts.join(", "));
                let rep = mer_format_npm::sync(&st, &repo, &client, &cfg)?;
                println!(
                    "{} packages, {} versions; downloaded {} ({}), {} already in the store",
                    rep.packages,
                    rep.versions,
                    rep.downloaded,
                    human(rep.downloaded_bytes),
                    rep.reused
                );
                for w in &rep.warnings {
                    println!("  WARNING: {w}");
                }
                match rep.snapshot {
                    Some(seq) => println!("repo {repo}: snapshot {seq} sealed"),
                    None => println!("repo {repo}: no change"),
                }
            }
        },
        Cmd::Oci { cmd } => match cmd {
            OciCmd::Sync { repo, registry, images, platforms, allow_hosts } => {
                let st = open(&data)?;
                let mut hosts = allow_hosts;
                let host = mer_fetch::host_of(&registry).ok_or_else(|| anyhow!("{registry:?} is not an http(s) URL"))?;
                if host == "registry-1.docker.io" {
                    hosts.push("auth.docker.io".into()); // Docker Hub hands out pull tokens here
                }
                hosts.push(host);
                let client = mer_fetch::HttpClient::new(&hosts);
                let cfg = mer_format_oci::OciConfig { registry, images, platforms };
                println!("syncing {repo} from {} (allowed hosts: {})", cfg.registry, hosts.join(", "));
                let rep = mer_format_oci::sync(&st, &repo, &client, &cfg)?;
                for (image, digest) in &rep.images {
                    println!("  {image} -> {digest}");
                }
                println!(
                    "{} image manifests, {} blobs; downloaded {} ({}), {} already in the store",
                    rep.manifests,
                    rep.blobs,
                    rep.downloaded,
                    human(rep.downloaded_bytes),
                    rep.reused
                );
                for w in &rep.warnings {
                    println!("  WARNING: {w}");
                }
                match rep.snapshot {
                    Some(seq) => println!("repo {repo}: snapshot {seq} sealed"),
                    None => println!("repo {repo}: no change"),
                }
            }
        },
        Cmd::Nuget { cmd } => match cmd {
            NugetCmd::Sync { repo, feed, packages, frameworks, pre, allow_hosts } => {
                let st = open(&data)?;
                let mut hosts = allow_hosts;
                hosts.push(mer_fetch::host_of(&feed).ok_or_else(|| anyhow!("{feed:?} is not an http(s) URL"))?);
                let client = mer_fetch::HttpClient::new(&hosts);
                let cfg = mer_format_nuget::NugetConfig { service_index: feed, packages, frameworks, prerelease: pre };
                println!("syncing {repo} from {} (allowed hosts: {})", cfg.service_index, hosts.join(", "));
                let rep = mer_format_nuget::sync(&st, &repo, &client, &cfg)?;
                println!(
                    "{} packages, {} versions; downloaded {} ({}), {} already in the store",
                    rep.packages,
                    rep.versions,
                    rep.downloaded,
                    human(rep.downloaded_bytes),
                    rep.reused
                );
                for w in &rep.warnings {
                    println!("  WARNING: {w}");
                }
                match rep.snapshot {
                    Some(seq) => println!("repo {repo}: snapshot {seq} sealed"),
                    None => println!("repo {repo}: no change"),
                }
            }
        },
        Cmd::Helm { cmd } => match cmd {
            HelmCmd::Sync { repo, url, charts, pre, allow_hosts } => {
                let st = open(&data)?;
                let mut hosts = allow_hosts;
                hosts.push(mer_fetch::host_of(&url).ok_or_else(|| anyhow!("{url:?} is not an http(s) URL"))?);
                let client = mer_fetch::HttpClient::new(&hosts);
                let cfg = mer_format_helm::HelmConfig { repo_url: url, charts, prerelease: pre };
                println!("syncing {repo} from {} (allowed hosts: {})", cfg.repo_url, hosts.join(", "));
                let rep = mer_format_helm::sync(&st, &repo, &client, &cfg)?;
                for (name, version) in &rep.charts {
                    println!("  {name} {version}");
                }
                println!("{} charts; downloaded {} ({}), {} already in the store", rep.charts.len(), rep.downloaded, human(rep.downloaded_bytes), rep.reused);
                for w in &rep.warnings {
                    println!("  WARNING: {w}");
                }
                match rep.snapshot {
                    Some(seq) => println!("repo {repo}: snapshot {seq} sealed"),
                    None => println!("repo {repo}: no change"),
                }
            }
        },
        Cmd::Cargo { cmd } => match cmd {
            CargoCmd::Sync { repo, index, crates, lockfiles, default_features_only, features, allow_hosts } => {
                let st = open(&data)?;
                let mut hosts = allow_hosts;
                let index_host = mer_fetch::host_of(&index).ok_or_else(|| anyhow!("{index:?} is not an http(s) URL"))?;
                if index_host == "index.crates.io" {
                    hosts.push("static.crates.io".into()); // where crates.io keeps the crate files
                }
                hosts.push(index_host);
                let mut pins = Vec::new();
                for path in &lockfiles {
                    let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
                    let found = mer_format_cargo::parse_lockfile(&text);
                    println!("{}: {} registry packages", path.display(), found.len());
                    pins.extend(found);
                }
                let client = mer_fetch::HttpClient::new(&hosts);
                let cfg = mer_format_cargo::CargoConfig { index_url: index, crates, pins, default_features_only, features };
                println!("syncing {repo} from {} (allowed hosts: {})", cfg.index_url, hosts.join(", "));
                let rep = mer_format_cargo::sync(&st, &repo, &client, &cfg)?;
                println!(
                    "{} crates, {} versions; downloaded {} ({}), {} already in the store",
                    rep.crates,
                    rep.versions,
                    rep.downloaded,
                    human(rep.downloaded_bytes),
                    rep.reused
                );
                for w in &rep.warnings {
                    println!("  WARNING: {w}");
                }
                match rep.snapshot {
                    Some(seq) => println!("repo {repo}: snapshot {seq} sealed"),
                    None => println!("repo {repo}: no change"),
                }
            }
        },
        Cmd::Go { cmd } => match cmd {
            GoCmd::Sync { repo, proxy, modules, gomods, sumdb, no_sumdb, allow_hosts } => {
                let st = open(&data)?;
                let mut hosts = allow_hosts;
                hosts.push(mer_fetch::host_of(&proxy).ok_or_else(|| anyhow!("{proxy:?} is not an http(s) URL"))?);
                let sumdb_url = (!no_sumdb).then_some(sumdb);
                if let Some(url) = &sumdb_url {
                    hosts.push(mer_fetch::host_of(url).ok_or_else(|| anyhow!("{url:?} is not an http(s) URL"))?);
                }
                let mut texts = Vec::new();
                for path in &gomods {
                    texts.push(std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?);
                }
                let client = mer_fetch::HttpClient::new(&hosts);
                let cfg = mer_format_go::GoConfig { proxy_url: proxy, modules, gomods: texts, sumdb_url };
                println!("syncing {repo} from {} (allowed hosts: {})", cfg.proxy_url, hosts.join(", "));
                let rep = mer_format_go::sync(&st, &repo, &client, &cfg)?;
                println!(
                    "{} modules, {} source archives, {} go.mod files; downloaded {} files ({}), {} already mirrored",
                    rep.modules,
                    rep.archives,
                    rep.gomods,
                    rep.downloaded,
                    human(rep.downloaded_bytes),
                    rep.reused
                );
                match &cfg.sumdb_url {
                    Some(url) => println!("{} files compared with the checksum database at {url}", rep.verified),
                    None => println!("  WARNING: the checksum database was not consulted"),
                }
                for w in &rep.warnings {
                    println!("  WARNING: {w}");
                }
                match rep.snapshot {
                    Some(seq) => println!("repo {repo}: snapshot {seq} sealed"),
                    None => println!("repo {repo}: no change"),
                }
            }
        },
        Cmd::Serve { listen, admin_listen, no_admin, tls_cert, tls_key, threads, quiet } => {
            let app = mer_http::App::new(open(&data)?);
            let tls = match (tls_cert, tls_key) {
                (Some(c), Some(k)) => Some((
                    std::fs::read(&c).with_context(|| format!("reading {}", c.display()))?,
                    std::fs::read(&k).with_context(|| format!("reading {}", k.display()))?,
                )),
                _ => None,
            };
            let scheme = if tls.is_some() { "https" } else { "http" };
            let base = mer_http::ServerConfig { listen, threads, access_log: !quiet, packages: true, admin: true, tls };
            let separate_panel = admin_listen.is_some();
            let main = mer_http::HttpServer::start(
                app.clone(),
                mer_http::ServerConfig { admin: !no_admin && !separate_panel, ..base.clone() },
            )?;
            println!("packages: {scheme}://{}/<repo>/", main.addr());
            let panel = match admin_listen {
                Some(addr) => {
                    let s = mer_http::HttpServer::start(
                        app.clone(),
                        mer_http::ServerConfig { listen: addr, packages: false, ..base },
                    )?;
                    println!("web panel: {scheme}://{}/ui/", s.addr());
                    Some(s)
                }
                None => {
                    if !no_admin {
                        println!("web panel: {scheme}://{}/ui/", main.addr());
                    }
                    None
                }
            };
            if !no_admin && scheme == "http" {
                println!("note: the web panel is on plain HTTP, so passwords and sessions are not encrypted on the wire (use --tls-cert and --tls-key)");
            }
            if !no_admin && app.auth().needs_setup()? {
                println!("note: no user exists yet; create the first administrator with `mer user add admin --role admin`");
            }
            if let Some(p) = panel {
                std::thread::spawn(move || p.wait());
            }
            main.wait();
        }
        Cmd::User { cmd } => {
            let auth = mer_auth::Auth::new(open(&data)?);
            match cmd {
                UserCmd::Add { name, role, password_stdin } => {
                    let role = mer_auth::Role::parse(&role)?;
                    let (password, generated) = read_or_generate_password(password_stdin)?;
                    auth.create_user(&name, &password, role, "cli")?;
                    println!("user {name} created with role {}", role.as_str());
                    if generated {
                        println!("password (shown once): {password}");
                    }
                }
                UserCmd::List => {
                    for u in auth.users()? {
                        println!("{:<24} {:<9} {}", u.name, u.role, if u.disabled { "disabled" } else { "active" });
                    }
                }
                UserCmd::Passwd { name, password_stdin } => {
                    let (password, generated) = read_or_generate_password(password_stdin)?;
                    auth.set_password(&name, &password, "cli")?;
                    println!("password of {name} changed and their sessions ended");
                    if generated {
                        println!("password (shown once): {password}");
                    }
                }
                UserCmd::Role { name, role } => {
                    auth.update_user(&name, Some(mer_auth::Role::parse(&role)?), None, "cli")?;
                    println!("{name} is now {role}");
                }
                UserCmd::Disable { name } => {
                    auth.update_user(&name, None, Some(true), "cli")?;
                    println!("{name} disabled");
                }
                UserCmd::Enable { name } => {
                    auth.update_user(&name, None, Some(false), "cli")?;
                    println!("{name} enabled");
                }
            }
        }
        Cmd::Token { cmd } => {
            let auth = mer_auth::Auth::new(open(&data)?);
            match cmd {
                TokenCmd::Add { name, repo } => {
                    let secret = auth.create_token(&name, repo.as_deref(), "cli")?;
                    println!("token {name} created for {}", repo.as_deref().unwrap_or("every repository"));
                    println!("secret (shown once): {secret}");
                }
                TokenCmd::List => {
                    for t in auth.tokens()? {
                        println!(
                            "{:<24} {:<20} {}",
                            t.name,
                            t.repo.as_deref().unwrap_or("*"),
                            if t.revoked { "revoked" } else { "active" }
                        );
                    }
                }
                TokenCmd::Revoke { name } => {
                    auth.revoke_token(&name, "cli")?;
                    println!("token {name} revoked");
                }
            }
        }
        Cmd::Status => status(&open(&data)?.clone())?,
        Cmd::Wishlist { limit } => {
            let st = open(&data)?;
            let misses = st.meta().read(|t| t.miss_list(limit))?;
            if misses.is_empty() {
                println!("no misses recorded");
            }
            for m in misses {
                println!("{:>6}x  {}  {}", m.count, m.repo, m.path);
            }
        }
        Cmd::Gc { grace_hours } => {
            let r = open(&data)?.gc(grace_hours * 3_600_000)?;
            println!(
                "{} unreferenced blob(s), {} deleted, {} freed",
                r.orphaned,
                r.deleted,
                human(r.bytes_freed)
            );
        }
        Cmd::Compact { repo, keep_from } => {
            let n = open(&data)?.compact(&repo, keep_from, "cli")?;
            println!("{repo}: history before snapshot {keep_from} dropped ({n} old versions); run `mer gc` to free space");
        }
        Cmd::Fsck { deep } => {
            let r = open(&data)?.fsck(deep)?;
            println!(
                "{} blobs checked, {} missing, {} corrupt, {} without a database row",
                r.checked,
                r.missing.len(),
                r.corrupt.len(),
                r.unknown.len()
            );
            for d in r.missing.iter().chain(&r.corrupt) {
                println!("  damaged: {d}");
            }
            if !r.is_clean() {
                bail!("store is damaged");
            }
        }
        Cmd::Audit { limit } => {
            let st = open(&data)?;
            match st.meta().read(|t| t.audit_verify())? {
                Ok(n) => println!("audit chain intact: {n} entries"),
                Err(seq) => bail!("AUDIT LOG TAMPERED: chain breaks at entry {seq}"),
            }
            for row in st.meta().read(|t| t.audit_list(limit))?.iter().rev() {
                println!("{:>5}  {:<22} {:<18} {}", row.seq, row.actor, row.action, row.detail);
            }
        }
    }
    Ok(())
}
