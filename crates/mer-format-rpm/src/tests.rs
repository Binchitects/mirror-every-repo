//! Sync tests against fake RPM repositories served over HTTP.
use crate::repodata::{decompress, gzip, parse_package, parse_repomd};
use crate::*;
use mer_core::Digest;
use mer_fetch::{host_of, HttpClient};
use mer_format_apt::{verify_detached, SiteKey};
use mer_storage::Storage;
use std::collections::{BTreeMap, HashMap};
use std::io::Read;
use std::sync::{Arc, Mutex};

type Routes = Arc<Mutex<HashMap<String, Vec<u8>>>>;

/// One package of a fake repository.
struct Pkg {
    name: &'static str,
    evr: &'static str,
    arch: &'static str,
    provides: Vec<&'static str>,
    requires: Vec<&'static str>,
    /// Files in the package list.
    files: Vec<&'static str>,
    /// Files only in the complete file lists.
    more_files: Vec<&'static str>,
}

fn pkg(name: &'static str, evr: &'static str, requires: &[&'static str]) -> Pkg {
    Pkg { name, evr, arch: "x86_64", provides: Vec::new(), requires: requires.to_vec(), files: Vec::new(), more_files: Vec::new() }
}

fn body(p: &Pkg) -> Vec<u8> {
    format!("rpm file of {}-{}.{} ", p.name, p.evr, p.arch).repeat(10).into_bytes()
}

fn entry(cap: &str) -> String {
    let mut words = cap.split(' ');
    match (cap.starts_with('('), words.next(), words.next(), words.next()) {
        (false, Some(name), Some(op), Some(version)) => {
            let flag = match op {
                ">=" => "GE",
                "<" => "LT",
                _ => "EQ",
            };
            format!("<rpm:entry name=\"{name}\" flags=\"{flag}\" epoch=\"0\" ver=\"{version}\"/>")
        }
        _ => format!("<rpm:entry name=\"{cap}\"/>"),
    }
}

struct Server {
    base: String,
    routes: Routes,
    hits: Arc<Mutex<Vec<String>>>,
}

impl Server {
    fn new() -> Server {
        let routes: Routes = Arc::default();
        let hits: Arc<Mutex<Vec<String>>> = Arc::default();
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let base = format!("http://{}", server.server_addr().to_ip().unwrap());
        let (r, h) = (routes.clone(), hits.clone());
        std::thread::spawn(move || {
            for req in server.incoming_requests() {
                let path = req.url().to_string();
                h.lock().unwrap().push(path.clone());
                let found = r.lock().unwrap().get(&path).cloned();
                let _ = match found {
                    Some(b) => req.respond(tiny_http::Response::from_data(b)),
                    None => req.respond(tiny_http::Response::from_string("no").with_status_code(404)),
                };
            }
        });
        Server { base, routes, hits }
    }

    /// Publish a repository under `/<name>/`, signed if a key is given.
    fn publish(&self, name: &str, packages: &[Pkg], key: Option<&SiteKey>) {
        let mut routes = self.routes.lock().unwrap();
        let mut primary = format!("<?xml version=\"1.0\"?>\n<metadata xmlns=\"http://linux.duke.edu/metadata/common\" xmlns:rpm=\"http://linux.duke.edu/metadata/rpm\" packages=\"{}\">\n", packages.len());
        let mut filelists = String::from("<?xml version=\"1.0\"?>\n<filelists xmlns=\"http://linux.duke.edu/metadata/filelists\">\n");
        for p in packages {
            let data = body(p);
            let id = Digest::of(&data).to_hex();
            let (ver, rel) = p.evr.split_once('-').unwrap();
            let href = format!("Packages/{}/{}-{}.{}.rpm", &p.name[..1], p.name, p.evr, p.arch);
            routes.insert(format!("/{name}/{href}"), data);
            let caps = |list: &[&str]| list.iter().map(|c| entry(c)).collect::<String>();
            let files = |list: &[&str]| list.iter().map(|f| format!("<file>{f}</file>")).collect::<String>();
            primary.push_str(&format!(
                "<package type=\"rpm\"><name>{}</name><arch>{}</arch><version epoch=\"0\" ver=\"{ver}\" rel=\"{rel}\"/><checksum type=\"sha256\" pkgid=\"YES\">{id}</checksum><summary>About {} &amp; co</summary>\
                 <location href=\"{href}\"/><format><rpm:provides>{}<rpm:entry name=\"{}\" flags=\"EQ\" epoch=\"0\" ver=\"{ver}\" rel=\"{rel}\"/></rpm:provides><rpm:requires>{}</rpm:requires>{}</format></package>\n",
                p.name, p.arch, p.name, caps(&p.provides), p.name, caps(&p.requires), files(&p.files)
            ));
            filelists.push_str(&format!("<package pkgid=\"{id}\" name=\"{}\" arch=\"{}\"><version epoch=\"0\" ver=\"{ver}\" rel=\"{rel}\"/>{}{}</package>\n", p.name, p.arch, files(&p.files), files(&p.more_files)));
        }
        primary.push_str("</metadata>\n");
        filelists.push_str("</filelists>\n");
        let mut repomd = String::from("<?xml version=\"1.0\"?>\n<repomd xmlns=\"http://linux.duke.edu/metadata/repo\"><revision>1</revision>\n");
        for (kind, xml) in [("primary", primary), ("filelists", filelists)] {
            let packed = gzip(xml.as_bytes());
            let sum = Digest::of(&packed).to_hex();
            repomd.push_str(&format!("<data type=\"{kind}\"><checksum type=\"sha256\">{sum}</checksum><location href=\"repodata/{sum}-{kind}.xml.gz\"/><timestamp>1700000000</timestamp></data>\n"));
            routes.insert(format!("/{name}/repodata/{sum}-{kind}.xml.gz"), packed);
        }
        repomd.push_str("</repomd>\n");
        match key {
            Some(k) => routes.insert(format!("/{name}/repodata/repomd.xml.asc"), k.detach_sign(repomd.as_bytes()).unwrap()),
            None => routes.remove(&format!("/{name}/repodata/repomd.xml.asc")),
        };
        routes.insert(format!("/{name}/repodata/repomd.xml"), repomd.into_bytes());
    }

    fn downloads(&self) -> usize {
        self.hits.lock().unwrap().iter().filter(|p| p.ends_with(".rpm")).count()
    }
}

struct Rig {
    dir: tempfile::TempDir,
    st: Storage,
    up: Server,
    cfg: RpmConfig,
}

fn rig() -> Rig {
    let dir = tempfile::tempdir().unwrap();
    let st = Storage::open(dir.path().join("data")).unwrap();
    st.create_repo("el", "rpm").unwrap();
    let up = Server::new();
    let cfg = RpmConfig { repo_urls: vec![format!("{}/base", up.base)], packages: vec!["app".into()], archs: vec!["x86_64".into()], ..Default::default() };
    Rig { dir, st, up, cfg }
}

impl Rig {
    fn sync(&self) -> Result<RpmReport, RpmError> {
        sync(&self.st, "el", &HttpClient::new(&[host_of(&self.up.base).unwrap()]), &self.cfg)
    }

    fn view(&self) -> BTreeMap<String, Vec<u8>> {
        let repo = self.st.repo("el").unwrap();
        let Some(head) = self.st.head("el").unwrap() else { return BTreeMap::new() };
        self.st.meta().read(|t| t.entries_at(repo.id, head.seq)).unwrap().into_iter().map(|e| (e.path, self.st.read_blob(&e.digest).unwrap())).collect()
    }

    fn rpms(&self) -> Vec<String> {
        self.view().keys().filter(|p| p.ends_with(".rpm")).map(|p| p.rsplit('/').next().unwrap().to_string()).collect()
    }

    /// The mirrored metadata of one kind, unpacked, with its checksums checked against repomd.xml.
    fn metadata(&self, kind: &str) -> String {
        let v = self.view();
        let repomd = parse_repomd(&String::from_utf8_lossy(&v["repodata/repomd.xml"])).unwrap();
        let e = repomd.iter().find(|e| e.kind == kind).unwrap_or_else(|| panic!("no {kind} in repomd.xml"));
        let packed = &v[&e.href];
        assert_eq!(Digest::of(packed).to_hex(), e.checksum, "{kind}");
        let mut text = String::new();
        decompress(&e.href, packed).unwrap().read_to_string(&mut text).unwrap();
        text
    }
}

fn base_packages() -> Vec<Pkg> {
    vec![
        Pkg { files: vec!["/usr/bin/app"], ..pkg("app", "1.0-1", &["libfoo.so.1()(64bit)", "/usr/bin/sh", "data >= 2", "/usr/libexec/helper", "rpmlib(Zstd) <= 5"]) },
        pkg("app", "0.9-1", &[]),
        Pkg { provides: vec!["libfoo.so.1()(64bit)"], ..pkg("foo-libs", "3-1", &["glibc"]) },
        Pkg { arch: "i686", provides: vec!["libfoo.so.1"], ..pkg("foo-libs", "3-1", &[]) },
        Pkg { files: vec!["/usr/bin/sh"], ..pkg("bash", "5-1", &[]) },
        Pkg { arch: "noarch", ..pkg("data", "2.5-1", &[]) },
        pkg("glibc", "2.34-2", &[]),
        Pkg { more_files: vec!["/usr/libexec/helper"], ..pkg("helper", "1-1", &[]) },
        pkg("nodejs", "20-1.module+el9+1", &[]),
        pkg("unrelated", "1-1", &[]),
    ]
}

#[test]
fn packages_and_what_they_need_are_mirrored_with_metadata_that_lists_them() {
    let r = rig();
    r.up.publish("base", &base_packages(), None);
    let rep = r.sync().unwrap();
    assert_eq!(
        r.rpms(),
        ["app-1.0-1.x86_64.rpm", "bash-5-1.x86_64.rpm", "data-2.5-1.noarch.rpm", "foo-libs-3-1.x86_64.rpm", "glibc-2.34-2.x86_64.rpm", "helper-1-1.x86_64.rpm"],
        "the newest app; the 64-bit library; a file found only in the complete file lists"
    );
    assert_eq!((rep.available, rep.packages, rep.downloaded, rep.snapshot, rep.signatures_verified), (8, 6, 6, Some(1), false));
    assert_eq!(rep.warnings.len(), 1, "{:?}", rep.warnings);
    assert!(rep.warnings[0].contains("module streams"));

    // The package list holds exactly those six, each entry as upstream wrote it.
    let primary = r.metadata("primary");
    assert!(primary.contains("packages=\"6\"") && primary.contains("About app &amp; co"));
    let v = r.view();
    let mut listed = Vec::new();
    crate::repodata::for_each_package::<std::io::Error>(primary.as_bytes(), |f| {
        let p = parse_package(f).unwrap();
        assert_eq!(Digest::of(&v[&p.href]).to_hex(), p.pkgid, "{}: the entry points at the mirrored file", p.name);
        listed.push(p.name);
        Ok(())
    })
    .unwrap();
    listed.sort();
    assert_eq!(listed, ["app", "bash", "data", "foo-libs", "glibc", "helper"]);
    let filelists = r.metadata("filelists");
    assert!(filelists.contains("packages=\"6\"") && filelists.contains("<file>/usr/libexec/helper</file>") && !filelists.contains("unrelated"));
    assert!(v.contains_key("Packages/a/app-1.0-1.x86_64.rpm") && !v.contains_key("repodata/repomd.xml.asc"));
}

#[test]
fn several_upstream_repositories_become_one() {
    let mut r = rig();
    r.up.publish("base", &[pkg("glibc", "2.34-2", &[]), Pkg { files: vec!["/usr/bin/sh"], ..pkg("bash", "5-1", &["glibc"]) }], None);
    r.up.publish("apps", &[pkg("app", "1.0-1", &["/usr/bin/sh", "(extra if bash)", "(pick-a or pick-b)"]), pkg("pick-b", "1-1", &[]), pkg("extra", "1-1", &[])], None);
    r.cfg.repo_urls.push(format!("{}/apps", r.up.base));
    let rep = r.sync().unwrap();
    assert_eq!(r.rpms(), ["app-1.0-1.x86_64.rpm", "bash-5-1.x86_64.rpm", "extra-1-1.x86_64.rpm", "glibc-2.34-2.x86_64.rpm", "pick-b-1-1.x86_64.rpm"]);
    assert!(rep.warnings.is_empty(), "{:?}", rep.warnings);
    assert!(r.metadata("primary").contains("packages=\"5\""));
    // By what a package provides, and another architecture.
    r.cfg.packages = vec!["/usr/bin/sh".into()];
    r.sync().unwrap();
    assert_eq!(r.rpms(), ["bash-5-1.x86_64.rpm", "glibc-2.34-2.x86_64.rpm"]);
    r.cfg.archs = vec!["aarch64".into()];
    assert!(matches!(r.sync(), Err(RpmError::UnknownPackage(p)) if p == "/usr/bin/sh"));
}

#[test]
fn tampered_files_are_refused() {
    let r = rig();
    r.up.publish("base", &base_packages(), None);
    r.up.routes.lock().unwrap().insert("/base/Packages/b/bash-5-1.x86_64.rpm".into(), b"not the package".to_vec());
    assert!(matches!(r.sync(), Err(RpmError::Storage(_))));
    assert!(r.st.head("el").unwrap().is_none());

    let r = rig();
    r.up.publish("base", &base_packages(), None);
    let mut routes = r.up.routes.lock().unwrap();
    let primary = routes.keys().find(|k| k.ends_with("-primary.xml.gz")).unwrap().clone();
    routes.insert(primary, gzip(b"<metadata></metadata>"));
    drop(routes);
    match r.sync() {
        Err(RpmError::Repo(m)) => assert!(m.contains("repomd.xml states SHA-256"), "{m}"),
        other => panic!("{other:?}"),
    }
}

#[test]
fn upstream_signatures_are_verified_and_the_mirror_can_be_signed() {
    let mut r = rig();
    let upstream_key = SiteKey::generate(r.dir.path().join("upstream"), "Distribution Key").unwrap();
    let keyring = r.dir.path().join("RPM-GPG-KEY-distro");
    std::fs::write(&keyring, upstream_key.public_armored().unwrap()).unwrap();
    r.cfg.keyring = Some(keyring);

    // No signature upstream: refused when a keyring is given.
    r.up.publish("base", &base_packages(), None);
    assert!(matches!(r.sync(), Err(RpmError::Signature(m)) if m.contains("does not sign")));
    // Signed by somebody else: refused.
    let stranger = SiteKey::generate(r.dir.path().join("stranger"), "Somebody Else").unwrap();
    r.up.publish("base", &base_packages(), Some(&stranger));
    assert!(matches!(r.sync(), Err(RpmError::Signature(_))));
    // Signed by the distribution, then altered: refused.
    r.up.publish("base", &base_packages(), Some(&upstream_key));
    let good = r.up.routes.lock().unwrap()["/base/repodata/repomd.xml"].clone();
    r.up.routes.lock().unwrap().insert("/base/repodata/repomd.xml".into(), String::from_utf8(good.clone()).unwrap().replace("<revision>1<", "<revision>2<").into_bytes());
    assert!(matches!(r.sync(), Err(RpmError::Signature(_))));
    assert!(r.st.head("el").unwrap().is_none());
    r.up.routes.lock().unwrap().insert("/base/repodata/repomd.xml".into(), good);

    // Properly signed; and the mirror signs its own metadata with the site key.
    let site = SiteKey::generate(r.dir.path().join("site"), "Site Key").unwrap();
    r.cfg.sign_home = Some(r.dir.path().join("site"));
    let rep = r.sync().unwrap();
    assert!(rep.signatures_verified && rep.snapshot == Some(1));
    let v = r.view();
    let site_ring = r.dir.path().join("site.asc");
    std::fs::write(&site_ring, &v["keys/site.asc"]).unwrap();
    assert_eq!(v["keys/site.asc"], site.public_armored().unwrap());
    verify_detached(&site_ring, &v["repodata/repomd.xml"], &v["repodata/repomd.xml.asc"]).unwrap();
    // Nothing changed upstream: nothing changes here, signature included.
    let again = r.sync().unwrap();
    assert_eq!((again.snapshot, again.downloaded, again.reused), (None, 0, 6));
}

#[test]
fn problems_are_reported_clearly() {
    let mut r = rig();
    r.up.publish("base", &[pkg("app", "1.0-1", &["missing-thing", "/nowhere/file"])], None);
    let rep = r.sync().unwrap();
    assert_eq!(r.rpms(), ["app-1.0-1.x86_64.rpm"]);
    assert_eq!(rep.warnings.len(), 2, "{:?}", rep.warnings);
    assert!(rep.warnings.iter().all(|w| w.starts_with("nothing provides ")));

    r.cfg.packages = vec!["nope".into()];
    assert!(matches!(r.sync(), Err(RpmError::UnknownPackage(p)) if p == "nope"));
    r.cfg.packages = vec!["app".into()];
    for change in [|c: &mut RpmConfig| c.repo_urls.clear(), |c: &mut RpmConfig| c.archs.clear(), |c: &mut RpmConfig| c.repo_urls = vec!["ftp://x/y".into()], |c: &mut RpmConfig| c.packages.clear()] {
        let mut cfg = r.cfg.clone();
        change(&mut cfg);
        let client = HttpClient::new(&[host_of(&r.up.base).unwrap()]);
        assert!(matches!(sync(&r.st, "el", &client, &cfg), Err(RpmError::Config(_))));
    }
    r.cfg.repo_urls = vec![format!("{}/nothing-here", r.up.base)];
    assert!(matches!(r.sync(), Err(RpmError::Fetch(_))));
    r.st.create_repo("files", "raw").unwrap();
    let client = HttpClient::new(&[host_of(&r.up.base).unwrap()]);
    assert!(matches!(sync(&r.st, "files", &client, &r.cfg), Err(RpmError::Config(_))));
}

#[test]
fn a_second_sync_downloads_only_what_is_new() {
    let r = rig();
    r.up.publish("base", &[pkg("app", "1.0-1", &["lib"]), pkg("lib", "1-1", &[])], None);
    assert_eq!(r.sync().unwrap().snapshot, Some(1));
    let again = r.sync().unwrap();
    assert_eq!((again.snapshot, again.downloaded, again.reused), (None, 0, 2));
    r.up.publish("base", &[pkg("app", "1.0-1", &["lib"]), pkg("app", "1.1-1", &["lib"]), pkg("lib", "1-1", &[])], None);
    let rep = r.sync().unwrap();
    assert_eq!((rep.downloaded, rep.reused, r.up.downloads()), (1, 1, 3));
    assert_eq!(r.rpms(), ["app-1.1-1.x86_64.rpm", "lib-1-1.x86_64.rpm"]);
}
