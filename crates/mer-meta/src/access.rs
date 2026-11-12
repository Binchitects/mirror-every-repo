//! Users, sessions, client tokens and traffic statistics.
use crate::{MetaError, Result, Tx};
use mer_core::Digest;
use rusqlite::{params, OptionalExtension};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UserRow {
    pub id: i64,
    pub name: String,
    pub pw_hash: String,
    pub role: String,
    pub disabled: bool,
    pub created_at: i64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionRow {
    pub user: UserRow,
    pub csrf: String,
    pub expires_at: i64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TokenRow {
    pub id: i64,
    pub name: String,
    /// `None` means every repo.
    pub repo: Option<String>,
    pub created_by: String,
    pub created_at: i64,
    pub last_used_at: Option<i64>,
    pub revoked: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TrafficRow {
    pub repo: String,
    pub path: String,
    pub hits: u64,
    pub bytes: u64,
    pub last_at: i64,
}

const USER_COLS: &str = "u.id, u.name, u.pw_hash, u.role, u.disabled, u.created_at";

fn user_row(r: &rusqlite::Row, at: usize) -> rusqlite::Result<UserRow> {
    Ok(UserRow {
        id: r.get(at)?,
        name: r.get(at + 1)?,
        pw_hash: r.get(at + 2)?,
        role: r.get(at + 3)?,
        disabled: r.get::<_, i64>(at + 4)? != 0,
        created_at: r.get(at + 5)?,
    })
}

fn token_row(r: &rusqlite::Row) -> rusqlite::Result<TokenRow> {
    Ok(TokenRow {
        id: r.get(0)?,
        name: r.get(1)?,
        repo: r.get(2)?,
        created_by: r.get(3)?,
        created_at: r.get(4)?,
        last_used_at: r.get(5)?,
        revoked: r.get::<_, i64>(6)? != 0,
    })
}

const TOKEN_COLS: &str = "id, name, repo, created_by, created_at, last_used_at, revoked";

impl Tx<'_> {
    // ---------------------------------------------------------------- users

    pub fn user_create(&self, name: &str, pw_hash: &str, role: &str, now: i64) -> Result<UserRow> {
        if self.user_get(name)?.is_some() {
            return Err(MetaError::Conflict(format!("user {name} already exists")));
        }
        self.c.execute(
            "INSERT INTO user(name, pw_hash, role, created_at) VALUES (?1, ?2, ?3, ?4)",
            params![name, pw_hash, role, now],
        )?;
        self.user_get(name)?.ok_or(MetaError::Corrupt("user vanished"))
    }

    pub fn user_get(&self, name: &str) -> Result<Option<UserRow>> {
        Ok(self
            .c
            .query_row(&format!("SELECT {USER_COLS} FROM user u WHERE u.name = ?1"), params![name], |r| user_row(r, 0))
            .optional()?)
    }

    pub fn user_list(&self) -> Result<Vec<UserRow>> {
        let mut st = self.c.prepare(&format!("SELECT {USER_COLS} FROM user u ORDER BY u.name"))?;
        let rows = st.query_map([], |r| user_row(r, 0))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn user_count(&self) -> Result<u64> {
        Ok(self.c.query_row("SELECT COUNT(*) FROM user", [], |r| r.get::<_, i64>(0))? as u64)
    }

    /// Enabled administrators. The last one cannot be removed or demoted.
    pub fn admin_count(&self) -> Result<u64> {
        Ok(self
            .c
            .query_row("SELECT COUNT(*) FROM user WHERE role = 'admin' AND disabled = 0", [], |r| r.get::<_, i64>(0))?
            as u64)
    }

    fn user_update(&self, name: &str, sql: &str, value: &dyn rusqlite::ToSql) -> Result<()> {
        let n = self.c.execute(sql, params![name, value])?;
        if n == 0 {
            return Err(MetaError::NotFound(format!("user {name}")));
        }
        Ok(())
    }

    /// Changing a password ends every session of that user.
    pub fn user_set_password(&self, name: &str, pw_hash: &str) -> Result<()> {
        self.user_update(name, "UPDATE user SET pw_hash = ?2 WHERE name = ?1", &pw_hash)?;
        self.sessions_end(name)
    }

    pub fn user_set_role(&self, name: &str, role: &str) -> Result<()> {
        self.user_update(name, "UPDATE user SET role = ?2 WHERE name = ?1", &role)
    }

    pub fn user_set_disabled(&self, name: &str, disabled: bool) -> Result<()> {
        self.user_update(name, "UPDATE user SET disabled = ?2 WHERE name = ?1", &(disabled as i64))?;
        if disabled {
            self.sessions_end(name)?;
        }
        Ok(())
    }

    // ------------------------------------------------------------- sessions

    pub fn session_create(&self, id_hash: &Digest, user_id: i64, csrf: &str, now: i64, expires_at: i64) -> Result<()> {
        self.c.execute("DELETE FROM session WHERE expires_at <= ?1", params![now])?;
        self.c.execute(
            "INSERT INTO session(id_hash, user_id, csrf, created_at, expires_at) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![id_hash.as_bytes(), user_id, csrf, now, expires_at],
        )?;
        Ok(())
    }

    /// A live session of an enabled user, or nothing.
    pub fn session_get(&self, id_hash: &Digest, now: i64) -> Result<Option<SessionRow>> {
        Ok(self
            .c
            .query_row(
                &format!(
                    "SELECT {USER_COLS}, s.csrf, s.expires_at FROM session s JOIN user u ON u.id = s.user_id
                      WHERE s.id_hash = ?1 AND s.expires_at > ?2 AND u.disabled = 0"
                ),
                params![id_hash.as_bytes(), now],
                |r| Ok(SessionRow { user: user_row(r, 0)?, csrf: r.get(6)?, expires_at: r.get(7)? }),
            )
            .optional()?)
    }

    pub fn session_delete(&self, id_hash: &Digest) -> Result<()> {
        self.c.execute("DELETE FROM session WHERE id_hash = ?1", params![id_hash.as_bytes()])?;
        Ok(())
    }

    fn sessions_end(&self, user: &str) -> Result<()> {
        self.c.execute(
            "DELETE FROM session WHERE user_id IN (SELECT id FROM user WHERE name = ?1)",
            params![user],
        )?;
        Ok(())
    }

    // --------------------------------------------------------------- tokens

    pub fn token_create(&self, name: &str, secret_hash: &Digest, repo: Option<&str>, by: &str, now: i64) -> Result<()> {
        let taken: Option<i64> =
            self.c.query_row("SELECT id FROM token WHERE name = ?1", params![name], |r| r.get(0)).optional()?;
        if taken.is_some() {
            return Err(MetaError::Conflict(format!("token {name} already exists")));
        }
        self.c.execute(
            "INSERT INTO token(name, secret_hash, repo, created_by, created_at) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![name, secret_hash.as_bytes(), repo, by, now],
        )?;
        Ok(())
    }

    /// The token with this secret, unless it was revoked.
    pub fn token_find(&self, secret_hash: &Digest) -> Result<Option<TokenRow>> {
        Ok(self
            .c
            .query_row(
                &format!("SELECT {TOKEN_COLS} FROM token WHERE secret_hash = ?1 AND revoked = 0"),
                params![secret_hash.as_bytes()],
                token_row,
            )
            .optional()?)
    }

    pub fn token_touch(&self, id: i64, now: i64) -> Result<()> {
        self.c.execute("UPDATE token SET last_used_at = ?2 WHERE id = ?1", params![id, now])?;
        Ok(())
    }

    pub fn token_list(&self) -> Result<Vec<TokenRow>> {
        let mut st = self.c.prepare(&format!("SELECT {TOKEN_COLS} FROM token ORDER BY name"))?;
        let rows = st.query_map([], token_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn token_revoke(&self, name: &str) -> Result<()> {
        let n = self.c.execute("UPDATE token SET revoked = 1 WHERE name = ?1", params![name])?;
        if n == 0 {
            return Err(MetaError::NotFound(format!("token {name}")));
        }
        Ok(())
    }

    // -------------------------------------------------------------- traffic

    pub fn traffic_add(&self, repo: &str, path: &str, hits: u64, bytes: u64, now: i64) -> Result<()> {
        self.c.execute(
            "INSERT INTO traffic(repo, path, hits, bytes, last_at) VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(repo, path) DO UPDATE SET hits = hits + excluded.hits,
               bytes = bytes + excluded.bytes, last_at = excluded.last_at",
            params![repo, path, hits as i64, bytes as i64, now],
        )?;
        Ok(())
    }

    /// Most downloaded files.
    pub fn traffic_top(&self, limit: u32) -> Result<Vec<TrafficRow>> {
        let mut st = self.c.prepare(
            "SELECT repo, path, hits, bytes, last_at FROM traffic ORDER BY hits DESC, repo, path LIMIT ?1",
        )?;
        let rows = st.query_map(params![limit], |r| {
            Ok(TrafficRow {
                repo: r.get(0)?,
                path: r.get(1)?,
                hits: r.get::<_, i64>(2)? as u64,
                bytes: r.get::<_, i64>(3)? as u64,
                last_at: r.get(4)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// (repo, hits, bytes) totals.
    pub fn traffic_by_repo(&self) -> Result<Vec<(String, u64, u64)>> {
        let mut st = self.c.prepare("SELECT repo, SUM(hits), SUM(bytes) FROM traffic GROUP BY repo ORDER BY repo")?;
        let rows = st.query_map([], |r| Ok((r.get(0)?, r.get::<_, i64>(1)? as u64, r.get::<_, i64>(2)? as u64)))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }
}

#[cfg(test)]
mod tests {
    use crate::*;
    use mer_core::Digest;

    #[test]
    fn users_and_the_last_admin_count() {
        let m = Meta::open_in_memory().unwrap();
        m.write(|t| {
            assert_eq!(t.user_count()?, 0);
            t.user_create("alice", "hash-a", "admin", 1)?;
            t.user_create("bob", "hash-b", "viewer", 2)?;
            assert!(matches!(t.user_create("alice", "x", "viewer", 3), Err(MetaError::Conflict(_))));
            assert_eq!(t.user_list()?.iter().map(|u| u.name.as_str()).collect::<Vec<_>>(), ["alice", "bob"]);
            assert_eq!(t.admin_count()?, 1);
            t.user_set_role("bob", "admin")?;
            t.user_set_disabled("alice", true)?;
            assert_eq!(t.admin_count()?, 1, "a disabled admin does not count");
            assert!(matches!(t.user_set_role("nobody", "admin"), Err(MetaError::NotFound(_))));
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn sessions_expire_and_end_with_password_change_or_disable() {
        let m = Meta::open_in_memory().unwrap();
        let (s1, s2, s3) = (Digest::of(b"1"), Digest::of(b"2"), Digest::of(b"3"));
        m.write(|t| {
            let alice = t.user_create("alice", "h", "admin", 0)?;
            let bob = t.user_create("bob", "h", "viewer", 0)?;
            t.session_create(&s1, alice.id, "csrf1", 100, 200)?;
            t.session_create(&s2, bob.id, "csrf2", 100, 200)?;
            let s = t.session_get(&s1, 150)?.unwrap();
            assert_eq!((s.user.name.as_str(), s.csrf.as_str()), ("alice", "csrf1"));
            assert!(t.session_get(&s1, 200)?.is_none(), "expired exactly at expires_at");
            assert!(t.session_get(&Digest::of(b"unknown"), 150)?.is_none());

            t.user_set_password("alice", "new")?;
            assert!(t.session_get(&s1, 150)?.is_none(), "password change logs the user out everywhere");
            assert!(t.session_get(&s2, 150)?.is_some(), "other users are unaffected");

            t.user_set_disabled("bob", true)?;
            assert!(t.session_get(&s2, 150)?.is_none());
            // A session row for a disabled user is never honoured, even if one exists.
            t.session_create(&s3, bob.id, "c", 100, 200)?;
            assert!(t.session_get(&s3, 150)?.is_none());

            t.session_create(&s1, alice.id, "c", 100, 200)?;
            t.session_delete(&s1)?;
            assert!(t.session_get(&s1, 150)?.is_none());
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn tokens_are_found_by_secret_hash_until_revoked() {
        let m = Meta::open_in_memory().unwrap();
        let (a, b) = (Digest::of(b"secret-a"), Digest::of(b"secret-b"));
        m.write(|t| {
            t.token_create("ci", &a, Some("debian"), "alice", 1)?;
            t.token_create("all", &b, None, "alice", 2)?;
            assert!(matches!(t.token_create("ci", &Digest::of(b"x"), None, "alice", 3), Err(MetaError::Conflict(_))));
            let tok = t.token_find(&a)?.unwrap();
            assert_eq!((tok.name.as_str(), tok.repo.as_deref(), tok.last_used_at), ("ci", Some("debian"), None));
            t.token_touch(tok.id, 50)?;
            assert_eq!(t.token_find(&a)?.unwrap().last_used_at, Some(50));
            assert_eq!(t.token_find(&b)?.unwrap().repo, None);
            assert!(t.token_find(&Digest::of(b"guess"))?.is_none());
            t.token_revoke("ci")?;
            assert!(t.token_find(&a)?.is_none());
            assert_eq!(t.token_list()?.len(), 2, "revoked tokens stay listed");
            assert!(matches!(t.token_revoke("nope"), Err(MetaError::NotFound(_))));
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn traffic_accumulates_and_ranks() {
        let m = Meta::open_in_memory().unwrap();
        m.write(|t| {
            t.traffic_add("debian", "pool/a.deb", 3, 300, 10)?;
            t.traffic_add("debian", "pool/a.deb", 2, 200, 20)?;
            t.traffic_add("debian", "pool/b.deb", 1, 50, 5)?;
            t.traffic_add("pypi", "x.whl", 4, 40, 7)?;
            let top = t.traffic_top(2)?;
            assert_eq!((top[0].path.as_str(), top[0].hits, top[0].bytes, top[0].last_at), ("pool/a.deb", 5, 500, 20));
            assert_eq!(top[1].path, "x.whl");
            assert_eq!(t.traffic_by_repo()?, vec![("debian".into(), 6, 550), ("pypi".into(), 4, 40)]);
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn private_flag_round_trips() {
        let m = Meta::open_in_memory().unwrap();
        m.write(|t| {
            let r = t.repo_create("r", "raw")?;
            assert!(!r.private);
            t.repo_set_private(r.id, true)?;
            assert!(t.repo_require("r")?.private);
            assert!(t.repo_list()?[0].private);
            Ok(())
        })
        .unwrap();
    }

    /// A database created by the previous release must upgrade in place and keep its data.
    #[test]
    fn version_1_database_is_upgraded_in_place() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("meta.db");
        {
            let c = rusqlite::Connection::open(&path).unwrap();
            c.execute_batch(&format!("BEGIN;\n{}\nPRAGMA user_version = 1;\nCOMMIT;", include_str!("schema.sql")))
                .unwrap();
            c.execute("INSERT INTO repo(name, format) VALUES ('old', 'raw')", []).unwrap();
        }
        let m = Meta::open(&path).unwrap();
        m.write(|t| {
            let r = t.repo_require("old")?;
            assert!(!r.private, "existing repos stay public");
            t.user_create("alice", "h", "admin", 0)?;
            Ok(())
        })
        .unwrap();
        drop(m);
        // Opening again is a no-op, and a newer database is refused by an older build.
        Meta::open(&path).unwrap();
        rusqlite::Connection::open(&path).unwrap().execute_batch("PRAGMA user_version = 99;").unwrap();
        assert!(matches!(Meta::open(&path), Err(MetaError::Invalid(_))));
    }
}
