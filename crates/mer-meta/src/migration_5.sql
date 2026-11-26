-- Migration 5: schedules that queue sync jobs (and a seal) from the panel.

-- repos: JSON array of repository names; [] means every repository with a source.
-- rule:  JSON, validated by the server: {"kind":"daily","at":"02:30","days":["mon",...]}
--        or {"kind":"every","hours":6}. Times are UTC.
-- next_at: when it is due next (ms); NULL while disabled.
CREATE TABLE schedule (
  id         INTEGER PRIMARY KEY,
  repos      TEXT NOT NULL,
  rule       TEXT NOT NULL,
  seal       INTEGER NOT NULL,
  enabled    INTEGER NOT NULL,
  next_at    INTEGER,
  last_at    INTEGER,
  last_jobs  TEXT NOT NULL DEFAULT '[]',
  last_note  TEXT NOT NULL DEFAULT '',
  updated_at INTEGER NOT NULL,
  updated_by TEXT NOT NULL
);
