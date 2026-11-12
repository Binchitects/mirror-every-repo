//! Sync tests against a fake NuGet V3 feed served over HTTP.
use crate::*;
use base64::Engine;
use mer_fetch::{host_of, HttpClient};
use mer_storage::Storage;
use serde_json::{json, Value};
use sha2::{Digest as _, Sha512};
use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};

type Routes = Arc<Mutex<HashMap<String, Vec<u8>>>>;

/// One published version: (version, listed, dependency groups).
type Published = (String, bool, Vec<(String, Vec<(String, String)>)>);

struct FakeFeed {
    base: String,
    routes: Routes,
    hits: Arc<Mutex<Vec<String>>>,
    /// id (as published) -> versions
    packages: BTreeMap<String, Vec<Published>>,
    /// Serve registration pages as separate documents, like large packages on nuget.org.
    paged: bool,
    /// Leave the hash out of catalog records.
    no_hashes: bool,
}

fn nupkg(id: &str, version: &str) -> Vec<u8> {
    format!("nupkg {id} {version} ").repeat(40).into_bytes()
}

impl FakeFeed {
    fn new() -> FakeFeed {
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
                    None => req.respond(tiny_http::Response::from_string("{}").with_status_code(404)),
                };
            }
        });
        let feed = FakeFeed { base, routes, hits, packages: BTreeMap::new(), paged: false, no_hashes: false };
        feed.flush();
        feed
    }

    /// Publish a version. `groups` are (target framework, [(dependency id, range)]).
    fn publish(&mut self, id: &str, version: &str, groups: &[(&str, &[(&str, &str)])]) -> &mut Self {
        self.publish_as(id, version, true, groups)
    }

    fn publish_as(&mut self, id: &str, version: &str, listed: bool, groups: &[(&str, &[(&str, &str)])]) -> &mut Self {
        let groups = groups.iter().map(|(tfm, deps)| (tfm.to_string(), deps.iter().map(|(d, r)| (d.to_string(), r.to_string())).collect())).collect();
        self.packages.entry(id.to_string()).or_default().push((version.to_string(), listed, groups));
        self.flush();
        self
    }

    fn flush(&self) {
        let mut routes = self.routes.lock().unwrap();
        let b = &self.base;
        routes.insert(
            "/v3/index.json".into(),
            json!({ "version": "3.0.0", "resources": [
                { "@id": format!("{b}/flat/"), "@type": "PackageBaseAddress/3.0.0" },
                { "@id": format!("{b}/reg/"), "@type": "RegistrationsBaseUrl/3.6.0" },
                { "@id": format!("{b}/search"), "@type": "SearchQueryService" },
            ] }).to_string().into_bytes(),
        );
        for (id, versions) in &self.packages {
            let lower = id.to_ascii_lowercase();
            let mut sorted: Vec<&Published> = versions.iter().collect();
            sorted.sort_by(|a, b| Version::parse(&a.0).unwrap().cmp(&Version::parse(&b.0).unwrap()));
            routes.insert(format!("/flat/{lower}/index.json"), json!({ "versions": sorted.iter().map(|p| p.0.to_ascii_lowercase()).collect::<Vec<_>>() }).to_string().into_bytes());
            let mut leaves = Vec::new();
            for (version, listed, groups) in &sorted {
                let ver = Version::parse(version).unwrap().normalized();
                let body = nupkg(id, &ver);
                routes.insert(format!("/flat/{lower}/{ver}/{lower}.{ver}.nupkg"), body.clone());
                let mut leaf = json!({ "id": id, "version": version, "listed": listed });
                if !self.no_hashes {
                    leaf["packageHash"] = json!(base64::engine::general_purpose::STANDARD.encode(Sha512::digest(&body)));
                    leaf["packageHashAlgorithm"] = json!("SHA512");
                }
                routes.insert(format!("/catalog/{lower}.{ver}.json"), leaf.to_string().into_bytes());
                leaves.push(json!({ "catalogEntry": {
                    "@id": format!("{b}/catalog/{lower}.{ver}.json"), "id": id, "version": version, "listed": listed,
                    "dependencyGroups": groups.iter().map(|(tfm, deps)| {
                        let mut g = json!({ "dependencies": deps.iter().map(|(d, r)| if r.is_empty() { json!({ "id": d }) } else { json!({ "id": d, "range": r }) }).collect::<Vec<_>>() });
                        if !tfm.is_empty() { g["targetFramework"] = json!(tfm); }
                        g
                    }).collect::<Vec<_>>(),
                } }));
            }
            let (lo, hi) = (sorted[0].0.clone(), sorted[sorted.len() - 1].0.clone());
            let page = if self.paged {
                routes.insert(format!("/reg/{lower}/page.json"), json!({ "items": leaves }).to_string().into_bytes());
                json!({ "@id": format!("{b}/reg/{lower}/page.json"), "lower": lo, "upper": hi })
            } else {
                json!({ "@id": format!("{b}/reg/{lower}/index.json#page"), "lower": lo, "upper": hi, "items": leaves })
            };
            routes.insert(format!("/reg/{lower}/index.json"), json!({ "count": 1, "items": [page] }).to_string().into_bytes());
        }
    }

    fn package_downloads(&self) -> usize {
        self.hits.lock().unwrap().iter().filter(|p| p.ends_with(".nupkg")).count()
    }
}

struct Rig {
    _dir: tempfile::TempDir,
    st: Storage,
    feed: FakeFeed,
    cfg: NugetConfig,
}

fn rig() -> Rig {
    let dir = tempfile::tempdir().unwrap();
    let st = Storage::open(dir.path()).unwrap();
    st.create_repo("nuget", "nuget").unwrap();
    let feed = FakeFeed::new();
    let cfg = NugetConfig { service_index: format!("{}/v3/index.json", feed.base), packages: vec!["App".into()], frameworks: vec!["net8.0".into()], prerelease: false };
    Rig { _dir: dir, st, feed, cfg }
}

impl Rig {
    fn sync(&self) -> Result<NugetReport, NugetError> {
        sync(&self.st, "nuget", &HttpClient::new(&[host_of(&self.feed.base).unwrap()]), &self.cfg)
    }

    fn view(&self) -> BTreeMap<String, Vec<u8>> {
        let repo = self.st.repo("nuget").unwrap();
        let Some(head) = self.st.head("nuget").unwrap() else { return BTreeMap::new() };
        self.st.meta().read(|t| t.entries_at(repo.id, head.seq)).unwrap().into_iter().map(|e| (e.path, self.st.read_blob(&e.digest).unwrap())).collect()
    }

    /// Mirrored packages as "id version".
    fn packages(&self) -> Vec<String> {
        self.view().keys().filter(|p| p.ends_with(".nupkg")).map(|p| {
            let parts: Vec<&str> = p.split('/').collect();
            format!("{} {}", parts[1], parts[2])
        }).collect()
    }
}

const ANY: &[(&str, &[(&str, &str)])] = &[];

#[test]
fn a_range_resolves_to_the_lowest_version_that_fits_as_restore_does() {
    let mut r = rig();
    r.feed
        .publish("App", "1.0.0", &[("net8.0", &[("Lib", "[1.2.0, )"), ("Exact", "[2.0.0]")])])
        .publish("Lib", "1.0.0", ANY)
        .publish("Lib", "1.2.0", ANY)
        .publish("Lib", "1.9.0", ANY)
        .publish("Lib", "2.0.0", ANY)
        .publish("Exact", "1.0.0", ANY)
        .publish("Exact", "2.0.0", ANY)
        .publish("Exact", "3.0.0", ANY)
        .publish("Unrelated", "1.0.0", ANY);
    let rep = r.sync().unwrap();
    assert_eq!(r.packages(), ["app 1.0.0", "exact 2.0.0", "lib 1.2.0"], "Lib 1.2.0, not the newest 2.0.0");
    assert_eq!((rep.packages, rep.versions, rep.downloaded, rep.unverified, rep.snapshot), (3, 3, 3, 0, Some(1)));
    assert!(rep.warnings.is_empty(), "{:?}", rep.warnings);
}

#[test]
fn a_package_named_without_a_version_means_the_newest_listed_stable_one() {
    let mut r = rig();
    r.feed.publish("App", "1.0.0", ANY).publish("App", "2.0.0", ANY).publish_as("App", "2.1.0", false, ANY).publish("App", "3.0.0-beta.1", ANY);
    r.sync().unwrap();
    assert_eq!(r.packages(), ["app 2.0.0"], "2.1.0 is unlisted and 3.0.0-beta.1 is a pre-release");
    r.cfg.prerelease = true;
    r.sync().unwrap();
    assert_eq!(r.packages(), ["app 3.0.0-beta.1"]);
}

#[test]
fn roots_can_name_a_minimum_an_exact_version_or_an_interval() {
    let mut r = rig();
    for v in ["1.0.0", "1.5.0", "2.0.0", "2.5.0"] {
        r.feed.publish("App", v, ANY);
    }
    for (spec, want) in [("App@1.5.0", "app 1.5.0"), ("App@1.2", "app 1.5.0"), ("App@[2.0.0]", "app 2.0.0"), ("App@(1.5.0, )", "app 2.0.0"), ("App@[1.0,1.5)", "app 1.0.0")] {
        r.cfg.packages = vec![spec.into()];
        r.sync().unwrap();
        assert_eq!(r.packages(), [want], "{spec}");
    }
    r.cfg.packages = vec!["App@1.0.0".into(), "App@[2.5.0]".into()];
    r.sync().unwrap();
    assert_eq!(r.packages(), ["app 1.0.0", "app 2.5.0"]);
}

#[test]
fn dependencies_come_from_the_group_the_target_framework_uses() {
    let mut r = rig();
    r.feed
        .publish("App", "1.0.0", &[
            (".NETFramework4.6.2", &[("OnlyForFramework", "1.0.0")]),
            (".NETStandard2.0", &[("ForStandard", "1.0.0"), ("Shared", "1.0.0")]),
            ("net6.0", &[("ForNet6", "1.0.0"), ("Shared", "1.0.0")]),
            ("net9.0", &[("ForNet9", "1.0.0")]),
        ])
        .publish("OnlyForFramework", "1.0.0", ANY)
        .publish("ForStandard", "1.0.0", ANY)
        .publish("ForNet6", "1.0.0", ANY)
        .publish("ForNet9", "1.0.0", ANY)
        .publish("Shared", "1.0.0", ANY);
    r.sync().unwrap();
    assert_eq!(r.packages(), ["app 1.0.0", "fornet6 1.0.0", "shared 1.0.0"], "a net8.0 project uses the net6.0 group only");

    // Projects for two frameworks need the union.
    r.cfg.frameworks = vec!["net8.0".into(), "net48".into()];
    r.sync().unwrap();
    assert_eq!(r.packages(), ["app 1.0.0", "fornet6 1.0.0", "onlyforframework 1.0.0", "shared 1.0.0"]);

    r.cfg.frameworks = vec!["netstandard2.0".into()];
    r.sync().unwrap();
    assert_eq!(r.packages(), ["app 1.0.0", "forstandard 1.0.0", "shared 1.0.0"]);
}

#[test]
fn a_package_with_no_group_for_the_target_is_mirrored_with_a_warning() {
    let mut r = rig();
    r.feed.publish("App", "1.0.0", &[(".NETFramework4.5", &[("Old", "1.0.0")])]).publish("Old", "1.0.0", ANY);
    let rep = r.sync().unwrap();
    assert_eq!(r.packages(), ["app 1.0.0"]);
    assert!(rep.warnings.iter().any(|w| w.contains("no dependency group for net8.0")), "{:?}", rep.warnings);
}

#[test]
fn two_requirements_on_one_package_keep_both_candidates() {
    let mut r = rig();
    r.feed
        .publish("App", "1.0.0", &[("net8.0", &[("A", "1.0.0"), ("B", "1.0.0")])])
        .publish("A", "1.0.0", &[("", &[("Common", "1.0.0")])])
        .publish("B", "1.0.0", &[("", &[("Common", "2.0.0")])])
        .publish("Common", "1.0.0", ANY)
        .publish("Common", "2.0.0", ANY)
        .publish("Common", "3.0.0", ANY);
    r.sync().unwrap();
    // A restore will settle on 2.0.0; 1.0.0 is kept too, 3.0.0 is not needed.
    assert_eq!(r.packages().iter().filter(|p| p.starts_with("common")).collect::<Vec<_>>(), ["common 1.0.0", "common 2.0.0"]);
    let index: Value = serde_json::from_slice(&r.view()["flat/common/index.json"]).unwrap();
    assert_eq!(index, json!({ "versions": ["1.0.0", "2.0.0"] }));
}

#[test]
fn the_generated_feed_is_lower_case_normalised_and_points_at_the_vault() {
    let mut r = rig();
    r.cfg.packages = vec!["Newtonsoft.Json@13.0.3".into()];
    r.feed.publish("Newtonsoft.Json", "13.0.3.0", ANY);
    r.sync().unwrap();
    let v = r.view();
    assert!(v.contains_key("flat/newtonsoft.json/13.0.3/newtonsoft.json.13.0.3.nupkg"), "{:?}", v.keys().collect::<Vec<_>>());
    assert_eq!(v["flat/newtonsoft.json/13.0.3/newtonsoft.json.13.0.3.nupkg"], nupkg("Newtonsoft.Json", "13.0.3"));
    let index: Value = serde_json::from_slice(&v["v3/index.json"]).unwrap();
    assert_eq!(index["version"], "3.0.0");
    assert_eq!(index["resources"][0]["@type"], "PackageBaseAddress/3.0.0");
    assert_eq!(index["resources"][0]["@id"], "mer://base/flat/");
    assert!(!String::from_utf8_lossy(&v["v3/index.json"]).contains("127.0.0.1"), "no upstream address in the vault");
}

#[test]
fn packages_are_checked_against_the_catalog_hash() {
    let mut r = rig();
    r.feed.publish("App", "1.0.0", ANY);
    r.feed.routes.lock().unwrap().insert("/flat/app/1.0.0/app.1.0.0.nupkg".into(), b"trojan".to_vec());
    match r.sync() {
        Err(NugetError::Feed(m)) => assert!(m.contains("does not match the hash"), "{m}"),
        other => panic!("{other:?}"),
    }
    assert!(r.st.head("nuget").unwrap().is_none());

    // A feed without hashes: mirrored, but said so.
    let mut r = rig();
    r.feed.no_hashes = true;
    r.feed.publish("App", "1.0.0", ANY);
    let rep = r.sync().unwrap();
    assert_eq!((rep.versions, rep.unverified), (1, 1));
    assert!(rep.warnings[0].contains("no hash"));
}

#[test]
fn registration_pages_kept_in_separate_documents_are_followed() {
    let mut r = rig();
    r.feed.paged = true;
    r.feed.publish("App", "1.0.0", &[("net8.0", &[("Lib", "1.0.0")])]).publish("Lib", "1.0.0", ANY);
    r.sync().unwrap();
    assert_eq!(r.packages(), ["app 1.0.0", "lib 1.0.0"]);
}

#[test]
fn a_dependency_without_a_range_takes_the_lowest_version() {
    let mut r = rig();
    r.feed.publish("App", "1.0.0", &[("net8.0", &[("Lib", "")])]).publish("Lib", "0.5.0", ANY).publish("Lib", "3.0.0", ANY);
    r.sync().unwrap();
    assert_eq!(r.packages(), ["app 1.0.0", "lib 0.5.0"]);
}

#[test]
fn what_cannot_be_mirrored_is_reported() {
    let mut r = rig();
    r.feed.publish("App", "1.0.0", &[("net8.0", &[("Ghost", "1.0.0"), ("TooNew", "[9.0.0, )"), ("../Evil", "1.0.0"), ("Ok", "1.0.0")])]).publish("TooNew", "1.0.0", ANY).publish("Ok", "1.0.0", ANY);
    let rep = r.sync().unwrap();
    assert_eq!(r.packages(), ["app 1.0.0", "ok 1.0.0"]);
    let w = rep.warnings.join("\n");
    for needle in ["Ghost", "TooNew", "Evil"] {
        assert!(w.contains(needle), "warning about {needle} expected:\n{w}");
    }
    for bad in ["Nope", "App@9.0.0", "App@not-a-range"] {
        r.cfg.packages = vec![bad.into()];
        assert!(matches!(r.sync(), Err(NugetError::Unsatisfiable { .. })), "{bad}");
    }
    for bad in ["", "../x", "a b"] {
        r.cfg.packages = vec![bad.into()];
        assert!(matches!(r.sync(), Err(NugetError::Config(_))), "{bad:?}");
    }
    r.cfg.packages = vec!["App".into()];
    r.cfg.frameworks = vec!["silverlight5".into()];
    assert!(matches!(r.sync(), Err(NugetError::Config(_))));
}

#[test]
fn cycles_terminate() {
    let mut r = rig();
    r.feed.publish("App", "1.0.0", &[("", &[("A", "1.0.0")])]).publish("A", "1.0.0", &[("", &[("B", "1.0.0")])]).publish("B", "1.0.0", &[("", &[("A", "1.0.0"), ("App", "1.0.0")])]);
    assert_eq!(r.sync().unwrap().versions, 3);
}

#[test]
fn a_feed_that_points_at_another_host_is_refused() {
    let r = rig();
    r.feed.routes.lock().unwrap().insert(
        "/v3/index.json".into(),
        json!({ "resources": [{ "@id": "http://elsewhere.example/flat/", "@type": "PackageBaseAddress/3.0.0" }] }).to_string().into_bytes(),
    );
    match r.sync() {
        Err(NugetError::Fetch(e)) => assert!(e.to_string().contains("allowlist"), "{e}"),
        other => panic!("{other:?}"),
    }
    r.feed.routes.lock().unwrap().insert("/v3/index.json".into(), b"{\"resources\":[]}".to_vec());
    assert!(matches!(r.sync(), Err(NugetError::Feed(m)) if m.contains("PackageBaseAddress")));
}

#[test]
fn a_second_sync_downloads_only_what_is_new() {
    let mut r = rig();
    r.feed.publish("App", "1.0.0", &[("net8.0", &[("Lib", "1.0.0")])]).publish("Lib", "1.0.0", ANY);
    assert_eq!(r.sync().unwrap().snapshot, Some(1));
    assert_eq!(r.feed.package_downloads(), 2);
    let again = r.sync().unwrap();
    assert_eq!((again.snapshot, again.downloaded, again.reused), (None, 0, 2));
    assert_eq!(r.feed.package_downloads(), 2, "nothing was downloaded again");

    // A new release of the root; its dependency is unchanged.
    r.feed.publish("App", "1.1.0", &[("net8.0", &[("Lib", "1.0.0")])]);
    let rep = r.sync().unwrap();
    assert_eq!((rep.downloaded, rep.reused), (1, 1));
    assert_eq!(r.packages(), ["app 1.1.0", "lib 1.0.0"]);
    assert!(r.st.get_at("nuget", 1, "flat/app/1.0.0/app.1.0.0.nupkg").unwrap().is_some(), "the old version stays in the old snapshot");
}

#[test]
fn only_nuget_repos_are_accepted() {
    let mut r = rig();
    r.feed.publish("App", "1.0.0", ANY);
    r.st.create_repo("files", "raw").unwrap();
    let client = HttpClient::new(&[host_of(&r.feed.base).unwrap()]);
    assert!(matches!(sync(&r.st, "files", &client, &r.cfg), Err(NugetError::Config(_))));
}
