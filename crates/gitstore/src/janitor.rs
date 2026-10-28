//! Janitor operations: maintenance, gc, repack, prune,
//! commit-graph, corruption checks, temp-dir cleanup and disk-pressure
//! eviction. These are library functions; the repo role schedules them.

use std::path::Path;
use std::time::{Duration, Instant, SystemTime};

use tokio_util::sync::CancellationToken;

use crate::command::GitCommand;
use crate::error::{GitError, Result};
use crate::layout::Layout;
use crate::sync::dir_size;

/// A janitor task.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum JanitorTask {
    /// Incremental `git maintenance run` (commit-graph, loose-objects,
    /// incremental-repack) followed by `git pack-refs`. Cheap; the default
    /// periodic task.
    Maintenance,
    /// Full `git gc` (repack everything, prune old unreachable objects).
    Gc,
    /// `git repack -a -d -l`: consolidate packs without pruning.
    Repack,
    /// `git prune` of unreachable loose objects older than two weeks.
    Prune,
    /// Write the commit-graph with changed-path Bloom filters.
    CommitGraph,
    /// Connectivity check; a failure marks the repo corrupt (re-clone it).
    Fsck,
}

/// Result of [`run_task`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct JanitorOutcome {
    /// Set by [`JanitorTask::Fsck`] when the check failed.
    pub corrupt: bool,
    /// Tail of git's output.
    pub output: String,
    /// Wall time.
    pub duration: Duration,
    /// Repo size after the task.
    pub size_bytes_after: u64,
}

impl JanitorTask {
    fn command(self, git_bin: &Path, repo_dir: &Path) -> GitCommand {
        let base = |op: &str| GitCommand::new(git_bin, op).repo(repo_dir);
        match self {
            JanitorTask::Maintenance => {
                let mut cmd = base("maintenance").args([
                    "run",
                    "--task=commit-graph",
                    "--task=loose-objects",
                    "--quiet",
                ]);
                // incremental-repack fails ("no pack files to index") on a
                // repo whose objects are all loose, e.g. after small fetches.
                if fragmentation(repo_dir).is_ok_and(|f| f.packs > 0) {
                    cmd = cmd.arg("--task=incremental-repack");
                }
                cmd
            }
            JanitorTask::Gc => base("gc").arg("--quiet"),
            JanitorTask::Repack => base("repack").args(["-a", "-d", "-l", "-q"]),
            JanitorTask::Prune => base("prune").arg("--expire=2.weeks.ago"),
            JanitorTask::CommitGraph => base("commit-graph").args([
                "write",
                "--reachable",
                "--changed-paths",
                "--split",
                "--no-progress",
            ]),
            JanitorTask::Fsck => {
                base("fsck").args(["--connectivity-only", "--no-dangling", "--no-progress"])
            }
        }
    }
}

/// Runs `task` on the bare repo at `repo_dir`.
pub async fn run_task(
    git_bin: &Path,
    repo_dir: &Path,
    task: JanitorTask,
    timeout: Duration,
    cancel: CancellationToken,
) -> Result<JanitorOutcome> {
    if !repo_dir.exists() {
        return Err(GitError::PathNotFound(repo_dir.display().to_string()));
    }
    let start = Instant::now();
    let result = task
        .command(git_bin, repo_dir)
        .timeout(timeout)
        .cancel_on(cancel)
        .run()
        .await;
    if task == JanitorTask::Maintenance && result.is_ok() {
        // The `pack-refs` maintenance task needs git 2.42; the runtime image
        // (Debian bookworm) has 2.39, so pack refs with the plain command.
        GitCommand::new(git_bin, "pack-refs")
            .repo(repo_dir)
            .args(["--all", "--prune"])
            .timeout(timeout)
            .run()
            .await?;
    }
    let (corrupt, output) = match (task, result) {
        (_, Ok(out)) => (false, out.stderr_tail),
        (JanitorTask::Fsck, Err(GitError::Command { stderr, .. })) => (true, stderr),
        (_, Err(e)) => return Err(e),
    };
    let dir = repo_dir.to_owned();
    let size_bytes_after = tokio::task::spawn_blocking(move || dir_size(&dir))
        .await
        .map_err(|e| GitError::Io(std::io::Error::other(e)))??;
    Ok(JanitorOutcome {
        corrupt,
        output,
        duration: start.elapsed(),
        size_bytes_after,
    })
}

/// How fragmented a repo's object store is.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Fragmentation {
    /// Number of pack files.
    pub packs: u32,
    /// Estimated loose objects (sampled from one fan-out directory, as `git gc --auto` does).
    pub loose_objects: u64,
    /// Whether a commit-graph exists.
    pub has_commit_graph: bool,
}

/// Measures fragmentation by reading the object directory (no git process).
pub fn fragmentation(repo_dir: &Path) -> std::io::Result<Fragmentation> {
    let objects = repo_dir.join("objects");
    let count = |dir: &Path, pred: &dyn Fn(&str) -> bool| -> std::io::Result<u64> {
        match std::fs::read_dir(dir) {
            Ok(rd) => {
                let mut n = 0;
                for e in rd {
                    if pred(&e?.file_name().to_string_lossy()) {
                        n += 1;
                    }
                }
                Ok(n)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(0),
            Err(e) => Err(e),
        }
    };
    let packs = count(&objects.join("pack"), &|n| n.ends_with(".pack"))? as u32;
    let sample = count(&objects.join("17"), &|n| n.len() == 38)?;
    let info = objects.join("info");
    let has_commit_graph =
        info.join("commit-graph").exists() || info.join("commit-graphs").exists();
    Ok(Fragmentation {
        packs,
        loose_objects: sample * 256,
        has_commit_graph,
    })
}

/// When maintenance is due (defaults match git's `gc.auto` and `gc.autoPackLimit`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MaintenanceThresholds {
    /// Loose objects above which incremental maintenance runs.
    pub max_loose_objects: u64,
    /// Packs above which a full gc consolidates them.
    pub max_packs: u32,
}

impl Default for MaintenanceThresholds {
    fn default() -> Self {
        Self {
            max_loose_objects: 6700,
            max_packs: 50,
        }
    }
}

/// The task a repo needs, if any: gc when packs pile up, incremental
/// maintenance when loose objects do, a commit-graph when it is missing.
pub fn task_needed(f: &Fragmentation, t: &MaintenanceThresholds) -> Option<JanitorTask> {
    if f.packs > t.max_packs {
        Some(JanitorTask::Gc)
    } else if f.loose_objects > t.max_loose_objects {
        Some(JanitorTask::Maintenance)
    } else if !f.has_commit_graph && f.packs + (f.loose_objects > 0) as u32 > 0 {
        Some(JanitorTask::CommitGraph)
    } else {
        None
    }
}

/// Removes entries in the temp dir older than `older_than` (abandoned clones
/// from a crash). The askpass helper is kept. Returns how many were removed.
pub fn remove_stale_tmp(layout: &Layout, older_than: Duration) -> std::io::Result<usize> {
    let now = SystemTime::now();
    let mut removed = 0;
    let rd = match std::fs::read_dir(layout.tmp_dir()) {
        Ok(rd) => rd,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(e),
    };
    for entry in rd {
        let entry = entry?;
        if entry.file_name() == "askpass.sh" {
            continue;
        }
        let meta = entry.metadata()?;
        let age = now
            .duration_since(meta.modified()?)
            .unwrap_or(Duration::ZERO);
        if age < older_than {
            continue;
        }
        let path = entry.path();
        let res = if meta.is_dir() {
            std::fs::remove_dir_all(&path)
        } else {
            std::fs::remove_file(&path)
        };
        match res {
            Ok(()) => removed += 1,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
    }
    Ok(removed)
}

/// Space on the filesystem holding a path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiskUsage {
    /// Total bytes.
    pub total: u64,
    /// Bytes available to unprivileged users.
    pub available: u64,
}

impl DiskUsage {
    /// Available fraction in `[0, 1]`.
    pub fn free_ratio(&self) -> f64 {
        if self.total == 0 {
            0.0
        } else {
            self.available as f64 / self.total as f64
        }
    }
}

/// Reads filesystem usage for `path` (`statvfs`).
pub fn disk_usage(path: &Path) -> std::io::Result<DiskUsage> {
    let s = rustix::fs::statvfs(path)?;
    Ok(DiskUsage {
        total: s.f_blocks * s.f_frsize,
        available: s.f_bavail * s.f_frsize,
    })
}

/// Free-space watermarks. Each one is a fraction of the filesystem capped by an absolute
/// size: the threshold is the smaller of the two, so a large disk is not held to a ratio of
/// its size (10% of 1 TiB is 100 GiB, far more than clones need) and a small disk still keeps
/// a share of itself free. Below the watermarks the janitor cleans up and new clones pause.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DiskWatermarks {
    /// New clones pause when free space is below this fraction of the filesystem…
    pub pause_clones_below: f64,
    /// …and also below this many bytes.
    pub pause_clones_below_bytes: u64,
    /// Caches are evicted when free space is below this fraction…
    pub evict_below: f64,
    /// …and also below this many bytes. Eviction frees until free space is 20% above the
    /// clone pause threshold, so clones resume.
    pub evict_below_bytes: u64,
}

const GIB: u64 = 1 << 30;

impl Default for DiskWatermarks {
    fn default() -> Self {
        Self {
            pause_clones_below: 0.10,
            pause_clones_below_bytes: 20 * GIB,
            evict_below: 0.07,
            evict_below_bytes: 10 * GIB,
        }
    }
}

impl DiskWatermarks {
    fn threshold(total: u64, ratio: f64, bytes: u64) -> u64 {
        ((total as f64 * ratio.clamp(0.0, 1.0)) as u64).min(bytes)
    }

    /// Free bytes below which clones pause on a filesystem of `total` bytes.
    pub fn pause_clones_threshold(&self, total: u64) -> u64 {
        Self::threshold(
            total,
            self.pause_clones_below,
            self.pause_clones_below_bytes,
        )
    }

    /// Free bytes below which caches are evicted on a filesystem of `total` bytes.
    pub fn evict_threshold(&self, total: u64) -> u64 {
        Self::threshold(total, self.evict_below, self.evict_below_bytes)
    }

    /// Free bytes eviction aims for: 20% above the clone pause threshold (and at least the
    /// eviction threshold).
    pub fn evict_target(&self, total: u64) -> u64 {
        let pause = self.pause_clones_threshold(total);
        (pause + pause / 5)
            .max(self.evict_threshold(total))
            .min(total)
    }
}

/// What to do about disk space.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiskPressure {
    /// Refuse new clones.
    pub pause_clones: bool,
    /// Bytes to free through eviction (0 if none).
    pub bytes_to_free: u64,
}

/// Applies `w` to `usage`.
pub fn assess_disk(usage: DiskUsage, w: &DiskWatermarks) -> DiskPressure {
    let bytes_to_free = if usage.available < w.evict_threshold(usage.total) {
        w.evict_target(usage.total).saturating_sub(usage.available)
    } else {
        0
    };
    DiskPressure {
        pause_clones: usage.available < w.pause_clones_threshold(usage.total),
        bytes_to_free,
    }
}

/// Something that can be evicted to free space (a cache entry, an index shard
/// of an inactive repo, …).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EvictionCandidate {
    /// Opaque key understood by the hook that produced it.
    pub key: String,
    /// Bytes freed by evicting it.
    pub size_bytes: u64,
    /// Last use; least recently used goes first.
    pub last_used: SystemTime,
}

/// A source of evictable data, registered by subsystems that keep caches on
/// the repo node (highlighting, symbols, index shards).
pub trait EvictionHook: Send + Sync {
    /// A short name for logs.
    fn name(&self) -> &str;
    /// Current candidates.
    fn candidates(&self) -> Vec<EvictionCandidate>;
    /// Evicts the given keys; returns bytes actually freed.
    fn evict(&self, keys: &[String]) -> u64;
}

/// Picks least-recently-used candidates until their sizes add up to
/// `bytes_to_free` (or candidates run out). Pure; ties broken by key.
pub fn plan_eviction(
    mut candidates: Vec<EvictionCandidate>,
    bytes_to_free: u64,
) -> Vec<EvictionCandidate> {
    candidates.sort_by(|a, b| {
        a.last_used
            .cmp(&b.last_used)
            .then_with(|| a.key.cmp(&b.key))
    });
    let mut freed = 0u64;
    let mut out = Vec::new();
    for c in candidates {
        if freed >= bytes_to_free {
            break;
        }
        freed += c.size_bytes;
        out.push(c);
    }
    out
}

/// Evicts across `hooks` in global LRU order until `bytes_to_free` is
/// reached. Returns bytes freed.
pub fn run_eviction(hooks: &[&dyn EvictionHook], bytes_to_free: u64) -> u64 {
    if bytes_to_free == 0 {
        return 0;
    }
    let mut all = Vec::new();
    for (i, h) in hooks.iter().enumerate() {
        for mut c in h.candidates() {
            c.key = format!("{i}\u{0}{}", c.key);
            all.push(c);
        }
    }
    let plan = plan_eviction(all, bytes_to_free);
    let mut per_hook: Vec<Vec<String>> = vec![Vec::new(); hooks.len()];
    for c in plan {
        if let Some((i, key)) = c.key.split_once('\u{0}') {
            if let Ok(i) = i.parse::<usize>() {
                per_hook[i].push(key.to_owned());
            }
        }
    }
    let mut freed = 0;
    for (hook, keys) in hooks.iter().zip(per_hook) {
        if !keys.is_empty() {
            let n = hook.evict(&keys);
            tracing::info!(
                hook = hook.name(),
                entries = keys.len(),
                bytes = n,
                "evicted"
            );
            freed += n;
        }
    }
    freed
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    #[test]
    fn task_needed_follows_thresholds() {
        let t = MaintenanceThresholds::default();
        let f = |packs, loose, cg| Fragmentation {
            packs,
            loose_objects: loose,
            has_commit_graph: cg,
        };
        assert_eq!(task_needed(&f(60, 0, true), &t), Some(JanitorTask::Gc));
        assert_eq!(
            task_needed(&f(3, 10_000, true), &t),
            Some(JanitorTask::Maintenance)
        );
        assert_eq!(
            task_needed(&f(1, 0, false), &t),
            Some(JanitorTask::CommitGraph)
        );
        assert_eq!(task_needed(&f(1, 256, true), &t), None);
        assert_eq!(task_needed(&f(0, 0, false), &t), None); // empty repo
    }

    #[test]
    fn disk_pressure() {
        let w = DiskWatermarks::default();
        let gb = 1u64 << 30;
        let usage = |total, avail| DiskUsage {
            total: total * gb,
            available: avail * gb,
        };
        let calm = DiskPressure {
            pause_clones: false,
            bytes_to_free: 0,
        };
        // Small disk (100 GiB): the ratio applies (pause below 10 GiB, evict below 7 GiB).
        assert_eq!(assess_disk(usage(100, 50), &w), calm);
        let p = assess_disk(usage(100, 8), &w);
        assert!(p.pause_clones);
        assert_eq!(p.bytes_to_free, 0);
        let p = assess_disk(usage(100, 5), &w);
        assert!(p.pause_clones);
        // Target 12 GiB (pause threshold + 20%) minus 5 GiB available.
        assert!((p.bytes_to_free as i64 - (7 * gb) as i64).abs() < (gb / 100) as i64);

        // Large disk (1007 GiB, 68.8 GiB free, 93% used): fine, the 20 GiB cap applies.
        let big = DiskUsage {
            total: 1007 * gb,
            available: 68 * gb + 800 * (gb >> 10),
        };
        assert_eq!(assess_disk(big, &w), calm);
        assert_eq!(w.pause_clones_threshold(1007 * gb), 20 * gb);
        assert_eq!(w.evict_threshold(1007 * gb), 10 * gb);
        assert_eq!(assess_disk(usage(1007, 25), &w), calm);
        let p = assess_disk(usage(1007, 15), &w);
        assert!(p.pause_clones);
        assert_eq!(p.bytes_to_free, 0);
        let p = assess_disk(usage(1007, 4), &w);
        assert!(p.pause_clones);
        assert_eq!(p.bytes_to_free, 20 * gb);

        // Configured: ratio only (no cap) applies the fractions alone.
        let ratio_only = DiskWatermarks {
            pause_clones_below_bytes: u64::MAX,
            evict_below_bytes: u64::MAX,
            ..w
        };
        assert!(assess_disk(usage(1007, 68), &ratio_only).pause_clones);
        // Turned off: 0 never pauses or evicts.
        let off = DiskWatermarks {
            pause_clones_below: 0.0,
            evict_below: 0.0,
            ..w
        };
        assert_eq!(assess_disk(usage(100, 0), &off), calm);
        assert_eq!(assess_disk(usage(0, 0), &w), calm);

        let real = disk_usage(Path::new("/")).unwrap();
        assert!(real.total > 0 && real.available <= real.total);
    }

    fn cand(key: &str, size: u64, age_s: u64) -> EvictionCandidate {
        EvictionCandidate {
            key: key.into(),
            size_bytes: size,
            last_used: SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000 - age_s),
        }
    }

    #[test]
    fn eviction_is_lru_until_enough_is_freed() {
        let plan = plan_eviction(
            vec![
                cand("new", 10, 1),
                cand("old", 10, 100),
                cand("mid", 10, 50),
            ],
            15,
        );
        let keys: Vec<&str> = plan.iter().map(|c| c.key.as_str()).collect();
        assert_eq!(keys, ["old", "mid"]);
        assert!(plan_eviction(vec![cand("a", 1, 1)], 0).is_empty());

        struct Hook(&'static str, Vec<EvictionCandidate>, Mutex<Vec<String>>);
        impl EvictionHook for Hook {
            fn name(&self) -> &str {
                self.0
            }
            fn candidates(&self) -> Vec<EvictionCandidate> {
                self.1.clone()
            }
            fn evict(&self, keys: &[String]) -> u64 {
                self.2.lock().unwrap().extend(keys.iter().cloned());
                keys.len() as u64 * 10
            }
        }
        let a = Hook(
            "a",
            vec![cand("x", 10, 10), cand("y", 10, 300)],
            Mutex::default(),
        );
        let b = Hook("b", vec![cand("z", 10, 200)], Mutex::default());
        let freed = run_eviction(&[&a, &b], 20);
        assert_eq!(freed, 20);
        assert_eq!(*a.2.lock().unwrap(), ["y"]);
        assert_eq!(*b.2.lock().unwrap(), ["z"]);
    }

    #[test]
    fn stale_tmp_entries_are_removed() {
        let dir = tempfile::tempdir().unwrap();
        let layout = Layout::new(dir.path());
        layout.create_dirs().unwrap();
        std::fs::create_dir(layout.tmp_dir().join("clone-1-abc")).unwrap();
        std::fs::write(layout.tmp_dir().join("askpass.sh"), "").unwrap();
        assert_eq!(
            remove_stale_tmp(&layout, Duration::from_secs(3600)).unwrap(),
            0
        );
        assert_eq!(remove_stale_tmp(&layout, Duration::ZERO).unwrap(), 1);
        assert!(layout.tmp_dir().join("askpass.sh").exists());
    }
}
