-- Migration 2: access control and traffic statistics.

ALTER TABLE repo ADD COLUMN access TEXT NOT NULL DEFAULT 'public';

-- role: admin | operator | viewer
CREATE TABLE user (
  id         INTEGER PRIMARY KEY,
  name       TEXT NOT NULL UNIQUE,
  pw_hash    TEXT NOT NULL,
  role       TEXT NOT NULL,
  disabled   INTEGER NOT NULL DEFAULT 0,
  created_at INTEGER NOT NULL
);

-- Only the SHA-256 of a session id is stored.
CREATE TABLE session (
  id_hash    BLOB PRIMARY KEY,
  user_id    INTEGER NOT NULL REFERENCES user(id),
  csrf       TEXT NOT NULL,
  created_at INTEGER NOT NULL,
  expires_at INTEGER NOT NULL
) WITHOUT ROWID;

-- Read tokens for package clients. repo NULL = every repo.
CREATE TABLE token (
  id           INTEGER PRIMARY KEY,
  name         TEXT NOT NULL UNIQUE,
  secret_hash  BLOB NOT NULL UNIQUE,
  repo         TEXT,
  created_by   TEXT NOT NULL,
  created_at   INTEGER NOT NULL,
  last_used_at INTEGER,
  revoked      INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE traffic (
  repo    TEXT NOT NULL,
  path    TEXT NOT NULL,
  hits    INTEGER NOT NULL,
  bytes   INTEGER NOT NULL,
  last_at INTEGER NOT NULL,
  PRIMARY KEY (repo, path)
) WITHOUT ROWID;
