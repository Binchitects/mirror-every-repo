//! Sign-in against an LDAP directory (OpenLDAP, Active Directory, FreeIPA).
//!
//! The usual three steps: find the entry of the user with a search, bind as
//! that entry with the password that was typed, then read which groups the
//! user is in and map them to a role. The password is only ever sent to the
//! directory in that bind; nothing of it is stored.
use ldap3::{ldap_escape, LdapConn, LdapConnSettings, Scope, SearchEntry};
use mer_auth::{Directory, Role};
use std::time::Duration;

#[derive(Clone, Debug)]
pub struct LdapConfig {
    /// `ldap://host:389` or `ldaps://host:636`.
    pub url: String,
    /// Upgrade an `ldap://` connection to TLS before anything is sent.
    pub starttls: bool,
    /// The account used to search. `None`: search anonymously.
    pub bind_dn: Option<String>,
    pub bind_password: Option<String>,
    /// Where users are, for example `ou=people,dc=example,dc=org`.
    pub user_base: String,
    /// How a user is found; `{user}` is the name that was typed.
    pub user_filter: String,
    /// Where groups are. `None`: only the `memberOf` attribute of the user is used.
    pub group_base: Option<String>,
    /// How the groups of a user are found; `{dn}` is the entry of the user,
    /// `{user}` the name that was typed.
    pub group_filter: String,
    /// Groups (by name or by full DN) whose members get each role.
    pub admin_groups: Vec<String>,
    pub operator_groups: Vec<String>,
    pub viewer_groups: Vec<String>,
    pub timeout: Duration,
}

impl LdapConfig {
    /// Something to print at start-up; never the password.
    pub fn describe(&self) -> String {
        format!(
            "{}{}, users under {} matching {}, searching as {}",
            self.url,
            if self.starttls { " (StartTLS)" } else { "" },
            self.user_base,
            self.user_filter,
            self.bind_dn.as_deref().unwrap_or("anonymous")
        )
    }

    pub fn validate(&self) -> Result<(), String> {
        if !self.url.starts_with("ldap://") && !self.url.starts_with("ldaps://") {
            return Err(format!("{:?} is not an ldap:// or ldaps:// URL", self.url));
        }
        if !self.user_filter.contains("{user}") {
            return Err("the user filter must contain {user}".into());
        }
        if self.bind_dn.is_some() != self.bind_password.is_some() {
            return Err("a bind DN and its password go together".into());
        }
        if self.admin_groups.is_empty() && self.operator_groups.is_empty() && self.viewer_groups.is_empty() {
            return Err("no group grants a role: name at least one admin, operator or viewer group".into());
        }
        Ok(())
    }

    fn connect(&self) -> Result<LdapConn, String> {
        let settings = LdapConnSettings::new().set_conn_timeout(self.timeout).set_starttls(self.starttls);
        let mut conn = LdapConn::with_settings(settings, &self.url).map_err(|e| format!("cannot connect to {}: {e}", self.url))?;
        conn.with_timeout(self.timeout);
        Ok(conn)
    }
}

/// The highest role any of the groups grants. A group is named by its full
/// DN or by its `cn`; letter case does not matter, as in LDAP.
pub fn role_of(cfg: &LdapConfig, groups: &[String]) -> Option<Role> {
    let member_of = |wanted: &[String]| {
        wanted.iter().any(|w| {
            let w = w.to_lowercase();
            groups.iter().any(|g| {
                let g = g.to_lowercase();
                // "cn=admins,ou=groups,..." is also the group named "admins".
                g == w || g.split(',').next().and_then(|first| first.split_once('=')).is_some_and(|(k, v)| k.trim() == "cn" && v.trim() == w)
            })
        })
    };
    if member_of(&cfg.admin_groups) {
        Some(Role::Admin)
    } else if member_of(&cfg.operator_groups) {
        Some(Role::Operator)
    } else if member_of(&cfg.viewer_groups) {
        Some(Role::Viewer)
    } else {
        None
    }
}

/// A filter with the placeholders filled in, escaped so that nothing typed
/// by a user can change what the filter means.
pub fn fill_filter(template: &str, user: &str, dn: &str) -> String {
    template.replace("{user}", &ldap_escape(user)).replace("{dn}", &ldap_escape(dn))
}

impl Directory for LdapConfig {
    fn check(&self, name: &str, password: &str) -> Result<Option<Role>, String> {
        // An empty password would be an "unauthenticated bind", which many
        // servers accept as success. It is never a valid password here.
        if password.is_empty() || name.is_empty() {
            return Ok(None);
        }
        let mut search = self.connect()?;
        if let (Some(dn), Some(pw)) = (&self.bind_dn, &self.bind_password) {
            search.simple_bind(dn, pw).and_then(|r| r.success()).map_err(|e| format!("the search account cannot sign in: {e}"))?;
        }
        let filter = fill_filter(&self.user_filter, name, "");
        let (entries, _) = search
            .search(&self.user_base, Scope::Subtree, &filter, vec!["memberOf"])
            .and_then(|r| r.success())
            .map_err(|e| format!("searching for the user failed: {e}"))?;
        let mut entries: Vec<SearchEntry> = entries.into_iter().map(SearchEntry::construct).collect();
        let user = match entries.len() {
            0 => return Ok(None),
            1 => entries.remove(0),
            n => return Err(format!("{n} entries match the name {name:?}; the user filter must find exactly one")),
        };

        // The password check: bind as the user, on a connection of its own.
        let mut as_user = self.connect()?;
        match as_user.simple_bind(&user.dn, password) {
            Ok(result) if result.rc == 0 => {}
            // 49: invalid credentials. Other codes (account locked, password expired) are refusals too.
            Ok(_) => return Ok(None),
            Err(e) => return Err(format!("the directory did not answer the password check: {e}")),
        }
        let _ = as_user.unbind();

        let mut groups: Vec<String> = user.attrs.iter().filter(|(k, _)| k.eq_ignore_ascii_case("memberOf")).flat_map(|(_, v)| v.clone()).collect();
        if let Some(base) = &self.group_base {
            let filter = fill_filter(&self.group_filter, name, &user.dn);
            let (found, _) = search.search(base, Scope::Subtree, &filter, vec!["cn"]).and_then(|r| r.success()).map_err(|e| format!("searching for the groups of the user failed: {e}"))?;
            groups.extend(found.into_iter().map(|e| SearchEntry::construct(e).dn));
        }
        let _ = search.unbind();
        Ok(role_of(self, &groups))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> LdapConfig {
        LdapConfig {
            url: "ldap://directory:389".into(),
            starttls: false,
            bind_dn: Some("cn=search,dc=example,dc=org".into()),
            bind_password: Some("secret".into()),
            user_base: "ou=people,dc=example,dc=org".into(),
            user_filter: "(uid={user})".into(),
            group_base: Some("ou=groups,dc=example,dc=org".into()),
            group_filter: "(member={dn})".into(),
            admin_groups: vec!["mirror-admins".into()],
            operator_groups: vec!["cn=Release Managers,ou=groups,dc=example,dc=org".into()],
            viewer_groups: vec!["staff".into(), "contractors".into()],
            timeout: Duration::from_secs(5),
        }
    }

    #[test]
    fn groups_map_to_the_highest_role() {
        let c = cfg();
        let g = |list: &[&str]| list.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(role_of(&c, &g(&["cn=staff,ou=groups,dc=example,dc=org"])), Some(Role::Viewer));
        assert_eq!(role_of(&c, &g(&["cn=staff,ou=groups,dc=example,dc=org", "CN=Mirror-Admins,OU=Groups,DC=example,DC=org"])), Some(Role::Admin));
        assert_eq!(role_of(&c, &g(&["cn=release managers,ou=groups,dc=example,dc=org"])), Some(Role::Operator));
        assert_eq!(role_of(&c, &g(&["cn=Release Managers,ou=other,dc=example,dc=org"])), None, "a group named by DN is that DN, not any group of the same name");
        assert_eq!(role_of(&c, &g(&["cn=others,ou=groups,dc=example,dc=org", "ou=staff,dc=example,dc=org"])), None);
        assert_eq!(role_of(&c, &[]), None);
    }

    #[test]
    fn what_a_user_types_cannot_change_the_filter() {
        assert_eq!(fill_filter("(uid={user})", "dana", ""), "(uid=dana)");
        assert_eq!(fill_filter("(uid={user})", "*)(uid=*", ""), "(uid=\\2a\\29\\28uid=\\2a)");
        assert_eq!(fill_filter("(|(member={dn})(memberUid={user}))", "dana", "uid=dana,ou=people,dc=example,dc=org"), "(|(member=uid=dana,ou=people,dc=example,dc=org)(memberUid=dana))");
    }

    #[test]
    fn settings_are_checked_and_empty_passwords_never_reach_the_directory() {
        assert!(cfg().validate().is_ok());
        assert!(cfg().describe().contains("cn=search") && !cfg().describe().contains("secret"));
        for change in [
            |c: &mut LdapConfig| c.url = "http://directory".into(),
            |c: &mut LdapConfig| c.user_filter = "(uid=someone)".into(),
            |c: &mut LdapConfig| c.bind_password = None,
            |c: &mut LdapConfig| {
                c.admin_groups.clear();
                c.operator_groups.clear();
                c.viewer_groups.clear();
            },
        ] {
            let mut c = cfg();
            change(&mut c);
            assert!(c.validate().is_err());
        }
        // Nothing listens at this address; an empty password is refused before any connection is made.
        let mut c = cfg();
        c.url = "ldap://127.0.0.1:1".into();
        assert_eq!(c.check("dana", ""), Ok(None));
        assert!(c.check("dana", "something").is_err(), "a directory that cannot be reached is an error, not a refusal");
    }
}
