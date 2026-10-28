//! Repo → node routing with rendezvous (highest random weight) hashing.
//!
//! Every node gets a score `h(node, repo_id)`; the repo lives on the node with
//! the highest score. Adding or removing one of N nodes moves only ~1/N of the
//! repos, unlike `hash mod N`. The hash is fixed (FNV-1a + splitmix64) so all
//! processes and versions agree on the assignment.
//!
//! The computed assignment is persisted in `repo.shard` by the caller; pinned
//! repos (very large monorepos, rebalancing in progress) are expressed as
//! [`Router::pin`] overrides that win over the hash.

use std::collections::HashMap;

use sourcegull_types::RepoId;

/// Maps repos to node addresses.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Router {
    nodes: Vec<Node>,
    pins: HashMap<RepoId, String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Node {
    addr: String,
    seed: u64,
}

impl Router {
    /// Creates a router over `nodes` (addresses such as `http://repo-0.repo:3178`).
    /// Duplicates are ignored; order does not matter.
    pub fn new<I, S>(nodes: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let mut out: Vec<Node> = Vec::new();
        for addr in nodes {
            let addr = addr.into();
            if !out.iter().any(|n| n.addr == addr) {
                out.push(Node {
                    seed: fnv1a(addr.as_bytes()),
                    addr,
                });
            }
        }
        out.sort_by(|a, b| a.addr.cmp(&b.addr));
        Self {
            nodes: out,
            pins: HashMap::new(),
        }
    }

    /// Pins `repo` to `addr`, overriding the hash (e.g. from `repo.shard`).
    pub fn pin(&mut self, repo: RepoId, addr: impl Into<String>) {
        self.pins.insert(repo, addr.into());
    }

    /// Removes a pin.
    pub fn unpin(&mut self, repo: RepoId) {
        self.pins.remove(&repo);
    }

    /// The configured node addresses, sorted.
    pub fn nodes(&self) -> impl Iterator<Item = &str> {
        self.nodes.iter().map(|n| n.addr.as_str())
    }

    /// Whether no nodes are configured.
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    /// The node that owns `repo`: its pin if any, else the highest-scoring node.
    /// `None` only when no nodes are configured and the repo is not pinned.
    pub fn node_for(&self, repo: RepoId) -> Option<&str> {
        if let Some(pinned) = self.pins.get(&repo) {
            return Some(pinned);
        }
        self.hashed_node_for(repo)
    }

    /// The hash assignment for `repo`, ignoring pins. This is the value to
    /// persist in `repo.shard` for a newly discovered repo.
    pub fn hashed_node_for(&self, repo: RepoId) -> Option<&str> {
        self.nodes
            .iter()
            .max_by_key(|n| score(n.seed, repo))
            .map(|n| n.addr.as_str())
    }

    /// All nodes ordered by preference for `repo` (owner first). Useful for
    /// picking a destination when the owner is being drained.
    pub fn ranked(&self, repo: RepoId) -> Vec<&str> {
        let mut scored: Vec<(u64, &str)> = self
            .nodes
            .iter()
            .map(|n| (score(n.seed, repo), n.addr.as_str()))
            .collect();
        scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(b.1)));
        scored.into_iter().map(|(_, a)| a).collect()
    }

    /// Groups repos by owning node, preserving input order within a group.
    /// Repos with no owner (empty router) are dropped.
    pub fn group_by_node(
        &self,
        repos: impl IntoIterator<Item = RepoId>,
    ) -> HashMap<&str, Vec<RepoId>> {
        let mut out: HashMap<&str, Vec<RepoId>> = HashMap::new();
        for repo in repos {
            if let Some(node) = self.node_for(repo) {
                out.entry(node).or_default().push(repo);
            }
        }
        out
    }
}

fn score(seed: u64, repo: RepoId) -> u64 {
    splitmix64(seed ^ splitmix64(repo.get() as u32 as u64))
}

/// 64-bit FNV-1a.
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    h
}

/// splitmix64 finalizer: a fast, well-distributed 64-bit mix.
fn splitmix64(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9e37_79b9_7f4a_7c15);
    x = (x ^ (x >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    x ^ (x >> 31)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nodes(n: usize) -> Vec<String> {
        (0..n).map(|i| format!("http://repo-{i}:3178")).collect()
    }

    #[test]
    fn empty_router_has_no_owner() {
        let r = Router::default();
        assert!(r.is_empty());
        assert_eq!(r.node_for(RepoId(1)), None);
        assert!(r.ranked(RepoId(1)).is_empty());
    }

    #[test]
    fn assignment_is_deterministic_and_order_independent() {
        let a = Router::new(nodes(5));
        let mut reversed = nodes(5);
        reversed.reverse();
        reversed.push(nodes(1)[0].clone()); // duplicate is ignored
        let b = Router::new(reversed);
        assert_eq!(a, b);
        for id in 0..1000 {
            assert_eq!(a.node_for(RepoId(id)), b.node_for(RepoId(id)));
        }
        // A fixed value guards against accidental hash changes, which would
        // reshuffle every repo on upgrade.
        let r = Router::new(["a", "b", "c"]);
        let got: Vec<&str> = (1..=6).map(|i| r.node_for(RepoId(i)).unwrap()).collect();
        // Computed independently (Python reimplementation of fnv1a/splitmix64).
        assert_eq!(got, ["c", "c", "c", "b", "a", "a"]);
    }

    #[test]
    fn distribution_is_roughly_uniform() {
        let r = Router::new(nodes(8));
        let counts = r.group_by_node((0..80_000).map(RepoId));
        assert_eq!(counts.len(), 8);
        for (node, repos) in counts {
            let n = repos.len();
            assert!((9_000..11_000).contains(&n), "{node} got {n}");
        }
    }

    #[test]
    fn adding_a_node_moves_about_one_nth() {
        let before = Router::new(nodes(9));
        let after = Router::new(nodes(10));
        let total = 50_000;
        let mut moved = 0;
        for id in 0..total {
            let (b, a) = (
                before.node_for(RepoId(id)).unwrap(),
                after.node_for(RepoId(id)).unwrap(),
            );
            if a != b {
                // Repos only ever move to the new node.
                assert_eq!(a, "http://repo-9:3178");
                moved += 1;
            }
        }
        let frac = moved as f64 / total as f64;
        assert!((0.08..0.12).contains(&frac), "moved {frac}");
    }

    #[test]
    fn removing_a_node_only_moves_its_repos() {
        let before = Router::new(nodes(6));
        let mut remaining = nodes(6);
        let removed = remaining.remove(2);
        let after = Router::new(remaining);
        for id in 0..10_000 {
            let b = before.node_for(RepoId(id)).unwrap();
            let a = after.node_for(RepoId(id)).unwrap();
            if b != removed {
                assert_eq!(a, b);
            }
        }
    }

    #[test]
    fn pins_override_hash_and_ranked_starts_with_owner() {
        let mut r = Router::new(nodes(4));
        let id = RepoId(42);
        let owner = r.hashed_node_for(id).unwrap().to_owned();
        let ranked = r.ranked(id);
        assert_eq!(ranked.len(), 4);
        assert_eq!(ranked[0], owner);
        r.pin(id, "http://big-node:3178");
        assert_eq!(r.node_for(id), Some("http://big-node:3178"));
        assert_eq!(r.hashed_node_for(id), Some(owner.as_str()));
        r.unpin(id);
        assert_eq!(r.node_for(id), Some(owner.as_str()));
    }
}
