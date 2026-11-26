//! Schedules: at set times, queue a sync job for each chosen repository and,
//! when asked, a seal after them. The jobs are the ones the panel's buttons
//! queue, run by the same runner one at a time, so a schedule does nothing a
//! person could not do by hand.
//!
//! Times are UTC. A schedule that came due while the server was down runs once
//! when it is back, then keeps to its rule from then on.
use crate::App;
use mer_meta::ScheduleSpec;
use mer_storage::now_ms;
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;

const HOUR: i64 = 3_600_000;
const DAY: i64 = 24 * HOUR;
pub const DAYS: [&str; 7] = ["mon", "tue", "wed", "thu", "fri", "sat", "sun"];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rule {
    /// At a minute of the day, on the days set in `days` (bit 0 is Monday).
    Daily { minute: u32, days: u8 },
    /// Every so many hours, counted from the last run.
    Every { hours: u32 },
}

impl Rule {
    pub fn parse(v: &Value) -> Result<Rule, String> {
        match v["kind"].as_str() {
            Some("daily") => {
                let at = v["at"].as_str().ok_or("at is a time like 02:30")?;
                let (h, m) = at.split_once(':').ok_or("at is a time like 02:30")?;
                let digits = |s: &str, n: std::ops::RangeInclusive<usize>| n.contains(&s.len()) && s.bytes().all(|b| b.is_ascii_digit());
                if !digits(h, 1..=2) || !digits(m, 2..=2) {
                    return Err("at is a time like 02:30".into());
                }
                let (h, m): (u32, u32) = (h.parse().expect("digits"), m.parse().expect("digits"));
                if h > 23 || m > 59 {
                    return Err("at is a time like 02:30".into());
                }
                let mut days = 0u8;
                match &v["days"] {
                    Value::Null => days = 0x7f,
                    Value::Array(a) => {
                        for d in a {
                            let i = d.as_str().and_then(|d| DAYS.iter().position(|x| *x == d)).ok_or("days are mon, tue, wed, thu, fri, sat, sun")?;
                            days |= 1 << i;
                        }
                    }
                    _ => return Err("days is a list".into()),
                }
                if days == 0 {
                    return Err("choose at least one day".into());
                }
                Ok(Rule::Daily { minute: h * 60 + m, days })
            }
            Some("every") => match v["hours"].as_u64() {
                Some(h @ 1..=168) => Ok(Rule::Every { hours: h as u32 }),
                _ => Err("hours is a whole number from 1 to 168".into()),
            },
            _ => Err("kind is daily or every".into()),
        }
    }

    pub fn to_json(self) -> Value {
        match self {
            Rule::Daily { minute, days } => json!({
                "kind": "daily",
                "at": format!("{:02}:{:02}", minute / 60, minute % 60),
                "days": DAYS.iter().enumerate().filter(|(i, _)| days & (1 << i) != 0).map(|(_, d)| *d).collect::<Vec<_>>(),
            }),
            Rule::Every { hours } => json!({ "kind": "every", "hours": hours }),
        }
    }

    /// The first time after `now` (ms since the epoch, UTC) that the rule names.
    pub fn next_after(self, now: i64) -> i64 {
        match self {
            Rule::Every { hours } => now + i64::from(hours) * HOUR,
            Rule::Daily { minute, days } => {
                let today = now.div_euclid(DAY);
                (0..=7)
                    .map(|d| (today + d) * DAY + i64::from(minute) * 60_000)
                    .find(|&t| {
                        // 1970-01-01 was a Thursday: day 0 is weekday 3 counting from Monday.
                        let weekday = (t.div_euclid(DAY) + 3).rem_euclid(7);
                        t > now && days & (1 << weekday) != 0
                    })
                    .expect("some day of the next week is chosen")
            }
        }
    }
}

/// Check what a schedule is to do. Returns (repos JSON, rule JSON).
pub fn validate(app: &App, b: &Value) -> Result<(String, Rule), String> {
    let rule = Rule::parse(&b["rule"])?;
    let repos: Vec<String> = match &b["repos"] {
        Value::Null => Vec::new(),
        Value::Array(a) => a.iter().map(|r| r.as_str().map(str::to_string).ok_or("repos are names")).collect::<Result<_, _>>()?,
        _ => return Err("repos is a list of names".into()),
    };
    if repos.len() > 500 {
        return Err("too many repositories".into());
    }
    for r in &repos {
        if !matches!(app.st.meta().read(|t| t.repo_get(r)), Ok(Some(_))) {
            return Err(format!("there is no repository {r:?}"));
        }
    }
    let mut repos = repos;
    repos.sort();
    repos.dedup();
    Ok((serde_json::to_string(&repos).expect("JSON"), rule))
}

pub fn spec<'a>(repos: &'a str, rule: &'a str, seal: bool, enabled: bool, now: i64, parsed: Rule) -> ScheduleSpec<'a> {
    ScheduleSpec { repos, rule, seal, enabled, next_at: enabled.then(|| parsed.next_after(now)) }
}

/// Queue what schedule `id` does. Returns the jobs queued and a note on what
/// was left out.
pub fn run(app: &App, id: i64, repos: &[String], seal: bool) -> Result<(Vec<i64>, String), String> {
    let runner = app.jobs.as_ref().ok_or("this server does not run jobs")?;
    let actor = format!("schedule {id}");
    let st = &app.st;
    let sources = st.meta().read(|t| t.sources_list()).map_err(|e| e.to_string())?;
    let names: Vec<String> = if repos.is_empty() { sources.iter().map(|s| s.repo.clone()).collect() } else { repos.to_vec() };
    let (mut jobs, mut skipped) = (Vec::new(), Vec::new());
    for name in &names {
        let Some(source) = sources.iter().find(|s| &s.repo == name) else {
            skipped.push(format!("{name} has no source"));
            continue;
        };
        let args = st
            .meta()
            .read(|t| t.repo_get(name))
            .ok()
            .flatten()
            .ok_or_else(|| format!("{name} no longer exists"))
            .and_then(|r| {
                let spec: Value = serde_json::from_str(&source.spec).map_err(|_| format!("the source of {name} is unreadable"))?;
                crate::jobs::sync_args(&runner.cfg, &r.format, name, &spec).map_err(|e| format!("{name}: {e}"))
            });
        match args {
            Err(e) => skipped.push(e),
            Ok(args) => match submit(app, &actor, "sync", name, &args, true) {
                Ok(Some(job)) => jobs.push(job),
                Ok(None) => skipped.push(format!("{name} is already queued or syncing")),
                Err(e) => skipped.push(format!("{name}: {e}")),
            },
        }
    }
    if seal {
        match &runner.cfg.bundle_key {
            None => skipped.push("no seal: this server has no bundle key".into()),
            Some(key) => {
                let args = vec!["export".to_string(), "seal".to_string(), format!("--key={}", key.display())];
                // After the syncs in the queue, so the bundle carries what they
                // bring; not when a seal is already the last job waiting.
                let behind_seal = st.meta().read(|t| t.job_last_queued_is("seal")).unwrap_or(false);
                match if behind_seal { Ok(None) } else { submit(app, &actor, "seal", "bundle", &args, false) } {
                    Ok(Some(job)) => jobs.push(job),
                    Ok(None) => {}
                    Err(e) => skipped.push(format!("seal: {e}")),
                }
            }
        }
    }
    runner.notify();
    Ok((jobs, skipped.join("; ")))
}

/// Queue a job and audit it. With `unless_active`, nothing is queued while the
/// same job is still waiting or running.
pub(crate) fn submit(app: &App, actor: &str, kind: &str, target: &str, args: &[String], unless_active: bool) -> Result<Option<i64>, String> {
    let args_json = serde_json::to_string(args).expect("JSON");
    let now = now_ms();
    app.st
        .meta()
        .write(|t| {
            if unless_active && t.job_active(kind, target)? {
                return Ok(None);
            }
            let id = t.job_create(kind, target, &args_json, actor, now)?;
            t.audit(now, actor, &format!("job.{kind}"), &format!("job {id}: {target}"))?;
            Ok(Some(id))
        })
        .map_err(|e| e.to_string())
}

/// Queue the work of every schedule that has come due, and set its next time.
pub fn tick(app: &App, now: i64) {
    let due = match app.st.meta().read(|t| t.schedules_due(now)) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("ERROR: schedules: {e}");
            return;
        }
    };
    for s in due {
        let rule = serde_json::from_str::<Value>(&s.rule).map_err(|e| e.to_string()).and_then(|v| Rule::parse(&v));
        let repos: Vec<String> = serde_json::from_str(&s.repos).unwrap_or_default();
        let (next, jobs, note) = match rule {
            Err(e) => (None, Vec::new(), format!("the rule is unreadable: {e}")),
            Ok(rule) => {
                let (jobs, note) = run(app, s.id, &repos, s.seal).unwrap_or_else(|e| (Vec::new(), e));
                (Some(rule.next_after(now)), jobs, note)
            }
        };
        // An unreadable rule is not run again until it is saved anew.
        let write = app.st.meta().write(|t| {
            if next.is_none() {
                t.schedule_update(s.id, &ScheduleSpec { repos: &s.repos, rule: &s.rule, seal: s.seal, enabled: false, next_at: None }, now, "schedule")?;
            }
            t.schedule_ran(s.id, now, next, &serde_json::to_string(&jobs).expect("JSON"), &note)
        });
        if let Err(e) = write {
            eprintln!("ERROR: schedule {}: {e}", s.id);
        }
    }
}

/// Check the schedules twice a minute until the process ends.
pub fn run_forever(app: Arc<App>) {
    loop {
        tick(&app, now_ms());
        std::thread::sleep(Duration::from_secs(30));
    }
}

#[cfg(test)]
mod unit {
    use super::*;

    const T0: i64 = 1_795_651_200_000; // 2026-11-26 00:00 UTC, a Thursday

    #[test]
    fn rules_name_the_next_time_in_utc() {
        let daily = |at: &str, days: Value| Rule::parse(&json!({ "kind": "daily", "at": at, "days": days })).unwrap();
        let every_day = daily("02:30", Value::Null);
        assert_eq!(every_day.next_after(T0), T0 + 2 * HOUR + 30 * 60_000);
        assert_eq!(every_day.next_after(T0 + 2 * HOUR + 30 * 60_000), T0 + DAY + 2 * HOUR + 30 * 60_000, "not the same minute again");
        // Thursday after 02:30, weekdays only: Friday; on Friday evening: Monday.
        let weekdays = daily("02:30", json!(["mon", "tue", "wed", "thu", "fri"]));
        assert_eq!(weekdays.next_after(T0 + 3 * HOUR), T0 + DAY + 2 * HOUR + 30 * 60_000);
        assert_eq!(weekdays.next_after(T0 + DAY + 20 * HOUR), T0 + 4 * DAY + 2 * HOUR + 30 * 60_000);
        let sunday = daily("00:00", json!(["sun"]));
        assert_eq!(sunday.next_after(T0), T0 + 3 * DAY);
        assert_eq!(sunday.next_after(T0 + 3 * DAY), T0 + 10 * DAY, "a whole week on");
        assert_eq!(Rule::parse(&json!({ "kind": "every", "hours": 6 })).unwrap().next_after(T0 + 5), T0 + 5 + 6 * HOUR);

        assert_eq!(weekdays.to_json(), json!({ "kind": "daily", "at": "02:30", "days": ["mon", "tue", "wed", "thu", "fri"] }));
        assert_eq!(Rule::parse(&weekdays.to_json()).unwrap(), weekdays, "round trip");
        for bad in [
            json!({ "kind": "daily", "at": "24:00" }),
            json!({ "kind": "daily", "at": "2:300" }),
            json!({ "kind": "daily", "at": "noon" }),
            json!({ "kind": "daily", "at": "02:30", "days": [] }),
            json!({ "kind": "daily", "at": "02:30", "days": ["monday"] }),
            json!({ "kind": "every", "hours": 0 }),
            json!({ "kind": "every", "hours": 169 }),
            json!({ "kind": "every", "hours": "6" }),
            json!({ "kind": "cron", "expr": "* * * * *" }),
        ] {
            assert!(Rule::parse(&bad).is_err(), "{bad}");
        }
    }
}
