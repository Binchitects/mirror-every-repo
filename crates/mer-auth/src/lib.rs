//! Who may do what.
//!
//! - **Users** sign in to the web panel with a password and get a session.
//!   Roles: `viewer` (read), `operator` (approve, publish, roll back,
//!   maintenance), `admin` (users, tokens, repo settings).
//! - **Tokens** let package clients (apt, pip, CI) read private repos.
//!
//! Passwords are stored as Argon2id hashes. Session ids and token secrets are
//! random 256-bit values; only their SHA-256 is stored, so a copy of the
//! database does not let anyone sign in.
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::Argon2;
use mer_core::Digest;
use mer_meta::{MetaError, TokenRow, UserRow};
use mer_storage::{now_ms, Storage};
use rand::RngCore;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub const SESSION_TTL_MS: i64 = 12 * 3600 * 1000;
pub const MIN_PASSWORD_LEN: usize = 10;
const MAX_FAILURES: u32 = 5;
const LOCKOUT: Duration = Duration::from_secs(60);
const TOKEN_PREFIX: &str = "mer_";

#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    #[error("wrong user name or password")]
    BadCredentials,
    #[error("too many failed attempts; try again in {0} seconds")]
    LockedOut(u64),
    #[error("{0}")]
    Invalid(String),
    #[error("not allowed: {0}")]
    Forbidden(String),
    #[error(transparent)]
    Meta(#[from] MetaError),
    #[error("password hashing failed: {0}")]
    Hash(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Role {
    Viewer,
    Operator,
    Admin,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Role::Viewer => "viewer",
            Role::Operator => "operator",
            Role::Admin => "admin",
        }
    }
    pub fn parse(s: &str) -> Result<Role, AuthError> {
        match s {
            "viewer" => Ok(Role::Viewer),
            "operator" => Ok(Role::Operator),
            "admin" => Ok(Role::Admin),
            other => Err(AuthError::Invalid(format!("unknown role {other:?} (viewer, operator or admin)"))),
        }
    }
}

/// A signed-in user.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Identity {
    pub name: String,
    pub role: Role,
    /// Must be echoed in a header on every state-changing request.
    pub csrf: String,
}

/// What `login` hands back. `token` goes into the session cookie.
#[derive(Clone, Debug)]
pub struct Login {
    pub token: String,
    pub identity: Identity,
}

pub struct Auth {
    st: Arc<Storage>,
    /// user name -> (consecutive failures, locked until)
    failures: Mutex<HashMap<String, (u32, Option<Instant>)>>,
    /// A real hash of a throwaway password, verified against when the user
    /// does not exist, so that case costs the same time as a wrong password.
    decoy_hash: String,
}

fn random_hex() -> String {
    let mut b = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut b);
    hex::encode(b)
}

fn hash_password(password: &str) -> Result<String, AuthError> {
    let mut salt = [0u8; 16];
    rand::rngs::OsRng.fill_bytes(&mut salt);
    let salt = SaltString::encode_b64(&salt).map_err(|e| AuthError::Hash(e.to_string()))?;
    Ok(Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map_err(|e| AuthError::Hash(e.to_string()))?
        .to_string())
}

fn verify_password(password: &str, hash: &str) -> bool {
    PasswordHash::new(hash)
        .map(|parsed| Argon2::default().verify_password(password.as_bytes(), &parsed).is_ok())
        .unwrap_or(false)
}

fn check_name(kind: &str, name: &str) -> Result<(), AuthError> {
    let ok = !name.is_empty()
        && name.len() <= 64
        && name.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"._-".contains(&b))
        && !name.starts_with(['.', '-']);
    if ok {
        Ok(())
    } else {
        Err(AuthError::Invalid(format!(
            "{kind} name {name:?}: use 1 to 64 lower-case letters, digits, '.', '_' or '-'"
        )))
    }
}

fn check_password(password: &str) -> Result<(), AuthError> {
    if password.chars().count() < MIN_PASSWORD_LEN {
        return Err(AuthError::Invalid(format!("password must have at least {MIN_PASSWORD_LEN} characters")));
    }
    if password.len() > 1024 {
        return Err(AuthError::Invalid("password is too long".into()));
    }
    Ok(())
}

fn identity(user: &UserRow, csrf: String) -> Result<Identity, AuthError> {
    Ok(Identity { name: user.name.clone(), role: Role::parse(&user.role)?, csrf })
}

impl Auth {
    pub fn new(st: Arc<Storage>) -> Self {
        let decoy_hash = hash_password(&random_hex()).unwrap_or_default();
        Auth { st, failures: Mutex::default(), decoy_hash }
    }

    /// True until the first user exists. The first user must be an admin.
    pub fn needs_setup(&self) -> Result<bool, AuthError> {
        Ok(self.st.meta().read(|t| t.user_count())? == 0)
    }

    // ---------------------------------------------------------------- users

    pub fn create_user(&self, name: &str, password: &str, role: Role, actor: &str) -> Result<(), AuthError> {
        check_name("user", name)?;
        check_password(password)?;
        let hash = hash_password(password)?;
        self.st.meta().write(|t| {
            if t.user_count()? == 0 && role != Role::Admin {
                return Err(MetaError::Invalid("the first user must be an admin".into()));
            }
            t.user_create(name, &hash, role.as_str(), now_ms())?;
            t.audit(now_ms(), actor, "user.create", &format!("{name} ({})", role.as_str()))?;
            Ok(())
        })?;
        Ok(())
    }

    pub fn set_password(&self, name: &str, password: &str, actor: &str) -> Result<(), AuthError> {
        check_password(password)?;
        let hash = hash_password(password)?;
        self.st.meta().write(|t| {
            t.user_set_password(name, &hash)?;
            t.audit(now_ms(), actor, "user.password", name)?;
            Ok(())
        })?;
        self.failures.lock().unwrap_or_else(|e| e.into_inner()).remove(name);
        Ok(())
    }

    /// Change role and/or disabled state. The last enabled admin is protected,
    /// so the panel can never lock everyone out.
    pub fn update_user(&self, name: &str, role: Option<Role>, disabled: Option<bool>, actor: &str) -> Result<(), AuthError> {
        self.st.meta().write(|t| {
            if let Some(role) = role {
                t.user_set_role(name, role.as_str())?;
            }
            if let Some(disabled) = disabled {
                t.user_set_disabled(name, disabled)?;
            }
            if t.admin_count()? == 0 {
                return Err(MetaError::Conflict("this would leave no enabled admin".into()));
            }
            let detail = format!(
                "{name}{}{}",
                role.map_or(String::new(), |r| format!(" role={}", r.as_str())),
                disabled.map_or(String::new(), |d| format!(" disabled={d}"))
            );
            t.audit(now_ms(), actor, "user.update", &detail)?;
            Ok(())
        })?;
        Ok(())
    }

    pub fn users(&self) -> Result<Vec<UserRow>, AuthError> {
        Ok(self.st.meta().read(|t| t.user_list())?)
    }

    // ------------------------------------------------------------- sessions

    /// Check a password and open a session.
    pub fn login(&self, name: &str, password: &str) -> Result<Login, AuthError> {
        {
            let failures = self.failures.lock().unwrap_or_else(|e| e.into_inner());
            if let Some((_, Some(until))) = failures.get(name) {
                let now = Instant::now();
                if *until > now {
                    return Err(AuthError::LockedOut((*until - now).as_secs().max(1)));
                }
            }
        }
        let user = self.st.meta().read(|t| t.user_get(name))?;
        // Hash even when the user does not exist, so the answer takes the same
        // time and does not reveal which names are real.
        let good = match &user {
            Some(u) => verify_password(password, &u.pw_hash) && !u.disabled,
            None => {
                verify_password(password, &self.decoy_hash);
                false
            }
        };
        let Some(user) = user.filter(|_| good) else {
            let mut failures = self.failures.lock().unwrap_or_else(|e| e.into_inner());
            if failures.len() > 10_000 {
                failures.clear(); // bound memory under a flood of made-up names
            }
            let entry = failures.entry(name.to_string()).or_insert((0, None));
            entry.0 += 1;
            if entry.0 >= MAX_FAILURES {
                *entry = (0, Some(Instant::now() + LOCKOUT));
            }
            let _ = self.st.meta().write(|t| t.audit(now_ms(), name, "login.failed", ""));
            return Err(AuthError::BadCredentials);
        };
        self.failures.lock().unwrap_or_else(|e| e.into_inner()).remove(name);

        let token = random_hex();
        let csrf = random_hex();
        let now = now_ms();
        self.st.meta().write(|t| {
            t.session_create(&Digest::of(token.as_bytes()), user.id, &csrf, now, now + SESSION_TTL_MS)?;
            t.audit(now, &user.name, "login", "")?;
            Ok(())
        })?;
        Ok(Login { token, identity: identity(&user, csrf)? })
    }

    /// The user behind a session cookie, if the session is still valid.
    pub fn session(&self, token: &str) -> Option<Identity> {
        if token.len() != 64 {
            return None;
        }
        let row = self.st.meta().read(|t| t.session_get(&Digest::of(token.as_bytes()), now_ms())).ok()??;
        identity(&row.user, row.csrf).ok()
    }

    pub fn logout(&self, token: &str) {
        let _ = self.st.meta().write(|t| t.session_delete(&Digest::of(token.as_bytes())));
    }

    // --------------------------------------------------------------- tokens

    /// Create a read token for package clients. The secret is returned once
    /// and cannot be recovered later.
    pub fn create_token(&self, name: &str, repo: Option<&str>, actor: &str) -> Result<String, AuthError> {
        check_name("token", name)?;
        let secret = format!("{TOKEN_PREFIX}{}", random_hex());
        self.st.meta().write(|t| {
            if let Some(repo) = repo {
                t.repo_require(repo)?;
            }
            t.token_create(name, &Digest::of(secret.as_bytes()), repo, actor, now_ms())?;
            t.audit(now_ms(), actor, "token.create", &format!("{name} repo={}", repo.unwrap_or("*")))?;
            Ok(())
        })?;
        Ok(secret)
    }

    pub fn revoke_token(&self, name: &str, actor: &str) -> Result<(), AuthError> {
        self.st.meta().write(|t| {
            t.token_revoke(name)?;
            t.audit(now_ms(), actor, "token.revoke", name)?;
            Ok(())
        })?;
        Ok(())
    }

    pub fn tokens(&self) -> Result<Vec<TokenRow>, AuthError> {
        Ok(self.st.meta().read(|t| t.token_list())?)
    }

    /// May the bearer of `secret` read `repo`? Returns the token name.
    pub fn check_token(&self, secret: &str, repo: &str) -> Option<String> {
        if !secret.starts_with(TOKEN_PREFIX) || secret.len() != TOKEN_PREFIX.len() + 64 {
            return None;
        }
        let token = self.st.meta().read(|t| t.token_find(&Digest::of(secret.as_bytes()))).ok()??;
        if token.repo.as_deref().is_some_and(|r| r != repo) {
            return None;
        }
        // "Last used" is for the panel; one write per minute per token is enough.
        let now = now_ms();
        if token.last_used_at.is_none_or(|t| now - t > 60_000) {
            let _ = self.st.meta().write(|t| t.token_touch(token.id, now));
        }
        Some(token.name)
    }
}

/// Does `have` meet `need`? Used by every protected action.
pub fn require(have: Role, need: Role) -> Result<(), AuthError> {
    if have >= need {
        Ok(())
    } else {
        Err(AuthError::Forbidden(format!("needs the {} role", need.as_str())))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn auth() -> (tempfile::TempDir, Arc<Storage>, Auth) {
        let dir = tempfile::tempdir().unwrap();
        let st = Arc::new(Storage::open(dir.path()).unwrap());
        let auth = Auth::new(st.clone());
        (dir, st, auth)
    }

    const PW: &str = "correct horse battery";

    #[test]
    fn first_user_must_be_admin_and_names_and_passwords_are_checked() {
        let (_d, _st, a) = auth();
        assert!(a.needs_setup().unwrap());
        assert!(a.create_user("bob", PW, Role::Viewer, "cli").is_err(), "first user must be admin");
        a.create_user("alice", PW, Role::Admin, "cli").unwrap();
        assert!(!a.needs_setup().unwrap());
        a.create_user("bob", PW, Role::Viewer, "alice").unwrap();
        assert!(matches!(a.create_user("bob", PW, Role::Viewer, "alice"), Err(AuthError::Meta(MetaError::Conflict(_)))));
        for bad in ["", "Alice", "a b", "../x", "-x", &"x".repeat(65)] {
            assert!(matches!(a.create_user(bad, PW, Role::Viewer, "alice"), Err(AuthError::Invalid(_))), "{bad:?}");
        }
        assert!(matches!(a.create_user("carol", "short", Role::Viewer, "alice"), Err(AuthError::Invalid(_))));
    }

    #[test]
    fn login_gives_a_session_and_wrong_credentials_do_not() {
        let (_d, st, a) = auth();
        a.create_user("alice", PW, Role::Admin, "cli").unwrap();
        let login = a.login("alice", PW).unwrap();
        assert_eq!(login.token.len(), 64);
        assert_eq!(a.session(&login.token).unwrap(), login.identity);
        assert_eq!(login.identity.role, Role::Admin);

        assert!(matches!(a.login("alice", "wrong password!"), Err(AuthError::BadCredentials)));
        assert!(matches!(a.login("nobody", PW), Err(AuthError::BadCredentials)), "same error for unknown users");
        assert!(a.session("").is_none());
        assert!(a.session(&"0".repeat(64)).is_none());
        assert!(a.session(&login.token[..63]).is_none());

        // Neither the password nor the session id is stored in the database.
        let user = st.meta().read(|t| t.user_get("alice")).unwrap().unwrap();
        assert!(user.pw_hash.starts_with("$argon2id$") && !user.pw_hash.contains(PW));
        let raw = std::fs::read(st.root().join("meta.db")).unwrap();
        let wal = std::fs::read(st.root().join("meta.db-wal")).unwrap_or_default();
        for haystack in [&raw, &wal] {
            assert!(!haystack.windows(login.token.len()).any(|w| w == login.token.as_bytes()), "session id on disk");
        }

        a.logout(&login.token);
        assert!(a.session(&login.token).is_none());
    }

    #[test]
    fn two_users_get_different_hashes_for_the_same_password() {
        let (_d, st, a) = auth();
        a.create_user("alice", PW, Role::Admin, "cli").unwrap();
        a.create_user("bob", PW, Role::Viewer, "cli").unwrap();
        let hash = |n: &str| st.meta().read(|t| t.user_get(n)).unwrap().unwrap().pw_hash;
        assert_ne!(hash("alice"), hash("bob"), "per-user salt");
    }

    #[test]
    fn repeated_failures_lock_the_account_for_a_while() {
        let (_d, _st, a) = auth();
        a.create_user("alice", PW, Role::Admin, "cli").unwrap();
        for _ in 0..MAX_FAILURES {
            assert!(matches!(a.login("alice", "guess guess guess"), Err(AuthError::BadCredentials)));
        }
        // Even the right password is refused during the lockout.
        assert!(matches!(a.login("alice", PW), Err(AuthError::LockedOut(_))));
        // An admin resetting the password lifts it.
        a.set_password("alice", "a brand new password", "cli").unwrap();
        assert!(a.login("alice", "a brand new password").is_ok());
    }

    #[test]
    fn password_change_and_disable_end_sessions() {
        let (_d, _st, a) = auth();
        a.create_user("alice", PW, Role::Admin, "cli").unwrap();
        a.create_user("bob", PW, Role::Operator, "cli").unwrap();
        let s1 = a.login("bob", PW).unwrap().token;
        a.set_password("bob", "another long password", "alice").unwrap();
        assert!(a.session(&s1).is_none());
        assert!(a.login("bob", PW).is_err());
        let s2 = a.login("bob", "another long password").unwrap().token;
        a.update_user("bob", None, Some(true), "alice").unwrap();
        assert!(a.session(&s2).is_none());
        assert!(matches!(a.login("bob", "another long password"), Err(AuthError::BadCredentials)), "disabled user");
    }

    #[test]
    fn the_last_admin_cannot_be_demoted_or_disabled() {
        let (_d, _st, a) = auth();
        a.create_user("alice", PW, Role::Admin, "cli").unwrap();
        a.create_user("bob", PW, Role::Viewer, "cli").unwrap();
        assert!(a.update_user("alice", Some(Role::Viewer), None, "alice").is_err());
        assert!(a.update_user("alice", None, Some(true), "alice").is_err());
        assert_eq!(a.login("alice", PW).unwrap().identity.role, Role::Admin, "nothing changed");
        // With a second admin it is allowed.
        a.update_user("bob", Some(Role::Admin), None, "alice").unwrap();
        a.update_user("alice", Some(Role::Viewer), None, "bob").unwrap();
        assert_eq!(a.login("alice", PW).unwrap().identity.role, Role::Viewer);
    }

    #[test]
    fn role_order() {
        assert!(require(Role::Admin, Role::Operator).is_ok());
        assert!(require(Role::Operator, Role::Operator).is_ok());
        assert!(matches!(require(Role::Viewer, Role::Operator), Err(AuthError::Forbidden(_))));
        assert!(matches!(require(Role::Operator, Role::Admin), Err(AuthError::Forbidden(_))));
        assert!(Role::parse("root").is_err());
    }

    #[test]
    fn tokens_grant_read_access_to_their_repo_only() {
        let (_d, st, a) = auth();
        st.create_repo("debian", "apt").unwrap();
        st.create_repo("internal", "raw").unwrap();
        let scoped = a.create_token("ci-debian", Some("debian"), "alice").unwrap();
        let global = a.create_token("ops", None, "alice").unwrap();
        assert!(scoped.starts_with("mer_") && scoped.len() == 68);
        assert_ne!(scoped, global);

        assert_eq!(a.check_token(&scoped, "debian").as_deref(), Some("ci-debian"));
        assert_eq!(a.check_token(&scoped, "internal"), None);
        assert_eq!(a.check_token(&global, "internal").as_deref(), Some("ops"));
        assert_eq!(a.check_token("mer_guess", "debian"), None);
        assert_eq!(a.check_token(&scoped.replace("mer_", "xxx_"), "debian"), None);
        assert_eq!(a.check_token("", "debian"), None);

        assert!(a.create_token("x", Some("no-such-repo"), "alice").is_err());
        assert!(a.create_token("ci-debian", None, "alice").is_err(), "names are unique");

        a.revoke_token("ci-debian", "alice").unwrap();
        assert_eq!(a.check_token(&scoped, "debian"), None);
        let listed = a.tokens().unwrap();
        assert_eq!(listed.len(), 2);
        assert!(listed.iter().find(|t| t.name == "ops").unwrap().last_used_at.is_some());
    }

    #[test]
    fn access_changes_are_audited() {
        let (_d, st, a) = auth();
        a.create_user("alice", PW, Role::Admin, "cli").unwrap();
        let _ = a.login("alice", "wrong wrong wrong");
        a.login("alice", PW).unwrap();
        a.create_token("ops", None, "alice").unwrap();
        let actions: Vec<String> =
            st.meta().read(|t| t.audit_list(10)).unwrap().into_iter().rev().map(|r| r.action).collect();
        assert_eq!(actions, ["user.create", "login.failed", "login", "token.create"]);
        assert_eq!(st.meta().read(|t| t.audit_verify()).unwrap(), Ok(4));
    }
}
