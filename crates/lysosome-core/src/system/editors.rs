//! Editor caches: VS Code, Cursor and JetBrains IDEs. `regenerable`.

use std::path::{Path, PathBuf};

use super::{file_name, finish, real_dir, subdirs, Item, SystemEnv};
use crate::detector::ScanContext;
use crate::model::{Category, Finding, Tier};

const CAT: Category = Category::Editors;

pub(super) const TABLE: [(&str, Category, super::ScanFn); 3] = [
    ("vscode", CAT, vscode),
    ("cursor", CAT, cursor),
    ("jetbrains", CAT, jetbrains),
];

/// Cache folders inside an Electron editor's support folder.
const ELECTRON_CACHES: [&str; 4] = [
    "Cache",
    "CachedData",
    "CachedExtensionVSIXs",
    "Service Worker/CacheStorage",
];

/// `~/Library/Application Support/<app>` on macOS, `$XDG_CONFIG_HOME/<app>` elsewhere.
fn support_dir(env: &SystemEnv, home: &Path, app: &str) -> PathBuf {
    if cfg!(target_os = "macos") {
        home.join("Library/Application Support").join(app)
    } else {
        env.var_path("XDG_CONFIG_HOME")
            .unwrap_or_else(|| home.join(".config"))
            .join(app)
    }
}

fn electron(
    env: &SystemEnv,
    home: &Path,
    app: &str,
    label: &str,
    kind: &'static str,
) -> Vec<Finding> {
    let Some(base) = real_dir(support_dir(env, home, app)) else {
        return Vec::new();
    };
    let items = ELECTRON_CACHES
        .iter()
        .filter_map(|sub| {
            real_dir(base.join(sub)).map(|p| {
                Item::new(kind, CAT, p, format!("{label} {sub}"), Tier::Regenerable)
                    .reason("editor cache, rebuilt automatically (quit the editor before cleaning)")
            })
        })
        .collect();
    finish(items)
}

fn vscode(env: &SystemEnv, ctx: &ScanContext<'_>) -> Vec<Finding> {
    electron(env, &ctx.home, "Code", "VS Code", "vscode-cache")
}

fn cursor(env: &SystemEnv, ctx: &ScanContext<'_>) -> Vec<Finding> {
    electron(env, &ctx.home, "Cursor", "Cursor", "cursor-cache")
}

fn jetbrains(env: &SystemEnv, ctx: &ScanContext<'_>) -> Vec<Finding> {
    let root = if cfg!(target_os = "macos") {
        ctx.home.join("Library/Caches/JetBrains")
    } else {
        env.xdg_cache(&ctx.home).join("JetBrains")
    };
    let items = subdirs(&root)
        .into_iter()
        .map(|p| {
            let product = file_name(&p);
            Item::new(
                "jetbrains-cache",
                CAT,
                p,
                format!("JetBrains {product}"),
                Tier::Regenerable,
            )
            .reason("IDE caches and indexes, rebuilt on next launch")
            .detail("product", product)
        })
        .collect();
    finish(items)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::system::testutil::{bare_env, ctx, write};

    #[test]
    fn finds_editor_caches() {
        let t = tempfile::tempdir().unwrap();
        let h = t.path();
        let (code, jb) = if cfg!(target_os = "macos") {
            (
                h.join("Library/Application Support/Code"),
                h.join("Library/Caches/JetBrains"),
            )
        } else {
            (h.join(".config/Code"), h.join(".cache/JetBrains"))
        };
        write(&code.join("CachedData/abc/x"), 10_000);
        write(&code.join("Service Worker/CacheStorage/x"), 10_000);
        write(&code.join("User/settings.json"), 10_000);
        write(&jb.join("IntelliJIdea2025.2/index/x"), 10_000);
        let env = bare_env();
        let c = ctx(h);
        let v = vscode(&env, &c);
        let mut names: Vec<_> = v
            .iter()
            .map(|f| f.details["name"].as_str().unwrap())
            .collect();
        names.sort();
        assert_eq!(
            names,
            ["VS Code CachedData", "VS Code Service Worker/CacheStorage"]
        );
        assert!(cursor(&env, &c).is_empty());
        let j = jetbrains(&env, &c);
        assert_eq!(j.len(), 1);
        assert_eq!(j[0].details["product"], "IntelliJIdea2025.2");
    }
}
