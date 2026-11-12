//! Sync tests against a fake Maven repository served over HTTP.
use crate::*;
use mer_fetch::{host_of, HttpClient};
use mer_storage::Storage;
use sha1::{Digest as _, Sha1};
use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};

type Routes = Arc<Mutex<HashMap<String, Vec<u8>>>>;

struct Repo {
    base: String,
    routes: Routes,
    hits: Arc<Mutex<Vec<String>>>,
}

/// A dependency element: "g:a:v" with optional ":scope" and extras as raw XML.
fn dep(gav: &str, extra: &str) -> String {
    let p: Vec<&str> = gav.split(':').collect();
    let version = p.get(2).filter(|v| !v.is_empty()).map(|v| format!("<version>{v}</version>")).unwrap_or_default();
    let scope = p.get(3).map(|s| format!("<scope>{s}</scope>")).unwrap_or_default();
    format!("<dependency><groupId>{}</groupId><artifactId>{}</artifactId>{version}{scope}{extra}</dependency>", p[0], p[1])
}

impl Repo {
    fn new() -> Repo {
        let routes: Routes = Arc::default();
        let hits: Arc<Mutex<Vec<String>>> = Arc::default();
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let base = format!("http://{}/maven2", server.server_addr().to_ip().unwrap());
        let (r, h) = (routes.clone(), hits.clone());
        std::thread::spawn(move || {
            for req in server.incoming_requests() {
                let path = req.url().to_string();
                h.lock().unwrap().push(path.clone());
                let body = r.lock().unwrap().get(&path).cloned();
                let _ = match body {
                    Some(b) => req.respond(tiny_http::Response::from_data(b)),
                    None => req.respond(tiny_http::Response::from_string("no").with_status_code(404)),
                };
            }
        });
        Repo { base, routes, hits }
    }

    fn put(&self, path: &str, body: &[u8]) {
        let mut routes = self.routes.lock().unwrap();
        routes.insert(format!("/maven2/{path}.sha1"), hex::encode(Sha1::digest(body)).into_bytes());
        routes.insert(format!("/maven2/{path}"), body.to_vec());
    }

    /// Publish a POM (body is what goes inside <project>) and, unless the
    /// packaging is pom, a jar. Also lists the version in the metadata.
    fn publish(&self, gav: &str, body: &str) -> &Self {
        let p: Vec<&str> = gav.split(':').collect();
        let g = Gav::new(p[0], p[1], p[2]);
        let pom = format!(
            "<project xmlns=\"http://maven.apache.org/POM/4.0.0\"><modelVersion>4.0.0</modelVersion><groupId>{}</groupId><artifactId>{}</artifactId><version>{}</version>{body}</project>",
            p[0], p[1], p[2]
        );
        self.put(&g.file(None, "pom"), pom.as_bytes());
        if !body.contains("<packaging>pom</packaging>") && !body.contains("<relocation>") {
            let ext = if body.contains("<packaging>war</packaging>") { "war" } else { "jar" };
            self.put(&g.file(None, ext), format!("{ext} of {gav}").as_bytes());
        }
        let meta = format!("/maven2/{}/{}/maven-metadata.xml", p[0].replace('.', "/"), p[1]);
        let mut routes = self.routes.lock().unwrap();
        let mut versions: Vec<String> = routes
            .get(&meta)
            .map(|m| String::from_utf8_lossy(m).split("<version>").skip(1).map(|s| s.split('<').next().unwrap().to_string()).collect())
            .unwrap_or_default();
        versions.push(p[2].to_string());
        let list: String = versions.iter().map(|v| format!("<version>{v}</version>")).collect();
        let release = versions.iter().rev().find(|v| !v.contains("SNAPSHOT") && !v.contains("rc")).unwrap().clone();
        routes.insert(meta, format!("<metadata><versioning><latest>{}</latest><release>{release}</release><versions>{list}</versions></versioning></metadata>", p[2]).into_bytes());
        drop(routes);
        self
    }

    fn fetched(&self, suffix: &str) -> usize {
        self.hits.lock().unwrap().iter().filter(|p| p.ends_with(suffix)).count()
    }
}

struct Rig {
    _dir: tempfile::TempDir,
    st: Storage,
    up: Repo,
    cfg: MavenConfig,
}

fn rig() -> Rig {
    let dir = tempfile::tempdir().unwrap();
    let st = Storage::open(dir.path()).unwrap();
    st.create_repo("m2", "maven").unwrap();
    let up = Repo::new();
    let cfg = MavenConfig { repo_url: up.base.clone(), artifacts: vec!["org.ex:app:1.0".into()], ..Default::default() };
    Rig { _dir: dir, st, up, cfg }
}

impl Rig {
    fn sync(&self) -> Result<MavenReport, MavenError> {
        sync(&self.st, "m2", &HttpClient::new(&[host_of(&self.up.base).unwrap()]), &self.cfg)
    }

    fn view(&self) -> BTreeMap<String, Vec<u8>> {
        let repo = self.st.repo("m2").unwrap();
        let Some(head) = self.st.head("m2").unwrap() else { return BTreeMap::new() };
        self.st.meta().read(|t| t.entries_at(repo.id, head.seq)).unwrap().into_iter().map(|e| (e.path, self.st.read_blob(&e.digest).unwrap())).collect()
    }

    /// File names with the given ending, without their directories.
    fn files(&self, suffix: &str) -> Vec<String> {
        let mut names: Vec<String> = self.view().keys().filter(|p| p.ends_with(suffix)).map(|p| p.rsplit('/').next().unwrap().to_string()).collect();
        names.sort();
        names
    }
}

#[test]
fn dependencies_are_followed_as_maven_would() {
    let r = rig();
    r.up
        .publish(
            "org.ex:parent:5",
            &format!(
                "<packaging>pom</packaging><properties><lib.version>2.1</lib.version><util.version>0.1</util.version></properties>\
                 <dependencyManagement><dependencies>{}{}</dependencies></dependencyManagement><dependencies>{}</dependencies>",
                dep("org.ex:managed:3.3", "<exclusions><exclusion><groupId>org.ex</groupId><artifactId>unwanted</artifactId></exclusion></exclusions>"),
                dep("org.ex:util:${util.version}", ""),
                dep("org.ex:from-parent:1.0", "")
            ),
        )
        .publish(
            "org.ex:app:1.0",
            &format!(
                "<parent><groupId>org.ex</groupId><artifactId>parent</artifactId><version>5</version></parent>\
                 <properties><util.version>0.2</util.version></properties><dependencies>{}{}{}{}{}{}{}{}</dependencies>",
                dep("org.ex:lib:${lib.version}", ""),
                dep("org.ex:managed:", ""),
                dep("org.ex:util:", ""),
                dep("org.ex:rt:1.0:runtime", ""),
                dep("org.ex:tests-only:1.0:test", ""),
                dep("org.ex:container:1.0:provided", ""),
                dep("org.ex:maybe:1.0", "<optional>true</optional>"),
                dep("org.ex:same-as-me:${project.version}", "")
            ),
        )
        .publish("org.ex:lib:2.1", &format!("<dependencies>{}</dependencies>", dep("org.ex:deep:1.0", "")))
        .publish("org.ex:managed:3.3", &format!("<dependencies>{}{}</dependencies>", dep("org.ex:unwanted:1.0", ""), dep("org.ex:deep:1.0", "")))
        .publish("org.ex:util:0.1", "")
        .publish("org.ex:util:0.2", "")
        .publish("org.ex:rt:1.0", "")
        .publish("org.ex:deep:1.0", "")
        .publish("org.ex:from-parent:1.0", "")
        .publish("org.ex:same-as-me:1.0", "")
        .publish("org.ex:unwanted:1.0", "")
        .publish("org.ex:tests-only:1.0", "")
        .publish("org.ex:container:1.0", "")
        .publish("org.ex:maybe:1.0", "");
    let rep = r.sync().unwrap();
    assert_eq!(
        r.files(".jar"),
        ["app-1.0.jar", "deep-1.0.jar", "from-parent-1.0.jar", "lib-2.1.jar", "managed-3.3.jar", "rt-1.0.jar", "same-as-me-1.0.jar", "util-0.2.jar"],
        "no test, provided or optional dependency; the excluded one is left out; the child's property wins"
    );
    assert!(r.files(".pom").contains(&"parent-5.pom".to_string()), "the parent POM is needed to read the child");
    assert_eq!((rep.artifacts, rep.poms, rep.snapshot), (8, 9, Some(1)));
    assert!(rep.warnings.is_empty(), "{:?}", rep.warnings);

    // Every file has the checksum file Maven asks for, and it is right.
    let v = r.view();
    for (path, body) in v.iter().filter(|(p, _)| !p.ends_with(".sha1")) {
        assert_eq!(v[&format!("{path}.sha1")], hex::encode(Sha1::digest(body)).into_bytes(), "{path}");
    }
    let meta = String::from_utf8(v["org/ex/util/maven-metadata.xml"].clone()).unwrap();
    assert!(meta.contains("<release>0.2</release>") && meta.contains("<version>0.2</version>") && !meta.contains("0.1"), "{meta}");
}

#[test]
fn boms_manage_versions() {
    let mut r = rig();
    r.up
        .publish("org.ex:bom:9", &format!("<packaging>pom</packaging><properties><x>4.0</x></properties><dependencyManagement><dependencies>{}{}</dependencies></dependencyManagement>", dep("org.ex:lib:${x}", ""), dep("org.ex:deep:2.0", "")))
        .publish(
            "org.ex:app:1.0",
            &format!(
                "<dependencyManagement><dependencies>{}</dependencies></dependencyManagement><dependencies>{}</dependencies>",
                "<dependency><groupId>org.ex</groupId><artifactId>bom</artifactId><version>9</version><type>pom</type><scope>import</scope></dependency>",
                dep("org.ex:lib:", "")
            ),
        )
        .publish("org.ex:lib:4.0", &format!("<dependencies>{}</dependencies>", dep("org.ex:deep:1.0", "")))
        .publish("org.ex:deep:1.0", "")
        .publish("org.ex:deep:2.0", "");
    // The BOM imported by app gives lib its version. Below lib: the version lib names (for a
    // project that depends on app), and the one app's BOM manages (when app is resolved on its own).
    r.sync().unwrap();
    assert_eq!(r.files(".jar"), ["app-1.0.jar", "deep-1.0.jar", "deep-2.0.jar", "lib-4.0.jar"]);
    assert!(r.files(".pom").contains(&"bom-9.pom".to_string()));

    // A project that imports the BOM itself gets the BOM's version everywhere.
    r.cfg.boms = vec!["org.ex:bom:9".into()];
    r.sync().unwrap();
    assert_eq!(r.files(".jar"), ["app-1.0.jar", "deep-2.0.jar", "lib-4.0.jar"]);
    // And an artifact named without a version is the one the BOM manages.
    r.up.publish("org.ex:lib:5.0", "");
    r.cfg.artifacts = vec!["org.ex:lib".into()];
    r.sync().unwrap();
    assert_eq!(r.files(".jar"), ["deep-2.0.jar", "lib-4.0.jar"]);
    r.cfg.boms.clear();
    r.sync().unwrap();
    assert_eq!(r.files(".jar"), ["lib-5.0.jar"], "without a BOM: the latest release");
    r.cfg.boms = vec!["org.ex:nope:1".into()];
    assert!(matches!(r.sync(), Err(MavenError::NotFound { .. })));
}

#[test]
fn exclusions_ranges_types_and_relocations() {
    let mut r = rig();
    let exclude_c = "<exclusions><exclusion><groupId>org.ex</groupId><artifactId>c</artifactId></exclusion></exclusions>";
    r.up
        .publish("org.ex:app:1.0", &format!("<packaging>war</packaging><dependencies>{}{}{}{}{}</dependencies>", dep("org.ex:a:1.0", exclude_c), dep("org.ex:b:1.0", ""), dep("org.ex:ranged:[1.0,2.0)", ""), dep("org.ex:a:1.0", "<type>test-jar</type>"), dep("org.old:moved:1.0", "")))
        .publish("org.ex:a:1.0", &format!("<dependencies>{}</dependencies>", dep("org.ex:shared:1.0", "")))
        .publish("org.ex:b:1.0", &format!("<dependencies>{}</dependencies>", dep("org.ex:shared:1.0", "")))
        // shared needs c: excluded below a, but not below b, so it is needed after all.
        .publish("org.ex:shared:1.0", &format!("<dependencies>{}</dependencies>", dep("org.ex:c:1.0", "")))
        .publish("org.ex:c:1.0", "")
        .publish("org.ex:ranged:1.0", "")
        .publish("org.ex:ranged:1.9", "")
        .publish("org.ex:ranged:2.0-rc1", "")
        .publish("org.ex:ranged:2.0", "")
        .publish("org.old:moved:1.0", "<distributionManagement><relocation><groupId>org.new</groupId></relocation></distributionManagement>")
        .publish("org.new:moved:1.0", "");
    r.up.put("org/ex/a/1.0/a-1.0-tests.jar", b"tests of a");
    r.up.put("org/ex/a/1.0/a-1.0-sources.jar", b"sources of a");
    r.cfg.classifiers = vec!["sources".into()];
    let rep = r.sync().unwrap();
    assert_eq!(r.files(".jar"), ["a-1.0-sources.jar", "a-1.0-tests.jar", "a-1.0.jar", "b-1.0.jar", "c-1.0.jar", "moved-1.0.jar", "ranged-2.0-rc1.jar", "shared-1.0.jar"]);
    assert_eq!(r.files(".war"), ["app-1.0.war"]);
    assert!(r.view().contains_key("org/old/moved/1.0/moved-1.0.pom") && r.view().contains_key("org/new/moved/1.0/moved-1.0.jar"));
    assert!(rep.warnings.is_empty(), "{:?}", rep.warnings);
}

#[test]
fn checksums_are_checked_and_gaps_are_reported() {
    let r = rig();
    r.up.publish("org.ex:app:1.0", "");
    r.up.routes.lock().unwrap().insert("/maven2/org/ex/app/1.0/app-1.0.jar".into(), b"tampered".to_vec());
    match r.sync() {
        Err(MavenError::Checksum { path, .. }) => assert!(path.ends_with("app-1.0.jar")),
        other => panic!("{other:?}"),
    }
    assert!(r.st.head("m2").unwrap().is_none());

    let r = rig();
    r.up.publish("org.ex:app:1.0", &format!("<dependencies>{}{}{}{}</dependencies>", dep("org.ex:gone:1.0", ""), dep("org.ex:noversion:", ""), dep("org.ex:odd:${env.SECRET}", ""), dep("org.ex:broken:1.0", "")));
    r.up.put("org/ex/broken/1.0/broken-1.0.pom", b"<project><oops></project>");
    r.up.put("org/ex/broken/1.0/broken-1.0.jar", b"jar of broken");
    // "sha1  filename" is a form older tools wrote.
    let jar_sum = hex::encode(Sha1::digest(b"jar of org.ex:app:1.0"));
    r.up.routes.lock().unwrap().insert("/maven2/org/ex/app/1.0/app-1.0.jar.sha1".into(), format!("{jar_sum}  app-1.0.jar\n").into_bytes());
    r.up.routes.lock().unwrap().remove("/maven2/org/ex/app/1.0/app-1.0.pom.sha1");
    let rep = r.sync().unwrap();
    assert_eq!(r.files(".jar"), ["app-1.0.jar", "broken-1.0.jar"], "a POM that cannot be read still leaves its jar usable");
    for needle in ["org.ex:gone:1.0 is depended on", "noversion has no version", "odd uses a property", "broken:1.0 cannot be read", "app-1.0.pom: upstream has no checksum"] {
        assert!(rep.warnings.iter().any(|w| w.contains(needle)), "{needle}: {:?}", rep.warnings);
    }
    assert_eq!(rep.warnings.len(), 5, "{:?}", rep.warnings);
}

#[test]
fn problems_are_reported_clearly() {
    let mut r = rig();
    r.up.publish("org.ex:app:1.0", "");
    for (spec, kind) in [("org.ex:nope:1.0", "missing"), ("org.ex:nope", "missing"), ("org.ex", "config"), ("org.ex:app:1.0:jar:x", "config"), ("../x:app:1.0", "config"), ("", "config")] {
        r.cfg.artifacts = vec![spec.into()];
        match (r.sync(), kind) {
            (Err(MavenError::NotFound { .. }), "missing") | (Err(MavenError::Config(_)), "config") => {}
            (other, _) => panic!("{spec:?}: {other:?}"),
        }
    }
    r.cfg.artifacts = vec!["org.ex:app:1.0".into()];
    r.cfg.classifiers = vec!["../sources".into()];
    assert!(matches!(r.sync(), Err(MavenError::Config(_))));
    r.cfg.classifiers.clear();
    r.st.create_repo("files", "raw").unwrap();
    let client = HttpClient::new(&[host_of(&r.up.base).unwrap()]);
    assert!(matches!(sync(&r.st, "files", &client, &r.cfg), Err(MavenError::Config(_))));
}

#[test]
fn a_second_sync_downloads_only_what_is_new() {
    let r = rig();
    r.up.publish("org.ex:app:1.0", &format!("<dependencies>{}</dependencies>", dep("org.ex:lib:1.0", ""))).publish("org.ex:lib:1.0", "");
    let first = r.sync().unwrap();
    assert_eq!((first.snapshot, first.downloaded), (Some(1), 4));
    let again = r.sync().unwrap();
    assert_eq!((again.snapshot, again.downloaded, again.reused), (None, 0, 4));
    assert_eq!(r.up.fetched(".jar"), 2);
    r.up.publish("org.ex:app:1.1", &format!("<dependencies>{}</dependencies>", dep("org.ex:lib:1.0", "")));
    let mut r = r;
    r.cfg.artifacts = vec!["org.ex:app".into()];
    let rep = r.sync().unwrap();
    assert_eq!((rep.downloaded, rep.reused), (2, 2));
    assert_eq!(r.files(".jar"), ["app-1.1.jar", "lib-1.0.jar"]);
}

#[test]
fn an_artifact_resolved_on_its_own_manages_the_versions_below_it() {
    // A build plugin is the root of its own resolution: what its POM (or
    // its parent) manages replaces the versions its dependencies ask for.
    let r = rig();
    r.up
        .publish(
            "org.ex:app:1.0",
            &format!("<dependencyManagement><dependencies>{}</dependencies></dependencyManagement><dependencies>{}</dependencies>", dep("org.ex:io:2.16", ""), dep("org.ex:archiver:1.0", "")),
        )
        .publish("org.ex:archiver:1.0", &format!("<dependencies>{}</dependencies>", dep("org.ex:io:2.15", "")))
        .publish("org.ex:io:2.15", "")
        .publish("org.ex:io:2.16", "");
    r.sync().unwrap();
    assert_eq!(r.files(".jar"), ["app-1.0.jar", "archiver-1.0.jar", "io-2.15.jar", "io-2.16.jar"], "2.15 for a project that depends on app, 2.16 when app is resolved on its own");
}
