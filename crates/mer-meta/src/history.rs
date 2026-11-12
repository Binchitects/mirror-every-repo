//! Hourly time series behind the dashboards.
//!
//! Counters (requests, bytes, misses) are summed into the hour they happen
//! in. Sizes (store and repositories) are sampled: the last sample of an
//! hour is what that hour shows.
use crate::{Result, Tx};
use rusqlite::params;

pub const HOUR_MS: i64 = 3_600_000;
/// How long history is kept.
pub const HISTORY_KEEP_HOURS: i64 = 400 * 24;

pub fn hour_of(now_ms: i64) -> i64 {
    now_ms.div_euclid(HOUR_MS)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HistoryPoint {
    pub key: String,
    pub hour: i64,
    pub value: u64,
}

impl Tx<'_> {
    /// Add to a counter.
    pub fn history_add(&self, metric: &str, key: &str, hour: i64, delta: u64) -> Result<()> {
        self.c.execute(
            "INSERT INTO history(metric, key, hour, value) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(metric, key, hour) DO UPDATE SET value = value + excluded.value",
            params![metric, key, hour, delta as i64],
        )?;
        Ok(())
    }

    /// Record a sampled size.
    pub fn history_set(&self, metric: &str, key: &str, hour: i64, value: u64) -> Result<()> {
        self.c.execute(
            "INSERT INTO history(metric, key, hour, value) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(metric, key, hour) DO UPDATE SET value = excluded.value",
            params![metric, key, hour, value as i64],
        )?;
        Ok(())
    }

    /// Points of one metric with `from <= hour < to`, ordered by key and hour.
    pub fn history_range(&self, metric: &str, from: i64, to: i64) -> Result<Vec<HistoryPoint>> {
        let mut st = self.c.prepare("SELECT key, hour, value FROM history WHERE metric = ?1 AND hour >= ?2 AND hour < ?3 ORDER BY key, hour")?;
        let rows = st.query_map(params![metric, from, to], |r| Ok(HistoryPoint { key: r.get(0)?, hour: r.get(1)?, value: r.get::<_, i64>(2)? as u64 }))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// For a sampled metric: the newest point of each key before `hour`, so
    /// a chart knows what the size was where its window begins.
    pub fn history_before(&self, metric: &str, hour: i64) -> Result<Vec<HistoryPoint>> {
        let mut st = self.c.prepare(
            "SELECT h.key, h.hour, h.value FROM history h
             WHERE h.metric = ?1 AND h.hour = (SELECT MAX(hour) FROM history WHERE metric = ?1 AND key = h.key AND hour < ?2)
             ORDER BY h.key",
        )?;
        let rows = st.query_map(params![metric, hour], |r| Ok(HistoryPoint { key: r.get(0)?, hour: r.get(1)?, value: r.get::<_, i64>(2)? as u64 }))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Record the current size of the store and of every repository, and
    /// drop history that is older than what is kept.
    pub fn history_sample_sizes(&self, now_ms: i64) -> Result<()> {
        let hour = hour_of(now_ms);
        let (blobs, bytes) = self.blob_count()?;
        self.history_set("store_bytes", "", hour, bytes)?;
        self.history_set("store_blobs", "", hour, blobs)?;
        for r in self.repo_list()? {
            self.history_set("repo_bytes", &r.name, hour, self.repo_usage(r.id)?)?;
        }
        self.c.execute("DELETE FROM history WHERE hour < ?1", params![hour - HISTORY_KEEP_HOURS])?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Meta;

    #[test]
    fn counters_add_up_and_sizes_keep_the_last_sample() {
        let m = Meta::open_in_memory().unwrap();
        m.write(|t| {
            t.history_add("requests", "apt", 10, 3)?;
            t.history_add("requests", "apt", 10, 4)?;
            t.history_add("requests", "apt", 12, 1)?;
            t.history_add("requests", "npm", 11, 9)?;
            t.history_set("repo_bytes", "apt", 5, 100)?;
            t.history_set("repo_bytes", "apt", 9, 300)?;
            t.history_set("repo_bytes", "apt", 9, 250)?;
            t.history_set("repo_bytes", "npm", 11, 70)?;
            Ok(())
        })
        .unwrap();
        let p = |key: &str, hour, value| HistoryPoint { key: key.into(), hour, value };
        m.read(|t| {
            assert_eq!(t.history_range("requests", 10, 12)?, vec![p("apt", 10, 7), p("npm", 11, 9)]);
            assert_eq!(t.history_range("requests", 0, 100)?.len(), 3);
            assert_eq!(t.history_before("repo_bytes", 10)?, vec![p("apt", 9, 250)]);
            assert_eq!(t.history_before("repo_bytes", 20)?, vec![p("apt", 9, 250), p("npm", 11, 70)]);
            assert!(t.history_before("repo_bytes", 5)?.is_empty());
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn sampling_records_sizes_and_drops_old_history() {
        let m = Meta::open_in_memory().unwrap();
        let now = 1_000_000 * HOUR_MS + 5;
        m.write(|t| {
            t.repo_create("apt", "apt")?;
            t.history_add("requests", "apt", 1_000_000 - HISTORY_KEEP_HOURS - 1, 5)?;
            t.history_add("requests", "apt", 1_000_000 - HISTORY_KEEP_HOURS, 6)?;
            t.history_sample_sizes(now)
        })
        .unwrap();
        m.read(|t| {
            assert_eq!(t.history_range("store_bytes", 0, i64::MAX)?, vec![HistoryPoint { key: String::new(), hour: 1_000_000, value: 0 }]);
            assert_eq!(t.history_range("repo_bytes", 0, i64::MAX)?[0].key, "apt");
            assert_eq!(t.history_range("requests", 0, i64::MAX)?.len(), 1, "the point older than the limit is gone");
            Ok(())
        })
        .unwrap();
        assert_eq!(hour_of(-1), -1);
    }
}
