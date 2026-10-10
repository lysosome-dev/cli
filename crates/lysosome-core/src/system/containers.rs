//! Docker (via `docker system df -v`, only if the daemon already answers) and
//! the OrbStack data folder. Never starts Docker.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::Value;

use super::{finish, parse_human_bytes, parse_timestamp, real_dir, Item, SystemEnv};
use crate::detector::ScanContext;
use crate::model::{Category, Finding, Tier};

const CAT: Category = Category::Containers;

pub(super) const TABLE: [(&str, Category, super::ScanFn); 2] =
    [("docker", CAT, docker), ("orbstack", CAT, orbstack)];

/// How long the daemon gets to answer `docker version` before we give up.
const PROBE_TIMEOUT: Duration = Duration::from_secs(3);
/// `docker system df -v` computes sizes and can take a while.
const DF_TIMEOUT: Duration = Duration::from_secs(45);

fn docker(env: &SystemEnv, ctx: &ScanContext<'_>) -> Vec<Finding> {
    let tools = env.tools(&ctx.home);
    // Probe first: if the daemon isn't running this fails fast (the CLI never
    // starts Docker Desktop).
    if tools
        .output_with_timeout(
            "docker",
            ["version", "--format", "{{.Server.Version}}"],
            PROBE_TIMEOUT,
        )
        .is_none_or(|v| v.is_empty())
    {
        return Vec::new();
    }
    let Some(out) = tools.output_with_timeout(
        "docker",
        ["system", "df", "-v", "--format", "json"],
        DF_TIMEOUT,
    ) else {
        return Vec::new();
    };
    match serde_json::from_str::<Value>(&out) {
        Ok(v) => finish(docker_items(&v)),
        Err(_) => Vec::new(),
    }
}

fn s<'a>(v: &'a Value, key: &str) -> &'a str {
    v[key].as_str().unwrap_or("")
}

fn bytes(v: &Value, key: &str) -> u64 {
    match &v[key] {
        Value::Number(n) => n.as_u64().unwrap_or(0),
        Value::String(x) => parse_human_bytes(x).unwrap_or(0),
        _ => 0,
    }
}

fn count(v: &Value, key: &str) -> u64 {
    match &v[key] {
        Value::Number(n) => n.as_u64().unwrap_or(0),
        Value::String(x) => x.trim().parse().unwrap_or(0),
        _ => 0,
    }
}

fn truthy(v: &Value, key: &str) -> bool {
    match &v[key] {
        Value::Bool(b) => *b,
        Value::String(x) => x == "true",
        _ => false,
    }
}

/// Docker objects have no folder of their own; findings use a
/// `docker://<type>/<id>` pseudo-path and always clean up through `docker`.
fn pseudo(kind: &str, id: &str) -> PathBuf {
    PathBuf::from(format!("docker://{kind}/{id}"))
}

/// Turn `docker system df -v --format json` into items.
pub(super) fn docker_items(v: &Value) -> Vec<Item> {
    let mut items = Vec::new();
    for img in v["Images"].as_array().into_iter().flatten() {
        let id = s(img, "ID");
        if id.is_empty() {
            continue;
        }
        let (repo, tag) = (s(img, "Repository"), s(img, "Tag"));
        let short = id.trim_start_matches("sha256:");
        let short = &short[..short.len().min(12)];
        let dangling = repo.is_empty() || repo == "<none>";
        let name = if dangling {
            format!("<untagged> {short}")
        } else if tag.is_empty() || tag == "<none>" {
            repo.to_string()
        } else {
            format!("{repo}:{tag}")
        };
        let total = bytes(img, "Size");
        let unique = if img.get("UniqueSize").is_some() {
            bytes(img, "UniqueSize")
        } else {
            total
        };
        let containers = count(img, "Containers");
        let mut item = Item::new(
            "docker-image",
            CAT,
            pseudo("image", id),
            name,
            Tier::RegenerableExpensive,
        )
        .size(unique)
        .last_activity(parse_timestamp(s(img, "CreatedAt")))
        .detail("image_id", id)
        .detail("total_bytes", total)
        .detail("shared_bytes", bytes(img, "SharedSize"))
        .detail("containers", containers)
        .detail("dangling", dangling);
        item.redownload_bytes = Some(total);
        if containers > 0 {
            item.tier = Tier::Review;
            item = item.reason(format!(
                "used by {containers} container{}",
                if containers == 1 { "" } else { "s" }
            ));
        } else {
            item = item
                .command(["docker", "image", "rm", id])
                .reason(if dangling {
                    "untagged image, not used by any container"
                } else {
                    "not used by any container; pulled or rebuilt on next use"
                });
        }
        items.push(item);
    }

    // Build cache: what `docker builder prune` can free = entries neither in
    // use nor shared with images.
    let cache: Vec<&Value> = v["BuildCache"].as_array().into_iter().flatten().collect();
    if !cache.is_empty() {
        let reclaimable: u64 = cache
            .iter()
            .filter(|c| !truthy(c, "InUse") && !truthy(c, "Shared"))
            .map(|c| bytes(c, "Size"))
            .sum();
        let total: u64 = cache.iter().map(|c| bytes(c, "Size")).sum();
        let last = cache
            .iter()
            .filter_map(|c| parse_timestamp(s(c, "LastUsedAt")))
            .max();
        items.push(
            Item::new(
                "docker-build-cache",
                CAT,
                pseudo("build-cache", "all"),
                "Docker build cache",
                Tier::Regenerable,
            )
            .command(["docker", "builder", "prune", "--force"])
            .reason("rebuilt by the next `docker build`")
            .size(reclaimable)
            .last_activity(last)
            .detail("entries", cache.len() as u64)
            .detail("total_bytes", total),
        );
    }

    for vol in v["Volumes"].as_array().into_iter().flatten() {
        let name = s(vol, "Name");
        if name.is_empty() {
            continue;
        }
        let links = count(vol, "Links");
        items.push(
            Item::new(
                "docker-volume",
                CAT,
                pseudo("volume", name),
                name,
                Tier::Review,
            )
            .reason(if links > 0 {
                format!("volume data used by {links} container(s); may hold databases or state")
            } else {
                "volume data (databases, state) can't be rebuilt automatically".to_string()
            })
            .size(bytes(vol, "Size"))
            .detail("links", links)
            .detail("mountpoint", s(vol, "Mountpoint")),
        );
    }
    items
}

/// OrbStack keeps images, volumes and Linux machines in one data folder.
fn orbstack(env: &SystemEnv, ctx: &ScanContext<'_>) -> Vec<Finding> {
    let _ = env;
    orbstack_items(&ctx.home)
}

fn orbstack_items(home: &Path) -> Vec<Finding> {
    let candidates = [
        home.join("Library/Group Containers/HUAQ24HBR6.dev.orbstack/data"),
        home.join(".orbstack/data"),
    ];
    let Some(dir) = candidates.into_iter().find_map(real_dir) else {
        return Vec::new();
    };
    finish(vec![Item::new(
        "orbstack-data",
        CAT,
        dir,
        "OrbStack data (containers, images, volumes, machines)",
        Tier::Review,
    )
    .reason("manage from OrbStack; includes the Docker images listed above when OrbStack is your engine")])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Status;
    use crate::system::testutil;
    use serde_json::json;

    #[test]
    fn maps_docker_df_to_findings() {
        let v = json!({
          "Images": [
            {"Containers":"0","CreatedAt":"2026-04-28 21:21:29 -0400 EDT","ID":"sha256:aaaabbbbccccdddd","Repository":"web","Tag":"1.0","SharedSize":"100MB","Size":"1.2GB","UniqueSize":"1.1GB"},
            {"Containers":"2","CreatedAt":"2026-04-28 21:21:29 -0400 EDT","ID":"sha256:eeee","Repository":"db","Tag":"latest","SharedSize":"0B","Size":"300MB","UniqueSize":"300MB"},
            {"Containers":"0","CreatedAt":"2026-04-28 21:21:29 -0400 EDT","ID":"sha256:ffff0000","Repository":"<none>","Tag":"<none>","SharedSize":"0B","Size":"5MB","UniqueSize":"5MB"}
          ],
          "Containers": [],
          "Volumes": [{"Name":"pgdata","Links":"1","Size":"2GB","Mountpoint":"/var/lib/docker/volumes/pgdata/_data"}],
          "BuildCache": [
            {"ID":"a","InUse":"false","Shared":"false","Size":"40MB","LastUsedAt":"2026-04-04 00:19:24.8 +0000 UTC"},
            {"ID":"b","InUse":"false","Shared":"true","Size":"500MB","LastUsedAt":"2026-04-05 00:00:00 +0000 UTC"}
          ]
        });
        let f = finish(docker_items(&v));
        let web = f.iter().find(|x| x.details["name"] == "web:1.0").unwrap();
        assert_eq!(web.size_bytes, 1_100_000_000);
        assert_eq!(web.details["redownload_bytes"], 1_200_000_000u64);
        assert_eq!(web.status, Status::Removable);
        assert_eq!(
            web.cleanup.as_ref().unwrap().command,
            ["docker", "image", "rm", "sha256:aaaabbbbccccdddd"]
        );
        let db = f.iter().find(|x| x.details["name"] == "db:latest").unwrap();
        assert_eq!(db.status, Status::Review);
        assert!(db.cleanup.is_none());
        assert!(f.iter().any(|x| x.details["name"] == "<untagged> ffff0000"));
        let bc = testutil::by_kind(&f, "docker-build-cache");
        assert_eq!(bc[0].size_bytes, 40_000_000);
        assert_eq!(
            bc[0].cleanup.as_ref().unwrap().command,
            ["docker", "builder", "prune", "--force"]
        );
        assert_eq!(bc[0].tier, Tier::Regenerable);
        let vol = testutil::by_kind(&f, "docker-volume");
        assert_eq!(vol[0].status, Status::Review);
        assert_eq!(vol[0].tier, Tier::Review);
    }

    #[test]
    fn finds_orbstack_data_as_review() {
        let t = tempfile::tempdir().unwrap();
        testutil::write(
            &t.path()
                .join("Library/Group Containers/HUAQ24HBR6.dev.orbstack/data/data.img"),
            10_000,
        );
        let f = orbstack_items(t.path());
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].status, Status::Review);
    }
}
