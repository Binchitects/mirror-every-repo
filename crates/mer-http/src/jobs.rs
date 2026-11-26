//! Jobs started from the web panel: syncs, seals, exports and imports.
//!
//! A job runs the `mer` program itself as a child process, with a command
//! line built here from validated fields, so the panel does exactly what the
//! command line does. No shell is involved; every value is passed as
//! `--flag=value`, so no value can be read as another option. Paths in
//! requests name entries under one directory chosen by the operator of the
//! machine (`--files`), never arbitrary places.
use mer_storage::{now_ms, Storage};
use serde_json::{json, Map, Value};
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

/// Where jobs come from on this machine. Set by `mer serve --jobs`.
#[derive(Clone, Debug)]
pub struct JobsConfig {
    /// The `mer` program to run.
    pub exe: PathBuf,
    pub data: PathBuf,
    /// The one directory request paths may name (lockfiles, local Maven
    /// repositories, keyrings, export targets, import sources).
    pub files: Option<PathBuf>,
    /// Bundle signing key, for sealing.
    pub bundle_key: Option<PathBuf>,
    /// GnuPG home of the apt site key.
    pub apt_gnupghome: Option<PathBuf>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Text,
    Url,
    List,
    UrlList,
    Bool,
    Path,
    PathList,
    Choice(&'static [&'static str]),
}

#[derive(Clone, Copy, Debug)]
pub struct Field {
    pub name: &'static str,
    pub kind: Kind,
    pub required: bool,
    pub help: &'static str,
}

const fn f(name: &'static str, kind: Kind, required: bool, help: &'static str) -> Field {
    Field { name, kind, required, help }
}

const HOSTS: Field = f("allow_host", Kind::List, false, "Additional hosts the collector may contact");

/// The sync form of each format: its command and fields, as `mer <command> <repo> --<field>=...`.
pub fn forms() -> Vec<(&'static str, &'static [&'static str], Vec<Field>)> {
    use Kind::*;
    vec![
        ("raw", &["repo", "sync-dir"][..], vec![f("dir", Path, true, "Directory to copy, under the files directory")]),
        ("apt", &["apt", "sync"], vec![
            f("upstream", Url, true, "Archive URL, for example https://deb.debian.org/debian"),
            f("suite", Text, true, "Suite or codename, for example bookworm"),
            f("component", List, false, "Components (default main)"),
            f("arch", List, false, "Architectures (default amd64)"),
            f("package", List, true, "Packages to mirror; dependencies are added"),
            f("recommends", Bool, false, "Also follow Recommends"),
            f("keyring", Path, false, "Upstream archive keyring (default: the Debian archive keyring)"),
            f("ignore_valid_until", Bool, false, "Accept an expired Release"),
            HOSTS,
        ]),
        ("gem", &["gem", "sync"], vec![
            f("upstream", Url, false, "Gem source (default https://rubygems.org)"),
            f("gem", List, false, "Gems to mirror: rails, rails:~> 8.0, rack@3.2.9; what they depend on is added"),
            f("lockfile", PathList, false, "Gemfile.lock files whose exact gems are mirrored"),
            f("ruby", Text, false, "The Ruby inside the air gap, for example 3.4.1"),
            f("platform", List, false, "Platforms besides pure Ruby (default x86_64-linux and x86_64-linux-gnu)"),
            f("pre", Bool, false, "Let gems named without a requirement resolve to a prerelease"),
            HOSTS,
        ]),
        ("apk", &["apk", "sync"], vec![
            f("upstream", UrlList, true, "Repositories as in /etc/apk/repositories (for example .../alpine/v3.24/main and .../community)"),
            f("package", List, false, "Packages to mirror; what they need is added"),
            f("packages_from", PathList, false, "Lists of packages, as `apk info` on a client image prints them"),
            f("arch", List, false, "Architectures (default x86_64)"),
            f("keys", Path, false, "Directory of trusted keys, as /etc/apk/keys"),
            HOSTS,
        ]),
        ("rpm", &["rpm", "sync"], vec![
            f("upstream", UrlList, true, "Repository base URLs (directories holding repodata/)"),
            f("package", List, true, "Packages to mirror; what they require is added"),
            f("arch", List, false, "Architectures (default x86_64 and noarch)"),
            f("keyring", Path, false, "Distribution key that repomd.xml.asc must verify against"),
            HOSTS,
        ]),
        ("pypi", &["pypi", "sync"], vec![
            f("index_url", Url, false, "Index URL (default https://pypi.org/simple)"),
            f("requirement", List, true, "Requirements, for example requests or django>=4.2,<5"),
            f("python", List, true, "CPython versions of your machines, for example 3.12"),
            f("arch", Text, false, "Architecture (default x86_64)"),
            f("libc", List, false, "manylinux and/or musllinux (default manylinux)"),
            f("sdists", Choice(&["if-no-wheel", "never", "always"]), false, "When to keep source archives"),
            f("pre", Bool, false, "Allow pre-releases"),
            HOSTS,
        ]),
        ("npm", &["npm", "sync"], vec![
            f("registry", Url, false, "Registry (default https://registry.npmjs.org)"),
            f("package", List, true, "Packages, for example express or lodash@^4.17"),
            f("os", Text, false, "Operating system (default linux)"),
            f("cpu", Text, false, "CPU (default x64)"),
            f("libc", Text, false, "libc (default glibc)"),
            HOSTS,
        ]),
        ("nuget", &["nuget", "sync"], vec![
            f("feed", Url, false, "Service index (default nuget.org)"),
            f("package", List, true, "Packages, for example Serilog or Newtonsoft.Json@13.0.3"),
            f("framework", List, true, "Target frameworks, for example net8.0"),
            f("pre", Bool, false, "Allow pre-releases"),
            HOSTS,
        ]),
        ("maven", &["maven", "sync"], vec![
            f("upstream", Url, false, "Repository (default Maven Central)"),
            f("artifact", List, false, "Artifacts, group:artifact:version"),
            f("from_m2", PathList, false, "Local repositories filled by a build, under the files directory"),
            HOSTS,
        ]),
        ("cargo", &["cargo", "sync"], vec![
            f("index", Url, false, "Sparse index (default crates.io)"),
            f("crate", List, false, "Crates, for example serde@1:derive"),
            f("lockfile", PathList, false, "Cargo.lock files, under the files directory"),
            f("rust_version", Text, false, "Rust toolchain of your machines, for example 1.80"),
            HOSTS,
        ]),
        ("go", &["go", "sync"], vec![
            f("proxy", Url, false, "Module proxy (default proxy.golang.org)"),
            f("sumdb", Url, false, "Checksum database (default sum.golang.org)"),
            f("no_sumdb", Bool, false, "Do not consult the checksum database"),
            f("module", List, false, "Modules, for example rsc.io/quote@v1.5.2"),
            f("gomod", PathList, false, "go.mod files, under the files directory"),
            f("gosum", PathList, false, "go.sum files, under the files directory"),
            HOSTS,
        ]),
        ("helm", &["helm", "sync"], vec![
            f("upstream", Url, true, "Chart repository URL"),
            f("chart", List, true, "Charts, for example cert-manager or cert-manager@v1.16.1"),
            HOSTS,
        ]),
        ("oci", &["oci", "sync"], vec![
            f("registry", Url, false, "Registry (default Docker Hub)"),
            f("image", List, true, "Images, for example library/alpine:3.20"),
            f("platform", List, false, "Platforms (default linux/amd64)"),
            HOSTS,
        ]),
    ]
}

pub fn forms_json() -> Value {
    let mut out = Map::new();
    for (format, _, fields) in forms() {
        let fs: Vec<Value> = fields
            .iter()
            .map(|f| {
                let (kind, choices) = match f.kind {
                    Kind::Text => ("text", None),
                    Kind::Url => ("url", None),
                    Kind::List => ("list", None),
                    Kind::UrlList => ("url-list", None),
                    Kind::Bool => ("bool", None),
                    Kind::Path => ("path", None),
                    Kind::PathList => ("path-list", None),
                    Kind::Choice(c) => ("choice", Some(c)),
                };
                json!({ "name": f.name, "kind": kind, "required": f.required, "help": f.help, "choices": choices })
            })
            .collect();
        out.insert(format.into(), Value::Array(fs));
    }
    Value::Object(out)
}

fn text_ok(s: &str, max: usize) -> Result<(), String> {
    if s.is_empty() || s.len() > max {
        return Err(format!("a value must be 1 to {max} characters"));
    }
    if s.chars().any(|c| c.is_control()) {
        return Err("values cannot contain control characters".into());
    }
    Ok(())
}

fn url_ok(s: &str) -> Result<(), String> {
    text_ok(s, 2048)?;
    if !(s.starts_with("https://") || s.starts_with("http://")) || s.contains(char::is_whitespace) {
        return Err(format!("{s:?} is not an http(s) URL"));
    }
    Ok(())
}

/// A relative path under `root`: no absolute paths, no `..`, and it must stay
/// inside `root` after symbolic links are followed.
pub fn confine(root: Option<&Path>, rel: &str, must_exist: bool) -> Result<PathBuf, String> {
    let root = root.ok_or("paths are not accepted: this server was started without --files")?;
    text_ok(rel, 1024)?;
    let p = Path::new(rel);
    if p.components().any(|c| !matches!(c, Component::Normal(_))) {
        return Err(format!("{rel:?} must be a plain relative path under the files directory"));
    }
    let root = root.canonicalize().map_err(|e| format!("files directory: {e}"))?;
    let full = root.join(p);
    // A new path is checked through its nearest existing ancestor, so a
    // symbolic link on the way cannot lead outside.
    let mut check = full.clone();
    if !must_exist {
        while !check.exists() && check != root {
            check = check.parent().unwrap_or(&root).to_path_buf();
        }
    }
    let real = check.canonicalize().map_err(|_| format!("{rel:?} does not exist under the files directory"))?;
    if !real.starts_with(&root) {
        return Err(format!("{rel:?} leads outside the files directory"));
    }
    Ok(if must_exist { real } else { full })
}

/// Check a source against its format's form; returns the normalised spec.
pub fn validate_spec(format: &str, spec: &Value, files: Option<&Path>) -> Result<Value, String> {
    let (_, _, fields) = forms().into_iter().find(|(f, _, _)| *f == format).ok_or_else(|| format!("format {format} cannot be synced from the panel"))?;
    let obj = spec.as_object().ok_or("the source must be an object")?;
    for k in obj.keys() {
        if !fields.iter().any(|f| f.name == k) {
            return Err(format!("unknown field {k:?} for {format}"));
        }
    }
    let mut out = Map::new();
    for field in &fields {
        let v = obj.get(field.name).filter(|v| !v.is_null());
        let empty = match v {
            None => true,
            Some(Value::String(s)) => s.is_empty(),
            Some(Value::Array(a)) => a.is_empty(),
            Some(Value::Bool(b)) => !b,
            _ => false,
        };
        if empty {
            if field.required {
                return Err(format!("{} is required", field.name));
            }
            continue;
        }
        let v = v.expect("not empty");
        let err = |e: String| format!("{}: {e}", field.name);
        let list = |v: &Value| -> Result<Vec<String>, String> {
            let a = v.as_array().ok_or("must be a list")?;
            if a.len() > 500 {
                return Err("too many entries".into());
            }
            a.iter().map(|x| x.as_str().map(|s| s.trim().to_string()).ok_or_else(|| "entries must be text".to_string())).collect()
        };
        let norm = match field.kind {
            Kind::Text => {
                let s = v.as_str().ok_or("must be text").map_err(|e| err(e.into()))?.trim();
                text_ok(s, 2048).map_err(err)?;
                json!(s)
            }
            Kind::Url => {
                let s = v.as_str().ok_or("must be text").map_err(|e| err(e.into()))?.trim();
                url_ok(s).map_err(err)?;
                json!(s)
            }
            Kind::List | Kind::UrlList => {
                let items = list(v).map_err(err)?;
                for i in &items {
                    if field.kind == Kind::UrlList { url_ok(i) } else { text_ok(i, 2048) }.map_err(err)?;
                }
                json!(items)
            }
            Kind::Bool => json!(v.as_bool().ok_or("must be true or false").map_err(|e| err(e.into()))?),
            Kind::Path => {
                let s = v.as_str().ok_or("must be text").map_err(|e| err(e.into()))?.trim();
                confine(files, s, true).map_err(err)?;
                json!(s)
            }
            Kind::PathList => {
                let items = list(v).map_err(err)?;
                for i in &items {
                    confine(files, i, true).map_err(err)?;
                }
                json!(items)
            }
            Kind::Choice(c) => {
                let s = v.as_str().unwrap_or("");
                if !c.contains(&s) {
                    return Err(err(format!("must be one of {}", c.join(", "))));
                }
                json!(s)
            }
        };
        out.insert(field.name.into(), norm);
    }
    Ok(Value::Object(out))
}

/// The command line of a sync job (after `mer --data=...`).
pub fn sync_args(cfg: &JobsConfig, format: &str, repo: &str, spec: &Value) -> Result<Vec<String>, String> {
    let spec = validate_spec(format, spec, cfg.files.as_deref())?;
    let (_, command, fields) = forms().into_iter().find(|(f, _, _)| *f == format).expect("validated");
    let mut args: Vec<String> = command.iter().map(|s| s.to_string()).collect();
    args.push(repo.to_string());
    for field in &fields {
        let Some(v) = spec.get(field.name) else { continue };
        let flag = format!("--{}", field.name.replace('_', "-"));
        match field.kind {
            Kind::Bool => args.push(flag),
            Kind::Path if format == "raw" => {
                args.push(confine(cfg.files.as_deref(), v.as_str().unwrap_or(""), true)?.display().to_string());
            }
            Kind::Path => args.push(format!("{flag}={}", confine(cfg.files.as_deref(), v.as_str().unwrap_or(""), true)?.display())),
            Kind::PathList => {
                for p in v.as_array().into_iter().flatten().filter_map(Value::as_str) {
                    args.push(format!("{flag}={}", confine(cfg.files.as_deref(), p, true)?.display()));
                }
            }
            Kind::List | Kind::UrlList => {
                for i in v.as_array().into_iter().flatten().filter_map(Value::as_str) {
                    args.push(format!("{flag}={i}"));
                }
            }
            _ => args.push(format!("{flag}={}", v.as_str().unwrap_or(""))),
        }
    }
    if format == "apt" {
        if let Some(g) = &cfg.apt_gnupghome {
            args.push(format!("--gnupghome={}", g.display()));
        }
    }
    Ok(args)
}

/// How many finished jobs are kept, with their logs.
pub const KEEP_JOBS: u32 = 500;

/// The queue's runner: one job at a time, as a child process.
pub struct Jobs {
    pub cfg: JobsConfig,
    wake: (Mutex<bool>, Condvar),
    /// The job being run and whether it was asked to stop.
    current: Mutex<Option<(i64, bool)>>,
}

impl Jobs {
    pub fn new(cfg: JobsConfig) -> Jobs {
        Jobs { cfg, wake: (Mutex::new(false), Condvar::new()), current: Mutex::new(None) }
    }

    pub fn notify(&self) {
        *self.wake.0.lock().unwrap_or_else(|e| e.into_inner()) = true;
        self.wake.1.notify_all();
    }

    /// Ask the running job to stop. Returns whether `id` is the one running.
    pub fn cancel_running(&self, id: i64) -> bool {
        let mut cur = self.current.lock().unwrap_or_else(|e| e.into_inner());
        match cur.as_mut() {
            Some((running, stop)) if *running == id => {
                *stop = true;
                true
            }
            _ => false,
        }
    }

    fn stop_requested(&self, id: i64) -> bool {
        matches!(*self.current.lock().unwrap_or_else(|e| e.into_inner()), Some((r, true)) if r == id)
    }

    /// Run queued jobs until the process ends.
    pub fn run(self: Arc<Self>, st: Arc<Storage>) {
        if let Err(e) = st.meta().write(|t| t.jobs_interrupt_running(now_ms())) {
            eprintln!("ERROR: jobs: {e}");
        }
        loop {
            let next = match st.meta().write(|t| t.job_take(now_ms())) {
                Ok(j) => j,
                Err(e) => {
                    eprintln!("ERROR: jobs: {e}");
                    None
                }
            };
            let Some(job) = next else {
                let (lock, cv) = &self.wake;
                let mut woken = lock.lock().unwrap_or_else(|e| e.into_inner());
                if !*woken {
                    woken = cv.wait_timeout(woken, Duration::from_secs(5)).unwrap_or_else(|e| e.into_inner()).0;
                }
                *woken = false;
                continue;
            };
            *self.current.lock().unwrap_or_else(|e| e.into_inner()) = Some((job.id, false));
            let (state, code, log) = self.execute(&st, job.id, &job.args);
            *self.current.lock().unwrap_or_else(|e| e.into_inner()) = None;
            // The newest jobs are kept, with their logs; schedules make many.
            if let Err(e) = st.meta().write(|t| t.job_finish(job.id, state, code, &log, now_ms()).and_then(|_| t.jobs_prune(KEEP_JOBS).map(|_| ()))) {
                eprintln!("ERROR: jobs: job {}: {e}", job.id);
            }
        }
    }

    fn execute(&self, st: &Storage, id: i64, args_json: &str) -> (&'static str, Option<i64>, String) {
        let args: Vec<String> = match serde_json::from_str(args_json) {
            Ok(a) => a,
            Err(_) => return ("failed", None, "the job's command line is unreadable\n".into()),
        };
        let mut cmd = Command::new(&self.cfg.exe);
        cmd.arg(format!("--data={}", self.cfg.data.display())).args(&args).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
        // Its own process group, so cancelling stops what it started too (gpg, for example).
        #[cfg(unix)]
        std::os::unix::process::CommandExt::process_group(&mut cmd, 0);
        let log = Arc::new(Mutex::new(format!("$ mer {}\n", args.join(" "))));
        let mut child: Child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => return ("failed", None, format!("cannot start {}: {e}\n", self.cfg.exe.display())),
        };
        let mut readers = Vec::new();
        for pipe in [child.stdout.take().map(|p| Box::new(p) as Box<dyn Read + Send>), child.stderr.take().map(|p| Box::new(p) as Box<dyn Read + Send>)].into_iter().flatten() {
            let log = log.clone();
            readers.push(std::thread::spawn(move || {
                let mut pipe = pipe;
                let mut buf = [0u8; 8192];
                while let Ok(n) = pipe.read(&mut buf) {
                    if n == 0 {
                        break;
                    }
                    let mut l = log.lock().unwrap_or_else(|e| e.into_inner());
                    l.push_str(&String::from_utf8_lossy(&buf[..n]));
                    if l.len() > 2 * mer_meta::MAX_JOB_LOG {
                        let keep = mer_meta::clip_log(&l).to_string();
                        *l = keep;
                    }
                }
            }));
        }
        let mut last_flush = Instant::now();
        let mut cancelled = false;
        let status = loop {
            match child.try_wait() {
                Ok(Some(s)) => break Some(s),
                Ok(None) => {}
                Err(_) => break None,
            }
            if !cancelled && self.stop_requested(id) {
                cancelled = true;
                kill_group(&mut child);
            }
            if last_flush.elapsed() > Duration::from_secs(2) {
                let snapshot = log.lock().unwrap_or_else(|e| e.into_inner()).clone();
                let _ = st.meta().write(|t| t.job_log(id, &snapshot));
                last_flush = Instant::now();
            }
            std::thread::sleep(Duration::from_millis(200));
        };
        for r in readers {
            let _ = r.join();
        }
        let log = log.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let code = status.and_then(|s| s.code()).map(i64::from);
        let state = if cancelled { "cancelled" } else if code == Some(0) { "succeeded" } else { "failed" };
        (state, code, log)
    }
}

fn kill_group(child: &mut Child) {
    #[cfg(unix)]
    if let Ok(pid) = i32::try_from(child.id()) {
        // SAFETY: kill(2) on our own child's process group.
        unsafe {
            libc::kill(-pid, libc::SIGKILL);
        }
    }
    let _ = child.kill();
}

#[cfg(test)]
mod unit {
    use super::*;

    #[test]
    fn sources_are_checked_against_their_form_and_paths_stay_inside() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("m2/org")).unwrap();
        std::fs::write(root.path().join("Cargo.lock"), "x").unwrap();
        let files = Some(root.path());
        let ok = validate_spec("pypi", &json!({ "requirement": ["requests", " flask "], "python": ["3.12"], "pre": false, "sdists": "never" }), files).unwrap();
        assert_eq!(ok, json!({ "requirement": ["requests", "flask"], "python": ["3.12"], "sdists": "never" }));
        for (format, spec, why) in [
            ("pypi", json!({ "python": ["3.12"] }), "required"),
            ("pypi", json!({ "requirement": ["x"], "python": ["3.12"], "evil": "1" }), "unknown field"),
            ("pypi", json!({ "requirement": ["x\ny"], "python": ["3.12"] }), "control"),
            ("pypi", json!({ "requirement": ["x"], "python": ["3.12"], "index_url": "file:///etc" }), "http(s)"),
            ("pypi", json!({ "requirement": ["x"], "python": ["3.12"], "sdists": "sometimes" }), "one of"),
            ("cargo", json!({ "lockfile": ["../etc/passwd"] }), "plain relative path"),
            ("cargo", json!({ "lockfile": ["/etc/passwd"] }), "plain relative path"),
            ("cargo", json!({ "lockfile": ["nothere.lock"] }), "does not exist"),
            ("nosuch", json!({}), "cannot be synced"),
        ] {
            let e = validate_spec(format, &spec, files).unwrap_err();
            assert!(e.contains(why), "{format} {spec}: {e}");
        }
        assert!(validate_spec("cargo", &json!({ "lockfile": ["Cargo.lock"] }), None).unwrap_err().contains("--files"));
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink("/etc", root.path().join("out")).unwrap();
            assert!(confine(files, "out/passwd", true).unwrap_err().contains("outside"));
            assert!(confine(files, "out/new/dir", false).unwrap_err().contains("outside"), "a new path behind a link");
        }
        assert!(confine(files, "usb/new/bundle-1", false).unwrap().ends_with("usb/new/bundle-1"), "new nested directories are fine");

        let cfg = JobsConfig { exe: "mer".into(), data: "/d".into(), files: Some(root.path().into()), bundle_key: None, apt_gnupghome: Some("/k/gnupg".into()) };
        let args = sync_args(&cfg, "cargo", "rust", &json!({ "crate": ["serde@1:derive", "-x"], "lockfile": ["Cargo.lock"], "rust_version": "1.80" })).unwrap();
        let lock = root.path().canonicalize().unwrap().join("Cargo.lock");
        assert_eq!(args, ["cargo", "sync", "rust", "--crate=serde@1:derive", "--crate=-x", &format!("--lockfile={}", lock.display()), "--rust-version=1.80"], "values never become options");
        let args = sync_args(&cfg, "apt", "deb", &json!({ "upstream": "https://deb.debian.org/debian", "suite": "bookworm", "package": ["hello"], "recommends": true })).unwrap();
        assert_eq!(args, ["apt", "sync", "deb", "--upstream=https://deb.debian.org/debian", "--suite=bookworm", "--package=hello", "--recommends", "--gnupghome=/k/gnupg"]);
        assert_eq!(forms_json()["pypi"][0]["name"], "index_url");
    }
}
