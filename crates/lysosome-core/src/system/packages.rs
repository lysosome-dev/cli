//! Package-manager caches. All `regenerable`: they are re-filled on demand.
//! Where a tool has its own cleanup command, findings carry it; otherwise
//! the folder goes to the Trash.

use std::path::{Path, PathBuf};

use super::{finish, first_dir, Item, SystemEnv, Tools};
use crate::detector::ScanContext;
use crate::model::{Category, Finding, Tier};

const CAT: Category = Category::PackageCaches;

pub(super) const TABLE: [(&str, Category, super::ScanFn); 13] = [
    ("npm", CAT, npm),
    ("pnpm", CAT, pnpm),
    ("yarn", CAT, yarn),
    ("bun", CAT, bun),
    ("pip", CAT, pip),
    ("uv", CAT, uv),
    ("conda", CAT, conda),
    ("cargo", CAT, cargo),
    ("go", CAT, go),
    ("gradle", CAT, gradle),
    ("maven", CAT, maven),
    ("homebrew", CAT, homebrew),
    ("cocoapods", CAT, cocoapods),
];

const REFILLED: &str = "package cache, re-downloaded on demand";

fn item(kind: &'static str, path: PathBuf, name: &str) -> Item {
    Item::new(kind, CAT, path, name, Tier::Regenerable).reason(REFILLED)
}

fn one(i: Option<Item>) -> Vec<Finding> {
    finish(i.into_iter().collect())
}

/// Platform cache folder: `~/Library/Caches/<mac>` on macOS, else `$XDG_CACHE_HOME/<linux>`.
fn os_cache(env: &SystemEnv, home: &Path, mac: &str, linux: &str) -> PathBuf {
    if cfg!(target_os = "macos") {
        home.join("Library/Caches").join(mac)
    } else {
        env.xdg_cache(home).join(linux)
    }
}

/// Ask the tool (if installed) for its folder, then try well-known fallbacks.
fn resolve(
    tools: &Tools,
    program: &str,
    args: &[&str],
    fallbacks: impl IntoIterator<Item = PathBuf>,
) -> Option<PathBuf> {
    tools
        .dir_output(program, args)
        .or_else(|| first_dir(fallbacks))
}

fn npm(env: &SystemEnv, ctx: &ScanContext<'_>) -> Vec<Finding> {
    let base = env
        .var_path("npm_config_cache")
        .unwrap_or_else(|| ctx.home.join(".npm"));
    one(first_dir([base.join("_cacache")])
        .map(|p| item("npm-cache", p, "npm cache").command(["npm", "cache", "clean", "--force"])))
}

/// `key` from a simple rc file: `key=value` (`.npmrc`, pnpm's `rc`) or
/// `key value` / `key "value"` (`.yarnrc`). `~/` is expanded; anything that
/// isn't then an absolute path is ignored.
fn rc_path(file: &Path, key: &str, home: &Path) -> Option<PathBuf> {
    let text = std::fs::read_to_string(file).ok()?;
    text.lines().find_map(|l| {
        let rest = l.trim().strip_prefix(key)?;
        if !rest.starts_with(['=', ' ', '\t']) {
            return None; // `keyfoo=...`, a different key
        }
        let v = rest.trim_start();
        let v = v.strip_prefix('=').unwrap_or(v).trim();
        let v = v.trim_matches('"');
        let p = match v.strip_prefix("~/") {
            Some(r) => home.join(r),
            None => PathBuf::from(v),
        };
        p.is_absolute().then_some(p)
    })
}

/// The newest `v<N>` layout folder inside a store/cache root (what the tool
/// itself reports and what its cleanup command clears), else the root.
fn versioned(root: PathBuf) -> PathBuf {
    super::subdirs(&root)
        .into_iter()
        .filter_map(|d| {
            let n = d
                .file_name()?
                .to_str()?
                .strip_prefix('v')?
                .parse::<u32>()
                .ok()?;
            Some((n, d))
        })
        .max_by_key(|(n, _)| *n)
        .map_or(root, |(_, d)| d)
}

/// Whether the `pnpm` on PATH is a real pnpm install (npm global, Homebrew,
/// the standalone installer, `@pnpm/exe`): some folder above it is named
/// `pnpm…` / `@pnpm`. Corepack, Volta and version-manager shims (asdf, mise,
/// nodenv) are not, and may run corepack behind the shim.
fn real_pnpm(tools: &Tools) -> bool {
    let Some(exe) = tools.which("pnpm") else {
        return false;
    };
    let resolved = std::fs::canonicalize(&exe).unwrap_or(exe);
    resolved.parent().is_some_and(|dir| {
        dir.ancestors()
            .filter_map(|a| a.file_name()?.to_str())
            .any(|n| n.starts_with("pnpm") || n == "@pnpm")
    })
}

/// pnpm's store. Asks `pnpm store path` only when `pnpm` is a real install;
/// behind corepack (or a Volta / version-manager shim, which may hand off to
/// corepack) asking could download pnpm, so then the configured and default
/// store locations are checked instead.
fn pnpm(env: &SystemEnv, ctx: &ScanContext<'_>) -> Vec<Finding> {
    let h = &ctx.home;
    let tools = env.tools(h);
    let asked = real_pnpm(&tools)
        .then(|| tools.dir_output("pnpm", ["store", "path"]))
        .flatten();
    let found = asked.or_else(|| {
        let mut c = Vec::new();
        for k in [
            "pnpm_config_store_dir",
            "npm_config_store_dir",
            "NPM_CONFIG_STORE_DIR",
        ] {
            c.extend(env.var_path(k));
        }
        let config = env
            .var_path("XDG_CONFIG_HOME")
            .unwrap_or_else(|| h.join(".config"));
        let mut rcs = vec![config.join("pnpm/rc")];
        if cfg!(target_os = "macos") {
            rcs.push(h.join("Library/Preferences/pnpm/rc"));
        }
        rcs.push(
            env.var_path("NPM_CONFIG_USERCONFIG")
                .unwrap_or_else(|| h.join(".npmrc")),
        );
        c.extend(rcs.iter().filter_map(|rc| rc_path(rc, "store-dir", h)));
        // pnpm's own default order: $PNPM_HOME, $XDG_DATA_HOME, the OS default.
        c.extend(env.var_path("PNPM_HOME").map(|p| p.join("store")));
        c.extend(env.var_path("XDG_DATA_HOME").map(|p| p.join("pnpm/store")));
        if cfg!(target_os = "macos") {
            c.push(h.join("Library/pnpm/store"));
        }
        c.push(h.join(".local/share/pnpm/store"));
        c.push(h.join(".pnpm-store"));
        first_dir(c).map(versioned)
    });
    one(found.map(|p| {
        item("pnpm-store", p, "pnpm store")
            .command(["pnpm", "store", "prune"])
            .reason("package store; `pnpm store prune` frees only packages no project uses, so this size is an upper bound")
            .detail("size_is_upper_bound", true)
    }))
}

/// Yarn's cache, from `YARN_CACHE_FOLDER`, `~/.yarnrc` or the default
/// locations. Yarn is never asked: `yarn cache dir` creates the cache folder
/// when it is missing (and through corepack can download Yarn itself), which a
/// scan must not do.
fn yarn(env: &SystemEnv, ctx: &ScanContext<'_>) -> Vec<Finding> {
    let h = &ctx.home;
    let mut c = Vec::new();
    c.extend(env.var_path("YARN_CACHE_FOLDER"));
    c.extend(rc_path(&h.join(".yarnrc"), "cache-folder", h));
    c.push(os_cache(env, h, "Yarn", "yarn"));
    // Yarn 2+ global cache: `$XDG_DATA_HOME/yarn/berry` or `~/.yarn/berry`.
    c.extend(
        env.var_path("XDG_DATA_HOME")
            .map(|p| p.join("yarn/berry/cache")),
    );
    c.push(h.join(".yarn/berry/cache"));
    one(first_dir(c).map(|p| {
        // Yarn 2+ keeps a global mirror that only `--mirror` clears.
        if p.ends_with("berry/cache") {
            item("yarn-cache", p, "Yarn cache").command(["yarn", "cache", "clean", "--mirror"])
        } else {
            item("yarn-cache", versioned(p), "Yarn cache").command(["yarn", "cache", "clean"])
        }
    }))
}

fn bun(env: &SystemEnv, ctx: &ScanContext<'_>) -> Vec<Finding> {
    let mut c = Vec::new();
    c.extend(env.var_path("BUN_INSTALL_CACHE_DIR"));
    c.extend(env.var_path("BUN_INSTALL").map(|b| b.join("install/cache")));
    c.push(ctx.home.join(".bun/install/cache"));
    one(first_dir(c)
        .map(|p| item("bun-cache", p, "Bun cache").command(["bun", "pm", "cache", "rm"])))
}

fn pip(env: &SystemEnv, ctx: &ScanContext<'_>) -> Vec<Finding> {
    let h = &ctx.home;
    let mut fallbacks = Vec::new();
    fallbacks.extend(env.var_path("PIP_CACHE_DIR"));
    fallbacks.push(os_cache(env, h, "pip", "pip"));
    one(resolve(&env.tools(h), "pip", &["cache", "dir"], fallbacks)
        .map(|p| item("pip-cache", p, "pip cache").command(["pip", "cache", "purge"])))
}

fn uv(env: &SystemEnv, ctx: &ScanContext<'_>) -> Vec<Finding> {
    let h = &ctx.home;
    let mut fallbacks = Vec::new();
    fallbacks.extend(env.var_path("UV_CACHE_DIR"));
    fallbacks.push(env.xdg_cache(h).join("uv"));
    one(resolve(&env.tools(h), "uv", &["cache", "dir"], fallbacks)
        .map(|p| item("uv-cache", p, "uv cache").command(["uv", "cache", "clean"])))
}

/// conda's package cache. `conda info` is slow, so check the usual install
/// locations instead.
fn conda(env: &SystemEnv, ctx: &ScanContext<'_>) -> Vec<Finding> {
    let h = &ctx.home;
    let mut c: Vec<PathBuf> = Vec::new();
    if let Some(dirs) = env.var("CONDA_PKGS_DIRS") {
        c.extend(std::env::split_paths(dirs).filter(|p| p.is_absolute()));
    }
    for base in [
        "miniconda3",
        "anaconda3",
        "miniforge3",
        "mambaforge",
        "opt/miniconda3",
        "opt/anaconda3",
        "opt/miniforge3",
        ".conda",
    ] {
        c.push(h.join(base).join("pkgs"));
    }
    let mut seen = std::collections::HashSet::new();
    let items = c
        .into_iter()
        .filter_map(super::real_dir)
        .filter(|p| seen.insert(p.clone()))
        .map(|p| {
            item("conda-pkgs", p, "conda package cache")
                .command(["conda", "clean", "--all", "--yes"])
                .reason("package cache; files hard-linked into environments stay until those environments are removed, so this size is an upper bound")
                .detail("size_is_upper_bound", true)
        })
        .collect();
    finish(items)
}

fn cargo(env: &SystemEnv, ctx: &ScanContext<'_>) -> Vec<Finding> {
    let home = env
        .var_path("CARGO_HOME")
        .unwrap_or_else(|| ctx.home.join(".cargo"));
    let items = [
        ("cargo-registry", "registry", "Cargo registry cache"),
        ("cargo-git", "git", "Cargo git checkouts"),
    ]
    .into_iter()
    .filter_map(|(kind, sub, name)| super::real_dir(home.join(sub)).map(|p| item(kind, p, name)))
    .collect();
    finish(items)
}

fn go(env: &SystemEnv, ctx: &ScanContext<'_>) -> Vec<Finding> {
    let h = &ctx.home;
    let tools = env.tools(h);
    let (mut modcache, mut buildcache) = (None, None);
    if let Some(out) = tools.output("go", ["env", "GOMODCACHE", "GOCACHE"]) {
        let mut lines = out.lines().map(|l| PathBuf::from(l.trim()));
        modcache = lines.next().and_then(super::real_dir);
        buildcache = lines.next().and_then(super::real_dir);
    }
    let gopath = env.var_path("GOPATH").unwrap_or_else(|| h.join("go"));
    let modcache = modcache.or_else(|| first_dir([gopath.join("pkg/mod")]));
    let buildcache = buildcache.or_else(|| first_dir([os_cache(env, h, "go-build", "go-build")]));
    let mut items = Vec::new();
    if let Some(p) = modcache {
        items.push(item("go-modcache", p, "Go module cache").command(["go", "clean", "-modcache"]));
    }
    if let Some(p) = buildcache {
        items.push(
            item("go-buildcache", p, "Go build cache")
                .command(["go", "clean", "-cache"])
                .reason("build cache, rebuilt on the next `go build`"),
        );
    }
    finish(items)
}

fn gradle(env: &SystemEnv, ctx: &ScanContext<'_>) -> Vec<Finding> {
    let base = env
        .var_path("GRADLE_USER_HOME")
        .unwrap_or_else(|| ctx.home.join(".gradle"));
    one(first_dir([base.join("caches")]).map(|p| item("gradle-cache", p, "Gradle caches")))
}

fn maven(env: &SystemEnv, ctx: &ScanContext<'_>) -> Vec<Finding> {
    let _ = env;
    one(first_dir([ctx.home.join(".m2/repository")]).map(|p| {
        item("maven-repository", p, "Maven local repository").reason(
            "dependency cache, re-downloaded on demand (artifacts you `mvn install`ed locally are rebuilt from source)",
        )
    }))
}

fn homebrew(env: &SystemEnv, ctx: &ScanContext<'_>) -> Vec<Finding> {
    let h = &ctx.home;
    let mut fallbacks = Vec::new();
    fallbacks.extend(env.var_path("HOMEBREW_CACHE"));
    fallbacks.push(os_cache(env, h, "Homebrew", "Homebrew"));
    one(
        resolve(&env.tools(h), "brew", &["--cache"], fallbacks).map(|p| {
            item("homebrew-cache", p, "Homebrew downloads")
                .command(["brew", "cleanup", "-s"])
                .reason("downloaded bottles and source archives, re-downloaded on demand")
        }),
    )
}

fn cocoapods(env: &SystemEnv, ctx: &ScanContext<'_>) -> Vec<Finding> {
    let _ = env;
    one(
        first_dir([ctx.home.join("Library/Caches/CocoaPods")]).map(|p| {
            item("cocoapods-cache", p, "CocoaPods cache")
                .command(["pod", "cache", "clean", "--all"])
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Status;
    use crate::system::testutil::{bare_env, by_kind, ctx, write};

    #[test]
    fn finds_fallback_locations_without_tools() {
        let t = tempfile::tempdir().unwrap();
        let h = t.path();
        write(&h.join(".npm/_cacache/content-v2/x"), 10_000);
        write(&h.join(".cargo/registry/cache/a.crate"), 10_000);
        write(&h.join(".cargo/git/db/x"), 10_000);
        write(&h.join(".gradle/caches/x"), 10_000);
        write(&h.join(".m2/repository/x.jar"), 10_000);
        write(&h.join(".bun/install/cache/x"), 10_000);
        write(&h.join("go/pkg/mod/cache/x"), 10_000);
        write(&h.join("miniconda3/pkgs/x"), 10_000);
        let env = bare_env();
        let c = ctx(h);
        let mut all = Vec::new();
        for (_, _, f) in TABLE {
            all.extend(f(&env, &c));
        }
        for kind in [
            "npm-cache",
            "cargo-registry",
            "cargo-git",
            "gradle-cache",
            "maven-repository",
            "bun-cache",
            "go-modcache",
            "conda-pkgs",
        ] {
            let f = by_kind(&all, kind);
            assert_eq!(f.len(), 1, "{kind}");
            assert_eq!(f[0].status, Status::Removable);
            assert_eq!(f[0].tier, Tier::Regenerable);
            assert!(f[0].size_bytes >= 10_000);
            assert!(f[0].cleanup.is_some());
        }
        let npm = by_kind(&all, "npm-cache")[0];
        assert_eq!(
            npm.cleanup.as_ref().unwrap().command,
            ["npm", "cache", "clean", "--force"]
        );
        let cargo = by_kind(&all, "cargo-registry")[0];
        assert_eq!(
            cargo.cleanup.as_ref().unwrap().paths,
            [h.join(".cargo/registry")]
        );
        assert!(by_kind(&all, "pip-cache").is_empty());
    }

    fn exe(path: &Path, body: &str) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[test]
    fn rc_files_and_versioned_layouts() {
        let t = tempfile::tempdir().unwrap();
        let h = t.path();
        std::fs::write(
            h.join(".npmrc"),
            "store-dir-other=/nope\nregistry=https://x\n  store-dir = ~/big/store\n",
        )
        .unwrap();
        assert_eq!(
            rc_path(&h.join(".npmrc"), "store-dir", h),
            Some(h.join("big/store"))
        );
        std::fs::write(h.join(".yarnrc"), "cache-folder \"/abs/yarn\"\n").unwrap();
        assert_eq!(
            rc_path(&h.join(".yarnrc"), "cache-folder", h),
            Some(PathBuf::from("/abs/yarn"))
        );
        std::fs::write(h.join("rel"), "store-dir=relative/path\n").unwrap();
        assert_eq!(rc_path(&h.join("rel"), "store-dir", h), None);
        assert_eq!(rc_path(&h.join("missing"), "store-dir", h), None);

        for d in ["root/v3", "root/v10", "root/v9", "root/vx", "flat/files"] {
            std::fs::create_dir_all(h.join(d)).unwrap();
        }
        assert_eq!(versioned(h.join("root")), h.join("root/v10"));
        assert_eq!(versioned(h.join("flat")), h.join("flat"));
    }

    #[test]
    fn pnpm_is_asked_only_when_it_is_a_real_install() {
        let t = tempfile::tempdir().unwrap();
        let h = t.path().join("home");
        let marker = t.path().join("asked");
        let reported = t.path().join("reported-store");
        write(&reported.join("x"), 10_000);
        let default = h.join(if cfg!(target_os = "macos") {
            "Library/pnpm/store/v10"
        } else {
            ".local/share/pnpm/store/v10"
        });
        write(&default.join("files/x"), 10_000);
        let body = format!(
            "#!/bin/sh\n/usr/bin/touch '{}'\necho '{}'\n",
            marker.display(),
            reported.display()
        );
        let bin = t.path().join("bin");
        let run = || {
            let env = SystemEnv::isolated([("PATH", bin.clone())]);
            pnpm(&env, &ctx(&h))
        };

        // An asdf/mise-style shim (may run corepack behind it): never asked.
        exe(&bin.join("pnpm"), &format!("{body}# asdf exec pnpm\n"));
        let f = run();
        assert!(!marker.exists(), "a version-manager shim was run");
        assert_eq!(f[0].path, default);

        // npm-global layout: bin/pnpm -> ../lib/node_modules/pnpm/bin/pnpm.cjs
        std::fs::remove_file(bin.join("pnpm")).unwrap();
        let real = t.path().join("lib/node_modules/pnpm/bin/pnpm.cjs");
        exe(&real, &body);
        std::os::unix::fs::symlink(&real, bin.join("pnpm")).unwrap();
        let f = run();
        assert!(marker.exists());
        assert_eq!(f[0].path, reported);
    }

    #[test]
    fn yarn_reads_yarnrc_and_xdg_berry() {
        let t = tempfile::tempdir().unwrap();
        let h = t.path();
        write(&h.join("custom/v6/npm-a/x"), 10_000);
        std::fs::write(h.join(".yarnrc"), "cache-folder ~/custom\n").unwrap();
        let f = yarn(&bare_env(), &ctx(h));
        assert_eq!(f[0].path, h.join("custom/v6"));
        assert_eq!(
            f[0].cleanup.as_ref().unwrap().command,
            ["yarn", "cache", "clean"]
        );

        std::fs::remove_file(h.join(".yarnrc")).unwrap();
        std::fs::remove_dir_all(h.join("custom")).unwrap();
        write(&h.join("data/yarn/berry/cache/a.zip"), 10_000);
        let env = SystemEnv::isolated([("XDG_DATA_HOME", h.join("data"))]);
        let f = yarn(&env, &ctx(h));
        assert_eq!(f[0].path, h.join("data/yarn/berry/cache"));
        assert_eq!(
            f[0].cleanup.as_ref().unwrap().command,
            ["yarn", "cache", "clean", "--mirror"]
        );
    }
}
