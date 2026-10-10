//! Local AI models: Ollama, Hugging Face hub, LM Studio, torch and whisper
//! caches. Findings name the model (`llama3.1:8b`, `meta-llama/Llama-3-8B`),
//! never a blob hash.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use super::{
    file_name, files, finish, first_dir, last_used, measure, real_dir, subdirs, touched, Item,
    SystemEnv,
};
use crate::detector::ScanContext;
use crate::model::{Category, Finding, Tier};

const CAT: Category = Category::AiModels;
const EXPENSIVE: Tier = Tier::RegenerableExpensive;

pub(super) const TABLE: [(&str, Category, super::ScanFn); 5] = [
    ("ollama", CAT, ollama),
    ("huggingface", CAT, huggingface),
    ("lmstudio", CAT, lmstudio),
    ("torch", CAT, torch),
    ("whisper", CAT, whisper),
];

// ---------------------------------------------------------------------------
// Ollama

/// `$OLLAMA_MODELS`, else `~/.ollama/models`.
fn ollama_dir(env: &SystemEnv, home: &Path) -> Option<PathBuf> {
    first_dir(
        env.var_path("OLLAMA_MODELS")
            .into_iter()
            .chain([home.join(".ollama/models")]),
    )
}

struct Manifest {
    name: String,
    path: PathBuf,
    /// Blob file names (`sha256-<hex>`) this model uses.
    blobs: BTreeSet<String>,
}

fn ollama(env: &SystemEnv, ctx: &ScanContext<'_>) -> Vec<Finding> {
    let Some(models) = ollama_dir(env, &ctx.home) else {
        return Vec::new();
    };
    ollama_findings(&models)
}

pub(super) fn ollama_findings(models: &Path) -> Vec<Finding> {
    let blobs_dir = models.join("blobs");
    let manifests = read_manifests_at(models);

    // Blob sizes and timestamps, measured once each.
    let mut blob_info: HashMap<String, (u64, Option<SystemTime>)> = HashMap::new();
    for f in files(&blobs_dir) {
        let name = file_name(&f);
        let size = measure(&f).map(|(b, _)| b).unwrap_or(0);
        blob_info.insert(name, (size, touched(&f)));
    }
    let mut users: HashMap<&str, Vec<&str>> = HashMap::new();
    for m in &manifests {
        for b in &m.blobs {
            users.entry(b.as_str()).or_default().push(m.name.as_str());
        }
    }

    let mut items = Vec::new();
    for m in &manifests {
        let (mut unique, mut total, mut shared) = (0u64, 0u64, 0u64);
        let mut last = touched(&m.path);
        let mut missing = 0u64;
        for b in &m.blobs {
            let Some((size, t)) = blob_info.get(b) else {
                missing += 1;
                continue;
            };
            total += size;
            last = last.max(*t);
            if users.get(b.as_str()).map_or(0, |u| u.len()) > 1 {
                shared += size;
            } else {
                unique += size;
            }
        }
        let mut item = Item::new("ollama-model", CAT, m.path.clone(), &m.name, EXPENSIVE)
            .command(["ollama", "rm", m.name.as_str()])
            .reason("re-downloaded with `ollama pull`")
            .size(unique)
            .last_activity(last)
            .detail("model", m.name.clone())
            .detail("total_bytes", total)
            .detail("shared_bytes", shared)
            .detail("layers", m.blobs.len() as u64);
        if missing > 0 {
            item = item.detail("missing_layers", missing);
        }
        item.redownload_bytes = Some(total);
        items.push(item);
    }

    // Layers used by several models: freed only when all of them are removed.
    let shared: Vec<(&String, &(u64, Option<SystemTime>))> = blob_info
        .iter()
        .filter(|(b, _)| users.get(b.as_str()).is_some_and(|u| u.len() > 1))
        .collect();
    if !shared.is_empty() {
        let bytes: u64 = shared.iter().map(|(_, (s, _))| s).sum();
        let mut models: BTreeSet<&str> = BTreeSet::new();
        for (b, _) in &shared {
            models.extend(users[b.as_str()].iter().copied());
        }
        items.push(
            Item::new(
                "ollama-shared-layers",
                CAT,
                blobs_dir.clone(),
                "Ollama layers shared by several models",
                Tier::Review,
            )
            .reason(format!(
                "shared by {} models; freed only when all of them are removed",
                models.len()
            ))
            .size(bytes)
            .last_activity(shared.iter().filter_map(|(_, (_, t))| *t).max())
            .detail("layers", shared.len() as u64)
            .detail("models", models.into_iter().collect::<Vec<_>>()),
        );
    }

    // Blobs no manifest references (interrupted pulls, removed models).
    let orphans: Vec<&(u64, Option<SystemTime>)> = blob_info
        .iter()
        .filter(|(b, _)| !users.contains_key(b.as_str()))
        .map(|(_, v)| v)
        .collect();
    if !orphans.is_empty() {
        items.push(
            Item::new(
                "ollama-unreferenced",
                CAT,
                blobs_dir,
                "Ollama blobs not used by any model",
                Tier::Review,
            )
            .reason("not referenced by any model (interrupted pull?); Ollama prunes these when it starts")
            .size(orphans.iter().map(|(s, _)| s).sum())
            .last_activity(orphans.iter().filter_map(|(_, t)| *t).max())
            .detail("blobs", orphans.len() as u64),
        );
    }
    finish(items)
}

// ---------------------------------------------------------------------------
// Hugging Face

/// `$HF_HUB_CACHE`, `$HUGGINGFACE_HUB_CACHE`, `$HF_HOME/hub`,
/// `$XDG_CACHE_HOME/huggingface/hub`, `~/.cache/huggingface/hub`.
fn hf_hub_dir(env: &SystemEnv, home: &Path) -> Option<PathBuf> {
    let mut c: Vec<PathBuf> = Vec::new();
    c.extend(env.var_path("HF_HUB_CACHE"));
    c.extend(env.var_path("HUGGINGFACE_HUB_CACHE"));
    c.extend(env.var_path("HF_HOME").map(|h| h.join("hub")));
    c.push(env.xdg_cache(home).join("huggingface/hub"));
    first_dir(c)
}

fn huggingface(env: &SystemEnv, ctx: &ScanContext<'_>) -> Vec<Finding> {
    let Some(hub) = hf_hub_dir(env, &ctx.home) else {
        return Vec::new();
    };
    let mut items = Vec::new();
    for dir in subdirs(&hub) {
        let folder = file_name(&dir);
        let (kind, rest) = if let Some(r) = folder.strip_prefix("models--") {
            ("hf-model", r)
        } else if let Some(r) = folder.strip_prefix("datasets--") {
            ("hf-dataset", r)
        } else if let Some(r) = folder.strip_prefix("spaces--") {
            ("hf-space", r)
        } else {
            continue;
        };
        let name = rest.replace("--", "/");
        // Blob files carry the real access times; snapshots are symlinks.
        let last = files(&dir.join("blobs"))
            .iter()
            .filter_map(|f| touched(f))
            .chain(last_used(&dir.join("snapshots")))
            .chain(last_used(&dir.join("refs")))
            .chain(touched(&dir))
            .max();
        let revisions = subdirs(&dir.join("snapshots")).len() as u64;
        items.push(
            Item::new(kind, CAT, dir, name.clone(), EXPENSIVE)
                .reason("re-downloaded from the Hugging Face Hub on next use")
                .last_activity(last)
                .detail("repo", name)
                .detail("revisions", revisions),
        );
    }
    finish(items)
}

// ---------------------------------------------------------------------------
// LM Studio

fn lmstudio(env: &SystemEnv, ctx: &ScanContext<'_>) -> Vec<Finding> {
    let _ = env;
    let roots = [
        ctx.home.join(".lmstudio/models"),
        ctx.home.join(".cache/lm-studio/models"),
    ];
    let mut items = Vec::new();
    for root in roots.into_iter().filter_map(real_dir) {
        // models/<publisher>/<model>/...
        for publisher in subdirs(&root) {
            for model in subdirs(&publisher) {
                let name = format!("{}/{}", file_name(&publisher), file_name(&model));
                let last = deep_last_used(&model);
                items.push(
                    Item::new("lmstudio-model", CAT, model, name.clone(), EXPENSIVE)
                        .reason("re-downloaded from LM Studio's model catalog")
                        .last_activity(last)
                        .detail("model", name),
                );
            }
        }
    }
    finish(items)
}

/// Newest timestamp of the files directly in `dir` and one level below
/// (model folders hold a few big weight files, sometimes in a subfolder).
fn deep_last_used(dir: &Path) -> Option<SystemTime> {
    subdirs(dir)
        .iter()
        .filter_map(|d| last_used(d))
        .chain(last_used(dir))
        .max()
}

// ---------------------------------------------------------------------------
// torch / whisper

fn torch(env: &SystemEnv, ctx: &ScanContext<'_>) -> Vec<Finding> {
    let Some(dir) = first_dir(
        env.var_path("TORCH_HOME")
            .into_iter()
            .chain([env.xdg_cache(&ctx.home).join("torch")]),
    ) else {
        return Vec::new();
    };
    let parts: Vec<String> = subdirs(&dir).iter().map(|d| file_name(d)).collect();
    finish(vec![Item::new(
        "torch-cache",
        CAT,
        dir,
        "PyTorch hub & checkpoints",
        EXPENSIVE,
    )
    .reason("model weights re-downloaded by PyTorch on next use")
    .last_activity(None)
    .detail("contents", parts)])
}

fn whisper(env: &SystemEnv, ctx: &ScanContext<'_>) -> Vec<Finding> {
    let Some(dir) = real_dir(env.xdg_cache(&ctx.home).join("whisper")) else {
        return Vec::new();
    };
    let items = files(&dir)
        .into_iter()
        .map(|f| {
            let name = file_name(&f);
            let model = name.strip_suffix(".pt").unwrap_or(&name).to_string();
            Item::new(
                "whisper-model",
                CAT,
                f,
                format!("whisper {model}"),
                EXPENSIVE,
            )
            .reason("re-downloaded by whisper on next use")
            .detail("model", model)
        })
        .collect();
    finish(items)
}

/// Manifests live at `manifests/<host>/<namespace>/<model>/<tag>`.
fn read_manifests_at(models: &Path) -> Vec<Manifest> {
    let root = models.join("manifests");
    let mut out = Vec::new();
    for host in subdirs(&root) {
        for ns in subdirs(&host) {
            for model in subdirs(&ns) {
                for tag in files(&model) {
                    if let Some(m) = parse_manifest_at(&host, &ns, &model, &tag) {
                        out.push(m);
                    }
                }
            }
        }
    }
    out
}

/// Display names follow `ollama list`: `llama3:latest`, `user/model:tag`,
/// `host/ns/model:tag`.
fn parse_manifest_at(host: &Path, ns: &Path, model: &Path, tag: &Path) -> Option<Manifest> {
    let raw = std::fs::read(tag).ok()?;
    let v: serde_json::Value = serde_json::from_slice(&raw).ok()?;
    let mut blobs = BTreeSet::new();
    let layers = v["layers"].as_array().cloned().unwrap_or_default();
    for l in layers.iter().chain(std::iter::once(&v["config"])) {
        if let Some(d) = l["digest"].as_str() {
            blobs.insert(d.replace(':', "-"));
        }
    }
    if blobs.is_empty() {
        return None;
    }
    let (h, n, m, t) = (
        file_name(host),
        file_name(ns),
        file_name(model),
        file_name(tag),
    );
    let name = match (h.as_str(), n.as_str()) {
        ("registry.ollama.ai", "library") => format!("{m}:{t}"),
        ("registry.ollama.ai", _) => format!("{n}/{m}:{t}"),
        _ => format!("{h}/{n}/{m}:{t}"),
    };
    Some(Manifest {
        name,
        path: tag.to_path_buf(),
        blobs,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Status;
    use crate::system::testutil::{bare_env, by_kind, ctx, write};

    fn manifest(models: &Path, rel: &str, digests: &[&str]) {
        let layers: Vec<_> = digests
            .iter()
            .map(|d| serde_json::json!({"digest": format!("sha256:{d}"), "size": 1}))
            .collect();
        let p = models.join("manifests").join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(
            &p,
            serde_json::json!({"config": {"digest": "sha256:cfg"}, "layers": layers}).to_string(),
        )
        .unwrap();
    }

    #[test]
    fn ollama_names_models_and_counts_shared_blobs_once() {
        let t = tempfile::tempdir().unwrap();
        let m = t.path().join(".ollama/models");
        write(&m.join("blobs/sha256-aaa"), 400_000); // llama only
        write(&m.join("blobs/sha256-bbb"), 200_000); // qwen only
        write(&m.join("blobs/sha256-shared"), 100_000); // both
        write(&m.join("blobs/sha256-cfg"), 4_096); // both (config)
        write(&m.join("blobs/sha256-orphan"), 50_000);
        manifest(
            &m,
            "registry.ollama.ai/library/llama3.1/8b",
            &["aaa", "shared"],
        );
        manifest(
            &m,
            "registry.ollama.ai/someone/qwen/latest",
            &["bbb", "shared"],
        );
        let f = ollama(&bare_env(), &ctx(t.path()));
        let models = by_kind(&f, "ollama-model");
        let llama = models
            .iter()
            .find(|x| x.details["name"] == "llama3.1:8b")
            .unwrap();
        assert!(models
            .iter()
            .any(|x| x.details["name"] == "someone/qwen:latest"));
        assert_eq!(llama.status, Status::Removable);
        assert_eq!(llama.tier, Tier::RegenerableExpensive);
        assert_eq!(
            llama.cleanup.as_ref().unwrap().command,
            ["ollama", "rm", "llama3.1:8b"]
        );
        assert!(llama.size_bytes >= 400_000 && llama.size_bytes < 500_000);
        let redl = llama.details["redownload_bytes"].as_u64().unwrap();
        assert!(redl >= 500_000, "re-download includes shared layers");
        assert!(llama.last_activity.is_some());
        // Total of all findings equals the blob folder once: nothing double counted.
        let sum: u64 = f.iter().map(|x| x.size_bytes).sum();
        let blobs = measure(&m.join("blobs")).unwrap().0;
        let dir_self = std::fs::symlink_metadata(m.join("blobs")).map(|md| {
            use std::os::unix::fs::MetadataExt;
            md.blocks() * 512
        });
        assert_eq!(sum, blobs - dir_self.unwrap());
        let shared = by_kind(&f, "ollama-shared-layers");
        assert_eq!(shared[0].status, Status::Review);
        assert!(shared[0].cleanup.is_none());
        assert_eq!(by_kind(&f, "ollama-unreferenced").len(), 1);
    }

    #[test]
    fn huggingface_respects_hf_home_and_names_repos() {
        let t = tempfile::tempdir().unwrap();
        let hub = t.path().join("hfhome/hub");
        write(
            &hub.join("models--meta-llama--Llama-3-8B/blobs/abc"),
            300_000,
        );
        std::fs::create_dir_all(hub.join("models--meta-llama--Llama-3-8B/snapshots/rev1")).unwrap();
        write(&hub.join("datasets--squad/blobs/x"), 10_000);
        write(&hub.join(".locks/x"), 10);
        // The default location must be ignored when HF_HOME is set.
        write(
            &t.path().join(".cache/huggingface/hub/models--x--y/blobs/z"),
            10_000,
        );
        let env = SystemEnv::isolated([("HF_HOME", t.path().join("hfhome"))]);
        let f = huggingface(&env, &ctx(t.path()));
        assert_eq!(f.len(), 2);
        let m = by_kind(&f, "hf-model");
        assert_eq!(m[0].details["name"], "meta-llama/Llama-3-8B");
        assert_eq!(m[0].details["revisions"], 1);
        assert_eq!(
            m[0].cleanup.as_ref().unwrap().paths,
            [hub.join("models--meta-llama--Llama-3-8B")]
        );
        assert_eq!(by_kind(&f, "hf-dataset")[0].details["name"], "squad");
    }

    #[test]
    fn lmstudio_torch_and_whisper() {
        let t = tempfile::tempdir().unwrap();
        let h = t.path();
        write(
            &h.join(".lmstudio/models/lmstudio-community/Qwen3-8B-GGUF/q4.gguf"),
            50_000,
        );
        write(
            &h.join(".cache/lm-studio/models/TheBloke/old-GGUF/a.gguf"),
            50_000,
        );
        write(&h.join(".cache/torch/hub/checkpoints/r.pth"), 50_000);
        write(&h.join(".cache/whisper/large-v3.pt"), 50_000);
        let env = bare_env();
        let c = ctx(h);
        let lm = lmstudio(&env, &c);
        let mut names: Vec<_> = lm
            .iter()
            .map(|f| f.details["name"].as_str().unwrap())
            .collect();
        names.sort();
        assert_eq!(
            names,
            ["TheBloke/old-GGUF", "lmstudio-community/Qwen3-8B-GGUF"]
        );
        assert_eq!(torch(&env, &c).len(), 1);
        let w = whisper(&env, &c);
        assert_eq!(w[0].details["name"], "whisper large-v3");
    }
}
