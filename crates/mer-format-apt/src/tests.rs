//! Sync tests against a synthetic, properly signed upstream archive.
use crate::*;
use mer_core::Digest;
use mer_fetch::DirUpstream;
use mer_storage::Storage;
use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

struct Pkg {
    name: &'static str,
    version: &'static str,
    depends: &'static str,
    body: Vec<u8>,
}

fn pkg(name: &'static str, version: &'static str, depends: &'static str) -> Pkg {
    let body = format!("fake deb {name} {version} ").repeat(50).into_bytes();
    Pkg { name, version, depends, body }
}

fn gz(data: &[u8]) -> Vec<u8> {
    let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    e.write_all(data).unwrap();
    e.finish().unwrap()
}

fn deb_path(p: &Pkg) -> String {
    format!("pool/main/{}/{}/{}_{}_amd64.deb", &p.name[..1], p.name, p.name, p.version)
}

/// Write an upstream archive for `suite` and sign it with `key`.
fn publish(dir: &Path, key: &SiteKey, suite: &str, pkgs: &[Pkg], extra_release: &str) {
    let mut packages = String::new();
    for p in pkgs {
        let path = deb_path(p);
        std::fs::create_dir_all(dir.join(&path).parent().unwrap()).unwrap();
        std::fs::write(dir.join(&path), &p.body).unwrap();
        packages.push_str(&format!("Package: {}\nVersion: {}\nArchitecture: amd64\n", p.name, p.version));
        if !p.depends.is_empty() {
            packages.push_str(&format!("Depends: {}\n", p.depends));
        }
        packages.push_str(&format!(
            "Filename: {path}\nSize: {}\nSHA256: {}\nDescription: test package\n\n",
            p.body.len(),
            Digest::of(&p.body)
        ));
    }
    let idx = dir.join(format!("dists/{suite}/main/binary-amd64"));
    std::fs::create_dir_all(&idx).unwrap();
    let packed = gz(packages.as_bytes());
    std::fs::write(idx.join("Packages.gz"), &packed).unwrap();
    let release = format!(
        "Origin: Fake\nSuite: stable\nCodename: {suite}\nDate: Thu, 01 Jan 2026 00:00:00 UTC\n{extra_release}SHA256:\n {} {} main/binary-amd64/Packages.gz\n",
        Digest::of(&packed),
        packed.len()
    );
    std::fs::write(dir.join(format!("dists/{suite}/InRelease")), key.clearsign(release.as_bytes()).unwrap()).unwrap();
}

struct Rig {
    dir: tempfile::TempDir,
    upstream_key: SiteKey,
    site: SiteKey,
    st: Storage,
    cfg: AptConfig,
}

impl Rig {
    fn new() -> Rig {
        let dir = tempfile::tempdir().unwrap();
        let upstream_key = SiteKey::generate(dir.path().join("upkey"), "Fake Archive Key").unwrap();
        let keyring = dir.path().join("archive.gpg");
        std::fs::write(&keyring, upstream_key.public_binary().unwrap()).unwrap();
        let site = SiteKey::generate(dir.path().join("sitekey"), "Site Mirror Key").unwrap();
        let st = Storage::open(dir.path().join("data")).unwrap();
        st.create_repo("debian", "apt").unwrap();
        std::fs::create_dir_all(dir.path().join("up")).unwrap();
        let cfg = AptConfig {
            suite: "bookworm".into(),
            components: vec!["main".into()],
            architectures: vec!["amd64".into()],
            packages: vec!["hello".into()],
            recommends: false,
            keyring,
            ignore_valid_until: false,
            origin: "Site Mirror".into(),
        };
        Rig { dir, upstream_key, site, st, cfg }
    }

    fn up(&self) -> PathBuf {
        self.dir.path().join("up")
    }

    fn world(&self) -> Vec<Pkg> {
        vec![
            pkg("hello", "2.10-3", "libc6 (>= 2.34), libgreet | libhowdy"),
            pkg("libc6", "2.36-9", "libgcc-s1"),
            pkg("libgcc-s1", "12.2.0-14", ""),
            pkg("libgreet", "1.0", ""),
            pkg("libhowdy", "1.0", ""),
            pkg("unrelated", "9.9", ""),
        ]
    }

    fn publish(&self, pkgs: &[Pkg]) {
        publish(&self.up(), &self.upstream_key, "bookworm", pkgs, "");
    }

    fn sync(&self) -> Result<SyncReport, AptError> {
        // "now" is fixed so the tests do not depend on the clock.
        sync(&self.st, "debian", &DirUpstream::new(self.up()).unwrap(), &self.cfg, &self.site, 1_770_000_000)
    }

    /// path -> content of the newest snapshot.
    fn view(&self) -> BTreeMap<String, Vec<u8>> {
        let repo = self.st.repo("debian").unwrap();
        let Some(head) = self.st.head("debian").unwrap() else { return BTreeMap::new() };
        self.st
            .meta()
            .read(|t| t.entries_at(repo.id, head.seq))
            .unwrap()
            .into_iter()
            .map(|e| (e.path, self.st.read_blob(&e.digest).unwrap()))
            .collect()
    }
}

fn text(view: &BTreeMap<String, Vec<u8>>, path: &str) -> String {
    String::from_utf8(view.get(path).unwrap_or_else(|| panic!("{path} missing")).clone()).unwrap()
}

#[test]
fn mirrors_only_the_wanted_packages_and_their_dependencies() {
    let r = Rig::new();
    r.publish(&r.world());
    let rep = r.sync().unwrap();
    assert_eq!((rep.upstream_packages, rep.selected, rep.downloaded, rep.reused), (6, 4, 4, 0));
    assert!(rep.unresolved.is_empty());
    assert_eq!(rep.snapshot, Some(1));

    let v = r.view();
    let pool: Vec<&str> = v.keys().filter(|p| p.starts_with("pool/")).map(String::as_str).collect();
    assert_eq!(
        pool,
        [
            "pool/main/h/hello/hello_2.10-3_amd64.deb",
            "pool/main/l/libc6/libc6_2.36-9_amd64.deb",
            "pool/main/l/libgcc-s1/libgcc-s1_12.2.0-14_amd64.deb",
            "pool/main/l/libgreet/libgreet_1.0_amd64.deb",
        ]
    );
    let packages = text(&v, "dists/bookworm/main/binary-amd64/Packages");
    let listed: Vec<String> =
        parse_stanzas(&packages).unwrap().iter().map(|s| s.get("Package").unwrap().to_string()).collect();
    assert_eq!(listed, ["hello", "libc6", "libgcc-s1", "libgreet"]);
    assert!(!packages.contains("unrelated"));
}

#[test]
fn the_generated_repository_is_internally_consistent_and_signed_by_the_site() {
    let r = Rig::new();
    r.publish(&r.world());
    r.sync().unwrap();
    let v = r.view();

    // Site signature over Release; InRelease carries the same text.
    let site_ring = r.dir.path().join("site.gpg");
    std::fs::write(&site_ring, r.site.public_binary().unwrap()).unwrap();
    let signed = verify_inrelease(&site_ring, &v["dists/bookworm/InRelease"]).unwrap();
    assert_eq!(signed, v["dists/bookworm/Release"]);
    // The upstream archive key does NOT verify our Release: clients trust the site key.
    assert!(verify_inrelease(&r.cfg.keyring, &v["dists/bookworm/InRelease"]).is_err());
    assert!(v["keys/site.asc"].starts_with(b"-----BEGIN PGP PUBLIC KEY BLOCK-----"));
    assert!(v.contains_key("dists/bookworm/Release.gpg"));

    // Every file the Release lists exists with that size and hash, also under by-hash.
    let release = parse_release(&text(&v, "dists/bookworm/Release")).unwrap();
    assert_eq!(release.files.len(), 2);
    assert_eq!(release.valid_until, None);
    for f in &release.files {
        let body = &v[&format!("dists/bookworm/{}", f.path)];
        assert_eq!((body.len() as u64, Digest::of(body)), (f.size, f.sha256), "{}", f.path);
        let dir = f.path.rsplit_once('/').unwrap().0;
        assert_eq!(&v[&format!("dists/bookworm/{dir}/by-hash/SHA256/{}", f.sha256)], body);
    }
    // Every package the index lists exists with that size and hash.
    for s in parse_stanzas(&text(&v, "dists/bookworm/main/binary-amd64/Packages")).unwrap() {
        let body = &v[s.get("Filename").unwrap()];
        assert_eq!(body.len().to_string(), s.get("Size").unwrap());
        assert_eq!(Digest::of(body).to_hex(), s.get("SHA256").unwrap());
    }
    // What upstream signed is kept as evidence.
    assert_eq!(v["evidence/bookworm/InRelease"], std::fs::read(r.up().join("dists/bookworm/InRelease")).unwrap());
}

#[test]
fn unchanged_upstream_gives_no_new_snapshot_even_when_re_signed() {
    let r = Rig::new();
    r.publish(&r.world());
    r.sync().unwrap();
    let before = r.view();
    r.publish(&r.world()); // upstream signs the same content again (new signature bytes)
    let rep = r.sync().unwrap();
    assert_eq!((rep.snapshot, rep.downloaded, rep.reused), (None, 0, 4));
    assert_eq!(r.view(), before);
}

#[test]
fn an_upstream_update_replaces_the_package_and_downloads_only_what_is_new() {
    let r = Rig::new();
    r.publish(&r.world());
    r.sync().unwrap();
    let mut world = r.world();
    world[1] = pkg("libc6", "2.36-10", "libgcc-s1"); // security update
    r.publish(&world);
    let rep = r.sync().unwrap();
    assert_eq!((rep.snapshot, rep.downloaded, rep.reused), (Some(2), 1, 3));
    let v = r.view();
    assert!(v.contains_key("pool/main/l/libc6/libc6_2.36-10_amd64.deb"));
    assert!(!v.contains_key("pool/main/l/libc6/libc6_2.36-9_amd64.deb"), "old version leaves the new snapshot");
    assert!(text(&v, "dists/bookworm/main/binary-amd64/Packages").contains("Version: 2.36-10"));
    // Snapshot 1 is untouched: rollback would serve the old, consistent repository.
    let old = r.st.get_at("debian", 1, "pool/main/l/libc6/libc6_2.36-9_amd64.deb").unwrap();
    assert!(old.is_some());
}

#[test]
fn every_break_in_the_trust_chain_aborts_the_sync() {
    // 1. InRelease signed by a key that is not in the keyring.
    let r = Rig::new();
    let stranger = SiteKey::generate(r.dir.path().join("stranger"), "Stranger").unwrap();
    publish(&r.up(), &stranger, "bookworm", &r.world(), "");
    assert!(matches!(r.sync(), Err(AptError::Signature(_))));
    nothing_sealed(&r);

    // 2. InRelease text altered after signing.
    let r = Rig::new();
    r.publish(&r.world());
    let p = r.up().join("dists/bookworm/InRelease");
    std::fs::write(&p, std::fs::read_to_string(&p).unwrap().replace("Origin: Fake", "Origin: Evil")).unwrap();
    assert!(matches!(r.sync(), Err(AptError::Signature(_))));
    nothing_sealed(&r);

    // 3. Index swapped after the Release was signed.
    let r = Rig::new();
    r.publish(&r.world());
    std::fs::write(r.up().join("dists/bookworm/main/binary-amd64/Packages.gz"), gz(b"Package: evil\nVersion: 1\n")).unwrap();
    assert!(matches!(r.sync(), Err(AptError::Metadata(_))));
    nothing_sealed(&r);

    // 4. A package file altered on the mirror (same size).
    let r = Rig::new();
    r.publish(&r.world());
    let deb = r.up().join("pool/main/l/libc6/libc6_2.36-9_amd64.deb");
    let mut body = std::fs::read(&deb).unwrap();
    body[10] ^= 1;
    std::fs::write(&deb, body).unwrap();
    assert!(matches!(r.sync(), Err(AptError::Storage(_))));
    nothing_sealed(&r);

    // 5. A package file missing on the mirror.
    let r = Rig::new();
    r.publish(&r.world());
    std::fs::remove_file(r.up().join("pool/main/h/hello/hello_2.10-3_amd64.deb")).unwrap();
    assert!(matches!(r.sync(), Err(AptError::Fetch(_))));
    nothing_sealed(&r);
}

/// A failed sync must leave the repo exactly where it was.
fn nothing_sealed(r: &Rig) {
    assert!(r.st.head("debian").unwrap().is_none(), "a failed sync seals nothing");
    let id = r.st.repo("debian").unwrap().id;
    assert_eq!(r.st.meta().read(|t| t.snapshot_building(id)).unwrap(), None, "and leaves no half-built snapshot");
}

#[test]
fn a_validly_signed_release_of_another_suite_is_refused() {
    let r = Rig::new();
    // Genuine, signed content for "sid", copied to where "bookworm" should be.
    publish(&r.up(), &r.upstream_key, "sid", &r.world(), "");
    std::fs::create_dir_all(r.up().join("dists/bookworm")).unwrap();
    std::fs::copy(r.up().join("dists/sid/InRelease"), r.up().join("dists/bookworm/InRelease")).unwrap();
    match r.sync() {
        Err(AptError::Metadata(m)) => assert!(m.contains("asked for suite bookworm"), "{m}"),
        other => panic!("{other:?}"),
    }
}

#[test]
fn an_expired_release_is_refused_unless_explicitly_allowed() {
    let mut r = Rig::new();
    let expired = "Valid-Until: Thu, 08 Jan 2026 00:00:00 UTC\n"; // before the fixed "now"
    publish(&r.up(), &r.upstream_key, "bookworm", &r.world(), expired);
    match r.sync() {
        Err(AptError::Metadata(m)) => assert!(m.contains("expired"), "{m}"),
        other => panic!("{other:?}"),
    }
    r.cfg.ignore_valid_until = true;
    assert_eq!(r.sync().unwrap().snapshot, Some(1));
    // Our own Release must not inherit an expiry date.
    assert!(!text(&r.view(), "dists/bookworm/Release").contains("Valid-Until"));

    let r2 = Rig::new();
    publish(&r2.up(), &r2.upstream_key, "bookworm", &r2.world(), "Valid-Until: Fri, 01 Jan 2100 00:00:00 UTC\n");
    assert!(r2.sync().is_ok());
}

#[test]
fn hostile_index_entries_are_refused() {
    let r = Rig::new();
    let mut world = r.world();
    world[0].name = "hello";
    r.publish(&world);
    // Rewrite the (re-signed) index with a Filename that escapes the pool.
    let idx = r.up().join("dists/bookworm/main/binary-amd64/Packages.gz");
    let body = "Package: hello\nVersion: 1\nFilename: pool/../../../etc/passwd\nSize: 1\nSHA256: ".to_string()
        + &Digest::of(b"x").to_hex()
        + "\n";
    let packed = gz(body.as_bytes());
    std::fs::write(&idx, &packed).unwrap();
    let release = format!(
        "Suite: stable\nCodename: bookworm\nSHA256:\n {} {} main/binary-amd64/Packages.gz\n",
        Digest::of(&packed),
        packed.len()
    );
    std::fs::write(r.up().join("dists/bookworm/InRelease"), r.upstream_key.clearsign(release.as_bytes()).unwrap()).unwrap();
    match r.sync() {
        Err(AptError::Metadata(m)) => assert!(m.contains("Filename"), "{m}"),
        other => panic!("{other:?}"),
    }
}

#[test]
fn unknown_package_and_unsatisfiable_dependency_are_reported() {
    let mut r = Rig::new();
    r.publish(&[pkg("hello", "1", "libmissing (>= 2)")]);
    let rep = r.sync().unwrap();
    assert_eq!(rep.unresolved, vec![("hello".to_string(), "libmissing".to_string())]);
    r.cfg.packages = vec!["does-not-exist".into()];
    assert!(matches!(r.sync(), Err(AptError::UnknownPackage(_))));
}

#[test]
fn only_apt_repos_can_be_synced() {
    let r = Rig::new();
    r.publish(&r.world());
    r.st.create_repo("files", "raw").unwrap();
    let up = DirUpstream::new(r.up()).unwrap();
    assert!(matches!(sync(&r.st, "files", &up, &r.cfg, &r.site, 0), Err(AptError::Metadata(_))));
}

/// The real client: `apt-get update` must accept the generated repository
/// (signature, Release, by-hash indices) and `apt-get download` must fetch a
/// package and verify it against our index.
#[test]
fn real_apt_accepts_the_generated_repository() {
    let have_apt = Command::new("apt-get").arg("--version").output().map(|o| o.status.success()).unwrap_or(false);
    let is_root = Command::new("id").arg("-u").output().map(|o| o.stdout == b"0\n").unwrap_or(false);
    if !have_apt || !is_root {
        eprintln!("skipped: needs apt-get and root");
        return;
    }
    let r = Rig::new();
    r.publish(&r.world());
    r.sync().unwrap();

    // Lay the snapshot out as files, as the HTTP server will serve them.
    let site = r.dir.path().join("served");
    for (path, body) in r.view() {
        let dest = site.join(&path);
        std::fs::create_dir_all(dest.parent().unwrap()).unwrap();
        std::fs::write(dest, body).unwrap();
    }
    let work = r.dir.path().join("apt");
    for d in ["lists/partial", "cache/archives/partial", "dl"] {
        std::fs::create_dir_all(work.join(d)).unwrap();
    }
    std::fs::write(work.join("status"), b"").unwrap();
    let list = work.join("sources.list");
    std::fs::write(
        &list,
        format!("deb [arch=amd64 signed-by={}] file:{} bookworm main\n", site.join("keys/site.asc").display(), site.display()),
    )
    .unwrap();
    let apt = |tool: &str, args: &[&str]| {
        let out = Command::new(tool)
            .current_dir(work.join("dl"))
            .arg("-o").arg(format!("Dir::Etc::sourcelist={}", list.display()))
            .arg("-o").arg("Dir::Etc::sourceparts=/dev/null")
            .arg("-o").arg("Dir::Etc::trusted=/dev/null")
            .arg("-o").arg("Dir::Etc::trustedparts=/dev/null")
            .arg("-o").arg(format!("Dir::State::lists={}", work.join("lists").display()))
            .arg("-o").arg(format!("Dir::State::status={}", work.join("status").display()))
            .arg("-o").arg(format!("Dir::Cache={}", work.join("cache").display()))
            .arg("-o").arg("APT::Sandbox::User=root")
            .arg("-o").arg("APT::Architecture=amd64")
            .arg("-o").arg("APT::Architectures=amd64")
            .arg("-o").arg("Acquire::Languages=none")
            .arg("-o").arg("Debug::NoLocking=1")
            .args(args)
            .output()
            .unwrap();
        (out.status.success(), String::from_utf8_lossy(&out.stdout).to_string() + &String::from_utf8_lossy(&out.stderr))
    };

    let (ok, log) = apt("apt-get", &["update"]);
    assert!(ok, "apt-get update failed:\n{log}");
    assert!(!log.contains("W:") && !log.contains("E:"), "apt-get update complained:\n{log}");

    let (ok, log) = apt("apt-cache", &["policy", "hello"]);
    assert!(ok && log.contains("Candidate: 2.10-3"), "{log}");
    let (_, log) = apt("apt-cache", &["policy", "unrelated"]);
    assert!(!log.contains("Candidate: 9.9"), "a package outside the closure must not be offered:\n{log}");

    let (ok, log) = apt("apt-get", &["download", "libc6"]);
    assert!(ok, "apt-get download failed:\n{log}");
    let got = std::fs::read(work.join("dl/libc6_2.36-9_amd64.deb")).unwrap();
    assert_eq!(got, r.world()[1].body, "apt fetched and hash-checked the package");
}
