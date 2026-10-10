//! Squash-merge awareness through the GitHub CLI (`gh`).
//!
//! Squash-merging a PR (and deleting its branch) leaves the worktree's
//! commits "unpushed and not merged" as far as git can tell: the squash commit
//! has another SHA and the remote branch is gone. Such worktrees classify as
//! `REVIEW`. When `gh` is installed and signed in, we ask GitHub:
//!
//! ```text
//! gh pr list -R <host/owner/repo> --state merged --head <branch> \
//!     --json number,headRefOid,mergedAt,url
//! ```
//!
//! Only PRs merged into the repository's default branch, from a branch in
//! the repository itself (not a fork with the same branch name), count.
//!
//! * A merged PR whose `headRefOid` **is** the worktree's HEAD means every
//!   local commit was in that PR, which was merged: the (clean, unlocked)
//!   worktree becomes `REMOVABLE`, "clean, squash-merged in PR #123".
//! * A merged PR with another head keeps it `REVIEW` with a better reason
//!   ("PR #123 was merged, but 2 local commits came after it").
//! * Anything else (no `gh`, not signed in, a timeout, no GitHub remote, no
//!   merged PR) leaves the classification untouched.
//!
//! Only worktrees that are `REVIEW` *solely* because of commit safety are
//! looked up ([`super::classify::commit_safety_only`]). `gh` runs through the
//! same sandboxed runner as every other tool ([`crate::system::tools`]): from
//! `/`, stdin closed, a timeout, its own process group, never prompting
//! (`GH_PROMPT_DISABLED`), no update checks. At most [`GH_CONCURRENCY`]
//! lookups run at once. `clean` repeats the lookup right before removing.

use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use rayon::prelude::*;
use serde::Serialize;

use crate::git::Git;
use crate::system::tools::Tools;
use crate::system::SystemEnv;

/// Maximum concurrent `gh` lookups.
pub const GH_CONCURRENCY: usize = 4;

/// Per-call timeout for `gh` (it talks to the network).
pub const GH_TIMEOUT: Duration = Duration::from_secs(15);

/// Variables set on every `gh` call: never prompt, no update notices, plain
/// output.
const GH_ENV: &[(&str, &str)] = &[
    ("GH_PROMPT_DISABLED", "1"),
    ("GH_NO_UPDATE_NOTIFIER", "1"),
    ("GH_NO_EXTENSION_UPDATE_NOTIFIER", "1"),
    ("GH_SPINNER_DISABLED", "1"),
    ("GH_PAGER", "cat"),
    ("PAGER", "cat"),
    ("NO_COLOR", "1"),
    ("CLICOLOR", "0"),
    ("DO_NOT_TRACK", "1"),
];

/// The merged pull request a worktree's HEAD was squash-merged in
/// (`details.merged_pr`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MergedPr {
    pub number: u64,
    pub url: String,
}

/// One merged PR as `gh` reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Pr {
    pub number: u64,
    pub head: String,
    pub merged_at: String,
    pub url: String,
    /// Branch it was merged into.
    pub base: String,
    /// Owner of the repository the PR's branch lives in.
    pub head_owner: String,
}

/// What GitHub says about one worktree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Verdict {
    /// HEAD is the head of a merged PR: `REMOVABLE`.
    SquashMerged(MergedPr),
    /// A PR from this branch was merged, but HEAD isn't its head: stays
    /// `REVIEW` with this reason.
    MergedEarlier(String),
    /// Nothing to go on: no GitHub remote, no matching merged PR.
    Unknown,
    /// GitHub could not be asked (not signed in, timeout, offline, bad
    /// output): the reason.
    Unavailable(String),
}

impl Verdict {
    pub fn reason(&self) -> Option<String> {
        match self {
            Verdict::SquashMerged(pr) => Some(squash_reason(pr.number)),
            Verdict::MergedEarlier(r) => Some(r.clone()),
            Verdict::Unknown | Verdict::Unavailable(_) => None,
        }
    }
}

/// `clean, squash-merged in PR #123`.
pub fn squash_reason(number: u64) -> String {
    format!("clean, squash-merged in PR #{number}")
}

/// Merged PRs of one (repo, branch), computed once (`None`: gh failed).
type PrCell = Arc<OnceLock<Option<Vec<Pr>>>>;

/// A `gh` runner. Construction only looks `gh` up on `PATH`; nothing runs
/// until the first lookup (which first checks `gh auth status` for the host).
pub struct Gh {
    tools: Tools,
    /// host -> signed in?
    auth: Mutex<HashMap<String, Arc<OnceLock<bool>>>>,
    /// (repo, branch) -> merged PRs (None: gh failed).
    prs: Mutex<HashMap<(String, String), PrCell>>,
    /// repo -> default branch name (None: gh failed).
    default_branch: Mutex<HashMap<String, Arc<OnceLock<Option<String>>>>>,
    calls: AtomicU64,
}

impl std::fmt::Debug for Gh {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Gh").finish_non_exhaustive()
    }
}

impl Gh {
    /// `gh` from the process environment, if it is on `PATH`.
    pub fn from_process() -> Option<Gh> {
        let env = SystemEnv::from_process();
        let home = env.var_path("HOME");
        Gh::with_env(&env, home.as_deref())
    }

    /// `gh` from `env`'s `PATH`, if it is there (tests point it at a fake).
    pub fn with_env(env: &SystemEnv, home: Option<&Path>) -> Option<Gh> {
        let mut vars = env.vars().clone();
        for (k, v) in GH_ENV {
            vars.insert((*k).to_string(), (*v).into());
        }
        let tools = Tools::new(
            env.var("PATH").cloned(),
            home.map(Path::to_path_buf),
            GH_TIMEOUT,
        )
        .with_env(Arc::new(vars));
        tools.which("gh")?;
        Some(Gh {
            tools,
            auth: Mutex::default(),
            prs: Mutex::default(),
            default_branch: Mutex::default(),
            calls: AtomicU64::new(0),
        })
    }

    /// `gh` processes run so far.
    pub fn calls(&self) -> u64 {
        self.calls.load(Ordering::Relaxed)
    }

    fn run(&self, args: &[&str]) -> Option<String> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        self.tools.output("gh", args)
    }

    fn signed_in(&self, host: &str) -> bool {
        let cell = {
            let mut m = self.auth.lock().unwrap_or_else(|e| e.into_inner());
            m.entry(host.to_string()).or_default().clone()
        };
        *cell.get_or_init(|| self.run(&["auth", "status", "--hostname", host]).is_some())
    }

    /// The default branch of `repo` (`host/owner/name`).
    fn default_branch(&self, repo: &str) -> Option<String> {
        let cell = {
            let mut m = self
                .default_branch
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            m.entry(repo.to_string()).or_default().clone()
        };
        cell.get_or_init(|| {
            let out = self.run(&["repo", "view", repo, "--json", "defaultBranchRef"])?;
            let v: serde_json::Value = serde_json::from_str(&out).ok()?;
            v.get("defaultBranchRef")?
                .get("name")?
                .as_str()
                .filter(|s| !s.is_empty())
                .map(str::to_string)
        })
        .clone()
    }

    /// Merged PRs whose head branch is `branch` in `repo` (`host/owner/name`).
    fn merged_prs(&self, repo: &str, branch: &str) -> Option<Vec<Pr>> {
        let cell = {
            let mut m = self.prs.lock().unwrap_or_else(|e| e.into_inner());
            m.entry((repo.to_string(), branch.to_string()))
                .or_default()
                .clone()
        };
        cell.get_or_init(|| {
            let out = self.run(&[
                "pr",
                "list",
                "--repo",
                repo,
                "--state",
                "merged",
                "--head",
                branch,
                "--limit",
                "100",
                "--json",
                "number,headRefOid,mergedAt,url,baseRefName,headRepositoryOwner",
            ])?;
            parse_prs(&out)
        })
        .clone()
    }

    /// Ask GitHub about the worktree at `wt` on `branch`.
    pub(crate) fn verdict(&self, git: &Git, wt: &Path, branch: &str) -> Verdict {
        if branch.is_empty() || branch.starts_with('(') {
            return Verdict::Unknown;
        }
        let head = git.run(wt, ["rev-parse", "--verify", "--quiet", "HEAD"]);
        let head = head.text();
        if !is_sha(&head) {
            return Verdict::Unknown;
        }
        let Some(repo) = github_repo(git, wt, branch) else {
            return Verdict::Unknown;
        };
        let host = repo.split('/').next().unwrap_or_default();
        if !self.signed_in(host) {
            return Verdict::Unavailable(format!("gh is not signed in to {host}"));
        }
        let Some(base) = self.default_branch(&repo) else {
            return Verdict::Unavailable(format!("gh could not read {repo}'s default branch"));
        };
        let Some(prs) = self.merged_prs(&repo, branch) else {
            return Verdict::Unavailable(format!("gh could not list {repo}'s merged PRs"));
        };
        let owner = repo.split('/').nth(1).unwrap_or_default();
        let prs = relevant(prs, &base, owner);
        match decide(&head, &prs) {
            Decision::Squash(pr) => Verdict::SquashMerged(MergedPr {
                number: pr.number,
                url: pr.url.clone(),
            }),
            Decision::Other(pr) => {
                let ancestor = is_sha(&pr.head)
                    && git
                        .run(wt, ["merge-base", "--is-ancestor", &pr.head, "HEAD"])
                        .success;
                let after = ancestor
                    .then(|| {
                        git.run(wt, ["rev-list", "--count", &format!("{}..HEAD", pr.head)])
                            .text()
                            .parse::<u64>()
                            .ok()
                    })
                    .flatten();
                Verdict::MergedEarlier(match after {
                    Some(n) => format!(
                        "PR #{} was merged, but {n} local {} came after it",
                        pr.number,
                        if n == 1 { "commit" } else { "commits" }
                    ),
                    None => format!(
                        "PR #{} was merged, but HEAD is not the commit that was merged",
                        pr.number
                    ),
                })
            }
            Decision::None => Verdict::Unknown,
        }
    }

    /// Run [`Gh::verdict`] for many worktrees, [`GH_CONCURRENCY`] at a time.
    pub(crate) fn verdicts(&self, git: &Git, jobs: &[(&Path, &str)]) -> Vec<Verdict> {
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(GH_CONCURRENCY)
            .thread_name(|i| format!("lysosome-gh-{i}"))
            .build();
        let run = || {
            jobs.par_iter()
                .map(|(wt, branch)| self.verdict(git, wt, branch))
                .collect()
        };
        match pool {
            Ok(p) => p.install(run),
            Err(_) => jobs
                .iter()
                .map(|(wt, branch)| self.verdict(git, wt, branch))
                .collect(),
        }
    }
}

/// PRs merged into `base` (the default branch) from a branch of the repo
/// itself (`owner`), not from a fork that happens to share the branch name.
fn relevant(prs: Vec<Pr>, base: &str, owner: &str) -> Vec<Pr> {
    prs.into_iter()
        .filter(|p| p.base == base && p.head_owner.eq_ignore_ascii_case(owner))
        .collect()
}

#[derive(Debug, PartialEq, Eq)]
enum Decision<'a> {
    /// A merged PR's head is HEAD (latest merge if several).
    Squash(&'a Pr),
    /// Merged PRs exist, none with HEAD as head: the latest one.
    Other(&'a Pr),
    None,
}

fn decide<'a>(head: &str, prs: &'a [Pr]) -> Decision<'a> {
    // RFC 3339 timestamps from one API compare correctly as strings.
    let latest =
        |it: &mut dyn Iterator<Item = &'a Pr>| it.max_by(|a, b| a.merged_at.cmp(&b.merged_at));
    if let Some(pr) = latest(&mut prs.iter().filter(|p| p.head.eq_ignore_ascii_case(head))) {
        return Decision::Squash(pr);
    }
    match latest(&mut prs.iter()) {
        Some(pr) => Decision::Other(pr),
        None => Decision::None,
    }
}

fn is_sha(s: &str) -> bool {
    (s.len() == 40 || s.len() == 64) && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// `gh pr list --json number,headRefOid,mergedAt,url` output. Entries
/// missing a field (or not merged) are dropped; malformed JSON is `None`.
fn parse_prs(json: &str) -> Option<Vec<Pr>> {
    let v: serde_json::Value = serde_json::from_str(json).ok()?;
    Some(
        v.as_array()?
            .iter()
            .filter_map(|p| {
                Some(Pr {
                    number: p.get("number")?.as_u64()?,
                    head: p.get("headRefOid")?.as_str()?.to_string(),
                    merged_at: p
                        .get("mergedAt")?
                        .as_str()
                        .filter(|s| !s.is_empty())?
                        .to_string(),
                    url: p.get("url")?.as_str()?.to_string(),
                    base: p.get("baseRefName")?.as_str()?.to_string(),
                    head_owner: p
                        .get("headRepositoryOwner")?
                        .get("login")?
                        .as_str()?
                        .to_string(),
                })
            })
            .collect(),
    )
}

/// `host/owner/name` of the GitHub repository the branch would be pushed
/// to: the branch's configured remote, else `origin`, else the only remote.
fn github_repo(git: &Git, wt: &Path, branch: &str) -> Option<String> {
    let configured = git.run(wt, ["config", "--get", &format!("branch.{branch}.remote")]);
    let mut remotes = Vec::new();
    if configured.success && !configured.text().is_empty() && configured.text() != "." {
        remotes.push(configured.text());
    }
    remotes.push("origin".to_string());
    let all = git.run(wt, ["remote"]).text();
    let all: Vec<&str> = all.lines().filter(|l| !l.is_empty()).collect();
    if all.len() == 1 {
        remotes.push(all[0].to_string());
    }
    remotes
        .iter()
        .filter(|r| all.contains(&r.as_str()))
        .find_map(|r| {
            let url = git.run(wt, ["remote", "get-url", r]);
            out_ok(&url).and_then(|u| parse_remote(&u))
        })
}

fn out_ok(o: &crate::git::GitOutput) -> Option<String> {
    o.success.then(|| o.text()).filter(|t| !t.is_empty())
}

/// `host/owner/name` from a remote URL (`https://github.com/o/r.git`,
/// `git@github.com:o/r.git`, `ssh://git@host:22/o/r`...). Local paths and
/// anything that isn't exactly `owner/name` give `None`.
pub(crate) fn parse_remote(url: &str) -> Option<String> {
    let url = url.trim();
    let (host, path) = if let Some((scheme, rest)) = url.split_once("://") {
        if !matches!(
            scheme,
            "https" | "http" | "ssh" | "git" | "git+ssh" | "ssh+git"
        ) {
            return None;
        }
        let (authority, path) = rest.split_once('/')?;
        let host = authority.rsplit('@').next()?;
        let host = host.split(':').next()?;
        (host, path)
    } else {
        // scp-like `[user@]host:path` (not a local path, which has no `:`
        // before its first `/`).
        let (left, path) = url.split_once(':')?;
        // `host:/abs/path` is a path on that host, never `owner/repo`
        // (this also rejects `C:/...`).
        if left.contains('/') || left.is_empty() || path.starts_with('/') {
            return None;
        }
        (left.rsplit('@').next()?, path)
    };
    let path = path.trim_matches('/');
    let path = path.strip_suffix(".git").unwrap_or(path);
    let parts: Vec<&str> = path.split('/').collect();
    let ok = |s: &str| {
        !s.is_empty()
            && s != "."
            && s != ".."
            && s.bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
    };
    if host.is_empty() || !ok(host) || parts.len() != 2 || !parts.iter().all(|p| ok(p)) {
        return None;
    }
    Some(format!(
        "{}/{}/{}",
        host.to_ascii_lowercase(),
        parts[0],
        parts[1]
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_github_remotes_and_rejects_local_paths() {
        for (url, want) in [
            (
                "https://github.com/acme/app.git",
                Some("github.com/acme/app"),
            ),
            ("https://github.com/acme/app", Some("github.com/acme/app")),
            (
                "https://user:tok@GitHub.com/acme/app.git/",
                Some("github.com/acme/app"),
            ),
            ("git@github.com:acme/app.git", Some("github.com/acme/app")),
            (
                "ssh://git@github.com:22/acme/app",
                Some("github.com/acme/app"),
            ),
            (
                "ssh://git@ghe.corp.io/team/svc.git",
                Some("ghe.corp.io/team/svc"),
            ),
            ("/Users/me/remote.git", None),
            ("../remote.git", None),
            ("file:///Users/me/remote.git", None),
            ("C:/x/y", None),
            ("https://gitlab.com/group/sub/proj.git", None),
            ("https://github.com/acme", None),
            ("git@github.com:acme/../x", None),
            ("https://github.com/acme/app;rm", None),
        ] {
            assert_eq!(parse_remote(url).as_deref(), want, "{url}");
        }
    }

    fn pr(number: u64, head: &str, merged_at: &str) -> Pr {
        Pr {
            number,
            head: head.into(),
            merged_at: merged_at.into(),
            url: format!("https://github.com/acme/app/pull/{number}"),
            base: "main".into(),
            head_owner: "acme".into(),
        }
    }

    #[test]
    fn only_prs_into_the_default_branch_from_the_repo_itself_count() {
        let a = "a".repeat(40);
        let mut stacked = pr(1, &a, "2026-01-01T00:00:00Z");
        stacked.base = "feat-a".into();
        let mut fork = pr(2, &a, "2026-01-01T00:00:00Z");
        fork.head_owner = "stranger".into();
        let mut ok = pr(3, &a, "2026-01-01T00:00:00Z");
        ok.head_owner = "ACME".into();
        let got = relevant(vec![stacked, fork, ok.clone()], "main", "acme");
        assert_eq!(got, vec![ok]);
    }

    #[test]
    fn decides_on_exact_head_match_only() {
        let a = "a".repeat(40);
        let b = "b".repeat(40);
        let prs = vec![
            pr(1, &b, "2026-01-01T00:00:00Z"),
            pr(2, &a, "2026-02-01T00:00:00Z"),
            pr(3, &b, "2026-03-01T00:00:00Z"),
        ];
        assert_eq!(decide(&a, &prs), Decision::Squash(&prs[1]));
        assert_eq!(decide(&a.to_uppercase(), &prs), Decision::Squash(&prs[1]));
        let c = "c".repeat(40);
        assert_eq!(decide(&c, &prs), Decision::Other(&prs[2]), "latest merged");
        assert_eq!(decide(&a, &[]), Decision::None);
    }

    #[test]
    fn parses_gh_json() {
        let json = r#"[
          {"number": 12, "headRefOid": "abc", "mergedAt": "2026-01-02T03:04:05Z", "url": "u12",
           "baseRefName": "main", "headRepositoryOwner": {"login": "acme"}},
          {"number": 13, "headRefOid": "def", "mergedAt": null, "url": "u13",
           "baseRefName": "main", "headRepositoryOwner": {"login": "acme"}},
          {"number": "x"}
        ]"#;
        assert_eq!(
            parse_prs(json).unwrap(),
            vec![pr_u(12, "abc", "2026-01-02T03:04:05Z", "u12")]
        );
        assert_eq!(parse_prs("[]").unwrap(), vec![]);
        assert!(parse_prs("not json").is_none());
        assert!(parse_prs("{}").is_none());
    }

    fn pr_u(number: u64, head: &str, merged_at: &str, url: &str) -> Pr {
        Pr {
            number,
            head: head.into(),
            merged_at: merged_at.into(),
            url: url.into(),
            base: "main".into(),
            head_owner: "acme".into(),
        }
    }

    #[test]
    fn shas() {
        assert!(is_sha(&"0".repeat(40)));
        assert!(is_sha(&"f".repeat(64)));
        assert!(!is_sha("HEAD"));
        assert!(!is_sha(&"g".repeat(40)));
    }
}
