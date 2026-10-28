//! Feature flags: boolean and percentage-rollout flags with per-user and per-org
//! overrides.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

/// Basis points in 100%.
pub const FULL_ROLLOUT: u32 = 10_000;

/// A feature flag definition.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FeatureFlag {
    /// Unique flag name, e.g. `"search-ranking-v2"`.
    pub name: String,
    /// How the default value is computed.
    pub kind: FlagKind,
}

/// The default value of a flag.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum FlagKind {
    /// The same value for everyone.
    Bool(bool),
    /// Enabled for a stable pseudo-random subset of users: `basis_points / 10000` of them.
    Rollout {
        /// Share of users, in basis points (0..=10000).
        basis_points: u32,
    },
}

/// Overrides of one flag for specific users and orgs.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FlagOverrides {
    /// Per-user overrides, keyed by user id.
    pub users: HashMap<i64, bool>,
    /// Per-org overrides, keyed by org id.
    pub orgs: HashMap<i64, bool>,
}

/// Evaluates `flag` for a viewer.
///
/// Precedence: a user override, then org overrides (if the viewer's orgs disagree, `true`
/// wins), then the flag default. A rollout flag hashes (flag name, user id); anonymous
/// viewers (`user_id == None`) only get a rollout flag when it is at 100%.
pub fn evaluate(
    flag: &FeatureFlag,
    user_id: Option<i64>,
    org_ids: &[i64],
    overrides: &FlagOverrides,
) -> bool {
    if let Some(v) = user_id.and_then(|u| overrides.users.get(&u)) {
        return *v;
    }
    let mut org_override = None;
    for v in org_ids.iter().filter_map(|o| overrides.orgs.get(o)) {
        org_override = Some(org_override.unwrap_or(false) || *v);
    }
    if let Some(v) = org_override {
        return v;
    }
    match flag.kind {
        FlagKind::Bool(v) => v,
        FlagKind::Rollout { basis_points } => match user_id {
            _ if basis_points >= FULL_ROLLOUT => true,
            Some(u) => rollout_bucket(&flag.name, u) < basis_points,
            None => false,
        },
    }
}

/// The user's stable bucket in `0..10000` for a flag. The same (flag, user) always maps to
/// the same bucket, across processes and releases.
pub fn rollout_bucket(flag_name: &str, user_id: i64) -> u32 {
    const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut h = FNV_OFFSET;
    for b in flag_name
        .as_bytes()
        .iter()
        .chain(&[0u8])
        .chain(&user_id.to_le_bytes())
    {
        h ^= u64::from(*b);
        h = h.wrapping_mul(FNV_PRIME);
    }
    // FNV-1a mixes the low bits of the last bytes poorly; finish with a SplitMix64 step.
    h ^= h >> 30;
    h = h.wrapping_mul(0xbf58_476d_1ce4_e5b9);
    h ^= h >> 27;
    h = h.wrapping_mul(0x94d0_49bb_1331_11eb);
    h ^= h >> 31;
    // The remainder is < 10000, so the cast is lossless.
    (h % u64::from(FULL_ROLLOUT)) as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rollout(name: &str, bp: u32) -> FeatureFlag {
        FeatureFlag {
            name: name.to_string(),
            kind: FlagKind::Rollout { basis_points: bp },
        }
    }

    #[test]
    fn bool_flags_and_override_precedence() {
        let flag = FeatureFlag {
            name: "f".into(),
            kind: FlagKind::Bool(false),
        };
        let mut ov = FlagOverrides::default();
        assert!(!evaluate(&flag, Some(1), &[10], &ov));
        ov.orgs.insert(10, true);
        assert!(evaluate(&flag, Some(1), &[10], &ov));
        assert!(!evaluate(&flag, Some(1), &[11], &ov));
        ov.users.insert(1, false);
        assert!(!evaluate(&flag, Some(1), &[10], &ov));
        // Disagreeing orgs: true wins.
        ov.orgs.insert(11, false);
        assert!(evaluate(&flag, Some(2), &[11, 10], &ov));
        assert!(!evaluate(&flag, Some(2), &[11], &ov));
    }

    #[test]
    fn rollout_is_deterministic() {
        // Pinned values: changing the hash would silently reshuffle every rollout.
        let pinned: Vec<u32> = (1..=3).map(|u| rollout_bucket("my-flag", u)).collect();
        assert_eq!(pinned, vec![2445, 5554, 8404]);
        assert_ne!(rollout_bucket("a", 1), rollout_bucket("b", 1));
    }

    #[test]
    fn rollout_bounds() {
        assert!(!evaluate(
            &rollout("x", 0),
            Some(42),
            &[],
            &FlagOverrides::default()
        ));
        assert!(evaluate(
            &rollout("x", 10_000),
            Some(42),
            &[],
            &FlagOverrides::default()
        ));
        assert!(evaluate(
            &rollout("x", 10_000),
            None,
            &[],
            &FlagOverrides::default()
        ));
        assert!(!evaluate(
            &rollout("x", 9_999),
            None,
            &[],
            &FlagOverrides::default()
        ));
    }

    #[test]
    fn rollout_distribution_and_monotonicity() {
        let ov = FlagOverrides::default();
        let n = 100_000;
        for bp in [100u32, 2_500, 5_000, 9_000] {
            let on = (1..=n)
                .filter(|&u| evaluate(&rollout("dist", bp), Some(u), &[], &ov))
                .count();
            let share = on as f64 / n as f64;
            let want = f64::from(bp) / 10_000.0;
            assert!((share - want).abs() < 0.01, "bp={bp} share={share}");
        }
        // Raising the percentage only adds users.
        for u in 1..=10_000 {
            if evaluate(&rollout("m", 2_000), Some(u), &[], &ov) {
                assert!(evaluate(&rollout("m", 3_000), Some(u), &[], &ov));
            }
        }
    }

    #[test]
    fn rollouts_of_different_flags_are_independent() {
        let ov = FlagOverrides::default();
        let both = (1..=100_000)
            .filter(|&u| {
                evaluate(&rollout("a", 5_000), Some(u), &[], &ov)
                    && evaluate(&rollout("b", 5_000), Some(u), &[], &ov)
            })
            .count();
        assert!((both as f64 / 100_000.0 - 0.25).abs() < 0.01, "{both}");
    }
}
