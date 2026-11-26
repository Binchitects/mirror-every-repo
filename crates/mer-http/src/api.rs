//! JSON API behind the web panel. Every call except sign-in needs a session,
//! and every call that changes something needs the session's CSRF token.
use crate::catalog::{package_of, wish_hint};
use crate::{header, lock, percent_decode, App, Reply, Surface};
use mer_auth::{require, AuthError, Identity, Role};
use mer_core::EntryOp;
use mer_meta::MetaError;
use mer_storage::StorageError;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::io::Read;
use std::sync::atomic::Ordering;
use tiny_http::{Method, Request};

const COOKIE: &str = "mer_session";
const MAX_BODY: u64 = 64 * 1024;

/// An error that becomes an HTTP status and a JSON message.
struct ApiError(u16, String);

type ApiResult = Result<Value, ApiError>;

fn bad(msg: impl Into<String>) -> ApiError {
    ApiError(400, msg.into())
}

impl From<MetaError> for ApiError {
    fn from(e: MetaError) -> Self {
        let status = match &e {
            MetaError::NotFound(_) => 404,
            MetaError::Conflict(_) => 409,
            MetaError::Invalid(_) | MetaError::Core(_) => 400,
            MetaError::Busy(_) => 503,
            _ => 500,
        };
        if status == 500 {
            eprintln!("ERROR: api: {e}");
            return ApiError(500, "internal error".into());
        }
        ApiError(status, e.to_string())
    }
}

impl From<StorageError> for ApiError {
    fn from(e: StorageError) -> Self {
        match e {
            StorageError::Meta(m) => m.into(),
            StorageError::QuotaExceeded { .. } => ApiError(409, e.to_string()),
            other => {
                eprintln!("ERROR: api: {other}");
                ApiError(500, "internal error".into())
            }
        }
    }
}

impl From<AuthError> for ApiError {
    fn from(e: AuthError) -> Self {
        match e {
            AuthError::BadCredentials => ApiError(401, e.to_string()),
            AuthError::LockedOut(_) => ApiError(429, e.to_string()),
            AuthError::Invalid(_) => ApiError(400, e.to_string()),
            AuthError::Forbidden(_) => ApiError(403, e.to_string()),
            AuthError::Meta(m) => m.into(),
            AuthError::Hash(_) => ApiError(500, "internal error".into()),
        }
    }
}

fn session_cookie(req: &Request) -> Option<String> {
    header(req, "Cookie")?.split(';').find_map(|part| {
        let (k, v) = part.trim().split_once('=')?;
        (k == COOKIE).then(|| v.to_string())
    })
}

fn body(req: &mut Request) -> Result<Value, ApiError> {
    let mut raw = Vec::new();
    req.as_reader().take(MAX_BODY + 1).read_to_end(&mut raw).map_err(|_| bad("cannot read the request body"))?;
    if raw.len() as u64 > MAX_BODY {
        return Err(ApiError(413, "request body too large".into()));
    }
    if raw.is_empty() {
        return Ok(json!({}));
    }
    let v: Value = serde_json::from_slice(&raw).map_err(|_| bad("request body is not JSON"))?;
    if !v.is_object() {
        return Err(bad("request body must be a JSON object"));
    }
    Ok(v)
}

fn str_field<'a>(v: &'a Value, name: &str) -> Result<&'a str, ApiError> {
    v.get(name).and_then(Value::as_str).ok_or_else(|| bad(format!("missing field {name:?}")))
}

fn query_u32(query: &str, name: &str, default: u32, max: u32) -> u32 {
    query
        .split('&')
        .find_map(|kv| kv.split_once('=').filter(|(k, _)| *k == name).and_then(|(_, v)| v.parse().ok()))
        .unwrap_or(default)
        .min(max)
}

/// A text parameter of the query string, decoded.
fn query_str(query: &str, name: &str) -> Option<String> {
    let raw = query.split('&').find_map(|kv| kv.split_once('=').filter(|(k, _)| *k == name).map(|(_, v)| v))?;
    percent_decode(&raw.replace('+', " ")).map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

/// The snapshot a listing is about: the one asked for, else what is
/// published, else the newest.
fn listing_snapshot(t: &mer_meta::Tx, repo: &mer_meta::Repo, query: &str) -> mer_meta::Result<Option<u64>> {
    if let Some(seq) = query_str(query, "snapshot").and_then(|s| s.parse::<u64>().ok()) {
        return Ok(t.snapshot_get(repo.id, seq)?.map(|s| s.seq));
    }
    Ok(match t.live(repo.id)? {
        Some(l) => Some(l.seq),
        None => t.snapshot_head(repo.id)?.map(|h| h.seq),
    })
}

/// The wishlist as text, to carry to the collector: one line per wish with
/// what to add to the sync command there.
fn wishlist_export(app: &App) -> Result<Reply, ApiError> {
    let (rows, formats) = app.st.meta().read(|t| {
        let formats: BTreeMap<String, String> = t.repo_list()?.into_iter().map(|r| (r.name, r.format)).collect();
        Ok((t.miss_list(100_000)?, formats))
    })?;
    let mut out = String::from("# Wishlist: what clients asked for and the vault does not have.\n# repository\trequests\titem\tadd to the source or sync command on the collector\n");
    for m in &rows {
        // Tabs and line breaks cannot occur in stored paths, but nothing here relies on that.
        let clean = |s: &str| s.replace(['\t', '\n', '\r'], " ");
        let hint = formats.get(&m.repo).and_then(|f| wish_hint(f, &m.path)).unwrap_or_default();
        out.push_str(&format!("{}\t{}\t{}\t{}\n", clean(&m.repo), m.count, clean(&m.path), clean(&hint)));
    }
    Ok(Reply::text(200, out).with("Content-Disposition", "attachment; filename=\"wishlist.txt\"").with("Cache-Control", "no-store"))
}

pub(crate) fn handle(app: &App, req: &mut Request, path: &str, query: &str, surface: Surface) -> Reply {
    let method = req.method().clone();
    let segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    let token = session_cookie(req);

    // Sign-in and the "is this a fresh install" probe are the only calls
    // that work without a session.
    match (&method, segments.as_slice()) {
        (Method::Post, ["login"]) => return login(app, req, surface),
        (Method::Get, ["setup"]) => {
            return match app.auth.needs_setup() {
                Ok(needs) => Reply::json(200, json!({ "needs_setup": needs })),
                Err(_) => Reply::json(500, json!({ "error": "internal error" })),
            }
        }
        _ => {}
    }
    let Some(me) = token.as_deref().and_then(|t| app.auth.session(t)) else {
        return Reply::json(401, json!({ "error": "sign in first" }));
    };
    let result = match method {
        Method::Get if segments.as_slice() == ["wishlist", "export"] => {
            return wishlist_export(app).unwrap_or_else(|ApiError(status, msg)| Reply::json(status, json!({ "error": msg })));
        }
        Method::Get => get(app, &me, &segments, query),
        Method::Post => {
            // A page on another site can make the browser send our cookie, but
            // it cannot read this token, so it cannot forge a change.
            if header(req, "X-CSRF-Token") != Some(me.csrf.as_str()) {
                return Reply::json(403, json!({ "error": "missing or wrong CSRF token" }));
            }
            if segments.as_slice() == ["logout"] {
                app.auth.logout(token.as_deref().unwrap_or(""));
                return Reply::json(200, json!({})).with("Set-Cookie", cookie("", 0, surface.tls));
            }
            match body(req) {
                Ok(b) => post(app, &me, &segments, &b),
                Err(e) => Err(e),
            }
        }
        _ => return Reply::json(405, json!({ "error": "method not allowed" })).with("Allow", "GET, POST"),
    };
    match result {
        Ok(v) => Reply::json(200, v),
        Err(ApiError(status, msg)) => Reply::json(status, json!({ "error": msg })),
    }
}

fn cookie(value: &str, max_age: i64, tls: bool) -> String {
    // HttpOnly: scripts cannot read it. SameSite=Strict: other sites cannot send it.
    format!(
        "{COOKIE}={value}; Path=/; HttpOnly; SameSite=Strict; Max-Age={max_age}{}",
        if tls { "; Secure" } else { "" }
    )
}

fn login(app: &App, req: &mut Request, surface: Surface) -> Reply {
    let attempt = body(req).and_then(|b| {
        let user = str_field(&b, "user")?.to_string();
        let password = str_field(&b, "password")?.to_string();
        Ok(app.auth.login(&user, &password)?)
    });
    match attempt {
        Ok(login) => Reply::json(200, me_json(&login.identity))
            .with("Set-Cookie", cookie(&login.token, mer_auth::SESSION_TTL_MS / 1000, surface.tls)),
        Err(ApiError(status, msg)) => Reply::json(status, json!({ "error": msg })),
    }
}

fn me_json(me: &Identity) -> Value {
    json!({ "user": me.name, "role": me.role.as_str(), "csrf": me.csrf })
}

fn get(app: &App, me: &Identity, segments: &[&str], query: &str) -> ApiResult {
    let st = &app.st;
    match segments {
        ["me"] => Ok(me_json(me)),
        ["schedules"] => {
            let rows = st.meta().read(|t| t.schedule_list())?;
            Ok(json!(rows.iter().map(schedule_json).collect::<Vec<_>>()))
        }
        ["jobs", "forms"] => Ok(json!({
            "enabled": app.jobs.is_some(),
            "files": app.jobs.as_ref().is_some_and(|j| j.cfg.files.is_some()),
            "seal": app.jobs.as_ref().is_some_and(|j| j.cfg.bundle_key.is_some()),
            "forms": crate::jobs::forms_json(),
        })),
        ["jobs"] => {
            let rows = st.meta().read(|t| t.job_list(100))?;
            Ok(json!(rows.iter().map(job_json).collect::<Vec<_>>()))
        }
        ["jobs", id] => {
            let id: i64 = id.parse().map_err(|_| bad("job number"))?;
            let j = st.meta().read(|t| t.job_get(id))?.ok_or_else(|| ApiError(404, "no such job".into()))?;
            let mut v = job_json(&j);
            v["log"] = json!(j.log);
            Ok(v)
        }
        ["repos", repo, "source"] => {
            let r = st.repo(repo)?;
            let s = st.meta().read(|t| t.source_get(r.id))?;
            Ok(match s {
                Some(s) => json!({ "format": r.format, "spec": serde_json::from_str::<Value>(&s.spec).unwrap_or(Value::Null), "updated_at": s.updated_at, "updated_by": s.updated_by }),
                None => json!({ "format": r.format, "spec": null }),
            })
        }
        ["overview"] => overview(app),
        ["history"] => history(app, query),
        ["repos", repo, "snapshots"] => {
            let rows = st.meta().read(|t| {
                let r = t.repo_require(repo)?;
                Ok((t.snapshot_list(r.id)?, t.live(r.id)?))
            })?;
            let live = rows.1.map(|l| l.seq);
            Ok(json!({
                "live": live,
                "snapshots": rows.0.iter().rev().map(|s| json!({
                    "seq": s.seq, "state": s.state.as_str(), "created_at": s.created_at,
                    "sealed_at": s.sealed_at, "files": s.entry_count, "bytes": s.total_bytes,
                    "live": Some(s.seq) == live,
                })).collect::<Vec<_>>(),
            }))
        }
        ["repos", repo, "packages"] => {
            let limit = query_u32(query, "limit", 200, 2000) as usize;
            let needle = query_str(query, "q").map(|q| q.to_lowercase());
            let (seq, format, entries) = st.meta().read(|t| {
                let r = t.repo_require(repo)?;
                let seq = listing_snapshot(t, &r, query)?;
                let entries = match seq {
                    Some(s) => t.entries_at(r.id, s)?,
                    None => Vec::new(),
                };
                Ok((seq, r.format, entries))
            })?;
            // name -> (versions, files, bytes)
            let mut packages: BTreeMap<String, (Vec<String>, u64, u64)> = BTreeMap::new();
            let (mut other_files, mut total_bytes) = (0u64, 0u64);
            for e in &entries {
                total_bytes += e.size;
                match package_of(&format, &e.path) {
                    Some((name, version)) => {
                        let p = packages.entry(name).or_default();
                        if !p.0.contains(&version) {
                            p.0.push(version);
                        }
                        p.1 += 1;
                        p.2 += e.size;
                    }
                    None => other_files += 1,
                }
            }
            let total_packages = packages.len();
            let shown: Vec<Value> = packages
                .into_iter()
                .filter(|(name, _)| needle.as_ref().is_none_or(|n| name.to_lowercase().contains(n)))
                .take(limit)
                .map(|(name, (versions, files, bytes))| json!({ "name": name, "versions": versions, "files": files, "bytes": bytes }))
                .collect();
            Ok(json!({
                "snapshot": seq, "format": format, "files": entries.len(), "bytes": total_bytes,
                "total_packages": total_packages, "other_files": other_files, "packages": shown,
            }))
        }
        ["repos", repo, "files"] => {
            let limit = query_u32(query, "limit", 200, 2000) as usize;
            let needle = query_str(query, "q").map(|q| q.to_lowercase());
            let (seq, entries) = st.meta().read(|t| {
                let r = t.repo_require(repo)?;
                let seq = listing_snapshot(t, &r, query)?;
                Ok((seq, match seq {
                    Some(s) => t.entries_at(r.id, s)?,
                    None => Vec::new(),
                }))
            })?;
            let matching: Vec<&mer_core::EntryRec> = entries.iter().filter(|e| needle.as_ref().is_none_or(|n| e.path.to_lowercase().contains(n))).collect();
            Ok(json!({
                "snapshot": seq, "files": entries.len(), "matching": matching.len(),
                "items": matching.iter().take(limit).map(|e| json!({ "path": e.path, "size": e.size, "sha256": e.digest.to_hex() })).collect::<Vec<_>>(),
            }))
        }
        ["repos", repo, "snapshots", seq, "changes"] => {
            let seq: u64 = seq.parse().map_err(|_| bad("snapshot number"))?;
            let limit = query_u32(query, "limit", 500, 5000) as usize;
            let (base, ops, format) = st.meta().read(|t| {
                let r = t.repo_require(repo)?;
                let format = r.format.clone();
                t.snapshot_get(r.id, seq)?.ok_or_else(|| MetaError::NotFound(format!("snapshot {seq}")))?;
                let base = t.live(r.id)?.map_or(0, |l| l.seq);
                // What clients would see change if this snapshot went live now.
                let ops = t.diff(r.id, base, seq)?;
                let mut items = Vec::new();
                for op in ops {
                    let before = t.lookup(r.id, base, op.path())?;
                    items.push((op, before.is_some()));
                }
                Ok((base, items, format))
            })?;
            // The same changes, as packages: which versions come and which go.
            let mut by_package: BTreeMap<String, (Vec<String>, Vec<String>)> = BTreeMap::new();
            for (op, _) in &ops {
                if let Some((name, version)) = package_of(&format, op.path()) {
                    let p = by_package.entry(name).or_default();
                    let list = if matches!(op, EntryOp::Put(_)) { &mut p.0 } else { &mut p.1 };
                    if !list.contains(&version) {
                        list.push(version);
                    }
                }
            }
            let (mut added, mut changed, mut removed, mut bytes) = (0u64, 0u64, 0u64, 0u64);
            let mut items = Vec::new();
            for (op, existed) in &ops {
                let (kind, size) = match op {
                    EntryOp::Put(e) if *existed => {
                        changed += 1;
                        bytes += e.size;
                        ("change", e.size)
                    }
                    EntryOp::Put(e) => {
                        added += 1;
                        bytes += e.size;
                        ("add", e.size)
                    }
                    EntryOp::Del { .. } => {
                        removed += 1;
                        ("remove", 0)
                    }
                };
                if items.len() < limit {
                    items.push(json!({ "op": kind, "path": op.path(), "size": size }));
                }
            }
            Ok(json!({
                "base": base, "target": seq, "added": added, "changed": changed, "removed": removed,
                "bytes": bytes, "total": ops.len(), "items": items,
                "packages": by_package.iter().take(2000).map(|(name, (added, removed))| json!({ "name": name, "added": added, "removed": removed })).collect::<Vec<_>>(),
                "total_packages": by_package.len(),
            }))
        }
        ["wishlist"] => {
            let (rows, formats) = st.meta().read(|t| {
                let formats: BTreeMap<String, String> = t.repo_list()?.into_iter().map(|r| (r.name, r.format)).collect();
                Ok((t.miss_list(query_u32(query, "limit", 200, 2000))?, formats))
            })?;
            Ok(json!(rows.iter().map(|m| json!({
                "repo": m.repo, "path": m.path, "count": m.count, "last_at": m.last_at,
                "format": formats.get(&m.repo), "hint": formats.get(&m.repo).and_then(|f| wish_hint(f, &m.path)),
            })).collect::<Vec<_>>()))
        }
        ["audit"] => {
            let (rows, chain) = st.meta().read(|t| Ok((t.audit_list(query_u32(query, "limit", 200, 2000))?, t.audit_verify()?)))?;
            Ok(json!({
                "intact": chain.is_ok(),
                "entries": chain.unwrap_or(0),
                "broken_at": chain.err(),
                "rows": rows.iter().map(|r| json!({
                    "seq": r.seq, "at": r.at, "actor": r.actor, "action": r.action, "detail": r.detail,
                })).collect::<Vec<_>>(),
            }))
        }
        ["traffic"] => {
            app.flush_traffic();
            let (top, by_repo) = st.meta().read(|t| Ok((t.traffic_top(query_u32(query, "limit", 50, 500))?, t.traffic_by_repo()?)))?;
            let mut clients: Vec<_> = lock(&app.traffic.clients).iter().map(|(ip, c)| (*ip, *c)).collect();
            clients.sort_by_key(|c| std::cmp::Reverse(c.1.bytes));
            clients.truncate(50);
            Ok(json!({
                "top": top.iter().map(|r| json!({
                    "repo": r.repo, "path": r.path, "hits": r.hits, "bytes": r.bytes, "last_at": r.last_at,
                })).collect::<Vec<_>>(),
                "repos": by_repo.iter().map(|(repo, hits, bytes)| json!({ "repo": repo, "hits": hits, "bytes": bytes })).collect::<Vec<_>>(),
                "clients": clients.iter().map(|(ip, c)| json!({
                    "address": ip.to_string(), "hits": c.hits, "bytes": c.bytes, "last_at": c.last_at,
                })).collect::<Vec<_>>(),
            }))
        }
        ["users"] => {
            require(me.role, Role::Admin)?;
            Ok(json!(app.auth.users()?.iter().map(|u| json!({
                "name": u.name, "role": u.role, "disabled": u.disabled, "created_at": u.created_at,
            })).collect::<Vec<_>>()))
        }
        ["tokens"] => {
            require(me.role, Role::Admin)?;
            Ok(json!(app.auth.tokens()?.iter().map(|t| json!({
                "name": t.name, "repo": t.repo, "created_by": t.created_by, "created_at": t.created_at,
                "last_used_at": t.last_used_at, "revoked": t.revoked,
            })).collect::<Vec<_>>()))
        }
        _ => Err(ApiError(404, "no such API".into())),
    }
}

/// Time series for the charts. The window ends now and is cut into equal
/// steps; counters are summed per step, sizes show the last sample of each
/// step (and keep the previous value where there was no sample).
fn history(app: &App, query: &str) -> ApiResult {
    let range = query.split('&').find_map(|kv| kv.strip_prefix("range=")).unwrap_or("7d");
    let (step, points): (i64, usize) = match range {
        "24h" => (1, 24),
        "7d" => (6, 28),
        "30d" => (24, 30),
        "1y" => (168, 52),
        _ => return Err(bad("range must be 24h, 7d, 30d or 1y")),
    };
    app.flush_traffic();
    app.sample_sizes();
    let now_hour = mer_meta::hour_of(mer_storage::now_ms());
    // The window ends at the next step boundary, so the current hour is inside it.
    let end = (now_hour + step).div_euclid(step) * step;
    let start = end - step * points as i64;
    let slot = |hour: i64| ((hour - start) / step) as usize;
    let st = &app.st;
    let mut out = json!({ "range": range, "step_hours": step, "start_ms": start * mer_meta::HOUR_MS, "points": points });
    st.meta().read(|t| {
        for metric in ["requests", "bytes", "misses"] {
            let mut total = vec![0u64; points];
            let mut by_repo: BTreeMap<String, Vec<u64>> = BTreeMap::new();
            for p in t.history_range(metric, start, end)? {
                total[slot(p.hour)] += p.value;
                by_repo.entry(p.key).or_insert_with(|| vec![0; points])[slot(p.hour)] += p.value;
            }
            out[metric] = json!({ "total": total, "by_repo": by_repo });
        }
        let sizes = |metric: &str| -> mer_meta::Result<BTreeMap<String, Vec<Option<u64>>>> {
            let mut series: BTreeMap<String, Vec<Option<u64>>> = BTreeMap::new();
            for p in t.history_before(metric, start)? {
                series.entry(p.key).or_insert_with(|| vec![None; points])[0] = Some(p.value);
            }
            for p in t.history_range(metric, start, end)? {
                series.entry(p.key).or_insert_with(|| vec![None; points])[slot(p.hour)] = Some(p.value);
            }
            let now_slot = slot(now_hour);
            for values in series.values_mut() {
                for i in 1..=now_slot.min(points - 1) {
                    if values[i].is_none() {
                        values[i] = values[i - 1];
                    }
                }
            }
            Ok(series)
        };
        let store = sizes("store_bytes")?.remove("").unwrap_or_else(|| vec![None; points]);
        // Repositories that no longer exist are left out of the breakdown.
        let names: Vec<String> = t.repo_list()?.into_iter().map(|r| r.name).collect();
        let by_repo: BTreeMap<String, Vec<Option<u64>>> = sizes("repo_bytes")?.into_iter().filter(|(k, _)| names.contains(k)).collect();
        out["storage"] = json!({ "total": store, "by_repo": by_repo });
        Ok(())
    })?;
    Ok(out)
}

fn overview(app: &App) -> ApiResult {
    let st = &app.st;
    let now = mer_storage::now_ms();
    let v = st.meta().read(|t| {
        let (blobs, bytes) = t.blob_count()?;
        let repos = t.repo_list()?;
        let mut repo_json = Vec::new();
        for r in &repos {
            let live = t.live(r.id)?;
            let head = t.snapshot_head(r.id)?;
            let live_seq = live.as_ref().map(|l| l.seq);
                // How old what clients get is: when the published snapshot was sealed.
                let live_at = match live_seq {
                    Some(seq) => t.snapshot_get(r.id, seq)?.and_then(|s| s.sealed_at),
                    None => None,
                };
                let stale = match (live_at, r.stale_after_days) {
                    (Some(at), Some(days)) => now - at > i64::from(days) * 86_400_000,
                    _ => false,
                };
            // Sealed snapshots newer than what is live: waiting for a decision.
            let pending: Vec<u64> = t
                .snapshot_list(r.id)?
                .iter()
                .filter(|s| s.state == mer_meta::SnapState::Sealed && live_seq.is_none_or(|l| s.seq > l))
                .map(|s| s.seq)
                .collect();
            repo_json.push(json!({
                "name": r.name, "format": r.format, "private": r.private,
                "quota_bytes": r.quota_bytes, "bytes": t.repo_usage(r.id)?,
                "live": live_seq, "live_kind": live.map(|l| l.kind),
                "live_at": live_at, "stale_after_days": r.stale_after_days, "stale": stale,
                "newest": head.as_ref().map(|h| h.seq),
                "files": head.as_ref().map(|h| h.entry_count),
                "pending": pending,
            }));
        }
        let mut channels = Vec::new();
        for c in t.channel_list()? {
            let mut repo_state = Vec::new();
            for r in &repos {
                if let Some(ri) = t.repo_import_get(&c.id, r.id)? {
                    repo_state.push(json!({ "repo": r.name, "applied_seq": ri.applied_seq, "state": ri.state, "detail": ri.detail }));
                }
            }
            let bundles: Vec<Value> = t
                .bundle_import_list(&c.id)?
                .iter()
                .rev()
                .take(50)
                .map(|b| json!({ "seq": b.seq, "state": b.state, "id": b.bundle_id.to_hex() }))
                .collect();
            channels.push(json!({
                "id": c.id.to_hex(), "sealed_seq": c.head_seq, "imported_seq": c.imported_seq,
                "bundles": bundles, "repos": repo_state,
            }));
        }
        Ok(json!({
            "store": { "blobs": blobs, "bytes": bytes },
            "repos": repo_json,
            "channels": channels,
            "wishes": t.miss_list(1_000_000)?.len(),
            "audit_intact": t.audit_verify()?.is_ok(),
        }))
    })?;
    let mut v = v;
    let codes: serde_json::Map<String, Value> =
        lock(&app.metrics.by_status).iter().map(|(code, n)| (code.to_string(), json!(n))).collect();
    v["http"] = json!({
        "by_status": codes,
        "bytes_sent": app.metrics.bytes_sent.load(Ordering::Relaxed),
        "misses": app.metrics.misses.load(Ordering::Relaxed),
    });
    Ok(v)
}

fn job_json(j: &mer_meta::JobRow) -> Value {
    json!({
        "id": j.id, "kind": j.kind, "target": j.target, "state": j.state, "actor": j.actor,
        "created_at": j.created_at, "started_at": j.started_at, "finished_at": j.finished_at, "exit_code": j.exit_code,
        "args": serde_json::from_str::<Value>(&j.args).unwrap_or(Value::Null),
    })
}

/// Queue a job; audited with who asked for it.
fn enqueue(app: &App, actor: &str, kind: &str, target: &str, args: Vec<String>) -> ApiResult {
    let runner = app.jobs.as_ref().ok_or_else(no_jobs)?;
    let id = crate::schedule::submit(app, actor, kind, target, &args, false).map_err(|e| ApiError(500, e))?.expect("always queued");
    runner.notify();
    Ok(json!({ "job": id }))
}

fn schedule_json(s: &mer_meta::ScheduleRow) -> Value {
    json!({
        "id": s.id,
        "repos": serde_json::from_str::<Value>(&s.repos).unwrap_or(json!([])),
        "rule": serde_json::from_str::<Value>(&s.rule).unwrap_or(Value::Null),
        "seal": s.seal, "enabled": s.enabled, "next_at": s.next_at, "last_at": s.last_at,
        "last_jobs": serde_json::from_str::<Value>(&s.last_jobs).unwrap_or(json!([])),
        "last_note": s.last_note, "updated_at": s.updated_at, "updated_by": s.updated_by,
    })
}

/// Create (`id` None) or change a schedule from the panel's form.
fn schedule_save(app: &App, actor: &str, id: Option<i64>, b: &Value) -> ApiResult {
    app.jobs.as_ref().ok_or_else(no_jobs)?;
    let (repos, rule) = crate::schedule::validate(app, b).map_err(bad)?;
    let rule_text = rule.to_json().to_string();
    let seal = b.get("seal").and_then(Value::as_bool).unwrap_or(false);
    let enabled = b.get("enabled").and_then(Value::as_bool).unwrap_or(true);
    let now = mer_storage::now_ms();
    let spec = crate::schedule::spec(&repos, &rule_text, seal, enabled, now, rule);
    let what = format!("{rule_text} repos {repos}{}{}", if seal { " then seal" } else { "" }, if enabled { "" } else { " (off)" });
    let id = app.st.meta().write(|t| {
        let id = match id {
            None => t.schedule_create(&spec, now, actor)?,
            Some(id) if t.schedule_update(id, &spec, now, actor)? => id,
            Some(_) => return Ok(None),
        };
        t.audit(now, actor, "schedule.set", &format!("schedule {id}: {what}"))?;
        Ok(Some(id))
    })?;
    let id = id.ok_or_else(|| ApiError(404, "no such schedule".into()))?;
    let row = app.st.meta().read(|t| t.schedule_get(id))?.expect("just written");
    Ok(schedule_json(&row))
}

fn no_jobs() -> ApiError {
    ApiError(409, "this server does not run jobs (start it with mer serve --jobs)".into())
}

fn post(app: &App, me: &Identity, segments: &[&str], b: &Value) -> ApiResult {
    let st = &app.st;
    let actor = me.name.as_str();
    match segments {
        ["repos"] => {
            require(me.role, Role::Admin)?;
            app.jobs.as_ref().ok_or_else(no_jobs)?;
            let name = str_field(b, "name")?;
            let format = str_field(b, "format")?;
            if !crate::jobs::forms().iter().any(|(f, _, _)| *f == format) && format != "raw" {
                return Err(bad(format!("format {format} is not known")));
            }
            st.create_repo(name, format).map_err(|e| match e {
                StorageError::Meta(MetaError::Invalid(m)) => bad(m),
                other => other.into(),
            })?;
            st.meta().write(|t| t.audit(mer_storage::now_ms(), actor, "repo.create", &format!("{name} ({format})")))?;
            Ok(json!({}))
        }
        ["repos", repo, "source"] => {
            require(me.role, Role::Admin)?;
            let runner = app.jobs.as_ref().ok_or_else(no_jobs)?;
            let r = st.repo(repo)?;
            let spec = crate::jobs::validate_spec(&r.format, b.get("spec").unwrap_or(&Value::Null), runner.cfg.files.as_deref()).map_err(bad)?;
            let text = spec.to_string();
            let now = mer_storage::now_ms();
            st.meta().write(|t| {
                t.source_set(r.id, &text, now, actor)?;
                t.audit(now, actor, "source.set", &format!("{repo}: {text}"))
            })?;
            Ok(json!({ "spec": spec }))
        }
        ["repos", repo, "sync"] => {
            require(me.role, Role::Operator)?;
            let runner = app.jobs.as_ref().ok_or_else(no_jobs)?;
            let r = st.repo(repo)?;
            let s = st.meta().read(|t| t.source_get(r.id))?.ok_or_else(|| bad(format!("{repo} has no source yet; an admin sets it first")))?;
            let spec: Value = serde_json::from_str(&s.spec).map_err(|_| bad("the stored source is unreadable"))?;
            let args = crate::jobs::sync_args(&runner.cfg, &r.format, repo, &spec).map_err(bad)?;
            enqueue(app, actor, "sync", repo, args)
        }
        ["bundles", "seal"] => {
            require(me.role, Role::Operator)?;
            let runner = app.jobs.as_ref().ok_or_else(no_jobs)?;
            let key = runner.cfg.bundle_key.clone().ok_or_else(|| bad("this server has no bundle key (start it with --bundle-key)"))?;
            enqueue(app, actor, "seal", "bundle", vec!["export".into(), "seal".into(), format!("--key={}", key.display())])
        }
        ["bundles", "export"] => {
            require(me.role, Role::Operator)?;
            let runner = app.jobs.as_ref().ok_or_else(no_jobs)?;
            let seq = b.get("seq").and_then(Value::as_u64).filter(|s| *s > 0).ok_or_else(|| bad("seq must be a bundle number"))?;
            let out = crate::jobs::confine(runner.cfg.files.as_deref(), str_field(b, "out")?, false).map_err(bad)?;
            if out.exists() {
                return Err(bad("the export directory exists already; name a new one"));
            }
            let mut args = vec!["export".into(), "media".into(), format!("--seq={seq}"), format!("--out={}", out.display())];
            if let Some(v) = b.get("volume_size").and_then(Value::as_str).filter(|v| !v.is_empty()) {
                if !v.bytes().all(|c| c.is_ascii_alphanumeric()) {
                    return Err(bad("volume_size is a size like 4G or fat32"));
                }
                args.push(format!("--volume-size={v}"));
            }
            enqueue(app, actor, "export", &format!("bundle {seq}"), args)
        }
        ["bundles", "import"] => {
            require(me.role, Role::Operator)?;
            let runner = app.jobs.as_ref().ok_or_else(no_jobs)?;
            let dir = crate::jobs::confine(runner.cfg.files.as_deref(), str_field(b, "dir")?, true).map_err(bad)?;
            let mut args = vec!["import".into(), "media".into(), dir.display().to_string()];
            if b.get("auto_publish").and_then(Value::as_bool) == Some(true) {
                args.push("--auto-publish".into());
            }
            enqueue(app, actor, "import", &dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(), args)
        }
        ["schedules"] => {
            require(me.role, Role::Operator)?;
            schedule_save(app, actor, None, b)
        }
        ["schedules", id] => {
            require(me.role, Role::Operator)?;
            schedule_save(app, actor, Some(id.parse().map_err(|_| bad("schedule number"))?), b)
        }
        ["schedules", id, "delete"] => {
            require(me.role, Role::Operator)?;
            let id: i64 = id.parse().map_err(|_| bad("schedule number"))?;
            let now = mer_storage::now_ms();
            let gone = st.meta().write(|t| {
                let gone = t.schedule_delete(id)?;
                if gone {
                    t.audit(now, actor, "schedule.delete", &format!("schedule {id}"))?;
                }
                Ok(gone)
            })?;
            if !gone {
                return Err(ApiError(404, "no such schedule".into()));
            }
            Ok(json!({}))
        }
        ["schedules", id, "run"] => {
            // Now, by hand: the schedule keeps its next time.
            require(me.role, Role::Operator)?;
            app.jobs.as_ref().ok_or_else(no_jobs)?;
            let id: i64 = id.parse().map_err(|_| bad("schedule number"))?;
            let s = st.meta().read(|t| t.schedule_get(id))?.ok_or_else(|| ApiError(404, "no such schedule".into()))?;
            let repos: Vec<String> = serde_json::from_str(&s.repos).unwrap_or_default();
            let now = mer_storage::now_ms();
            st.meta().write(|t| t.audit(now, actor, "schedule.run", &format!("schedule {id}")))?;
            let (jobs, note) = crate::schedule::run(app, id, &repos, s.seal).map_err(bad)?;
            st.meta().write(|t| t.schedule_ran(id, now, None, &serde_json::to_string(&jobs).expect("JSON"), &note))?;
            Ok(json!({ "jobs": jobs, "note": note }))
        }
        ["jobs", id, "cancel"] => {
            require(me.role, Role::Operator)?;
            let runner = app.jobs.as_ref().ok_or_else(no_jobs)?;
            let id: i64 = id.parse().map_err(|_| bad("job number"))?;
            let now = mer_storage::now_ms();
            let queued = st.meta().write(|t| t.job_cancel_queued(id, now))?;
            if !queued && !runner.cancel_running(id) {
                return Err(ApiError(409, "the job is not queued or running".into()));
            }
            st.meta().write(|t| t.audit(now, actor, "job.cancel", &format!("job {id}")))?;
            Ok(json!({}))
        }
        ["repos", repo, "snapshots", seq, action] => {
            require(me.role, Role::Operator)?;
            let seq: u64 = seq.parse().map_err(|_| bad("snapshot number"))?;
            match *action {
                "approve" => st.decide(repo, seq, true, actor)?,
                "reject" => st.decide(repo, seq, false, actor)?,
                "publish" => {
                    st.publish(repo, seq, actor)?;
                }
                "rollback" => {
                    st.rollback(repo, seq, actor)?;
                }
                _ => return Err(ApiError(404, "no such action".into())),
            }
            Ok(json!({ "ok": true }))
        }
        ["repos", repo, "settings"] => {
            require(me.role, Role::Admin)?;
            let private = b.get("private").map(|v| v.as_bool().ok_or_else(|| bad("private must be true or false"))).transpose()?;
            let quota = match b.get("quota_bytes") {
                None => None,
                Some(Value::Null) => Some(None),
                Some(v) => Some(Some(v.as_u64().filter(|q| *q > 0).ok_or_else(|| bad("quota_bytes must be a positive number or null"))?)),
            };
            let stale = match b.get("stale_after_days") {
                None => None,
                Some(Value::Null) => Some(None),
                Some(v) => Some(Some(v.as_u64().filter(|d| (1..=3650).contains(d)).ok_or_else(|| bad("stale_after_days must be between 1 and 3650, or null"))? as u32)),
            };
            st.meta().write(|t| {
                let r = t.repo_require(repo)?;
                if let Some(p) = private {
                    t.repo_set_private(r.id, p)?;
                }
                if let Some(q) = quota {
                    t.repo_set_quota(r.id, q)?;
                }
                if let Some(days) = stale {
                    t.repo_set_stale_days(r.id, days)?;
                }
                t.audit(mer_storage::now_ms(), actor, "repo.settings", &format!("{repo} {b}"))?;
                Ok(())
            })?;
            Ok(json!({ "ok": true }))
        }
        ["users"] => {
            require(me.role, Role::Admin)?;
            let role = Role::parse(str_field(b, "role")?)?;
            app.auth.create_user(str_field(b, "name")?, str_field(b, "password")?, role, actor)?;
            Ok(json!({ "ok": true }))
        }
        ["users", name] => {
            require(me.role, Role::Admin)?;
            let role = b.get("role").and_then(Value::as_str).map(Role::parse).transpose()?;
            let disabled = b.get("disabled").and_then(Value::as_bool);
            if role.is_some() || disabled.is_some() {
                app.auth.update_user(name, role, disabled, actor)?;
            }
            if let Some(pw) = b.get("password").and_then(Value::as_str) {
                app.auth.set_password(name, pw, actor)?;
            }
            Ok(json!({ "ok": true }))
        }
        ["me", "password"] => {
            // Proving the current password stops a hijacked session from
            // taking the account over.
            app.auth.login(&me.name, str_field(b, "current")?).map_err(|e| match e {
                AuthError::BadCredentials => ApiError(403, "current password is wrong".into()),
                other => other.into(),
            })?;
            app.auth.set_password(&me.name, str_field(b, "new")?, actor)?;
            Ok(json!({ "ok": true, "signed_out": true }))
        }
        ["tokens"] => {
            require(me.role, Role::Admin)?;
            let repo = b.get("repo").and_then(Value::as_str).filter(|r| !r.is_empty());
            let secret = app.auth.create_token(str_field(b, "name")?, repo, actor)?;
            // Shown once. Only its hash is stored.
            Ok(json!({ "secret": secret }))
        }
        ["tokens", name, "revoke"] => {
            require(me.role, Role::Admin)?;
            app.auth.revoke_token(name, actor)?;
            Ok(json!({ "ok": true }))
        }
        ["wishlist", "dismiss"] => {
            require(me.role, Role::Operator)?;
            let (repo, path) = (str_field(b, "repo")?, str_field(b, "path")?);
            let existed = st.meta().write(|t| {
                let existed = t.miss_delete(repo, path)?;
                if existed {
                    t.audit(mer_storage::now_ms(), actor, "wishlist.dismiss", &format!("{repo} {path}"))?;
                }
                Ok(existed)
            })?;
            if !existed {
                return Err(ApiError(404, "no such wish".into()));
            }
            Ok(json!({ "ok": true }))
        }
        ["wishlist", "clear"] => {
            require(me.role, Role::Operator)?;
            let repo = b.get("repo").and_then(Value::as_str).filter(|r| !r.is_empty());
            let removed = st.meta().write(|t| {
                let removed = t.miss_clear_all(repo)?;
                t.audit(mer_storage::now_ms(), actor, "wishlist.clear", &format!("{} ({removed} wishes)", repo.unwrap_or("every repository")))?;
                Ok(removed)
            })?;
            Ok(json!({ "removed": removed }))
        }
        ["maintenance", "gc"] => {
            require(me.role, Role::Operator)?;
            let hours = b.get("grace_hours").and_then(Value::as_u64).unwrap_or(24).min(24 * 365);
            let r = st.gc(hours as i64 * 3_600_000)?;
            Ok(json!({ "unreferenced": r.orphaned, "deleted": r.deleted, "bytes_freed": r.bytes_freed }))
        }
        _ => Err(ApiError(404, "no such API".into())),
    }
}
