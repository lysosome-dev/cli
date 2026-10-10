//! Pure classification, a rule-for-rule port of `analyze()` in
//! `reference/wtscan.py`. Kept free of I/O so every branch is unit-tested.

use crate::model::Status;

/// Everything classification needs to know about a (non-orphan) worktree.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Facts {
    /// Whole days since last activity; `None` if no activity could be found.
    pub idle_days: Option<i64>,
    pub locked: bool,
    /// `false` if `git status` failed or timed out (we then can't vouch it's clean).
    pub status_ok: bool,
    pub changed: u64,
    pub untracked: u64,
    /// Default branch HEAD is merged into, if any.
    pub merged_into: Option<String>,
    /// The repository has at least one remote.
    pub has_remote: bool,
    /// Commits on HEAD not on any remote; `None` without a remote or if counting failed.
    pub unpushed: Option<u64>,
}

/// Classify a worktree whose gitdir exists. Returns the status and a
/// human-readable reason (empty for `ACTIVE`).
///
/// Order matters and mirrors the prototype: activity first, then lock, then
/// dirtiness, then commit safety. Anything uncertain lands in `REVIEW`.
pub fn classify(f: &Facts, dormant_after_days: u64) -> (Status, String) {
    let threshold = i64::try_from(dormant_after_days).unwrap_or(i64::MAX);
    let dormant = matches!(f.idle_days, Some(d) if d >= threshold);
    if !dormant {
        return (Status::Active, String::new());
    }
    if f.locked {
        return (Status::Review, "locked worktree".into());
    }
    if !f.status_ok {
        return (Status::Review, "could not read git status".into());
    }
    if f.changed > 0 || f.untracked > 0 {
        let mut bits = Vec::new();
        if f.changed > 0 {
            bits.push(plural(
                f.changed,
                "uncommitted change",
                "uncommitted changes",
            ));
        }
        if f.untracked > 0 {
            bits.push(plural(f.untracked, "untracked file", "untracked files"));
        }
        return (Status::Review, bits.join(", "));
    }
    let commits_safe = f.merged_into.is_some() || f.unpushed == Some(0);
    if !commits_safe {
        let reason = match f.unpushed {
            Some(n) => format!(
                "{}, not merged",
                plural(n, "unpushed commit", "unpushed commits")
            ),
            None if f.has_remote => "could not count unpushed commits, not merged".into(),
            None => "no remote and not merged".into(),
        };
        return (Status::Review, reason);
    }
    let reason = match &f.merged_into {
        Some(base) => format!("clean, merged into {base}"),
        None => "clean, fully pushed".into(),
    };
    (Status::Removable, reason)
}

/// `REVIEW` *only* because of commit safety: dormant, unlocked, `git status`
/// read and clean, but HEAD is neither merged nor fully pushed. These are
/// the worktrees a squash-merged PR can vouch for (see `worktree::gh`).
pub fn commit_safety_only(f: &Facts, dormant_after_days: u64) -> bool {
    classify(f, dormant_after_days).0 == Status::Review
        && !f.locked
        && f.status_ok
        && f.changed == 0
        && f.untracked == 0
        && f.merged_into.is_none()
        && f.unpushed != Some(0)
}

/// Reason used for orphans (gitdir missing).
pub const ORPHAN_REASON: &str = "parent repo moved or deleted";

fn plural(n: u64, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dormant_clean() -> Facts {
        Facts {
            idle_days: Some(100),
            status_ok: true,
            has_remote: true,
            unpushed: Some(0),
            ..Facts::default()
        }
    }

    #[test]
    fn active_when_recent_or_unknown() {
        let mut f = dormant_clean();
        f.idle_days = Some(29);
        assert_eq!(classify(&f, 30).0, Status::Active);
        f.idle_days = None;
        assert_eq!(classify(&f, 30).0, Status::Active);
        f.idle_days = Some(30);
        assert_eq!(classify(&f, 30).0, Status::Removable, "boundary is >=");
    }

    #[test]
    fn locked_wins_over_dirty() {
        let mut f = dormant_clean();
        f.locked = true;
        f.changed = 3;
        assert_eq!(classify(&f, 30), (Status::Review, "locked worktree".into()));
    }

    #[test]
    fn dirty_reasons() {
        let mut f = dormant_clean();
        f.changed = 1;
        f.untracked = 2;
        assert_eq!(
            classify(&f, 30),
            (
                Status::Review,
                "1 uncommitted change, 2 untracked files".into()
            )
        );
    }

    #[test]
    fn status_failure_is_review_not_clean() {
        let mut f = dormant_clean();
        f.status_ok = false;
        f.merged_into = Some("main".into());
        assert_eq!(classify(&f, 30).0, Status::Review);
    }

    #[test]
    fn only_commit_safety_reviews_can_be_vouched_for_by_a_pr() {
        let mut f = dormant_clean();
        f.unpushed = Some(2);
        assert!(commit_safety_only(&f, 30));
        f.has_remote = false;
        f.unpushed = None;
        assert!(commit_safety_only(&f, 30), "no remote and not merged");
        for spoil in [
            |f: &mut Facts| f.locked = true,
            |f: &mut Facts| f.status_ok = false,
            |f: &mut Facts| f.changed = 1,
            |f: &mut Facts| f.untracked = 1,
            |f: &mut Facts| f.idle_days = Some(1),
            |f: &mut Facts| f.merged_into = Some("main".into()),
        ] {
            let mut g = f.clone();
            spoil(&mut g);
            assert!(!commit_safety_only(&g, 30), "{g:?}");
        }
        assert!(
            !commit_safety_only(&dormant_clean(), 30),
            "REMOVABLE already"
        );
    }

    #[test]
    fn commit_safety() {
        let mut f = dormant_clean();
        f.unpushed = Some(2);
        assert_eq!(
            classify(&f, 30),
            (Status::Review, "2 unpushed commits, not merged".into())
        );
        f.has_remote = false;
        f.unpushed = None;
        assert_eq!(
            classify(&f, 30),
            (Status::Review, "no remote and not merged".into())
        );
        f.merged_into = Some("origin/main".into());
        assert_eq!(
            classify(&f, 30),
            (Status::Removable, "clean, merged into origin/main".into())
        );
        let f = dormant_clean();
        assert_eq!(
            classify(&f, 30),
            (Status::Removable, "clean, fully pushed".into())
        );
    }
}
