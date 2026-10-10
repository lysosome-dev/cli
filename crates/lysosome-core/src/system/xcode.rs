//! Xcode (macOS only): DerivedData, device support files, simulators and
//! archives.

use std::path::{Path, PathBuf};

use serde_json::Value;

use super::{file_name, finish, measure, parse_timestamp, real_dir, subdirs, Item, SystemEnv};
use crate::detector::ScanContext;
use crate::model::{Category, Finding, Tier};

const CAT: Category = Category::Xcode;

pub(super) const TABLE: [(&str, Category, super::ScanFn); 4] = [
    ("xcode-deriveddata", CAT, derived_data),
    ("xcode-devicesupport", CAT, device_support),
    ("xcode-simulators", CAT, simulators),
    ("xcode-archives", CAT, archives),
];

fn xcode_dir(home: &Path) -> PathBuf {
    home.join("Library/Developer/Xcode")
}

fn derived_data(_: &SystemEnv, ctx: &ScanContext<'_>) -> Vec<Finding> {
    let Some(dir) = real_dir(xcode_dir(&ctx.home).join("DerivedData")) else {
        return Vec::new();
    };
    let projects = subdirs(&dir).len() as u64;
    finish(vec![Item::new(
        "xcode-deriveddata",
        CAT,
        dir,
        "Xcode DerivedData",
        Tier::Regenerable,
    )
    .reason("build products and indexes, rebuilt by the next build")
    .detail("projects", projects)])
}

/// `iOS DeviceSupport/<version>` (and watchOS/tvOS/visionOS), one finding each.
fn device_support(_: &SystemEnv, ctx: &ScanContext<'_>) -> Vec<Finding> {
    let base = xcode_dir(&ctx.home);
    let mut items = Vec::new();
    for platform in ["iOS", "watchOS", "tvOS", "visionOS"] {
        let root = base.join(format!("{platform} DeviceSupport"));
        for dir in subdirs(&root) {
            let version = file_name(&dir);
            items.push(
                Item::new(
                    "xcode-devicesupport",
                    CAT,
                    dir,
                    format!("{platform} {version}"),
                    Tier::Regenerable,
                )
                .reason("debug symbols copied from a device; re-copied when that device connects")
                .detail("platform", platform)
                .detail("version", version),
            );
        }
    }
    finish(items)
}

fn archives(_: &SystemEnv, ctx: &ScanContext<'_>) -> Vec<Finding> {
    let Some(dir) = real_dir(xcode_dir(&ctx.home).join("Archives")) else {
        return Vec::new();
    };
    let count: u64 = subdirs(&dir).iter().map(|d| subdirs(d).len() as u64).sum();
    finish(vec![Item::new(
        "xcode-archives",
        CAT,
        dir,
        "Xcode archives",
        Tier::Review,
    )
    .reason("contains dSYMs needed to symbolicate crash reports for shipped builds")
    .detail("archives", count)])
}

fn simulators(env: &SystemEnv, ctx: &ScanContext<'_>) -> Vec<Finding> {
    let tools = env.tools(&ctx.home);
    let mut items = Vec::new();
    if let Some(out) = tools.output("xcrun", ["simctl", "list", "--json", "devices"]) {
        if let Ok(v) = serde_json::from_str::<Value>(&out) {
            items.extend(unavailable_devices(&v, &ctx.home));
        }
    }
    if let Some(out) = tools.output("xcrun", ["simctl", "runtime", "list", "--json"]) {
        if let Ok(v) = serde_json::from_str::<Value>(&out) {
            items.extend(runtimes(&v));
        }
    }
    finish(items)
}

/// Devices whose runtime is gone: one finding, cleaned by `simctl delete unavailable`.
pub(super) fn unavailable_devices(v: &Value, home: &Path) -> Option<Item> {
    let mut bytes = 0u64;
    let mut names = Vec::new();
    let mut last = None;
    for (_runtime, devices) in v["devices"].as_object()? {
        for d in devices.as_array().into_iter().flatten() {
            if d["isAvailable"].as_bool().unwrap_or(true) {
                continue;
            }
            let device_dir = d["dataPath"]
                .as_str()
                .map(PathBuf::from)
                .and_then(|p| p.parent().map(Path::to_path_buf));
            let size = device_dir
                .as_deref()
                .and_then(measure)
                .map(|(b, _)| b)
                .filter(|b| *b > 0)
                .or_else(|| d["dataPathSize"].as_u64())
                .unwrap_or(0);
            bytes += size;
            names.push(d["name"].as_str().unwrap_or("?").to_string());
            last = last.max(d["lastBootedAt"].as_str().and_then(parse_timestamp));
        }
    }
    if names.is_empty() {
        return None;
    }
    let n = names.len();
    Some(
        Item::new(
            "xcode-simulator-unavailable",
            CAT,
            home.join("Library/Developer/CoreSimulator/Devices"),
            format!("{n} unavailable simulator{}", if n == 1 { "" } else { "s" }),
            Tier::RegenerableExpensive,
        )
        .command(["xcrun", "simctl", "delete", "unavailable"])
        .reason("simulators whose runtime is no longer installed")
        .size(bytes)
        .last_activity(last)
        .detail("devices", names),
    )
}

/// Simulator runtimes from `simctl runtime list --json`.
pub(super) fn runtimes(v: &Value) -> Vec<Item> {
    let Some(map) = v.as_object() else {
        return Vec::new();
    };
    let mut items: Vec<Item> = Vec::new();
    // Sorted by identifier so which entry wins a shared image is stable.
    let mut rts: Vec<&Value> = map.values().collect();
    rts.sort_by(|a, b| a["identifier"].as_str().cmp(&b["identifier"].as_str()));
    for rt in rts {
        let Some(id) = rt["identifier"].as_str() else {
            continue;
        };
        let size = rt["sizeBytes"].as_u64().unwrap_or(0);
        let platform = rt["runtimeIdentifier"]
            .as_str()
            .and_then(|r| r.rsplit('.').next())
            .and_then(|r| r.split('-').next())
            .unwrap_or("Simulator");
        let version = rt["version"].as_str().unwrap_or("?");
        let build = rt["build"].as_str().unwrap_or("");
        let name = if build.is_empty() {
            format!("{platform} {version} runtime")
        } else {
            format!("{platform} {version} ({build}) runtime")
        };
        let path = rt["path"]
            .as_str()
            .or(rt["runtimeBundlePath"].as_str())
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(format!("simruntime://{id}")));
        // Several runtime entries can share one disk image (seen with two
        // iOS 26.4 entries): report it once, or its bytes count twice.
        if let Some(first) = items.iter_mut().find(|i| i.path == path) {
            let ids = first
                .details
                .entry("shared_with_runtime_ids")
                .or_insert_with(|| Value::Array(Vec::new()));
            if let Some(a) = ids.as_array_mut() {
                a.push(Value::from(id));
            }
            continue;
        }
        let deletable = rt["deletable"].as_bool().unwrap_or(false);
        let mut item = Item::new(
            "xcode-simulator-runtime",
            CAT,
            path,
            name,
            Tier::RegenerableExpensive,
        )
        .size(size)
        .last_activity(rt["lastUsedAt"].as_str().and_then(parse_timestamp))
        .detail("runtime_id", id)
        .detail("version", version);
        if deletable {
            item = item
                .command(["xcrun", "simctl", "runtime", "delete", id])
                .reason("re-downloaded from Xcode > Settings > Components");
        } else {
            item.tier = Tier::Review;
            item = item.reason("bundled with Xcode; simctl can't delete it");
        }
        items.push(item);
    }
    items
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Status;
    use crate::system::testutil::{bare_env, by_kind, ctx, write};
    use serde_json::json;

    #[test]
    fn runtimes_sharing_one_image_are_reported_once() {
        let v = json!({
            "B": {"identifier": "B", "version": "26.4", "build": "23E244", "sizeBytes": 100,
                  "path": "/S/img.dmg", "deletable": true, "runtimeIdentifier": "com.apple.CoreSimulator.SimRuntime.iOS-26-4"},
            "A": {"identifier": "A", "version": "26.4", "build": "23E244", "sizeBytes": 100,
                  "path": "/S/img.dmg", "deletable": true, "runtimeIdentifier": "com.apple.CoreSimulator.SimRuntime.iOS-26-4"},
            "C": {"identifier": "C", "version": "18.0", "sizeBytes": 7,
                  "path": "/S/other.dmg", "deletable": true}
        });
        let items = runtimes(&v);
        assert_eq!(items.len(), 2);
        let shared = items
            .iter()
            .find(|i| i.path == Path::new("/S/img.dmg"))
            .unwrap();
        assert_eq!(shared.details["runtime_id"], "A", "lowest id wins");
        assert_eq!(shared.details["shared_with_runtime_ids"], json!(["B"]));
        assert_eq!(shared.size, Some(100));
    }

    #[test]
    fn finds_deriveddata_devicesupport_and_archives() {
        let t = tempfile::tempdir().unwrap();
        let x = t.path().join("Library/Developer/Xcode");
        write(&x.join("DerivedData/App-abc/Build/x"), 50_000);
        write(&x.join("iOS DeviceSupport/17.5 (21F79)/Symbols/x"), 20_000);
        write(
            &x.join("iOS DeviceSupport/18.0 (22A3354)/Symbols/x"),
            20_000,
        );
        write(&x.join("Archives/2026-01-01/App.xcarchive/x"), 20_000);
        let env = bare_env();
        let c = ctx(t.path());
        let dd = derived_data(&env, &c);
        assert_eq!(dd[0].status, Status::Removable);
        assert_eq!(dd[0].details["projects"], 1);
        let ds = device_support(&env, &c);
        let mut names: Vec<_> = ds
            .iter()
            .map(|f| f.details["name"].as_str().unwrap())
            .collect();
        names.sort();
        assert_eq!(names, ["iOS 17.5 (21F79)", "iOS 18.0 (22A3354)"]);
        let ar = archives(&env, &c);
        assert_eq!(ar[0].status, Status::Review);
        assert!(ar[0].cleanup.is_none());
        assert_eq!(ar[0].details["archives"], 1);
    }

    #[test]
    fn parses_simctl_output() {
        let t = tempfile::tempdir().unwrap();
        let devices = json!({"devices": {
            "com.apple.CoreSimulator.SimRuntime.iOS-16-0": [
                {"name": "iPhone 14", "isAvailable": false, "dataPath": "/nonexistent/X/data", "dataPathSize": 1234567, "lastBootedAt": "2025-01-01T00:00:00Z"}
            ],
            "com.apple.CoreSimulator.SimRuntime.iOS-18-6": [
                {"name": "iPhone 16", "isAvailable": true, "dataPath": "/nonexistent/Y/data", "dataPathSize": 999}
            ]
        }});
        let item = unavailable_devices(&devices, t.path()).unwrap();
        let rts = json!({
            "A": {"identifier": "A", "runtimeIdentifier": "com.apple.CoreSimulator.SimRuntime.iOS-26-0", "version": "26.0.1", "build": "23A8464", "sizeBytes": 8_000_000_000u64, "deletable": true, "lastUsedAt": "2026-09-27T22:36:43Z", "path": "/Library/x.dmg"},
            "B": {"identifier": "B", "runtimeIdentifier": "com.apple.CoreSimulator.SimRuntime.watchOS-11-0", "version": "11.0", "sizeBytes": 3_000_000_000u64, "deletable": false}
        });
        let mut items = runtimes(&rts);
        items.push(item);
        let f = finish(items);
        let un = by_kind(&f, "xcode-simulator-unavailable");
        assert_eq!(un[0].size_bytes, 1234567);
        assert_eq!(
            un[0].cleanup.as_ref().unwrap().command,
            ["xcrun", "simctl", "delete", "unavailable"]
        );
        let rt = by_kind(&f, "xcode-simulator-runtime");
        let a = rt.iter().find(|x| x.details["runtime_id"] == "A").unwrap();
        assert_eq!(a.details["name"], "iOS 26.0.1 (23A8464) runtime");
        assert_eq!(a.status, Status::Removable);
        assert_eq!(
            a.cleanup.as_ref().unwrap().command,
            ["xcrun", "simctl", "runtime", "delete", "A"]
        );
        let b = rt.iter().find(|x| x.details["runtime_id"] == "B").unwrap();
        assert_eq!(b.status, Status::Review);
    }
}
