//! Sync tests against a fake registry that requires token authentication.
use crate::*;
use mer_fetch::{host_of, HttpClient};
use serde_json::json;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

#[derive(Default)]
struct State {
    /// "name/manifests/ref" or "name/blobs/sha256:hex" -> body
    objects: HashMap<String, Vec<u8>>,
    /// Paths requested with a valid token.
    served: Vec<String>,
    /// Scopes for which a token was requested.
    scopes: Vec<String>,
    /// Lie about a manifest's digest in the response header.
    wrong_digest_header: bool,
}

struct Registry {
    base: String,
    state: Arc<Mutex<State>>,
}

const TOKEN: &str = "t0ken-for-tests";

impl Registry {
    fn new() -> Registry {
        let state: Arc<Mutex<State>> = Arc::default();
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let base = format!("http://{}", server.server_addr().to_ip().unwrap());
        let (st, b) = (state.clone(), base.clone());
        std::thread::spawn(move || {
            for req in server.incoming_requests() {
                let url = req.url().to_string();
                let header = |n: &str| tiny_http::Header::from_bytes(n.as_bytes(), &b""[..]).unwrap().field;
                let _ = header;
                if let Some(query) = url.strip_prefix("/token?") {
                    let scope = query.split('&').find_map(|kv| kv.strip_prefix("scope=")).unwrap_or("").to_string();
                    st.lock().unwrap().scopes.push(scope);
                    let _ = req.respond(tiny_http::Response::from_string(json!({ "token": TOKEN }).to_string()));
                    continue;
                }
                let Some(path) = url.strip_prefix("/v2/") else {
                    let _ = req.respond(tiny_http::Response::from_string("no").with_status_code(404));
                    continue;
                };
                let authorized = req.headers().iter().any(|h| h.field.equiv("Authorization") && h.value.as_str() == format!("Bearer {TOKEN}"));
                if !authorized {
                    let name = path.split("/manifests/").next().unwrap().split("/blobs/").next().unwrap();
                    let challenge = format!("Bearer realm=\"{b}/token\",service=\"fake registry\",scope=\"repository:{name}:pull,push\"");
                    let resp = tiny_http::Response::from_string("{}")
                        .with_status_code(401)
                        .with_header(tiny_http::Header::from_bytes(&b"WWW-Authenticate"[..], challenge.as_bytes()).unwrap());
                    let _ = req.respond(resp);
                    continue;
                }
                let mut s = st.lock().unwrap();
                s.served.push(path.to_string());
                match s.objects.get(path).cloned() {
                    Some(body) => {
                        let mut digest = format!("sha256:{}", Digest::of(&body));
                        if s.wrong_digest_header && path.contains("/manifests/") {
                            digest = format!("sha256:{}", Digest::of(b"something else"));
                        }
                        drop(s);
                        let resp = tiny_http::Response::from_data(body)
                            .with_header(tiny_http::Header::from_bytes(&b"Docker-Content-Digest"[..], digest.as_bytes()).unwrap());
                        let _ = req.respond(resp);
                    }
                    None => {
                        drop(s);
                        let _ = req.respond(tiny_http::Response::from_string("{}").with_status_code(404));
                    }
                }
            }
        });
        Registry { base, state }
    }

    fn put(&self, key: String, body: Vec<u8>) {
        self.state.lock().unwrap().objects.insert(key, body);
    }

    fn blob(&self, name: &str, body: &[u8]) -> Value {
        let d = Digest::of(body);
        self.put(format!("{name}/blobs/sha256:{d}"), body.to_vec());
        json!({ "mediaType": "application/vnd.docker.image.rootfs.diff.tar.gzip", "digest": format!("sha256:{d}"), "size": body.len() })
    }

    /// Publish a single-platform image; returns its manifest digest.
    fn image(&self, name: &str, config: &[u8], layers: &[&[u8]]) -> Digest {
        let manifest = json!({
            "schemaVersion": 2,
            "mediaType": "application/vnd.docker.distribution.manifest.v2+json",
            "config": self.blob(name, config),
            "layers": layers.iter().map(|l| self.blob(name, l)).collect::<Vec<_>>(),
        });
        self.manifest(name, manifest)
    }

    fn manifest(&self, name: &str, doc: Value) -> Digest {
        let body = doc.to_string().into_bytes();
        let d = Digest::of(&body);
        self.put(format!("{name}/manifests/sha256:{d}"), body);
        d
    }

    fn tag(&self, name: &str, tag: &str, digest: Digest) {
        let body = self.state.lock().unwrap().objects[&format!("{name}/manifests/sha256:{digest}")].clone();
        self.put(format!("{name}/manifests/{tag}"), body);
    }

    /// Publish a multi-platform index; returns (index digest, per-platform digests).
    fn index(&self, name: &str, platforms: &[(&str, &str, Digest)]) -> Digest {
        let mut manifests: Vec<Value> = platforms
            .iter()
            .map(|(os, arch, d)| json!({ "mediaType": "application/vnd.oci.image.manifest.v1+json", "digest": format!("sha256:{d}"), "size": 500, "platform": { "os": os, "architecture": arch } }))
            .collect();
        // Real indexes carry one attestation manifest per platform image, with
        // an "unknown" platform and an annotation naming the image it describes.
        for (_, arch, d) in platforms {
            let att = self.image(name, format!("attestation config {arch}").as_bytes(), &[format!("attestation layer {arch}").as_bytes()]);
            manifests.push(json!({
                "digest": format!("sha256:{att}"), "size": 10,
                "platform": { "os": "unknown", "architecture": "unknown" },
                "annotations": { "vnd.docker.reference.type": "attestation-manifest", "vnd.docker.reference.digest": format!("sha256:{d}") },
            }));
        }
        self.manifest(name, json!({ "schemaVersion": 2, "mediaType": "application/vnd.oci.image.index.v1+json", "manifests": manifests }))
    }

    fn blob_downloads(&self) -> usize {
        self.state.lock().unwrap().served.iter().filter(|p| p.contains("/blobs/")).count()
    }
}

struct Rig {
    _dir: tempfile::TempDir,
    st: Storage,
    reg: Registry,
    cfg: OciConfig,
}

fn rig() -> Rig {
    let dir = tempfile::tempdir().unwrap();
    let st = Storage::open(dir.path()).unwrap();
    st.create_repo("hub", "oci").unwrap();
    let reg = Registry::new();
    let cfg = OciConfig { registry: reg.base.clone(), images: vec!["library/app:1.0".into()], platforms: vec!["linux/amd64".into()] };
    Rig { _dir: dir, st, reg, cfg }
}

impl Rig {
    fn sync(&self) -> Result<OciReport, OciError> {
        sync(&self.st, "hub", &HttpClient::new(&[host_of(&self.reg.base).unwrap()]), &self.cfg)
    }

    fn paths(&self) -> Vec<String> {
        let repo = self.st.repo("hub").unwrap();
        let Some(head) = self.st.head("hub").unwrap() else { return Vec::new() };
        self.st.meta().read(|t| t.entries_at(repo.id, head.seq)).unwrap().into_iter().map(|e| e.path).collect()
    }

    fn read(&self, path: &str) -> Vec<u8> {
        let head = self.st.head("hub").unwrap().unwrap().seq;
        let e = self.st.get_at("hub", head, path).unwrap().unwrap_or_else(|| panic!("{path} missing"));
        self.st.read_blob(&e.digest).unwrap()
    }
}

#[test]
fn an_image_is_mirrored_as_tag_manifest_config_and_layers() {
    let r = rig();
    let d = r.reg.image("library/app", b"{\"config\":1}", &[b"layer one", b"layer two"]);
    r.reg.tag("library/app", "1.0", d);
    let rep = r.sync().unwrap();
    assert_eq!(rep.images, vec![("library/app:1.0".to_string(), format!("sha256:{d}"))]);
    assert_eq!((rep.manifests, rep.blobs, rep.downloaded, rep.snapshot), (1, 3, 3, Some(1)));
    assert_eq!(
        r.paths(),
        [
            format!("blobs/sha256/{}", Digest::of(b"{\"config\":1}")),
            format!("blobs/sha256/{}", Digest::of(b"layer one")),
            format!("blobs/sha256/{}", Digest::of(b"layer two")),
            format!("manifests/library/app/sha256/{d}"),
            "tags/library/app/1.0".to_string(),
        ]
        .into_iter()
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>()
    );
    assert_eq!(r.read("tags/library/app/1.0"), format!("sha256:{d}\n").as_bytes());
    // The manifest is stored byte for byte: its digest is what clients pin.
    assert_eq!(Digest::of(&r.read(&format!("manifests/library/app/sha256/{d}"))), d);
}

#[test]
fn the_token_flow_asks_only_for_pull_on_the_one_repository() {
    let r = rig();
    let d = r.reg.image("library/app", b"c", &[b"l"]);
    r.reg.tag("library/app", "1.0", d);
    r.sync().unwrap();
    // The registry suggested "pull,push"; the mirror asks for pull only.
    assert_eq!(r.reg.state.lock().unwrap().scopes, ["repository:library/app:pull"]);
}

#[test]
fn only_the_wanted_platforms_of_a_multi_platform_image_are_fetched() {
    let mut r = rig();
    let amd = r.reg.image("library/app", b"amd config", &[b"shared base", b"amd layer"]);
    let arm = r.reg.image("library/app", b"arm config", &[b"shared base", b"arm layer"]);
    let win = r.reg.image("library/app", b"win config", &[b"win layer"]);
    let idx = r.reg.index("library/app", &[("linux", "amd64", amd), ("linux", "arm64", arm), ("windows", "amd64", win)]);
    r.reg.tag("library/app", "1.0", idx);

    let rep = r.sync().unwrap();
    // The amd64 image and the attestation that describes it; nothing else.
    assert_eq!((rep.manifests, rep.blobs), (2, 5));
    let paths = r.paths();
    assert!(paths.contains(&format!("blobs/sha256/{}", Digest::of(b"attestation layer amd64"))), "the attestation of a kept image travels with it");
    assert!(!paths.contains(&format!("blobs/sha256/{}", Digest::of(b"attestation layer arm64"))));
    assert_eq!(paths.iter().filter(|p| p.starts_with("manifests/")).count(), 3, "index, image, attestation");
    assert!(paths.contains(&format!("manifests/library/app/sha256/{idx}")), "the index itself is kept unmodified");
    assert!(paths.contains(&format!("manifests/library/app/sha256/{amd}")));
    assert!(!paths.iter().any(|p| p.contains(&arm.to_hex()) || p.contains(&win.to_hex())));
    assert!(!paths.contains(&format!("blobs/sha256/{}", Digest::of(b"arm layer"))));
    assert_eq!(r.read("tags/library/app/1.0"), format!("sha256:{idx}\n").as_bytes(), "the tag names the index, as upstream does");

    // Adding a platform fetches only what is new; the shared base layer is reused.
    r.cfg.platforms.push("linux/arm64".into());
    let rep = r.sync().unwrap();
    assert_eq!((rep.manifests, rep.downloaded, rep.reused), (4, 4, 5));
}

#[test]
fn a_layer_shared_by_two_images_is_stored_and_downloaded_once() {
    let mut r = rig();
    let a = r.reg.image("library/app", b"config a", &[b"common base layer", b"app a"]);
    let b = r.reg.image("team/tool", b"config b", &[b"common base layer", b"tool b"]);
    r.reg.tag("library/app", "1.0", a);
    r.reg.tag("team/tool", "latest", b);
    r.cfg.images = vec!["library/app:1.0".into(), "team/tool".into()];
    let rep = r.sync().unwrap();
    assert_eq!((rep.blobs, rep.downloaded), (5, 5));
    assert_eq!(r.reg.blob_downloads(), 5, "six blob references, five downloads");
    assert_eq!(r.paths().iter().filter(|p| p.starts_with("blobs/")).count(), 5);
    assert!(r.paths().contains(&"tags/team/tool/latest".to_string()), "no tag means latest");
}

#[test]
fn pinning_by_digest_needs_no_tag() {
    let mut r = rig();
    let d = r.reg.image("library/app", b"c", &[b"l"]);
    r.cfg.images = vec![format!("library/app@sha256:{d}")];
    r.sync().unwrap();
    assert!(!r.paths().iter().any(|p| p.starts_with("tags/")));
    assert!(r.paths().contains(&format!("manifests/library/app/sha256/{d}")));
}

#[test]
fn every_object_must_match_the_digest_that_refers_to_it() {
    // A layer whose bytes were swapped on the registry.
    let r = rig();
    let d = r.reg.image("library/app", b"c", &[b"genuine layer"]);
    r.reg.tag("library/app", "1.0", d);
    r.reg.put(format!("library/app/blobs/sha256:{}", Digest::of(b"genuine layer")), b"backdoored".to_vec());
    assert!(matches!(r.sync(), Err(OciError::Storage(_))));
    assert!(r.st.head("hub").unwrap().is_none());

    // A platform manifest swapped under its digest.
    let r = rig();
    let amd = r.reg.image("library/app", b"c", &[b"l"]);
    let idx = r.reg.index("library/app", &[("linux", "amd64", amd)]);
    r.reg.tag("library/app", "1.0", idx);
    r.reg.put(format!("library/app/manifests/sha256:{amd}"), b"{\"schemaVersion\":2,\"config\":{}}".to_vec());
    match r.sync() {
        Err(OciError::Registry(m)) => assert!(m.contains("different content"), "{m}"),
        other => panic!("{other:?}"),
    }

    // A registry that announces one digest and sends other content.
    let r = rig();
    let d = r.reg.image("library/app", b"c", &[b"l"]);
    r.reg.tag("library/app", "1.0", d);
    r.reg.state.lock().unwrap().wrong_digest_header = true;
    match r.sync() {
        Err(OciError::Registry(m)) => assert!(m.contains("announced"), "{m}"),
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_missing_platform_is_a_clear_error() {
    let mut r = rig();
    let arm = r.reg.image("library/app", b"c", &[b"l"]);
    let idx = r.reg.index("library/app", &[("linux", "arm64", arm)]);
    r.reg.tag("library/app", "1.0", idx);
    match r.sync() {
        Err(OciError::Registry(m)) => assert!(m.contains("no image for linux/amd64") && m.contains("linux/arm64") && !m.contains("unknown"), "{m}"),
        other => panic!("{other:?}"),
    }
    r.cfg.images = vec!["library/app:does-not-exist".into()];
    assert!(matches!(r.sync(), Err(OciError::Fetch(_))));
}

#[test]
fn layers_hosted_elsewhere_and_old_manifest_formats() {
    let r = rig();
    let mut foreign = r.reg.blob("library/app", b"windows base");
    foreign["urls"] = json!(["https://elsewhere.example/layer"]);
    let d = r.reg.manifest("library/app", json!({ "schemaVersion": 2, "config": r.reg.blob("library/app", b"c"), "layers": [foreign, r.reg.blob("library/app", b"own layer")] }));
    r.reg.tag("library/app", "1.0", d);
    let rep = r.sync().unwrap();
    assert_eq!(rep.blobs, 2);
    assert!(rep.warnings[0].contains("another location"));

    let r = rig();
    let d = r.reg.manifest("library/app", json!({ "schemaVersion": 1, "fsLayers": [] }));
    r.reg.tag("library/app", "1.0", d);
    assert!(matches!(r.sync(), Err(OciError::Registry(m)) if m.contains("schema version 2")));

    let r = rig();
    let d = r.reg.manifest("library/app", json!({ "schemaVersion": 2, "config": { "digest": "md5:abc", "size": 1 }, "layers": [] }));
    r.reg.tag("library/app", "1.0", d);
    assert!(matches!(r.sync(), Err(OciError::Registry(m)) if m.contains("sha256")));
}

#[test]
fn references_are_validated() {
    assert_eq!(parse_image("library/alpine:3.20"), Some(("library/alpine".into(), Reference::Tag("3.20".into()))));
    assert_eq!(parse_image("alpine"), Some(("alpine".into(), Reference::Tag("latest".into()))));
    assert_eq!(parse_image("a/b/c:v1_2-x"), Some(("a/b/c".into(), Reference::Tag("v1_2-x".into()))));
    let d = Digest::of(b"x");
    assert_eq!(parse_image(&format!("library/alpine@sha256:{d}")), Some(("library/alpine".into(), Reference::Digest(d))));
    for bad in ["", "Library/Alpine", "library/../etc", "/alpine", "alpine/", "a//b", "alpine:", "alpine:bad tag", "alpine@sha256:abc", "alpine@md5:0123", "alpine:-x", "-a/b", "a b"] {
        assert_eq!(parse_image(bad), None, "{bad:?}");
    }
    assert!(platform_wanted(&["linux/arm64".into()], "linux/arm64/v8"));
    assert!(!platform_wanted(&["linux/arm".into()], "linux/arm64"));
    let mut r = rig();
    r.cfg.images = vec!["Not/Valid".into()];
    assert!(matches!(r.sync(), Err(OciError::Config(_))));
    r.st.create_repo("files", "raw").unwrap();
    let client = HttpClient::new(&[host_of(&r.reg.base).unwrap()]);
    assert!(matches!(sync(&r.st, "files", &client, &r.cfg), Err(OciError::Config(_))));
}

#[test]
fn a_second_sync_fetches_nothing_and_a_moved_tag_makes_a_new_snapshot() {
    let r = rig();
    let v1 = r.reg.image("library/app", b"c1", &[b"base", b"v1"]);
    r.reg.tag("library/app", "1.0", v1);
    assert_eq!(r.sync().unwrap().snapshot, Some(1));
    let before = r.reg.blob_downloads();
    let again = r.sync().unwrap();
    assert_eq!((again.snapshot, again.downloaded, again.reused), (None, 0, 3));
    assert_eq!(r.reg.blob_downloads(), before);

    // Upstream rebuilds the image and moves the tag.
    let v2 = r.reg.image("library/app", b"c2", &[b"base", b"v2"]);
    r.reg.tag("library/app", "1.0", v2);
    let rep = r.sync().unwrap();
    assert_eq!((rep.downloaded, rep.reused), (2, 1));
    assert_eq!(r.read("tags/library/app/1.0"), format!("sha256:{v2}\n").as_bytes());
    assert!(!r.paths().contains(&format!("manifests/library/app/sha256/{v1}")), "the old image leaves the new snapshot");
    assert!(r.st.get_at("hub", 1, &format!("manifests/library/app/sha256/{v1}")).unwrap().is_some(), "and stays in the old one");
}
