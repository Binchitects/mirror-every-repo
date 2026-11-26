//! Checks against rpm itself, and sync tests against fake repositories.
use crate::*;
use mer_fetch::{host_of, HttpClient};
use mer_storage::Storage;
use serde_json::Value;
use sha2::{Digest as _, Sha256};
use std::cmp::Ordering;
use std::collections::{BTreeMap, HashMap};
use std::io::Write;
use std::sync::{Arc, Mutex};

#[test]
fn version_order_matches_rpm() {
    let t: Value = serde_json::from_slice(&std::fs::read(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/rpm_truth.json")).unwrap()).unwrap();
    let ord = |n: i64| match n {
        -1 => Ordering::Less,
        0 => Ordering::Equal,
        _ => Ordering::Greater,
    };
    for c in t["vercmp"].as_array().unwrap() {
        let (a, b) = (c[0].as_str().unwrap(), c[1].as_str().unwrap());
        assert_eq!(vercmp(a, b), ord(c[2].as_i64().unwrap()), "vercmp({a:?}, {b:?})");
    }
    for c in t["evrcmp"].as_array().unwrap() {
        let e = |v: &Value| Evr::new(v[0].as_str().unwrap(), v[1].as_str().unwrap(), v[2].as_str().unwrap());
        let (a, b) = (e(&c[0]), e(&c[1]));
        assert_eq!(a.cmp(&b), ord(c[2].as_i64().unwrap()), "{a} vs {b}");
    }
}

#[test]
fn rich_dependencies() {
    assert_eq!(parse_rich("(a or b)"), Some(Rich::Or(vec![Rich::Cap("a".into()), Rich::Cap("b".into())])));
    assert_eq!(
        parse_rich("(foo >= 1.2 if (bar and baz))"),
        Some(Rich::If(Box::new(Rich::Cap("foo".into())), Box::new(Rich::And(vec![Rich::Cap("bar".into()), Rich::Cap("baz".into())])), None))
    );
    assert_eq!(parse_rich("(a with b)"), Some(Rich::And(vec![Rich::Cap("a".into()), Rich::Cap("b".into())])));
    assert!(matches!(parse_rich("(a if b else c)"), Some(Rich::If(_, _, Some(_)))));
    assert_eq!(
        parse_rich("(glibc-gconv-extra(x86-64) = 2.34-290.el9_9 if redhat-rpm-config)"),
        Some(Rich::If(Box::new(Rich::Cap("glibc-gconv-extra(x86-64)".into())), Box::new(Rich::Cap("redhat-rpm-config".into())), None))
    );
    assert_eq!(parse_rich("(perl(Foo::Bar) or libc.so.6(GLIBC_2.34)(64bit))"), Some(Rich::Or(vec![Rich::Cap("perl(Foo::Bar)".into()), Rich::Cap("libc.so.6(GLIBC_2.34)(64bit)".into())])));
    assert_eq!(parse_rich("(a or"), None);
    assert_eq!(parse_rich("a or b"), None);
}

type Routes = Arc<Mutex<HashMap<String, Vec<u8>>>>;

/// A package for the fake repository: name, version, requires, provides, files in primary, files only in filelists.
struct P {
    name: &'static str,
    ver: &'static str,
    arch: &'static str,
    requires: Vec<&'static str>,
    provides: Vec<&'static str>,
    files: Vec<&'static str>,
    more_files: Vec<&'static str>,
}

fn p(name: &'static str, ver: &'static str) -> P {
    P { name, ver, arch: "x86_64", requires: vec![], provides: vec![], files: vec![], more_files: vec![] }
}

fn rpm_body(p: &P) -> Vec<u8> {
    format!("rpm {} {} {}", p.name, p.ver, p.arch).repeat(20).into_bytes()
}

fn gz(b: &[u8]) -> Vec<u8> {
    let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    e.write_all(b).unwrap();
    e.finish().unwrap()
}

fn hexsum(b: &[u8]) -> String {
    hex::encode(Sha256::digest(b))
}

struct Fake {
    base: String,
    routes: Routes,
    hits: Arc<Mutex<Vec<String>>>,
}

impl Fake {
    fn new() -> Fake {
        let routes: Routes = Arc::default();
        let hits: Arc<Mutex<Vec<String>>> = Arc::default();
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let base = format!("http://{}", server.server_addr().to_ip().unwrap());
        let (r, h) = (routes.clone(), hits.clone());
        std::thread::spawn(move || {
            for req in server.incoming_requests() {
                let path = req.url().to_string();
                h.lock().unwrap().push(path.clone());
                let body = r.lock().unwrap().get(&path).cloned();
                let _ = match body {
                    Some(b) => req.respond(tiny_http::Response::from_data(b)),
                    None => req.respond(tiny_http::Response::from_string("not found").with_status_code(404)),
                };
            }
        });
        Fake { base, routes, hits }
    }

    /// Publish a repository at `/<name>/` holding these packages.
    fn repo(&self, name: &str, pkgs: &[P]) {
        let mut primary = String::from("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<metadata xmlns=\"http://linux.duke.edu/metadata/common\" xmlns:rpm=\"http://linux.duke.edu/metadata/rpm\" packages=\"0\">\n");
        let mut filelists = String::from("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<filelists xmlns=\"http://linux.duke.edu/metadata/filelists\" packages=\"0\">\n");
        let mut r = self.routes.lock().unwrap();
        for p in pkgs {
            let body = rpm_body(p);
            let id = hexsum(&body);
            let href = format!("Packages/{}/{}-{}-1.{}.rpm", &p.name[..1], p.name, p.ver, p.arch);
            r.insert(format!("/{name}/{href}"), body.clone());
            let entries = |v: &[&str]| v.iter().map(|n| format!("<rpm:entry name=\"{}\"/>", n.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;"))).collect::<String>();
            let mut provides = vec![p.name];
            provides.extend(&p.provides);
            primary.push_str(&format!(
                "<package type=\"rpm\">\n  <name>{n}</name>\n  <arch>{a}</arch>\n  <version epoch=\"0\" ver=\"{v}\" rel=\"1\"/>\n  <checksum type=\"sha256\" pkgid=\"YES\">{id}</checksum>\n  <summary>{n} &amp; more</summary>\n  <location href=\"{href}\"/>\n  <size package=\"{s}\" installed=\"1\" archive=\"1\"/>\n  <format>\n    <rpm:provides>{pv}</rpm:provides>\n    <rpm:requires>{rq}</rpm:requires>\n    {files}\n  </format>\n</package>\n",
                n = p.name, a = p.arch, v = p.ver, s = body.len(), pv = entries(&provides), rq = entries(&p.requires),
                files = p.files.iter().map(|f| format!("<file>{f}</file>")).collect::<String>(),
            ));
            filelists.push_str(&format!(
                "<package pkgid=\"{id}\" name=\"{}\" arch=\"{}\"><version epoch=\"0\" ver=\"{}\" rel=\"1\"/>{}</package>\n",
                p.name, p.arch, p.ver,
                p.files.iter().chain(&p.more_files).map(|f| format!("<file>{f}</file>")).collect::<String>()
            ));
        }
        primary.push_str("</metadata>\n");
        filelists.push_str("</filelists>\n");
        let mut repomd = String::from("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<repomd xmlns=\"http://linux.duke.edu/metadata/repo\">\n  <revision>1700000000</revision>\n");
        for (kind, xml) in [("primary", primary), ("filelists", filelists)] {
            let packed = gz(xml.as_bytes());
            let sum = hexsum(&packed);
            r.insert(format!("/{name}/repodata/{sum}-{kind}.xml.gz"), packed.clone());
            repomd.push_str(&format!(
                "  <data type=\"{kind}\">\n    <checksum type=\"sha256\">{sum}</checksum>\n    <open-checksum type=\"sha256\">{}</open-checksum>\n    <location href=\"repodata/{sum}-{kind}.xml.gz\"/>\n    <size>{}</size>\n    <open-size>{}</open-size>\n  </data>\n",
                hexsum(xml.as_bytes()), packed.len(), xml.len()
            ));
        }
        repomd.push_str("</repomd>\n");
        r.insert(format!("/{name}/repodata/repomd.xml"), repomd.into_bytes());
    }

    fn downloads(&self) -> usize {
        self.hits.lock().unwrap().iter().filter(|p| p.ends_with(".rpm")).count()
    }
}

struct Rig {
    _dir: tempfile::TempDir,
    st: Storage,
    up: Fake,
    cfg: RpmConfig,
}

fn rig() -> Rig {
    let dir = tempfile::tempdir().unwrap();
    let st = Storage::open(dir.path()).unwrap();
    st.create_repo("el", "rpm").unwrap();
    let up = Fake::new();
    let cfg = RpmConfig {
        upstreams: vec![format!("{}/baseos/", up.base), format!("{}/appstream", up.base)],
        packages: vec!["app".into()],
        arches: vec!["x86_64".into(), "noarch".into()],
        keyring: None,
    };
    Rig { _dir: dir, st, up, cfg }
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
        self.view().keys().filter(|k| k.ends_with(".rpm")).map(|k| k.rsplit('/').next().unwrap().to_string()).collect()
    }

    /// The generated metadata file of `kind`, unpacked.
    fn generated(&self, kind: &str) -> String {
        let view = self.view();
        let (_, body) = view.iter().find(|(k, _)| k.ends_with(&format!("-{kind}.xml.gz"))).unwrap();
        let mut s = String::new();
        std::io::Read::read_to_string(&mut flate2::read::GzDecoder::new(&body[..]), &mut s).unwrap();
        s
    }
}

#[test]
fn requirements_are_followed_across_repositories_through_names_files_and_rich_dependencies() {
    let r = rig();
    let mut app = p("app", "2.0");
    app.requires = vec!["libfoo.so.1()(64bit)", "/usr/bin/tool", "(pick-a or pick-b)", "(extra if lib)", "(never if absent)", "rpmlib(PayloadIsZstd)"];
    let mut lib = p("lib", "1.0");
    lib.provides = vec!["libfoo.so.1()(64bit)"];
    let mut tool = p("tool", "3.0");
    tool.more_files = vec!["/usr/bin/tool"];
    let mut old_app = p("app", "1.0");
    old_app.requires = vec!["old-only"];
    r.up.repo("baseos", &[lib, tool, p("pick-a", "1"), p("unrelated", "1"), old_app]);
    r.up.repo("appstream", &[app, p("pick-b", "1"), p("extra", "1"), p("never", "1"), P { arch: "i686", ..p("lib", "1.0") }]);
    let rep = r.sync().unwrap();
    // app 2.0 (newest, other repository), lib through a soname, tool through a file only
    // filelists lists, the first alternative, and the conditional whose condition holds.
    assert_eq!(r.rpms(), ["app-2.0-1.x86_64.rpm", "extra-1-1.x86_64.rpm", "lib-1.0-1.x86_64.rpm", "pick-a-1-1.x86_64.rpm", "tool-3.0-1.x86_64.rpm"]);
    assert_eq!((rep.packages, rep.downloaded, rep.snapshot), (5, 5, Some(1)));
    assert!(rep.warnings.iter().all(|w| w.contains("TLS alone")), "{:?}", rep.warnings);

    // The generated metadata repeats upstream's entries, and repomd checks out.
    let primary = r.generated("primary");
    assert_eq!(primary.matches("<package type=\"rpm\">").count(), 5);
    assert!(primary.contains("<summary>app &amp; more</summary>"), "upstream bytes kept, entities intact");
    assert!(parse_primary(&primary).unwrap().iter().any(|p| p.name == "tool"));
    assert!(r.generated("filelists").contains("<file>/usr/bin/tool</file>"));
    let view = r.view();
    let repomd = parse_repomd(&String::from_utf8(view["repodata/repomd.xml"].clone()).unwrap()).unwrap();
    assert_eq!(repomd.data.iter().map(|d| d.kind.as_str()).collect::<Vec<_>>(), ["primary", "filelists", "other"]);
    for d in &repomd.data {
        assert_eq!(hexsum(&view[&d.href]), d.checksum.1);
    }

    // Unchanged upstream: nothing downloaded again, no new snapshot.
    let again = r.sync().unwrap();
    assert_eq!((again.downloaded, again.reused, again.snapshot), (0, 5, None));
    assert_eq!(r.up.downloads(), 5);
}

#[test]
fn sha512_and_zstd_metadata_as_fedora_publishes_it_is_read() {
    let r = rig();
    r.up.repo("baseos", &[p("app", "1")]);
    r.up.repo("appstream", &[]);
    // Re-publish baseos primary as zstd, named and checked by SHA-512.
    let mut routes = r.up.routes.lock().unwrap();
    let md = String::from_utf8(routes["/baseos/repodata/repomd.xml"].clone()).unwrap();
    let href = md.split("location href=\"").nth(1).unwrap().split('"').next().unwrap().to_string();
    let mut xml = String::new();
    std::io::Read::read_to_string(&mut flate2::read::GzDecoder::new(&routes[&format!("/baseos/{href}")][..]), &mut xml).unwrap();
    let zst = zstd_frame(xml.as_bytes());
    let sum = hex::encode(<sha2::Sha512 as sha2::Digest>::digest(&zst));
    let new_href = format!("repodata/{sum}-primary.xml.zst");
    routes.insert(format!("/baseos/{new_href}"), zst.clone());
    let open = hex::encode(<sha2::Sha512 as sha2::Digest>::digest(xml.as_bytes()));
    let old_sum = md.split("<checksum type=\"sha256\">").nth(1).unwrap().split('<').next().unwrap().to_string();
    let old_open = md.split("<open-checksum type=\"sha256\">").nth(1).unwrap().split('<').next().unwrap().to_string();
    let md = md
        .replacen(&format!("<checksum type=\"sha256\">{old_sum}"), &format!("<checksum type=\"sha512\">{sum}"), 1)
        .replacen(&format!("<open-checksum type=\"sha256\">{old_open}"), &format!("<open-checksum type=\"sha512\">{open}"), 1)
        .replacen(&href, &new_href, 1);
    routes.insert("/baseos/repodata/repomd.xml".into(), md.into_bytes());
    drop(routes);
    r.sync().unwrap();
    assert_eq!(r.rpms(), ["app-1-1.x86_64.rpm"]);
}

/// A zstd frame of raw (uncompressed) blocks, enough to exercise the decoder.
fn zstd_frame(data: &[u8]) -> Vec<u8> {
    let mut out = vec![0x28, 0xb5, 0x2f, 0xfd];
    // Frame header: single segment, 4-byte content size.
    out.push(0b1010_0000);
    out.extend((data.len() as u32).to_le_bytes());
    let chunks: Vec<&[u8]> = data.chunks(100_000).collect();
    for (i, c) in chunks.iter().enumerate() {
        let last = (i == chunks.len() - 1) as u32;
        let header = last | ((c.len() as u32) << 3);
        out.extend(&header.to_le_bytes()[..3]);
        out.extend(*c);
    }
    out
}

#[test]
fn unsatisfiable_requirements_warn_and_unknown_packages_fail() {
    let mut r = rig();
    let mut app = p("app", "1");
    app.requires = vec!["missing-thing"];
    r.up.repo("baseos", &[app]);
    r.up.repo("appstream", &[]);
    let rep = r.sync().unwrap();
    assert!(rep.warnings.iter().any(|w| w.contains("missing-thing")), "{:?}", rep.warnings);
    r.cfg.packages = vec!["nothere".into()];
    assert!(matches!(r.sync(), Err(RpmError::Upstream(m)) if m.contains("nothere")));
}

#[test]
fn downloads_and_metadata_must_match_their_checksums() {
    let r = rig();
    r.up.repo("baseos", &[p("app", "1")]);
    r.up.repo("appstream", &[]);
    r.up.routes.lock().unwrap().insert("/baseos/Packages/a/app-1-1.x86_64.rpm".into(), b"tampered".to_vec());
    assert!(matches!(r.sync(), Err(RpmError::Upstream(m)) if m.contains("does not match")));

    let r = rig();
    r.up.repo("baseos", &[p("app", "1")]);
    r.up.repo("appstream", &[]);
    let md = String::from_utf8(r.up.routes.lock().unwrap()["/baseos/repodata/repomd.xml"].clone()).unwrap();
    let href = md.split("location href=\"").nth(1).unwrap().split('"').next().unwrap().to_string();
    r.up.routes.lock().unwrap().insert(format!("/baseos/{href}"), gz(b"<metadata/>"));
    assert!(matches!(r.sync(), Err(RpmError::Upstream(m)) if m.contains("checksum")));
}

#[test]
fn repomd_signatures_are_checked_against_the_keyring() {
    let mut r = rig();
    r.up.repo("baseos", &[p("app", "1")]);
    r.up.repo("appstream", &[]);
    let home = tempfile::tempdir().unwrap();
    let gpg = |args: &[&str]| {
        let out = std::process::Command::new("gpg").arg("--batch").arg("--homedir").arg(home.path()).args(args).output().unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        out.stdout
    };
    gpg(&["--passphrase", "", "--quick-gen-key", "repo@example.org", "ed25519", "sign", "never"]);
    let key = home.path().join("key.asc");
    std::fs::write(&key, gpg(&["--armor", "--export", "repo@example.org"])).unwrap();
    for name in ["baseos", "appstream"] {
        let md = r.up.routes.lock().unwrap()[&format!("/{name}/repodata/repomd.xml")].clone();
        let f = home.path().join(format!("{name}.xml"));
        std::fs::write(&f, &md).unwrap();
        let sig = gpg(&["--armor", "--detach-sign", "--output", "-", f.to_str().unwrap()]);
        r.up.routes.lock().unwrap().insert(format!("/{name}/repodata/repomd.xml.asc"), sig);
    }
    r.cfg.keyring = Some(key.clone());
    let rep = r.sync().unwrap();
    assert!(rep.warnings.is_empty(), "{:?}", rep.warnings);

    // A changed repomd no longer verifies.
    let md = r.up.routes.lock().unwrap()["/baseos/repodata/repomd.xml"].clone();
    r.up.routes.lock().unwrap().insert("/baseos/repodata/repomd.xml".into(), [md, b"<!-- changed -->".to_vec()].concat());
    assert!(matches!(r.sync(), Err(RpmError::Signature(_))));
}
