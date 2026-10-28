//! RBAC permissions compiled into a compact bitset.

use std::fmt;

/// A known `(namespace, action)` permission.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum Permission {
    /// `BATCH_CHANGES` / `READ`.
    BatchChangesRead = 0,
    /// `BATCH_CHANGES` / `WRITE`.
    BatchChangesWrite = 1,
    /// `OWNERSHIP` / `ASSIGN`.
    OwnershipAssign = 2,
    /// `REPO_METADATA` / `WRITE`.
    RepoMetadataWrite = 3,
}

impl Permission {
    /// Every known permission.
    pub const ALL: [Permission; 4] = [
        Permission::BatchChangesRead,
        Permission::BatchChangesWrite,
        Permission::OwnershipAssign,
        Permission::RepoMetadataWrite,
    ];

    /// The stored `(namespace, action)` pair.
    pub const fn pair(self) -> (&'static str, &'static str) {
        match self {
            Permission::BatchChangesRead => ("BATCH_CHANGES", "READ"),
            Permission::BatchChangesWrite => ("BATCH_CHANGES", "WRITE"),
            Permission::OwnershipAssign => ("OWNERSHIP", "ASSIGN"),
            Permission::RepoMetadataWrite => ("REPO_METADATA", "WRITE"),
        }
    }

    /// Looks up a permission by its stored pair; `None` if unknown.
    pub fn from_pair(namespace: &str, action: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|p| p.pair() == (namespace, action))
    }
}

impl fmt::Display for Permission {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (ns, action) = self.pair();
        write!(f, "{ns}#{action}")
    }
}

/// A set of [`Permission`]s as a bitset. `Copy`, one word.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct PermissionSet(u32);

impl PermissionSet {
    /// The empty set.
    pub const EMPTY: Self = Self(0);

    /// Builds a set from stored `(namespace, action)` pairs, ignoring unknown ones.
    pub fn from_pairs<'a>(pairs: impl IntoIterator<Item = (&'a str, &'a str)>) -> Self {
        pairs
            .into_iter()
            .filter_map(|(ns, a)| Permission::from_pair(ns, a))
            .collect()
    }

    /// Whether `p` is in the set.
    pub const fn has(self, p: Permission) -> bool {
        self.0 & (1 << p as u32) != 0
    }

    /// Adds `p`.
    pub fn insert(&mut self, p: Permission) {
        self.0 |= 1 << p as u32;
    }

    /// The permissions in the set.
    pub fn iter(self) -> impl Iterator<Item = Permission> {
        Permission::ALL.into_iter().filter(move |p| self.has(*p))
    }

    /// The raw bits.
    pub const fn bits(self) -> u32 {
        self.0
    }
}

impl FromIterator<Permission> for PermissionSet {
    fn from_iter<I: IntoIterator<Item = Permission>>(iter: I) -> Self {
        let mut s = Self::EMPTY;
        for p in iter {
            s.insert(p);
        }
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bitset() {
        let s = PermissionSet::from_pairs([("BATCH_CHANGES", "WRITE"), ("X", "Y")]);
        assert!(s.has(Permission::BatchChangesWrite));
        assert!(!s.has(Permission::BatchChangesRead));
        assert_eq!(s.iter().count(), 1);
        for p in Permission::ALL {
            let (ns, a) = p.pair();
            assert_eq!(Permission::from_pair(ns, a), Some(p));
        }
        assert_eq!(Permission::OwnershipAssign.to_string(), "OWNERSHIP#ASSIGN");
    }
}
