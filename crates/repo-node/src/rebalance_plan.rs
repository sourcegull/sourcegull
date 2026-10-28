//! The rebalancing planner: which repos to move when repo nodes are added or removed. Pure
//! functions over the node list, the heartbeats, the assignments and the open moves; the pass that
//! feeds them from the database is in [`super::rebalance`].

use std::collections::{HashMap, HashSet};

use sourcegull_db::{MoveKind, NewRepoMove, OpenRepoMove, move_state};
use sourcegull_rpc::Router;
use sourcegull_types::RepoId;

pub(crate) use sourcegull_db::{live_repo_nodes as live_nodes, unsettled_repo_nodes as unsettled};

/// What one planning pass decides.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Plan {
    /// Moves to queue.
    pub(crate) queue: Vec<NewRepoMove>,
    /// Open moves that are no longer wanted.
    pub(crate) cancel: Vec<RepoId>,
    /// Misplaced repos left for a later pass (over `max_new`).
    pub(crate) deferred: usize,
}

/// Plans moves: every repo whose assignment differs from its owner (its pin in `router` if
/// any, else its hash owner over the configured nodes) moves there, by copy when its current node is alive and by re-clone otherwise. Open
/// moves to the right node are kept (a failed one waits for a site admin's retry); moves to
/// another node, of repos that are already in place or gone, are cancelled. At most `max_new`
/// moves are queued per pass.
pub(crate) fn plan(
    router: &Router,
    live: &HashSet<String>,
    assignments: &[(RepoId, String)],
    open: &[OpenRepoMove],
    max_new: usize,
) -> Plan {
    let open: HashMap<RepoId, &OpenRepoMove> = open.iter().map(|m| (m.repo_id, m)).collect();
    let mut out = Plan::default();
    let mut seen: HashSet<RepoId> = HashSet::with_capacity(assignments.len());
    for (repo, shard) in assignments {
        seen.insert(*repo);
        let Some(owner) = router.node_for(*repo) else {
            return Plan::default();
        };
        if let Some(m) = open.get(repo) {
            if m.state == move_state::FLIPPED {
                // Waiting for the old copy's removal; a further move starts after.
                continue;
            }
            if shard == owner {
                out.cancel.push(*repo);
                continue;
            }
            if m.to_node == owner && &m.from_node == shard {
                continue;
            }
            out.cancel.push(*repo);
        } else if shard == owner {
            continue;
        }
        if out.queue.len() >= max_new {
            out.deferred += 1;
            continue;
        }
        let kind = if live.contains(shard) {
            MoveKind::Copy
        } else {
            MoveKind::Reclone
        };
        out.queue.push(NewRepoMove {
            repo_id: *repo,
            from_node: shard.clone(),
            to_node: owner.to_owned(),
            kind,
        });
    }
    // Moves of repos deleted or unassigned since.
    for (repo, m) in &open {
        if !seen.contains(repo) && m.state != move_state::FLIPPED {
            out.cancel.push(*repo);
        }
    }
    out.cancel.sort_unstable();
    out
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use chrono::{DateTime, Utc};
    use sourcegull_db::RepoNodeHeartbeat;

    use super::*;

    fn nodes(n: usize) -> Vec<String> {
        (0..n).map(|i| format!("http://repo-{i}:3178")).collect()
    }

    fn hb(addr: &str, list: &[String], age_secs: i64, now: DateTime<Utc>) -> RepoNodeHeartbeat {
        let mut list = list.to_vec();
        list.sort();
        RepoNodeHeartbeat {
            addr: addr.to_owned(),
            nodes: list,
            started_at: now,
            seen_at: now - chrono::Duration::seconds(age_secs),
        }
    }

    fn open(repo: i32, from: &str, to: &str, state: &str) -> OpenRepoMove {
        OpenRepoMove {
            repo_id: RepoId(repo),
            from_node: from.to_owned(),
            to_node: to.to_owned(),
            state: state.to_owned(),
        }
    }

    #[test]
    fn settled_only_when_every_node_is_alive_with_the_same_list() {
        let now = Utc::now();
        let four = nodes(4);
        let three = nodes(3);
        let mut beats: Vec<RepoNodeHeartbeat> = four.iter().map(|n| hb(n, &four, 1, now)).collect();
        let live = live_nodes(&beats, now, Duration::from_secs(60));
        assert!(unsettled(&four, &beats, &live).is_empty());

        // repo-0 still runs with the old list (rolling update): not settled.
        beats[0] = hb(&four[0], &three, 1, now);
        let live = live_nodes(&beats, now, Duration::from_secs(60));
        assert_eq!(unsettled(&four, &beats, &live), vec![four[0].clone()]);

        // repo-3 stopped sending heartbeats.
        beats[0] = hb(&four[0], &four, 1, now);
        beats[3] = hb(&four[3], &four, 600, now);
        let live = live_nodes(&beats, now, Duration::from_secs(60));
        assert!(!live.contains(&four[3]));
        assert_eq!(unsettled(&four, &beats, &live), vec![four[3].clone()]);

        // A node that never sent one.
        beats.pop();
        assert_eq!(unsettled(&four, &beats, &live), vec![four[3].clone()]);
    }

    /// Assignments as the hash over `n` nodes made them.
    fn assigned(router: &Router, repos: i32) -> Vec<(RepoId, String)> {
        (1..=repos)
            .map(|i| {
                (
                    RepoId(i),
                    router.hashed_node_for(RepoId(i)).unwrap().to_owned(),
                )
            })
            .collect()
    }

    #[test]
    fn scale_out_moves_about_one_nth_to_the_new_node_by_copy() {
        let before = Router::new(nodes(3));
        let after = Router::new(nodes(4));
        let assignments = assigned(&before, 4_000);
        let live: HashSet<String> = nodes(4).into_iter().collect();
        let p = plan(&after, &live, &assignments, &[], usize::MAX);
        assert!(p.cancel.is_empty());
        assert_eq!(p.deferred, 0);
        let frac = p.queue.len() as f64 / 4_000.0;
        assert!((0.2..0.3).contains(&frac), "moved {frac}");
        for m in &p.queue {
            assert_eq!(m.to_node, "http://repo-3:3178");
            assert_eq!(m.kind, MoveKind::Copy);
            assert_eq!(
                Some(m.from_node.as_str()),
                before.hashed_node_for(m.repo_id)
            );
        }
        // Planned moves are kept on the next pass, and nothing else is added.
        let open_moves: Vec<OpenRepoMove> = p
            .queue
            .iter()
            .map(|m| {
                open(
                    m.repo_id.get(),
                    &m.from_node,
                    &m.to_node,
                    move_state::COPYING,
                )
            })
            .collect();
        let again = plan(&after, &live, &assignments, &open_moves, usize::MAX);
        assert_eq!(again, Plan::default());
    }

    #[test]
    fn removed_node_repos_are_recloned_unless_it_is_still_alive() {
        let before = Router::new(nodes(4));
        let after = Router::new(nodes(3));
        let assignments = assigned(&before, 2_000);
        let gone = "http://repo-3:3178";
        let on_gone = assignments.iter().filter(|(_, s)| s == gone).count();
        assert!(on_gone > 0);

        // The removed node is down: re-clone from the code host.
        let live: HashSet<String> = nodes(3).into_iter().collect();
        let p = plan(&after, &live, &assignments, &[], usize::MAX);
        assert_eq!(p.queue.len(), on_gone);
        assert!(
            p.queue
                .iter()
                .all(|m| m.kind == MoveKind::Reclone && m.from_node == gone)
        );

        // Drained while still running: copy.
        let live: HashSet<String> = nodes(4).into_iter().collect();
        let p = plan(&after, &live, &assignments, &[], usize::MAX);
        assert!(p.queue.iter().all(|m| m.kind == MoveKind::Copy));
    }

    #[test]
    fn stale_moves_are_cancelled_and_replanned() {
        let router = Router::new(nodes(2));
        let live: HashSet<String> = nodes(3).into_iter().collect();
        let owner = |i: i32| router.hashed_node_for(RepoId(i)).unwrap().to_owned();
        let other = |i: i32| nodes(2).into_iter().find(|n| *n != owner(i)).unwrap();
        let assignments = vec![
            (RepoId(1), owner(1)), // in place, with a leftover move: cancel
            (RepoId(2), other(2)), // move towards a node no longer listed: cancel + new
            (RepoId(3), other(3)), // flipped move pending cleanup: leave alone
            (RepoId(4), other(4)), // failed move to the right node: keep
        ];
        let open_moves = vec![
            open(1, &other(1), &owner(1), move_state::QUEUED),
            open(2, &other(2), "http://repo-2:3178", move_state::COPYING),
            open(3, &owner(3), &other(3), move_state::FLIPPED),
            open(4, &other(4), &owner(4), move_state::FAILED),
            open(
                9,
                "http://repo-0:3178",
                "http://repo-1:3178",
                move_state::QUEUED,
            ), // deleted
        ];
        let p = plan(&router, &live, &assignments, &open_moves, usize::MAX);
        assert_eq!(p.cancel, vec![RepoId(1), RepoId(2), RepoId(9)]);
        assert_eq!(
            p.queue,
            vec![NewRepoMove {
                repo_id: RepoId(2),
                from_node: other(2),
                to_node: owner(2),
                kind: MoveKind::Copy,
            }]
        );
    }

    #[test]
    fn pinned_repos_move_to_their_pin_and_back_when_unpinned() {
        let router = Router::new(nodes(3));
        let live: HashSet<String> = nodes(3).into_iter().collect();
        let assignments = assigned(&router, 20);
        let owner = router.hashed_node_for(RepoId(5)).unwrap().to_owned();
        let pin = nodes(3).into_iter().find(|n| *n != owner).unwrap();
        let mut pinned = router.clone();
        pinned.pin(RepoId(5), pin.clone());
        let p = plan(&pinned, &live, &assignments, &[], usize::MAX);
        assert_eq!(
            p.queue,
            vec![NewRepoMove {
                repo_id: RepoId(5),
                from_node: owner.clone(),
                to_node: pin.clone(),
                kind: MoveKind::Copy,
            }]
        );
        assert!(p.cancel.is_empty());

        // Once there, the pin keeps it in place.
        let mut moved = assignments.clone();
        moved[4].1 = pin.clone();
        assert_eq!(
            plan(&pinned, &live, &moved, &[], usize::MAX),
            Plan::default()
        );

        // Unpinned: back to the hash owner; a pending move to the pin is cancelled.
        let p = plan(&router, &live, &moved, &[], usize::MAX);
        assert_eq!(p.queue.len(), 1);
        assert_eq!(
            (p.queue[0].from_node.as_str(), p.queue[0].to_node.as_str()),
            (pin.as_str(), owner.as_str())
        );
        let pending = [open(5, &owner, &pin, move_state::QUEUED)];
        let p = plan(&router, &live, &assignments, &pending, usize::MAX);
        assert_eq!(p.cancel, vec![RepoId(5)]);
        assert!(p.queue.is_empty());
    }

    #[test]
    fn new_moves_are_capped_per_pass() {
        let before = Router::new(nodes(1));
        let after = Router::new(nodes(2));
        let assignments = assigned(&before, 1_000);
        let live: HashSet<String> = nodes(2).into_iter().collect();
        let all = plan(&after, &live, &assignments, &[], usize::MAX)
            .queue
            .len();
        let p = plan(&after, &live, &assignments, &[], 10);
        assert_eq!(p.queue.len(), 10);
        assert_eq!(p.deferred, all - 10);
        // No nodes configured: nothing to do.
        let p = plan(&Router::default(), &live, &assignments, &[], 10);
        assert_eq!(p, Plan::default());
    }
}
