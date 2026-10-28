//! Server roles.

use std::fmt;
use std::str::FromStr;

/// A role a process can run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Role {
    /// Stateless frontend: UI, APIs, auth.
    Web,
    /// Repo node: git storage, index and search.
    Repo,
    /// Background jobs and singleton tasks.
    Worker,
    /// Blob store.
    Blob,
}

impl Role {
    /// Every role, in start order.
    pub const ALL: [Role; 4] = [Role::Web, Role::Repo, Role::Worker, Role::Blob];

    /// The role's name on the command line.
    pub fn as_str(self) -> &'static str {
        match self {
            Role::Web => "web",
            Role::Repo => "repo",
            Role::Worker => "worker",
            Role::Blob => "blob",
        }
    }
}

impl fmt::Display for Role {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A non-empty, de-duplicated set of roles, parsed from `web,worker` or `all`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoleSet(Vec<Role>);

impl RoleSet {
    /// Every role.
    pub fn all() -> Self {
        Self(Role::ALL.to_vec())
    }

    /// Whether `role` is in the set.
    pub fn contains(&self, role: Role) -> bool {
        self.0.contains(&role)
    }

    /// The roles, in start order.
    pub fn iter(&self) -> impl Iterator<Item = Role> + '_ {
        self.0.iter().copied()
    }

    /// Whether the roles need the database schema to be current before starting.
    pub fn needs_current_schema(&self) -> bool {
        self.contains(Role::Web) || self.contains(Role::Worker) || self.contains(Role::Repo)
    }
}

impl fmt::Display for RoleSet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let names: Vec<_> = self.0.iter().map(|r| r.as_str()).collect();
        f.write_str(&names.join(","))
    }
}

impl FromStr for RoleSet {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let mut roles = Vec::new();
        for name in s.split(',').map(str::trim).filter(|n| !n.is_empty()) {
            match name {
                "all" => roles.extend(Role::ALL),
                "web" => roles.push(Role::Web),
                "repo" => roles.push(Role::Repo),
                "worker" => roles.push(Role::Worker),
                "blob" => roles.push(Role::Blob),
                other => {
                    return Err(format!(
                        "unknown role {other:?} (expected web, repo, worker, blob or all)"
                    ));
                }
            }
        }
        roles.sort();
        roles.dedup();
        if roles.is_empty() {
            return Err("no roles given".to_owned());
        }
        Ok(Self(roles))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_role_lists() {
        assert_eq!("all".parse::<RoleSet>().unwrap(), RoleSet::all());
        let set: RoleSet = "worker, web,web".parse().unwrap();
        assert_eq!(set.to_string(), "web,worker");
        assert!(set.needs_current_schema());
        assert!(
            "repo,blob"
                .parse::<RoleSet>()
                .unwrap()
                .needs_current_schema()
        );
        assert!(!"blob".parse::<RoleSet>().unwrap().needs_current_schema());
        assert!("".parse::<RoleSet>().is_err());
        assert!("web,frontend".parse::<RoleSet>().is_err());
    }
}
