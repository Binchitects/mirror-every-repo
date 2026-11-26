-- Migration 3: what each repo syncs from, and jobs run from the web panel.

-- spec: JSON object of the format's sync fields, validated by the server.
CREATE TABLE sync_source (
  repo_id    INTEGER PRIMARY KEY REFERENCES repo(id),
  spec       TEXT NOT NULL,
  updated_at INTEGER NOT NULL,
  updated_by TEXT NOT NULL
);

-- state: queued | running | succeeded | failed | cancelled | interrupted
CREATE TABLE job (
  id          INTEGER PRIMARY KEY,
  kind        TEXT NOT NULL,
  target      TEXT NOT NULL,
  args        TEXT NOT NULL,
  state       TEXT NOT NULL,
  actor       TEXT NOT NULL,
  created_at  INTEGER NOT NULL,
  started_at  INTEGER,
  finished_at INTEGER,
  exit_code   INTEGER,
  log         TEXT NOT NULL DEFAULT ''
);
CREATE INDEX job_state ON job(state);
