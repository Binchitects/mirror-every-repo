//! Checks against Maven itself.
use crate::*;
use std::cmp::Ordering;

#[test]
fn version_order_matches_maven() {
    let t: serde_json::Value = serde_json::from_slice(&std::fs::read(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/maven_truth.json")).unwrap()).unwrap();
    let mut bad = Vec::new();
    for c in t["compare"].as_array().unwrap() {
        let (a, b) = (c[0].as_str().unwrap(), c[1].as_str().unwrap());
        let want = match c[2].as_i64().unwrap() { -1 => Ordering::Less, 0 => Ordering::Equal, _ => Ordering::Greater };
        if compare(a, b) != want {
            bad.push(format!("{a} vs {b}: want {want:?}, got {:?}", compare(a, b)));
        }
    }
    assert!(bad.is_empty(), "{} of {} differ:\n{}", bad.len(), t["compare"].as_array().unwrap().len(), bad[..bad.len().min(25)].join("\n"));
}

#[test]
fn ranges() {
    let r = Range::parse("[1.0,2.0)").unwrap().unwrap();
    assert!(r.contains("1.0") && r.contains("1.9.9") && !r.contains("2.0") && !r.contains("0.9"));
    let r = Range::parse("[1.5]").unwrap().unwrap();
    assert!(r.contains("1.5") && !r.contains("1.6"));
    let r = Range::parse("(,1.0],[1.2,)").unwrap().unwrap();
    assert!(r.contains("0.5") && r.contains("1.0") && !r.contains("1.1") && r.contains("3"));
    assert!(Range::parse("1.0").is_none(), "a plain version is not a range");
    assert!(Range::parse("[1.0").unwrap().is_err());
}

use mer_fetch::{host_of, HttpClient};
use mer_storage::Storage;
use sha1::{Digest as _, Sha1};
use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};

type Routes = Arc<Mutex<HashMap<String, Vec<u8>>>>;

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

    /// Publish a file with a .sha1 next to it, as Maven Central does.
    fn put(&self, path: &str, body: &[u8]) {
        let mut r = self.routes.lock().unwrap();
        r.insert(format!("/m2/{path}"), body.to_vec());
        r.insert(format!("/m2/{path}.sha1"), hex::encode(Sha1::digest(body)).into_bytes());
    }

    /// A POM (inner XML of <project>) and, unless packaging is pom, a jar.
    fn publish(&self, g: &str, a: &str, v: &str, inner: &str) {
        let pom = format!("<?xml version=\"1.0\"?>\n<project xmlns=\"http://maven.apache.org/POM/4.0.0\"><modelVersion>4.0.0</modelVersion><groupId>{g}</groupId><artifactId>{a}</artifactId><version>{v}</version>{inner}</project>");
        self.put(&file_path(g, a, v, None, "pom"), pom.as_bytes());
        if !inner.contains("<packaging>pom</packaging>") {
            self.put(&file_path(g, a, v, None, "jar"), format!("jar {g}:{a}:{v}").as_bytes());
        }
    }

    fn versions(&self, g: &str, a: &str, vs: &[&str]) {
        let list: String = vs.iter().map(|v| format!("<version>{v}</version>")).collect();
        self.routes.lock().unwrap().insert(format!("/m2/{}/{a}/maven-metadata.xml", g.replace('.', "/")), format!("<metadata><versioning><versions>{list}</versions></versioning></metadata>").into_bytes());
    }

    fn downloads(&self) -> usize {
        self.hits.lock().unwrap().iter().filter(|p| p.ends_with(".jar") || p.ends_with(".pom")).count()
    }
}

struct Rig {
    _dir: tempfile::TempDir,
    st: Storage,
    up: Fake,
    cfg: MavenConfig,
}

fn rig() -> Rig {
    let dir = tempfile::tempdir().unwrap();
    let st = Storage::open(dir.path()).unwrap();
    st.create_repo("mvn", "maven").unwrap();
    let up = Fake::new();
    let cfg = MavenConfig { repo: format!("{}/m2/", up.base), artifacts: vec!["org.app:app:1.0".into()], m2_dirs: vec![] };
    Rig { _dir: dir, st, up, cfg }
}

impl Rig {
    fn sync(&self) -> Result<MavenReport, MavenError> {
        sync(&self.st, "mvn", &HttpClient::new(&[host_of(&self.up.base).unwrap()]), &self.cfg)
    }

    fn view(&self) -> BTreeMap<String, Vec<u8>> {
        let repo = self.st.repo("mvn").unwrap();
        let Some(head) = self.st.head("mvn").unwrap() else { return BTreeMap::new() };
        self.st.meta().read(|t| t.entries_at(repo.id, head.seq)).unwrap().into_iter().map(|e| (e.path, self.st.read_blob(&e.digest).unwrap())).collect()
    }

    fn artifacts(&self) -> Vec<String> {
        self.view().keys().filter(|p| p.ends_with(".jar") || p.ends_with(".pom")).map(|p| p.rsplit('/').next().unwrap().to_string()).collect()
    }
}

#[test]
fn poms_are_followed_through_parents_properties_boms_scopes_exclusions_ranges_and_relocations() {
    let r = rig();
    // A parent that manages versions through a property the child overrides.
    r.up.publish("org.app", "parent", "1", "<packaging>pom</packaging><properties><lib.version>1.0</lib.version></properties>\
        <dependencyManagement><dependencies><dependency><groupId>org.lib</groupId><artifactId>lib</artifactId><version>${lib.version}</version></dependency>\
        <dependency><groupId>org.bom</groupId><artifactId>bom</artifactId><version>5</version><type>pom</type><scope>import</scope></dependency></dependencies></dependencyManagement>");
    r.up.publish("org.bom", "bom", "5", "<packaging>pom</packaging><dependencyManagement><dependencies><dependency><groupId>org.managed</groupId><artifactId>m</artifactId><version>2.5</version></dependency></dependencies></dependencyManagement>");
    r.up.publish("org.app", "app", "1.0", "<parent><groupId>org.app</groupId><artifactId>parent</artifactId><version>1</version></parent>\
        <properties><lib.version>1.2</lib.version></properties><dependencies>\
        <dependency><groupId>org.lib</groupId><artifactId>lib</artifactId></dependency>\
        <dependency><groupId>org.managed</groupId><artifactId>m</artifactId></dependency>\
        <dependency><groupId>org.test</groupId><artifactId>junit</artifactId><version>4</version><scope>test</scope></dependency>\
        <dependency><groupId>org.opt</groupId><artifactId>opt</artifactId><version>1</version><optional>true</optional></dependency>\
        <dependency><groupId>org.ranged</groupId><artifactId>r</artifactId><version>[1.0,2.0)</version></dependency>\
        <dependency><groupId>org.old</groupId><artifactId>moved</artifactId><version>1</version></dependency>\
        </dependencies>");
    r.up.publish("org.lib", "lib", "1.2", "<dependencies><dependency><groupId>org.deep</groupId><artifactId>deep</artifactId><version>3</version>\
        <exclusions><exclusion><groupId>org.unwanted</groupId><artifactId>*</artifactId></exclusion></exclusions></dependency></dependencies>");
    r.up.publish("org.deep", "deep", "3", "<dependencies><dependency><groupId>org.unwanted</groupId><artifactId>u</artifactId><version>1</version></dependency>\
        <dependency><groupId>org.deep</groupId><artifactId>rt</artifactId><version>1</version><scope>runtime</scope></dependency>\
        <dependency><groupId>org.deep</groupId><artifactId>prov</artifactId><version>1</version><scope>provided</scope></dependency></dependencies>");
    r.up.publish("org.deep", "rt", "1", "");
    r.up.publish("org.managed", "m", "2.5", "");
    r.up.publish("org.ranged", "r", "1.5", "");
    r.up.publish("org.ranged", "r", "2.0", "");
    r.up.versions("org.ranged", "r", &["1.0", "1.5", "2.0"]);
    r.up.publish("org.old", "moved", "1", "<packaging>pom</packaging><distributionManagement><relocation><groupId>org.new</groupId></relocation></distributionManagement>");
    r.up.publish("org.new", "moved", "1", "");
    let rep = r.sync().unwrap();
    assert_eq!(
        r.artifacts(),
        [
            "app-1.0.jar", "app-1.0.pom", "parent-1.pom", "bom-5.pom", "deep-3.jar", "deep-3.pom", "rt-1.jar", "rt-1.pom", "lib-1.2.jar", "lib-1.2.pom",
            "m-2.5.jar", "m-2.5.pom", "moved-1.jar", "moved-1.pom", "moved-1.pom", "r-1.5.jar", "r-1.5.pom",
        ],
        "test, optional, provided and excluded dependencies are left out; lib 1.2 from the child's property; the range picks 1.5"
    );
    assert!(rep.warnings.is_empty(), "{:?}", rep.warnings);
    let view = r.view();
    assert_eq!(view["org/lib/lib/1.2/lib-1.2.jar.sha1"], hex::encode(Sha1::digest(&view["org/lib/lib/1.2/lib-1.2.jar"])).into_bytes());
    let md = String::from_utf8(view["org/ranged/r/maven-metadata.xml"].clone()).unwrap();
    assert!(md.contains("<release>1.5</release>") && !md.contains("2.0"), "{md}");

    let again = r.sync().unwrap();
    assert_eq!((again.downloaded, again.snapshot), (0, None));
}

#[test]
fn a_local_repository_is_mirrored_exactly() {
    let mut r = rig();
    r.up.publish("org.apache.maven.plugins", "maven-jar-plugin", "3.4.1", "<packaging>maven-plugin</packaging>");
    r.up.publish("org.x", "y", "1", "<dependencies><dependency><groupId>org.not</groupId><artifactId>followed</artifactId><version>1</version></dependency></dependencies>");
    let m2 = tempfile::tempdir().unwrap();
    for (path, body) in [
        ("org/apache/maven/plugins/maven-jar-plugin/3.4.1/maven-jar-plugin-3.4.1.pom", "x"),
        ("org/apache/maven/plugins/maven-jar-plugin/3.4.1/maven-jar-plugin-3.4.1.jar", "x"),
        ("org/apache/maven/plugins/maven-jar-plugin/3.4.1/_remote.repositories", "x"),
        ("org/x/y/1/y-1.pom", "x"),
        ("org/x/y/1/y-1.pom.sha1", "x"),
        ("org/x/y/maven-metadata-central.xml", "x"),
    ] {
        let f = m2.path().join(path);
        std::fs::create_dir_all(f.parent().unwrap()).unwrap();
        std::fs::write(f, body).unwrap();
    }
    r.cfg.artifacts = vec![];
    r.cfg.m2_dirs = vec![m2.path().to_path_buf()];
    let rep = r.sync().unwrap();
    assert_eq!(r.artifacts(), ["maven-jar-plugin-3.4.1.jar", "maven-jar-plugin-3.4.1.pom", "y-1.pom"], "exactly what was there, nothing followed");
    assert_eq!(rep.files, 3);
    let groups = String::from_utf8(r.view()["org/apache/maven/plugins/maven-metadata.xml"].clone()).unwrap();
    assert!(groups.contains("<prefix>jar</prefix>"), "plugin prefix metadata: {groups}");

    std::fs::write(m2.path().join("org/x/y/1/y-1.jar"), "x").unwrap();
    std::fs::create_dir_all(m2.path().join("org/x/z/9")).unwrap();
    std::fs::write(m2.path().join("org/x/z/9/z-9.jar"), "x").unwrap();
    assert!(matches!(r.sync(), Err(MavenError::Upstream(m)) if m.contains("z-9.jar")));
}

#[test]
fn files_must_match_their_checksums_and_missing_roots_fail() {
    let mut r = rig();
    r.up.publish("org.app", "app", "1.0", "");
    r.up.routes.lock().unwrap().insert("/m2/org/app/app/1.0/app-1.0.jar".into(), b"tampered".to_vec());
    assert!(matches!(r.sync(), Err(MavenError::Upstream(m)) if m.contains("checksum")));
    r.cfg.artifacts = vec!["org.none:none:1".into()];
    assert!(matches!(r.sync(), Err(MavenError::Upstream(_))));
    r.cfg.artifacts = vec!["org.app:app".into()];
    r.up.versions("org.app", "app", &["0.9", "1.0", "1.1-SNAPSHOT"]);
    r.up.routes.lock().unwrap().insert("/m2/org/app/app/1.0/app-1.0.jar".into(), b"jar org.app:app:1.0".to_vec());
    r.sync().unwrap();
    assert_eq!(r.artifacts(), ["app-1.0.jar", "app-1.0.pom"], "newest release, not the snapshot");
    assert!(r.up.downloads() > 0);
}
