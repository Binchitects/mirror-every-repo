-- Migration 3: time series for the dashboards, and a freshness limit per repository.

-- NULL: the repository is never reported as out of date.
ALTER TABLE repo ADD COLUMN stale_after_days INTEGER;

-- One value per metric, key and hour (hour = Unix time / 3600).
--   requests, bytes, misses   key = repository, value = sum over the hour
--   repo_bytes                key = repository, value = size at the last sample of the hour
--   store_bytes, store_blobs  key = '',         value = size at the last sample of the hour
CREATE TABLE history (
  metric TEXT NOT NULL,
  key    TEXT NOT NULL,
  hour   INTEGER NOT NULL,
  value  INTEGER NOT NULL,
  PRIMARY KEY (metric, key, hour)
) WITHOUT ROWID;
