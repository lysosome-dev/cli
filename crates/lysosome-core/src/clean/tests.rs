use std::sync::Mutex;
use std::time::Duration;

use serde_json::json;

use super::*;
use crate::model::Cleanup;

fn finding(id: &str, cat: Category, status: Status, tier: Tier, size: u64) -> Finding {
    Finding {
        id: id.into(),
        category: cat,
        kind: id.split(':').next().unwrap_or("x").into(),
        path: PathBuf::from(id.split_once(':').map_or("/x", |x| x.1)),
        size_bytes: size,
        tier,
        status,
        reason: "r".into(),
        last_activity: None,
        details: json!({}),
        cleanup: None,
    }
}

fn worktree(path: &str, status: Status, artifacts: &[(&str, u64)]) -> Finding {
    let mut f = finding(
        &format!("worktree:{path}"),
        Category::Worktrees,
        status,
        if status == Status::Removable {
            Tier::Regenerable
        } else {
            Tier::Review
        },
        1000,
    );
    f.details = json!({
        "main_repo": "/c/app",
        "gitdir": "/c/app/.git/worktrees/x",
        "idle_days": 100,
        "artifacts": artifacts.iter().map(|(p, s)| json!({"path": p, "size_bytes": s})).collect::<Vec<_>>(),
    });
    if status == Status::Removable {
        f.cleanup = Some(Cleanup::command(worktree_remove_argv(
            Path::new("/c/app"),
            Path::new(path),
        )));
    }
    f
}

fn artifact(path: &str, status: Status, tier: Tier, size: u64) -> Finding {
    let name = Path::new(path).file_name().unwrap().to_string_lossy();
    let mut f = finding(
        &format!("{name}:{path}"),
        Category::Artifacts,
        status,
        tier,
        size,
    );
    f.details = json!({"project": "/c/old", "project_kind": "repo", "idle_days": 400});
    if status == Status::Removable {
        f.cleanup = Some(Cleanup::trash(vec![path.into()]));
    }
    f
}

fn cache_cmd(kind: &str, tier: Tier, size: u64) -> Finding {
    let mut f = finding(
        &format!("{kind}:/h/.{kind}"),
        Category::PackageCaches,
        Status::Removable,
        tier,
        size,
    );
    f.details = json!({"name": format!("{kind} cache")});
    f.cleanup = Some(Cleanup::command(vec![kind.into(), "clean".into()]));
    f
}

fn all_tiers() -> Selection {
    Selection {
        tiers: [Tier::Regenerable, Tier::RegenerableExpensive, Tier::Review]
            .into_iter()
            .collect(),
        ..Selection::default()
    }
}

#[test]
fn only_removable_findings_with_a_cleanup_are_planned() {
    let mut review_cmd = cache_cmd("x", Tier::Review, 5);
    review_cmd.status = Status::Removable; // a REMOVABLE finding in tier review: still never
    let findings = vec![
        worktree("/c/app-merged", Status::Removable, &[]),
        worktree(
            "/c/app-dirty",
            Status::Review,
            &[("/c/app-dirty/node_modules", 7)],
        ),
        {
            let mut o = worktree("/c/doomed", Status::Orphan, &[]);
            o.tier = Tier::Review;
            o
        },
        worktree("/c/app-active", Status::Active, &[]),
        artifact("/c/old/target", Status::Removable, Tier::Regenerable, 30),
        artifact("/c/old/.venv", Status::Review, Tier::Review, 40),
        cache_cmd("npm", Tier::Regenerable, 50),
        review_cmd,
    ];
    let p = plan(&findings, &all_tiers(), PlanOptions::default());
    let ids: Vec<&str> = p.items.iter().map(|i| i.id.as_str()).collect();
    assert_eq!(
        ids,
        [
            "worktree:/c/app-merged",
            "target:/c/old/target",
            "npm:/h/.npm"
        ]
    );
    assert_eq!(p.total_bytes, 1080);
    assert_eq!(
        p.excluded.review.items, 3,
        "dirty worktree, .venv, tier-review cache"
    );
    assert_eq!(p.excluded.orphan.items, 1);
    let wt = &p.items[0];
    assert_eq!(wt.action, ActionKind::WorktreeRemove);
    assert_eq!(
        wt.command,
        ["git", "-C", "/c/app", "worktree", "remove", "/c/app-merged"]
    );
    assert!(!wt.command.iter().any(|a| a.contains("force")));
    assert_eq!(p.items[1].action, ActionKind::Trash);
    assert_eq!(p.items[2].action, ActionKind::Command);
}

#[test]
fn default_selection_is_regenerable_only() {
    let findings = vec![
        artifact(
            "/c/old/node_modules",
            Status::Removable,
            Tier::RegenerableExpensive,
            100,
        ),
        artifact("/c/old/dist", Status::Removable, Tier::Regenerable, 10),
    ];
    let p = plan(&findings, &Selection::default(), PlanOptions::default());
    assert_eq!(p.items.len(), 1);
    assert_eq!(p.items[0].kind, "dist");
    assert_eq!(
        p.excluded.tier,
        Count {
            items: 1,
            bytes: 100
        }
    );
}

#[test]
fn scopes_age_and_only_filters() {
    let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1000 * 86_400);
    let mut young = cache_cmd("go", Tier::Regenerable, 1);
    young.last_activity = Some(now - Duration::from_secs(10 * 86_400));
    let mut unknown = cache_cmd("brew", Tier::Regenerable, 1);
    unknown.last_activity = None;
    let mut old = cache_cmd("npm", Tier::Regenerable, 1);
    old.last_activity = Some(now - Duration::from_secs(200 * 86_400));
    let findings = vec![
        young,
        unknown,
        old,
        artifact("/c/old/dist", Status::Removable, Tier::Regenerable, 10),
    ];
    let opts = PlanOptions {
        now: Some(now),
        ..PlanOptions::default()
    };

    let sel = Selection {
        older_than_days: Some(90),
        ..Selection::default()
    };
    let ids: Vec<String> = plan(&findings, &sel, opts)
        .items
        .into_iter()
        .map(|i| i.id)
        .collect();
    assert_eq!(
        ids,
        ["dist:/c/old/dist", "npm:/h/.npm"],
        "unknown age is excluded"
    );

    let sel = Selection {
        scopes: [Scope::Caches].into_iter().collect(),
        only: vec!["npm*".into(), "brew cache".into()],
        ..Selection::default()
    };
    let p = plan(&findings, &sel, opts);
    let ids: Vec<&str> = p.items.iter().map(|i| i.id.as_str()).collect();
    assert_eq!(ids, ["brew:/h/.brew", "npm:/h/.npm"]);
    assert_eq!(p.excluded.only.items, 1);
}

#[test]
fn strip_artifacts_keeps_worktrees_and_includes_review_ones() {
    let findings = vec![
        worktree(
            "/c/app-dirty",
            Status::Review,
            &[("/c/app-dirty/node_modules", 7), ("/c/app-dirty/target", 3)],
        ),
        worktree(
            "/c/app-merged",
            Status::Removable,
            &[("/c/app-merged/dist", 2)],
        ),
        worktree(
            "/c/app-active",
            Status::Active,
            &[("/c/app-active/dist", 2)],
        ),
        {
            let mut o = worktree("/c/doomed", Status::Orphan, &[("/c/doomed/dist", 9)]);
            o.tier = Tier::Review;
            o
        },
        worktree("/c/app-bare", Status::Removable, &[]),
    ];
    let sel = Selection {
        strip_artifacts: true,
        ..Selection::default()
    };
    let p = plan(
        &findings,
        &sel,
        PlanOptions {
            permanent: true,
            now: None,
        },
    );
    let paths: Vec<&Path> = p.items.iter().map(|i| i.path.as_path()).collect();
    assert_eq!(
        paths,
        [
            Path::new("/c/app-dirty/target"),
            Path::new("/c/app-merged/dist")
        ]
    );
    assert!(p.items.iter().all(|i| i.action == ActionKind::Delete));
    assert!(!p
        .items
        .iter()
        .any(|i| i.action == ActionKind::WorktreeRemove));
    assert_eq!(
        p.excluded.tier.bytes, 7,
        "node_modules is regenerable_expensive"
    );
    assert_eq!(p.excluded.orphan.items, 1);
    assert_eq!(p.excluded.nothing_to_do.items, 1);
    assert_eq!(
        p.items[0].project.as_deref(),
        Some(Path::new("/c/app-dirty"))
    );
}

#[test]
fn malformed_findings_are_not_planned() {
    // A worktree without main_repo / gitdir details can't be re-checked.
    let mut wt = worktree("/c/app-merged", Status::Removable, &[]);
    wt.details = json!({});
    // An artifact whose cleanup targets something other than its own path.
    let mut art = artifact("/c/old/dist", Status::Removable, Tier::Regenerable, 1);
    art.cleanup = Some(Cleanup::trash(vec!["/c/old".into()]));
    // A relative trash path.
    let mut rel = cache_cmd("x", Tier::Regenerable, 1);
    rel.cleanup = Some(Cleanup::trash(vec!["relative/x".into()]));
    let p = plan(&[wt, art, rel], &all_tiers(), PlanOptions::default());
    assert!(p.items.is_empty());
    assert_eq!(p.excluded.nothing_to_do.items, 3);
}

#[test]
fn nested_targets_are_planned_once() {
    let mut outer = cache_cmd("a", Tier::Regenerable, 10);
    outer.cleanup = Some(Cleanup::trash(vec!["/h/cache".into()]));
    let mut inner = cache_cmd("b", Tier::Regenerable, 5);
    inner.cleanup = Some(Cleanup::trash(vec!["/h/cache/sub".into()]));
    let p = plan(&[outer, inner], &all_tiers(), PlanOptions::default());
    assert_eq!(p.items.len(), 1);
    assert_eq!(p.items[0].paths, [PathBuf::from("/h/cache")]);
}

#[test]
fn globs_and_ages() {
    assert!(glob_match("npm*", "npm-cache"));
    assert!(glob_match("*node_modules", "/c/a/node_modules"));
    assert!(glob_match(
        "worktree:*/app-?erged",
        "worktree:/c/app-merged"
    ));
    assert!(!glob_match("npm", "npm-cache"));
    assert!(glob_match("*", ""));
    assert!(!glob_match("a*b", "acd"));
    assert_eq!(parse_age_days("90d"), Some(90));
    assert_eq!(parse_age_days("90"), Some(90));
    assert_eq!(parse_age_days("2w"), Some(14));
    assert_eq!(parse_age_days("6mo"), Some(180));
    assert_eq!(parse_age_days("1y"), Some(365));
    assert_eq!(parse_age_days("5m"), None, "ambiguous");
    assert_eq!(parse_age_days("x"), None);
}

/// Records what it was asked to trash and moves it into a folder.
struct FakeTrash {
    inner: DirTrash,
    seen: Mutex<Vec<PathBuf>>,
}

impl Trasher for FakeTrash {
    fn trash(&self, path: &Path) -> Result<String, String> {
        self.seen.lock().unwrap().push(path.to_path_buf());
        self.inner.trash(path)
    }
}

fn cache_trash(path: &Path, size: u64) -> Finding {
    let mut f = finding(
        &format!("deriveddata:{}", path.display()),
        Category::Xcode,
        Status::Removable,
        Tier::Regenerable,
        size,
    );
    f.path = path.to_path_buf();
    f.cleanup = Some(Cleanup::trash(vec![path.to_path_buf()]));
    f
}

#[test]
fn trash_items_move_to_the_trasher_and_are_logged() {
    let t = tempfile::tempdir().unwrap();
    let base = t.path().canonicalize().unwrap();
    let home = base.join("home");
    let dd = home.join("Library/Developer/Xcode/DerivedData");
    std::fs::create_dir_all(dd.join("App-1")).unwrap();
    std::fs::write(dd.join("App-1/blob"), vec![1u8; 10_000]).unwrap();
    let gone = home.join("gone-cache");
    std::fs::create_dir_all(&gone).unwrap();

    let findings = vec![cache_trash(&dd, 10_000), cache_trash(&gone, 1)];
    let p = plan(&findings, &Selection::default(), PlanOptions::default());
    assert_eq!(p.items.len(), 2);
    std::fs::remove_dir(&gone).unwrap(); // vanished between plan and act

    let trash = FakeTrash {
        inner: DirTrash::new(base.join("fake-trash")),
        seen: Mutex::new(Vec::new()),
    };
    let log_path = default_log_path(&home, None);
    let log = ActionLog::open(&log_path).unwrap();
    let ex = Executor::new(&trash, home.clone(), Some(&log));
    let mut calls = 0;
    let results = ex.execute(&p, &mut |_, _, _| calls += 1).unwrap();
    assert_eq!(calls, 2);
    let by_path: Vec<(&Path, Outcome)> = p
        .items
        .iter()
        .zip(&results)
        .map(|(i, r)| (i.path.as_path(), r.outcome))
        .collect();
    assert!(by_path.contains(&(dd.as_path(), Outcome::Done)));
    assert!(by_path.contains(&(gone.as_path(), Outcome::Skipped)));
    // Recoverable: it's in the fake trash, contents intact.
    assert!(!dd.exists());
    assert_eq!(
        std::fs::read(base.join("fake-trash/DerivedData/App-1/blob"))
            .unwrap()
            .len(),
        10_000
    );
    assert_eq!(*trash.seen.lock().unwrap(), vec![dd.clone()]);

    let lines: Vec<serde_json::Value> = std::fs::read_to_string(&log_path)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(lines.len(), 2);
    let done = lines.iter().find(|l| l["outcome"] == "done").unwrap();
    assert_eq!(done["action"], "trash");
    assert_eq!(done["target"], dd.to_string_lossy().as_ref());
    assert!(done["destination"].as_str().unwrap().contains("fake-trash"));
    assert!(done["freed_bytes"].as_u64().unwrap() >= 10_000);
    let skipped = lines.iter().find(|l| l["outcome"] == "skipped").unwrap();
    assert_eq!(skipped["message"], "already gone");
}

#[test]
fn symlink_swaps_and_protected_paths_are_skipped() {
    let t = tempfile::tempdir().unwrap();
    let base = t.path().canonicalize().unwrap();
    let home = base.join("home");
    let real = home.join("real/cache");
    std::fs::create_dir_all(&real).unwrap();
    let other = base.join("precious");
    std::fs::create_dir_all(other.join("cache")).unwrap();
    std::fs::write(other.join("cache/keep"), b"keep").unwrap();

    let p = plan(
        &[cache_trash(&real, 1), cache_trash(&home, 1)],
        &Selection::default(),
        PlanOptions::default(),
    );
    // After planning, `home/real` becomes a symlink to somewhere precious.
    std::fs::remove_dir_all(home.join("real")).unwrap();
    std::os::unix::fs::symlink(&other, home.join("real")).unwrap();

    let trash = DirTrash::new(base.join("fake-trash"));
    let ex = Executor::new(&trash, home.clone(), None);
    let results = ex.execute(&p, &mut |_, _, _| {}).unwrap();
    assert!(
        results.iter().all(|r| r.outcome == Outcome::Skipped),
        "{results:?}"
    );
    assert!(other.join("cache/keep").exists());
    assert!(home.exists());
}

#[test]
fn permanent_delete_never_follows_symlinks() {
    let t = tempfile::tempdir().unwrap();
    let base = t.path().canonicalize().unwrap();
    let keep = base.join("keep");
    std::fs::create_dir_all(&keep).unwrap();
    std::fs::write(keep.join("f"), b"x").unwrap();
    let doomed = base.join("doomed");
    std::fs::create_dir_all(&doomed).unwrap();
    std::os::unix::fs::symlink(&keep, doomed.join("link")).unwrap();
    std::os::unix::fs::symlink(&keep, base.join("toplink")).unwrap();

    assert!(delete_permanently(&base.join("toplink")).is_err());
    delete_permanently(&doomed).unwrap();
    assert!(!doomed.exists());
    assert!(keep.join("f").exists(), "symlink target untouched");
}

#[test]
fn commands_run_without_a_shell_and_failures_are_reported() {
    use std::os::unix::fs::PermissionsExt;
    let t = tempfile::tempdir().unwrap();
    let base = t.path().canonicalize().unwrap();
    let bin = base.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let marker = base.join("ran");
    let ok = bin.join("okclean");
    std::fs::write(
        &ok,
        format!(
            "#!/bin/sh\necho \"$@\" > '{}'\necho cleaned\n",
            marker.display()
        ),
    )
    .unwrap();
    let bad = bin.join("badclean");
    std::fs::write(&bad, "#!/bin/sh\necho 'disk on fire' >&2\nexit 4\n").unwrap();
    for f in [&ok, &bad] {
        std::fs::set_permissions(f, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let mut a = cache_cmd("okclean", Tier::Regenerable, 9);
    a.cleanup = Some(Cleanup::command(vec![
        "okclean".into(),
        "--all".into(),
        "$(touch /tmp/pwned)".into(),
    ]));
    let b = cache_cmd("badclean", Tier::Regenerable, 9);
    let c = cache_cmd("missingtool", Tier::Regenerable, 9);
    let p = plan(&[a, b, c], &Selection::default(), PlanOptions::default());
    assert_eq!(p.items.len(), 3);

    let trash = DirTrash::new(base.join("t"));
    let mut ex = Executor::new(&trash, base.join("home"), None);
    ex.path_env = Some(bin.as_os_str().to_owned());
    let results = ex.execute(&p, &mut |_, _, _| {}).unwrap();
    let get = |kind: &str| {
        let i = p.items.iter().position(|i| i.kind == kind).unwrap();
        results[i].clone()
    };
    let ok = get("okclean");
    assert_eq!(ok.outcome, Outcome::Done);
    assert_eq!(ok.message, "cleaned");
    assert_eq!(
        std::fs::read_to_string(&marker).unwrap().trim(),
        "--all $(touch /tmp/pwned)",
        "arguments are passed verbatim, never through a shell"
    );
    let bad = get("badclean");
    assert_eq!(bad.outcome, Outcome::Failed);
    assert_eq!(bad.message, "exit 4: disk on fire");
    let missing = get("missingtool");
    assert_eq!(missing.outcome, Outcome::Skipped);
    assert!(missing.message.contains("no longer on PATH"));
}

/// Moves a real folder to the real system Trash. Opt-in so CI and normal test
/// runs never litter anyone's Trash: `LYSOSOME_TEST_REAL_TRASH=1 cargo test
/// -p lysosome-core real_trash -- --nocapture`. Prints where the item went;
/// remove it from the Trash afterwards.
#[test]
fn real_trash_is_recoverable() {
    if std::env::var_os("LYSOSOME_TEST_REAL_TRASH").is_none_or(|v| v != "1") {
        eprintln!("skipped: set LYSOSOME_TEST_REAL_TRASH=1 to run");
        return;
    }
    let t = tempfile::tempdir().unwrap();
    let stamp = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let name = format!("lysosome-trash-test-{stamp}");
    let dir = t.path().join(&name);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("payload.txt"), b"recover me").unwrap();
    let dest = SystemTrash.trash(&dir).expect("trash");
    assert!(!dir.exists());
    let candidates: Vec<PathBuf> = if cfg!(target_os = "macos") {
        let home = std::env::var_os("HOME").map(PathBuf::from).unwrap();
        vec![home.join(".Trash").join(&name)]
    } else {
        let data = std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                PathBuf::from(std::env::var_os("HOME").unwrap()).join(".local/share")
            });
        vec![data.join("Trash/files").join(&name)]
    };
    let found = candidates.iter().find(|c| c.join("payload.txt").exists());
    eprintln!("trashed to {dest}; found at {found:?}");
    let found = found.expect("item is in the Trash");
    assert_eq!(
        std::fs::read(found.join("payload.txt")).unwrap(),
        b"recover me"
    );
}

#[test]
fn docker_pseudo_paths_are_commands_and_never_trash_targets() {
    let mut img = cache_cmd("docker-image", Tier::Regenerable, 5);
    img.category = Category::Containers;
    img.path = PathBuf::from("docker://image/sha256:1111");
    img.cleanup = Some(Cleanup::command(vec![
        "docker".into(),
        "image".into(),
        "rm".into(),
        "web:1.0".into(),
    ]));
    let mut bogus = img.clone();
    bogus.id = "docker-image:bogus".into();
    bogus.cleanup = Some(Cleanup::trash(vec!["docker://image/sha256:1111".into()]));
    let mut pnpm = cache_cmd("pnpm-store", Tier::Regenerable, 7);
    pnpm.details = json!({"name": "pnpm store", "size_is_upper_bound": true});
    let p = plan(
        &[img, bogus, pnpm],
        &Selection::default(),
        PlanOptions::default(),
    );
    assert_eq!(p.items.len(), 2, "{:?}", p.items);
    let docker = p.items.iter().find(|i| i.kind == "docker-image").unwrap();
    assert_eq!(docker.action, ActionKind::Command);
    assert_eq!(
        docker.guard,
        Guard::Command {
            existing_path: None
        }
    );
    assert_eq!(p.excluded.nothing_to_do.items, 1);
    assert!(
        p.items
            .iter()
            .find(|i| i.kind == "pnpm-store")
            .unwrap()
            .size_is_upper_bound
    );
}
