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

/// `key` from a simple rc file: `key=value` (`.npmrc`, pnpm's `rc`, Go's env
/// file) or `key value` / `key "value"` (`.yarnrc`). The last setting wins, as
/// in the tools themselves. `~/`, `$HOME/` and `${HOME}/` are expanded;
/// anything that isn't then an absolute path is ignored.
fn rc_path(file: &Path, key: &str, home: &Path) -> Option<PathBuf> {
    let text = std::fs::read_to_string(file).ok()?;
    let value = text.lines().rev().find_map(|l| {
        let rest = l.trim().strip_prefix(key)?;
        if !rest.starts_with(['=', ' ', '\t']) {
            return None; // `keyfoo=...`, a different key
        }
        let v = rest.trim_start();
        Some(v.strip_prefix('=').unwrap_or(v).trim().trim_matches('"'))
    })?;
    let p = ["~/", "$HOME/", "${HOME}/"]
        .iter()
        .find_map(|pre| value.strip_prefix(pre))
        .map_or_else(|| PathBuf::from(value), |r| home.join(r));
    p.is_absolute().then_some(p)
}

/// Whether the `pnpm` on PATH is a real pnpm install: it resolves to a file
/// named `pnpm…` that is either inside the `pnpm` / `@pnpm/*` npm package
/// (npm global, Homebrew's `Cellar/pnpm`) or a native executable (the
/// standalone installer, `@pnpm/exe`). Corepack, Volta, asdf, mise and nodenv
/// shims are none of these, and may run corepack behind the shim.
fn real_pnpm(tools: &Tools) -> bool {
    let Some(exe) = tools.which("pnpm") else {
        return false;
    };
    let resolved = std::fs::canonicalize(&exe).unwrap_or(exe);
    if !resolved
        .file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.starts_with("pnpm"))
    {
        return false;
    }
    let names: Vec<&str> = resolved
        .components()
        .filter_map(|c| c.as_os_str().to_str())
        .collect();
    let in_package = names.windows(2).any(|w| {
        (w[0] == "node_modules" && (w[1] == "pnpm" || w[1] == "@pnpm"))
            || (w[0] == "Cellar" && w[1] == "pnpm")
    });
    in_package || !is_script(&resolved)
}

/// Whether `p` starts with `#!`.
fn is_script(p: &Path) -> bool {
    use std::io::Read;
    let mut head = [0u8; 2];
    std::fs::File::open(p)
        .and_then(|mut f| f.read_exact(&mut head))
        .is_ok_and(|()| &head == b"#!")
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
        first_dir(c)
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
            item("yarn-cache", p, "Yarn cache").command(["yarn", "cache", "clean"])
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

/// Go's module and build caches, resolved the way the go command does:
/// environment, then the `go env -w` file, then the defaults. `go env` itself
/// is never run: since Go 1.23 every go command updates local telemetry
/// counters on disk, which a scan must not do.
fn go(env: &SystemEnv, ctx: &ScanContext<'_>) -> Vec<Finding> {
    let h = &ctx.home;
    let env_file = env.var_path("GOENV").unwrap_or_else(|| {
        if cfg!(target_os = "macos") {
            h.join("Library/Application Support/go/env")
        } else {
            env.var_path("XDG_CONFIG_HOME")
                .unwrap_or_else(|| h.join(".config"))
                .join("go/env")
        }
    });
    let setting = |key: &str| env.var_path(key).or_else(|| rc_path(&env_file, key, h));
    let gopath = env
        .var("GOPATH")
        .and_then(|v| std::env::split_paths(v).next())
        .filter(|p| p.is_absolute())
        .or_else(|| rc_path(&env_file, "GOPATH", h))
        .unwrap_or_else(|| h.join("go"));
    let modcache = first_dir(
        setting("GOMODCACHE")
            .into_iter()
            .chain([gopath.join("pkg/mod")]),
    );
    let buildcache = first_dir(
        setting("GOCACHE")
            .into_iter()
            .chain([os_cache(env, h, "go-build", "go-build")]),
    );
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
    fn rc_files_last_setting_wins_and_home_expands() {
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
        std::fs::write(
            h.join("twice"),
            "store-dir=/old\nstore-dir=${HOME}/new\nGOPATH=$HOME/gp\n",
        )
        .unwrap();
        assert_eq!(
            rc_path(&h.join("twice"), "store-dir", h),
            Some(h.join("new"))
        );
        assert_eq!(rc_path(&h.join("twice"), "GOPATH", h), Some(h.join("gp")));
    }

    #[test]
    fn pnpm_is_asked_only_when_it_is_a_real_install() {
        let t = tempfile::tempdir().unwrap();
        let h = t.path().join("home");
        let marker = t.path().join("asked");
        let reported = t.path().join("reported-store");
        write(&reported.join("x"), 10_000);
        let default = h.join(if cfg!(target_os = "macos") {
            "Library/pnpm/store"
        } else {
            ".local/share/pnpm/store"
        });
        write(&default.join("v10/files/x"), 10_000);
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

        // An asdf/nodenv-style shim script (may run corepack behind it), even
        // under a `pnpm…` folder: never asked.
        exe(
            &t.path().join("pnpm-dev/shims/pnpm"),
            &format!("{body}# asdf exec pnpm\n"),
        );
        std::fs::create_dir_all(&bin).unwrap();
        std::os::unix::fs::symlink(t.path().join("pnpm-dev/shims/pnpm"), bin.join("pnpm")).unwrap();
        let f = run();
        assert!(!marker.exists(), "a version-manager shim was run");
        assert_eq!(f[0].path, default);
        // A mise-style shim: a link to another program.
        std::fs::remove_file(bin.join("pnpm")).unwrap();
        exe(&t.path().join("mise/bin/mise"), &body);
        std::os::unix::fs::symlink(t.path().join("mise/bin/mise"), bin.join("pnpm")).unwrap();
        run();
        assert!(!marker.exists(), "a mise shim was run");

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
        assert_eq!(f[0].path, h.join("custom"));
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

    #[test]
    fn go_reads_env_then_go_env_file_then_defaults() {
        let t = tempfile::tempdir().unwrap();
        let h = t.path();
        write(&h.join("go/pkg/mod/cache/x"), 10_000);
        let f = go(&bare_env(), &ctx(h));
        assert_eq!(by_kind(&f, "go-modcache")[0].path, h.join("go/pkg/mod"));

        let file = h.join("goenv");
        write(&h.join("custom-mod/x"), 10_000);
        write(&h.join("custom-build/x"), 10_000);
        std::fs::write(
            &file,
            "GOMODCACHE=$HOME/custom-mod\nGOCACHE=${HOME}/custom-build\n",
        )
        .unwrap();
        let env = SystemEnv::isolated([("GOENV", file)]);
        let f = go(&env, &ctx(h));
        assert_eq!(by_kind(&f, "go-modcache")[0].path, h.join("custom-mod"));
        assert_eq!(by_kind(&f, "go-buildcache")[0].path, h.join("custom-build"));
    }
}
