//! Android emulators (AVDs) and SDK system images. `regenerable_expensive`:
//! recreated / re-downloaded through Android Studio or `sdkmanager`.

use std::path::{Path, PathBuf};

use super::{file_name, finish, first_dir, subdirs, Item, SystemEnv};
use crate::detector::ScanContext;
use crate::model::{Category, Finding, Tier};

const CAT: Category = Category::Android;

pub(super) const TABLE: [(&str, Category, super::ScanFn); 2] = [
    ("android-avd", CAT, avds),
    ("android-system-images", CAT, system_images),
];

/// `$ANDROID_HOME`, `$ANDROID_SDK_ROOT`, `~/Library/Android/sdk`, `~/Android/Sdk`.
fn sdk_dir(env: &SystemEnv, home: &Path) -> Option<PathBuf> {
    let mut c = Vec::new();
    c.extend(env.var_path("ANDROID_HOME"));
    c.extend(env.var_path("ANDROID_SDK_ROOT"));
    c.push(home.join("Library/Android/sdk"));
    c.push(home.join("Android/Sdk"));
    first_dir(c)
}

/// An SDK command-line tool, by absolute path (they are rarely on PATH).
fn sdk_tool(sdk: Option<&Path>, name: &str) -> Option<String> {
    let sdk = sdk?;
    [
        sdk.join("cmdline-tools/latest/bin").join(name),
        sdk.join("tools/bin").join(name),
    ]
    .into_iter()
    .find(|p| p.is_file())
    .map(|p| p.to_string_lossy().into_owned())
}

fn avds(env: &SystemEnv, ctx: &ScanContext<'_>) -> Vec<Finding> {
    let mut c = Vec::new();
    c.extend(env.var_path("ANDROID_AVD_HOME"));
    c.extend(env.var_path("ANDROID_USER_HOME").map(|h| h.join("avd")));
    c.push(ctx.home.join(".android/avd"));
    let Some(root) = first_dir(c) else {
        return Vec::new();
    };
    let avdmanager = sdk_tool(sdk_dir(env, &ctx.home).as_deref(), "avdmanager");
    let items = subdirs(&root)
        .into_iter()
        .filter_map(|dir| {
            let folder = file_name(&dir);
            let name = folder.strip_suffix(".avd")?.to_string();
            let ini = root.join(format!("{name}.ini"));
            let mut item = Item::new(
                "android-avd",
                CAT,
                dir,
                name.clone(),
                Tier::RegenerableExpensive,
            )
            .reason("emulator disk and snapshots; recreate it in Android Studio")
            .detail("avd", name.clone());
            match &avdmanager {
                Some(tool) => {
                    item = item.command([tool.as_str(), "delete", "avd", "-n", name.as_str()]);
                }
                None if ini.is_file() => item.extra_trash.push(ini),
                None => {}
            }
            Some(item)
        })
        .collect();
    finish(items)
}

fn system_images(env: &SystemEnv, ctx: &ScanContext<'_>) -> Vec<Finding> {
    let sdk = sdk_dir(env, &ctx.home);
    let Some(images) = sdk
        .as_ref()
        .and_then(|s| first_dir([s.join("system-images")]))
    else {
        return Vec::new();
    };
    let sdkmanager = sdk_tool(sdk.as_deref(), "sdkmanager");
    let mut items = Vec::new();
    // system-images/<api>/<tag>/<abi>
    for api in subdirs(&images) {
        for tag in subdirs(&api) {
            for abi in subdirs(&tag) {
                let parts = [file_name(&api), file_name(&tag), file_name(&abi)];
                let name = parts.join("/");
                let package = format!("system-images;{}", parts.join(";"));
                let mut item = Item::new(
                    "android-system-image",
                    CAT,
                    abi,
                    name,
                    Tier::RegenerableExpensive,
                )
                .reason("re-downloaded with the SDK Manager")
                .detail("package", package.clone());
                if let Some(tool) = &sdkmanager {
                    item = item.command([tool.clone(), "--uninstall".into(), package]);
                }
                items.push(item);
            }
        }
    }
    finish(items)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{CleanupAction, Status};
    use crate::system::testutil::{ctx, write};

    #[test]
    fn finds_avds_and_system_images() {
        let t = tempfile::tempdir().unwrap();
        let h = t.path();
        write(&h.join(".android/avd/Pixel_8.avd/userdata.img"), 20_000);
        std::fs::write(h.join(".android/avd/Pixel_8.ini"), "path=x\n").unwrap();
        write(
            &h.join("sdk/system-images/android-34/google_apis/arm64-v8a/system.img"),
            30_000,
        );
        let env = SystemEnv::isolated([("ANDROID_HOME", h.join("sdk"))]);
        let c = ctx(h);
        let a = avds(&env, &c);
        assert_eq!(a.len(), 1);
        assert_eq!(a[0].details["name"], "Pixel_8");
        assert_eq!(a[0].status, Status::Removable);
        let cl = a[0].cleanup.as_ref().unwrap();
        assert_eq!(cl.action, CleanupAction::Trash);
        assert_eq!(cl.paths.len(), 2, "the .avd folder and its .ini");
        let s = system_images(&env, &c);
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].details["name"], "android-34/google_apis/arm64-v8a");
        assert_eq!(
            s[0].details["package"],
            "system-images;android-34;google_apis;arm64-v8a"
        );
        assert!(s[0].details["redownload_bytes"].as_u64().unwrap() >= 30_000);
    }
}
