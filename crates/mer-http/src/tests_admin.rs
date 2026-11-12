//! The web panel's API, private repositories and download statistics.
use crate::*;
use mer_auth::Role;
use mer_core::EntryRole::Artifact;
use serde_json::{json, Value};

const PW: &str = "a long enough password";

struct Rig {
    _dir: tempfile::TempDir,
    st: Arc<Storage>,
    app: Arc<App>,
    base: String,
    server: Option<HttpServer>,
}

impl Drop for Rig {
    fn drop(&mut self) {
        if let Some(s) = self.server.take() {
            s.shutdown();
        }
    }
}

fn snapshot(st: &Storage, repo: &str, files: &[(&str, &[u8])]) -> u64 {
    let mut w = st.begin(repo).unwrap();
    for (path, body) in files {
        let (d, _) = st.ingest_bytes(body).unwrap();
        w.put(path, Artifact, &d).unwrap();
    }
    w.seal(None).unwrap().unwrap().seq
}

/// A vault with users of each role, one published repo and one snapshot in quarantine.
fn rig() -> Rig {
    let dir = tempfile::tempdir().unwrap();
    let st = Arc::new(Storage::open(dir.path()).unwrap());
    let app = App::new(st.clone());
    app.auth().create_user("admin", PW, Role::Admin, "test").unwrap();
    app.auth().create_user("olga", PW, Role::Operator, "test").unwrap();
    app.auth().create_user("vic", PW, Role::Viewer, "test").unwrap();

    st.create_repo("tools", "raw").unwrap();
    let s1 = snapshot(&st, "tools", &[("a.txt", b"one"), ("gone.txt", b"bye")]);
    st.decide("tools", s1, true, "test").unwrap();
    st.publish("tools", s1, "test").unwrap();
    // Snapshot 2 changes a.txt, adds new.bin, removes gone.txt, and waits for review.
    let mut w = st.begin("tools").unwrap();
    for (path, body) in [("a.txt", &b"two!"[..]), ("new.bin", &[7u8; 1000][..])] {
        let (d, _) = st.ingest_bytes(body).unwrap();
        w.put(path, Artifact, &d).unwrap();
    }
    w.remove("gone.txt").unwrap();
    w.seal(None).unwrap();

    let cfg = ServerConfig { listen: "127.0.0.1:0".into(), threads: 4, access_log: false, ..Default::default() };
    let server = HttpServer::start(app.clone(), cfg).unwrap();
    let base = format!("http://{}", server.addr());
    Rig { _dir: dir, st, app, base, server: Some(server) }
}

/// A signed-in browser: keeps the session cookie and the CSRF token.
struct Client {
    base: String,
    cookie: Option<String>,
    csrf: Option<String>,
}

struct Resp {
    status: u16,
    json: Value,
    text: String,
    set_cookie: Option<String>,
    headers: Vec<(String, String)>,
}

impl Resp {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }
}

impl Client {
    fn new(r: &Rig) -> Client {
        Client { base: r.base.clone(), cookie: None, csrf: None }
    }

    fn login(r: &Rig, user: &str) -> Client {
        let mut c = Client::new(r);
        let resp = c.post("/api/login", json!({ "user": user, "password": PW }));
        assert_eq!(resp.status, 200, "{}", resp.text);
        let set = resp.set_cookie.expect("session cookie");
        c.cookie = Some(set.split(';').next().unwrap().to_string());
        c.csrf = Some(resp.json["csrf"].as_str().unwrap().to_string());
        c
    }

    fn send(&self, method: &str, path: &str, body: Option<Value>, extra: &[(&str, &str)]) -> Resp {
        let agent = ureq::AgentBuilder::new().redirects(0).build();
        let mut req = agent.request(method, &format!("{}{path}", self.base));
        if let Some(c) = &self.cookie {
            req = req.set("Cookie", c);
        }
        if let (Some(csrf), "POST") = (&self.csrf, method) {
            req = req.set("X-CSRF-Token", csrf);
        }
        for (k, v) in extra {
            req = req.set(k, v);
        }
        let result = match body {
            Some(b) => req.set("Content-Type", "application/json").send_string(&b.to_string()),
            None => req.call(),
        };
        let resp = match result {
            Ok(r) => r,
            Err(ureq::Error::Status(_, r)) => r,
            Err(e) => panic!("{method} {path}: {e}"),
        };
        let status = resp.status();
        let set_cookie = resp.header("Set-Cookie").map(str::to_string);
        let headers = resp.headers_names().into_iter().map(|n| (n.clone(), resp.header(&n).unwrap_or("").to_string())).collect();
        let text = resp.into_string().unwrap_or_default();
        let json = serde_json::from_str(&text).unwrap_or(Value::Null);
        Resp { status, json, text, set_cookie, headers }
    }

    fn get(&self, path: &str) -> Resp {
        self.send("GET", path, None, &[])
    }
    fn post(&self, path: &str, body: Value) -> Resp {
        self.send("POST", path, Some(body), &[])
    }
}

#[test]
fn nothing_in_the_api_works_without_signing_in() {
    let r = rig();
    let anon = Client::new(&r);
    for path in ["/api/me", "/api/overview", "/api/audit", "/api/users", "/api/tokens", "/api/wishlist", "/api/traffic", "/api/repos/tools/snapshots"] {
        assert_eq!(anon.get(path).status, 401, "{path}");
    }
    assert_eq!(anon.post("/api/repos/tools/snapshots/2/approve", json!({})).status, 401);
    assert_eq!(anon.post("/api/users", json!({ "name": "evil", "password": PW, "role": "admin" })).status, 401);
    // A made-up cookie is not a session.
    let fake = Client { base: r.base.clone(), cookie: Some(format!("mer_session={}", "0".repeat(64))), csrf: Some("x".into()) };
    assert_eq!(fake.get("/api/overview").status, 401);
    // The only open calls: is-this-a-fresh-install, and sign-in itself.
    assert_eq!(anon.get("/api/setup").json, json!({ "needs_setup": false }));
}

#[test]
fn sign_in_sets_a_locked_down_cookie_and_sign_out_ends_the_session() {
    let r = rig();
    let anon = Client::new(&r);
    let bad = anon.post("/api/login", json!({ "user": "admin", "password": "not the password" }));
    assert_eq!((bad.status, bad.set_cookie.is_none()), (401, true));
    assert_eq!(anon.post("/api/login", json!({ "user": "ghost", "password": PW })).json, bad.json, "same answer for unknown users");
    assert_eq!(anon.post("/api/login", json!({ "user": "admin" })).status, 400);
    assert_eq!(anon.send("POST", "/api/login", None, &[]).status, 400);

    let resp = anon.post("/api/login", json!({ "user": "admin", "password": PW }));
    let cookie = resp.set_cookie.clone().unwrap();
    assert!(cookie.contains("HttpOnly") && cookie.contains("SameSite=Strict") && cookie.contains("Path=/"), "{cookie}");
    assert_eq!(resp.json["role"], "admin");
    assert_eq!(resp.header("Cache-Control"), Some("no-store"));

    let c = Client::login(&r, "admin");
    assert_eq!(c.get("/api/me").json["user"], "admin");
    assert_eq!(c.post("/api/logout", json!({})).status, 200);
    assert_eq!(c.get("/api/me").status, 401, "the cookie is dead after sign-out");
}

#[test]
fn changes_need_the_csrf_token_so_other_sites_cannot_forge_them() {
    let r = rig();
    let mut c = Client::login(&r, "admin");
    let real = c.csrf.take();
    // Cookie present (as a browser would send it from any page), token missing or wrong.
    assert_eq!(c.post("/api/repos/tools/snapshots/2/approve", json!({})).status, 403);
    c.csrf = Some("guess".into());
    assert_eq!(c.post("/api/repos/tools/snapshots/2/approve", json!({})).status, 403);
    assert_eq!(c.post("/api/logout", json!({})).status, 403);
    assert_eq!(r.st.head("tools").unwrap().unwrap().state.as_str(), "sealed", "nothing changed");
    c.csrf = real;
    assert_eq!(c.post("/api/repos/tools/snapshots/2/approve", json!({})).status, 200);
}

#[test]
fn roles_limit_what_each_user_can_do() {
    let r = rig();
    let (admin, olga, vic) = (Client::login(&r, "admin"), Client::login(&r, "olga"), Client::login(&r, "vic"));

    // Everyone signed in can look.
    for c in [&admin, &olga, &vic] {
        assert_eq!(c.get("/api/overview").status, 200);
        assert_eq!(c.get("/api/audit").status, 200);
    }
    // A viewer cannot change anything.
    assert_eq!(vic.post("/api/repos/tools/snapshots/2/approve", json!({})).status, 403);
    assert_eq!(vic.post("/api/maintenance/gc", json!({})).status, 403);
    assert_eq!(vic.get("/api/users").status, 403);
    // An operator runs the gate but does not manage access or settings.
    assert_eq!(olga.get("/api/users").status, 403);
    assert_eq!(olga.get("/api/tokens").status, 403);
    assert_eq!(olga.post("/api/users", json!({ "name": "x", "password": PW, "role": "admin" })).status, 403);
    assert_eq!(olga.post("/api/users/olga", json!({ "role": "admin" })).status, 403, "no self-promotion");
    assert_eq!(olga.post("/api/tokens", json!({ "name": "t" })).status, 403);
    assert_eq!(olga.post("/api/repos/tools/settings", json!({ "private": true })).status, 403);
    assert_eq!(olga.post("/api/repos/tools/snapshots/2/approve", json!({})).status, 200);
    assert_eq!(olga.post("/api/maintenance/gc", json!({ "grace_hours": 24 })).status, 200);
    // An admin can do all of it.
    assert_eq!(admin.get("/api/users").json.as_array().unwrap().len(), 3);
    assert_eq!(admin.post("/api/repos/tools/settings", json!({ "quota_bytes": 5_000_000 })).status, 200);
    assert_eq!(r.st.repo("tools").unwrap().quota_bytes, Some(5_000_000));
}

#[test]
fn review_then_approve_publish_and_roll_back_from_the_panel() {
    let r = rig();
    let c = Client::login(&r, "olga");
    let pkg = Client::new(&r);

    let o = c.get("/api/overview").json;
    assert_eq!(o["repos"][0]["live"], 1);
    assert_eq!(o["repos"][0]["pending"], json!([2]));

    // What would change if snapshot 2 went live.
    let ch = c.get("/api/repos/tools/snapshots/2/changes").json;
    assert_eq!((ch["base"].clone(), ch["added"].clone(), ch["changed"].clone(), ch["removed"].clone()), (json!(1), json!(1), json!(1), json!(1)));
    assert_eq!(ch["bytes"], 1004);
    let items: Vec<(String, String)> = ch["items"].as_array().unwrap().iter()
        .map(|i| (i["op"].as_str().unwrap().into(), i["path"].as_str().unwrap().into())).collect();
    assert_eq!(items, [("change".into(), "a.txt".into()), ("remove".into(), "gone.txt".into()), ("add".into(), "new.bin".into())]);

    // Publishing before approval is refused; clients still get snapshot 1.
    assert_eq!(c.post("/api/repos/tools/snapshots/2/publish", json!({})).status, 409);
    assert_eq!(pkg.get("/tools/a.txt").text, "one");

    assert_eq!(c.post("/api/repos/tools/snapshots/2/approve", json!({})).status, 200);
    assert_eq!(c.post("/api/repos/tools/snapshots/2/publish", json!({})).status, 200);
    assert_eq!(pkg.get("/tools/a.txt").text, "two!");
    assert_eq!(pkg.get("/tools/gone.txt").status, 404);

    // Looking at snapshot 1 now shows what a rollback would do.
    let back = c.get("/api/repos/tools/snapshots/1/changes").json;
    assert_eq!((back["base"].clone(), back["added"].clone(), back["removed"].clone()), (json!(2), json!(1), json!(1)));
    assert_eq!(c.post("/api/repos/tools/snapshots/1/rollback", json!({})).status, 200);
    assert_eq!(pkg.get("/tools/a.txt").text, "one");

    let snaps = c.get("/api/repos/tools/snapshots").json;
    assert_eq!(snaps["live"], 1);
    assert_eq!(snaps["snapshots"][0]["seq"], 2);
    assert_eq!(snaps["snapshots"][1]["live"], true);

    // The audit log names the person, not "cli" or "system".
    let audit = c.get("/api/audit").json;
    assert_eq!(audit["intact"], true);
    let by_olga: Vec<&str> = audit["rows"].as_array().unwrap().iter()
        .filter(|row| row["actor"] == "olga").map(|row| row["action"].as_str().unwrap()).collect();
    assert_eq!(by_olga, ["repo.rollback", "repo.publish", "snapshot.approve", "login"]);

    assert_eq!(c.post("/api/repos/tools/snapshots/99/approve", json!({})).status, 404);
    assert_eq!(c.post("/api/repos/nope/snapshots/1/approve", json!({})).status, 404);
    assert_eq!(c.post("/api/repos/tools/snapshots/1/explode", json!({})).status, 404);
}

#[test]
fn rejected_snapshot_stays_unpublishable_from_the_panel() {
    let r = rig();
    let c = Client::login(&r, "olga");
    assert_eq!(c.post("/api/repos/tools/snapshots/2/reject", json!({})).status, 200);
    assert_eq!(c.post("/api/repos/tools/snapshots/2/approve", json!({})).status, 409);
    assert_eq!(c.post("/api/repos/tools/snapshots/2/publish", json!({})).status, 409);
    assert_eq!(Client::new(&r).get("/tools/a.txt").text, "one");
}

#[test]
fn private_repositories_need_a_valid_token() {
    let r = rig();
    let admin = Client::login(&r, "admin");
    let pkg = Client::new(&r);
    r.st.create_repo("other", "raw").unwrap();
    let s = snapshot(&r.st, "other", &[("x", b"other")]);
    r.st.decide("other", s, true, "t").unwrap();
    r.st.publish("other", s, "t").unwrap();

    assert_eq!(pkg.get("/tools/a.txt").status, 200, "open by default");
    assert_eq!(admin.post("/api/repos/tools/settings", json!({ "private": true })).status, 200);

    let denied = pkg.get("/tools/a.txt");
    assert_eq!(denied.status, 401);
    assert!(denied.header("WWW-Authenticate").unwrap().starts_with("Basic"));
    assert_eq!(pkg.get("/tools/does-not-exist").status, 401, "no hint about what exists");
    // Being signed in to the panel is not a download permission.
    assert_eq!(admin.get("/tools/a.txt").status, 401);

    let made = admin.post("/api/tokens", json!({ "name": "ci", "repo": "tools" }));
    let secret = made.json["secret"].as_str().unwrap().to_string();
    let bearer = format!("Bearer {secret}");
    // What apt sends: Basic auth, any user name, the token as password.
    use base64::Engine;
    let basic = format!("Basic {}", base64::engine::general_purpose::STANDARD.encode(format!("apt:{secret}")));
    for auth in [&bearer, &basic] {
        let ok = pkg.send("GET", "/tools/a.txt", None, &[("Authorization", auth)]);
        assert_eq!((ok.status, ok.text.as_str()), (200, "one"));
    }
    for wrong in ["Bearer mer_wrong", "Basic bm90OnJpZ2h0", "Basic !!!", "Digest x", "nonsense"] {
        assert_eq!(pkg.send("GET", "/tools/a.txt", None, &[("Authorization", wrong)]).status, 401, "{wrong}");
    }
    // The secret is never listed again, and a token for another repo does not work here.
    let listed = admin.get("/api/tokens");
    assert!(!listed.text.contains(&secret));
    assert!(listed.json[0]["last_used_at"].is_number());
    let other = admin.post("/api/tokens", json!({ "name": "other-only", "repo": "other" })).json["secret"].as_str().unwrap().to_string();
    assert_eq!(pkg.send("GET", "/tools/a.txt", None, &[("Authorization", &format!("Bearer {other}"))]).status, 401);

    assert_eq!(admin.post("/api/tokens/ci/revoke", json!({})).status, 200);
    assert_eq!(pkg.send("GET", "/tools/a.txt", None, &[("Authorization", &bearer)]).status, 401, "revoked");
    assert_eq!(pkg.get("/other/x").status, 200, "other repositories are unaffected");
}

#[test]
fn user_management_from_the_panel() {
    let r = rig();
    let admin = Client::login(&r, "admin");
    assert_eq!(admin.post("/api/users", json!({ "name": "nina", "password": PW, "role": "viewer" })).status, 200);
    assert_eq!(admin.post("/api/users", json!({ "name": "nina", "password": PW, "role": "viewer" })).status, 409);
    assert_eq!(admin.post("/api/users", json!({ "name": "Bad Name", "password": PW, "role": "viewer" })).status, 400);
    assert_eq!(admin.post("/api/users", json!({ "name": "weak", "password": "short", "role": "viewer" })).status, 400);
    assert_eq!(admin.post("/api/users", json!({ "name": "odd", "password": PW, "role": "root" })).status, 400);

    let nina = Client::login(&r, "nina");
    assert_eq!(admin.post("/api/users/nina", json!({ "disabled": true })).status, 200);
    assert_eq!(nina.get("/api/me").status, 401, "disabling a user ends their session at once");
    assert_eq!(Client::new(&r).post("/api/login", json!({ "user": "nina", "password": PW })).status, 401);

    // The only admin cannot remove their own admin rights.
    assert_eq!(admin.post("/api/users/admin", json!({ "role": "viewer" })).status, 409);
    assert_eq!(admin.post("/api/users/admin", json!({ "disabled": true })).status, 409);
    assert_eq!(admin.get("/api/me").json["role"], "admin");

    // Changing your own password needs the current one, and signs you out.
    let olga = Client::login(&r, "olga");
    assert_eq!(olga.post("/api/me/password", json!({ "current": "wrong wrong wrong", "new": "brand new password" })).status, 403);
    assert_eq!(olga.post("/api/me/password", json!({ "current": PW, "new": "brand new password" })).status, 200);
    assert_eq!(olga.get("/api/me").status, 401);
    assert_eq!(Client::new(&r).post("/api/login", json!({ "user": "olga", "password": "brand new password" })).status, 200);
    // Password hashes never leave the server.
    assert!(!admin.get("/api/users").text.contains("argon2"));
}

#[test]
fn downloads_are_counted_per_file_repository_and_client() {
    let r = rig();
    let pkg = Client::new(&r);
    for _ in 0..3 {
        assert_eq!(pkg.get("/tools/a.txt").status, 200);
    }
    assert_eq!(pkg.get("/tools/gone.txt").status, 200);
    assert_eq!(pkg.send("HEAD", "/tools/a.txt", None, &[]).status, 200); // not a download
    assert_eq!(pkg.get("/tools/missing-1.0.tgz").status, 404); // a wish, not a download

    let c = Client::login(&r, "vic");
    let t = c.get("/api/traffic").json;
    assert_eq!(t["top"][0], json!({ "repo": "tools", "path": "a.txt", "hits": 3, "bytes": 9, "last_at": t["top"][0]["last_at"] }));
    assert_eq!(t["repos"], json!([{ "repo": "tools", "hits": 4, "bytes": 12 }]));
    assert_eq!(t["clients"][0]["address"], "127.0.0.1");
    assert_eq!(t["clients"][0]["hits"], 4);
    let w = c.get("/api/wishlist").json;
    assert_eq!((w[0]["path"].clone(), w[0]["count"].clone()), (json!("missing-1.0.tgz"), json!(1)));
    // Counters survive a restart because they are flushed to the database.
    r.app.flush_traffic();
    assert_eq!(r.st.meta().read(|t| t.traffic_by_repo()).unwrap(), vec![("tools".to_string(), 4, 12)]);
}

#[test]
fn panel_files_are_served_with_a_strict_content_policy() {
    let r = rig();
    let c = Client::new(&r);
    let root = c.get("/");
    assert_eq!((root.status, root.header("Location")), (302, Some("/ui/")));
    let page = c.get("/ui/");
    assert_eq!(page.status, 200);
    assert!(page.text.contains("<script src=\"app.js\"></script>"));
    let csp = page.header("Content-Security-Policy").unwrap();
    assert!(csp.contains("default-src 'self'") && csp.contains("frame-ancestors 'none'") && !csp.contains("unsafe"), "{csp}");
    assert_eq!(page.header("X-Frame-Options"), Some("DENY"));
    assert!(c.get("/ui/app.js").header("Content-Type").unwrap().starts_with("text/javascript"));
    assert!(c.get("/ui/app.css").header("Content-Type").unwrap().starts_with("text/css"));
    assert_eq!(c.get("/ui/../meta.db").status, 404);
    assert_eq!(c.get("/ui/nope.js").status, 404);
    // The panel never builds pages from HTML strings, and loads nothing from other hosts.
    let js = c.get("/ui/app.js").text;
    assert!(!js.contains("innerHTML") && !js.contains("document.write") && !js.contains("eval("));
    for file in [&js, &page.text, &c.get("/ui/app.css").text] {
        assert!(!file.contains("http://") && !file.contains("https://"), "external reference in a panel file");
    }
}

#[test]
fn the_panel_can_live_on_its_own_listener() {
    let r = rig();
    let pkg_only = HttpServer::start(
        r.app.clone(),
        ServerConfig { listen: "127.0.0.1:0".into(), threads: 2, access_log: false, admin: false, ..Default::default() },
    )
    .unwrap();
    let admin_only = HttpServer::start(
        r.app.clone(),
        ServerConfig { listen: "127.0.0.1:0".into(), threads: 2, access_log: false, packages: false, ..Default::default() },
    )
    .unwrap();
    let pkg = Client { base: format!("http://{}", pkg_only.addr()), cookie: None, csrf: None };
    let adm = Client { base: format!("http://{}", admin_only.addr()), cookie: None, csrf: None };

    assert_eq!(pkg.get("/tools/a.txt").status, 200);
    assert_eq!(pkg.get("/ui/").status, 404, "no panel on the package listener");
    assert_eq!(pkg.post("/api/login", json!({ "user": "admin", "password": PW })).status, 404);
    assert_eq!(pkg.get("/api/setup").status, 404);

    assert_eq!(adm.get("/ui/").status, 200);
    assert_eq!(adm.post("/api/login", json!({ "user": "admin", "password": PW })).status, 200);
    assert_eq!(adm.get("/tools/a.txt").status, 404, "no packages on the panel listener");
    pkg_only.shutdown();
    admin_only.shutdown();
}

#[test]
fn hostile_api_input_is_handled() {
    let r = rig();
    let c = Client::login(&r, "admin");
    let raw = |body: &str| {
        ureq::post(&format!("{}/api/users", r.base))
            .set("Cookie", c.cookie.as_ref().unwrap())
            .set("X-CSRF-Token", c.csrf.as_ref().unwrap())
            .send_string(body)
            .map(|r| r.status())
            .unwrap_or_else(|e| match e {
                ureq::Error::Status(code, _) => code,
                other => panic!("{other}"),
            })
    };
    assert_eq!(raw("{not json"), 400);
    assert_eq!(raw("[1,2,3]"), 400);
    assert_eq!(raw(&"x".repeat(100_000)), 413);
    assert_eq!(c.send("PUT", "/api/users", Some(json!({})), &[]).status, 405);
    assert_eq!(c.get("/api/nope").status, 404);
    assert_eq!(c.get("/api/repos/tools/snapshots/abc/changes").status, 400);
    assert_eq!(c.get("/api/repos/..%2f/snapshots").status, 404);
    // A repository cannot be named like one of the server's own paths.
    for name in ["ui", "api", "metrics", "healthz"] {
        assert!(r.st.create_repo(name, "raw").is_err(), "{name}");
    }
    assert_eq!(c.get("/api/overview").status, 200, "still healthy");
}

/// HTTPS with a real certificate, checked with curl as an independent client.
#[test]
fn tls_listener_serves_https_and_marks_the_cookie_secure() {
    use std::process::Command;
    let have = |tool: &str, arg: &str| Command::new(tool).arg(arg).output().map(|o| o.status.success()).unwrap_or(false);
    if !have("openssl", "version") || !have("curl", "--version") {
        eprintln!("skipped: needs openssl and curl");
        return;
    }
    let r = rig();
    let dir = tempfile::tempdir().unwrap();
    let (cert, key) = (dir.path().join("cert.pem"), dir.path().join("key.pem"));
    let made = Command::new("openssl")
        .args(["req", "-x509", "-newkey", "rsa:2048", "-nodes", "-days", "1", "-subj", "/CN=localhost"])
        .args(["-addext", "subjectAltName=DNS:localhost,IP:127.0.0.1"])
        .arg("-keyout").arg(&key).arg("-out").arg(&cert)
        .output()
        .unwrap();
    assert!(made.status.success(), "{}", String::from_utf8_lossy(&made.stderr));

    let server = HttpServer::start(
        r.app.clone(),
        ServerConfig {
            listen: "127.0.0.1:0".into(),
            threads: 2,
            access_log: false,
            tls: Some((std::fs::read(&cert).unwrap(), std::fs::read(&key).unwrap())),
            ..Default::default()
        },
    )
    .unwrap();
    let port = server.addr().port();
    let curl = |args: &[&str]| {
        let out = Command::new("curl").args(["--silent", "--show-error", "--max-time", "10"]).args(args).output().unwrap();
        (out.status.success(), String::from_utf8_lossy(&out.stdout).to_string() + &String::from_utf8_lossy(&out.stderr))
    };
    let ca = cert.to_str().unwrap();

    let (ok, body) = curl(&["--cacert", ca, &format!("https://localhost:{port}/tools/a.txt")]);
    assert!(ok && body == "one", "{body}");
    // Without trusting our certificate, the client refuses the connection.
    let (ok, _) = curl(&[&format!("https://localhost:{port}/healthz")]);
    assert!(!ok, "an untrusted certificate must be rejected by the client");
    // Plain HTTP on the TLS port gets nothing useful.
    let (ok, body) = curl(&[&format!("http://localhost:{port}/tools/a.txt")]);
    assert!(!ok || body != "one", "{body}");

    let login = format!("{{\"user\":\"admin\",\"password\":\"{PW}\"}}");
    let (ok, headers) = curl(&["--cacert", ca, "--include", "--data", &login, &format!("https://localhost:{port}/api/login")]);
    assert!(ok && headers.contains(" 200"), "{headers}");
    let cookie = headers.lines().find(|l| l.to_ascii_lowercase().starts_with("set-cookie:")).unwrap();
    assert!(cookie.contains("Secure") && cookie.contains("HttpOnly"), "{cookie}");
    server.shutdown();

    assert!(matches!(
        HttpServer::start(r.app.clone(), ServerConfig { listen: "127.0.0.1:0".into(), tls: Some((b"junk".to_vec(), b"junk".to_vec())), ..Default::default() }),
        Err(HttpError::Tls(_))
    ));
}
