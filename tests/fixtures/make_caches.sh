#!/usr/bin/env bash
# Build a fake home folder full of caches, models and tool data, plus fake
# developer tools, for `lysosome caches` tests.
#
#   tests/fixtures/make_caches.sh <dest-dir>
#
# Prints the canonical <dest-dir>. Run lysosome with HOME=<dest>/home and
# PATH=<dest>/bin (and nothing else) so no real tool or real home is touched.
#
# Layout:
#   home/.ollama/models        llama3.1:8b and someone/qwen:latest sharing a layer, + an orphan blob
#   home/.cache/huggingface/hub models--meta-llama--Llama-3-8B, datasets--squad
#   home/.lmstudio/models      lmstudio-community/Qwen3-8B-GGUF
#   home/.cache/whisper        large-v3.pt
#   home/.npm/_cacache         npm cache
#   home/fake-brew-cache       reported by the fake `brew --cache`
#   home/fake-go/{mod,build}   reported by the fake `go env GOMODCACHE GOCACHE`
#   home/.cargo/registry       Cargo registry
#   home/.android/avd          Pixel_8.avd + Pixel_8.ini
#   home/Library/Developer/Xcode  DerivedData, iOS DeviceSupport/17.5 (21F79), Archives
#   home/Library/Developer/CoreSimulator/Devices/DEAD-BEEF  an unavailable simulator
#   bin/docker                 daemon "running"; canned `docker system df -v --format json`
#   bin/xcrun                  canned `simctl list --json devices` / `simctl runtime list --json`
#   bin/brew, bin/go           print the fake cache paths above
#   bin/pip                    exits 1 (a broken tool must not break the scan)
#   bin/pnpm                   prints a path that doesn't exist (ignored)
#   bin/uv                     hangs if <dest>/hang-uv exists (proves tool timeouts work), else exits 1
set -euo pipefail

if [ $# -ne 1 ]; then
  echo "usage: $0 <dest-dir>" >&2
  exit 2
fi
mkdir -p "$1"
DEST="$(cd "$1" && pwd -P)"
H="$DEST/home"
B="$DEST/bin"
mkdir -p "$H" "$B"

# blob <path> <kilobytes>
blob() {
  mkdir -p "$(dirname "$1")"
  head -c "$(($2 * 1024))" /dev/zero | tr '\0' 'x' > "$1"
}

# --- Ollama ---------------------------------------------------------------
O="$H/.ollama/models"
blob "$O/blobs/sha256-aaa" 400
blob "$O/blobs/sha256-bbb" 200
blob "$O/blobs/sha256-shared" 100
blob "$O/blobs/sha256-cfg" 4
blob "$O/blobs/sha256-orphan" 50
mkdir -p "$O/manifests/registry.ollama.ai/library/llama3.1" "$O/manifests/registry.ollama.ai/someone/qwen"
cat > "$O/manifests/registry.ollama.ai/library/llama3.1/8b" <<'EOF'
{"schemaVersion":2,"config":{"digest":"sha256:cfg","size":4096},"layers":[{"digest":"sha256:aaa","size":409600},{"digest":"sha256:shared","size":102400}]}
EOF
cat > "$O/manifests/registry.ollama.ai/someone/qwen/latest" <<'EOF'
{"schemaVersion":2,"config":{"digest":"sha256:cfg","size":4096},"layers":[{"digest":"sha256:bbb","size":204800},{"digest":"sha256:shared","size":102400}]}
EOF

# --- Hugging Face, LM Studio, whisper --------------------------------------
HF="$H/.cache/huggingface/hub"
blob "$HF/models--meta-llama--Llama-3-8B/blobs/abc123" 300
mkdir -p "$HF/models--meta-llama--Llama-3-8B/snapshots/rev1" "$HF/models--meta-llama--Llama-3-8B/refs"
ln -s ../../blobs/abc123 "$HF/models--meta-llama--Llama-3-8B/snapshots/rev1/model.safetensors"
echo rev1 > "$HF/models--meta-llama--Llama-3-8B/refs/main"
blob "$HF/datasets--squad/blobs/def456" 20
blob "$H/.lmstudio/models/lmstudio-community/Qwen3-8B-GGUF/qwen3-8b-q4.gguf" 150
blob "$H/.cache/whisper/large-v3.pt" 80

# --- Package caches ---------------------------------------------------------
blob "$H/.npm/_cacache/content-v2/sha512/aa/bb" 64
blob "$H/fake-brew-cache/downloads/foo.tar.gz" 64
blob "$H/fake-go/mod/cache/download/x.zip" 64
blob "$H/fake-go/build/00/abc-d" 64
blob "$H/.cargo/registry/cache/index/serde.crate" 64

# --- Android ----------------------------------------------------------------
blob "$H/.android/avd/Pixel_8.avd/userdata-qemu.img" 128
printf 'avd.ini.encoding=UTF-8\npath=%s\n' "$H/.android/avd/Pixel_8.avd" > "$H/.android/avd/Pixel_8.ini"

# --- Xcode ------------------------------------------------------------------
X="$H/Library/Developer/Xcode"
blob "$X/DerivedData/App-abcdef/Build/Products/app" 96
blob "$X/iOS DeviceSupport/17.5 (21F79)/Symbols/dyld" 96
blob "$X/Archives/2026-01-02/App 1-2-26.xcarchive/dSYMs/App.dSYM" 32
blob "$H/Library/Developer/CoreSimulator/Devices/DEAD-BEEF/data/disk" 48

# --- Fake tools ---------------------------------------------------------------
cat > "$DEST/docker-df.json" <<'EOF'
{"Images":[{"Containers":"0","CreatedAt":"2026-04-28 21:21:29 -0400 EDT","ID":"sha256:1111aaaa2222bbbb","Repository":"web","Tag":"1.0","SharedSize":"100MB","Size":"1.2GB","UniqueSize":"1.1GB"},{"Containers":"1","CreatedAt":"2026-04-20 10:00:00 -0400 EDT","ID":"sha256:3333cccc","Repository":"postgres","Tag":"16","SharedSize":"0B","Size":"450MB","UniqueSize":"450MB"}],
 "Containers":[],
 "Volumes":[{"Name":"pgdata","Links":"1","Size":"2.5GB","Mountpoint":"/var/lib/docker/volumes/pgdata/_data"}],
 "BuildCache":[{"ID":"x1","InUse":"false","Shared":"false","Size":"300MB","LastUsedAt":"2026-04-04 00:19:24.8 +0000 UTC"},{"ID":"x2","InUse":"false","Shared":"true","Size":"900MB","LastUsedAt":"2026-04-05 00:00:00 +0000 UTC"}]}
EOF
cat > "$DEST/simctl-devices.json" <<EOF
{"devices":{
  "com.apple.CoreSimulator.SimRuntime.iOS-16-0":[{"name":"iPhone 14","udid":"DEAD-BEEF","isAvailable":false,"dataPath":"$H/Library/Developer/CoreSimulator/Devices/DEAD-BEEF/data","dataPathSize":49152,"lastBootedAt":"2025-01-01T00:00:00Z"}],
  "com.apple.CoreSimulator.SimRuntime.iOS-18-6":[{"name":"iPhone 16","udid":"OK-1","isAvailable":true,"dataPath":"$H/nope/data","dataPathSize":1}]
}}
EOF
cat > "$DEST/simctl-runtimes.json" <<'EOF'
{"RT-1":{"identifier":"RT-1","runtimeIdentifier":"com.apple.CoreSimulator.SimRuntime.iOS-26-0","version":"26.0.1","build":"23A8464","sizeBytes":8000000000,"deletable":true,"lastUsedAt":"2026-09-27T22:36:43Z","path":"/Library/Developer/CoreSimulator/Images/RT-1.dmg"}}
EOF

cat > "$B/docker" <<EOF
#!/bin/sh
case "\$1 \$2" in
  "version "*) echo 27.3.1 ;;
  "system df") /bin/cat "$DEST/docker-df.json" ;;
  *) exit 1 ;;
esac
EOF
cat > "$B/xcrun" <<EOF
#!/bin/sh
case "\$*" in
  "simctl list --json devices") /bin/cat "$DEST/simctl-devices.json" ;;
  "simctl runtime list --json") /bin/cat "$DEST/simctl-runtimes.json" ;;
  *) exit 1 ;;
esac
EOF
cat > "$B/brew" <<EOF
#!/bin/sh
[ "\$1" = "--cache" ] && echo "$H/fake-brew-cache" && exit 0
exit 1
EOF
cat > "$B/go" <<EOF
#!/bin/sh
[ "\$*" = "env GOMODCACHE GOCACHE" ] || exit 1
echo "$H/fake-go/mod"
echo "$H/fake-go/build"
EOF
cat > "$B/pip" <<'EOF'
#!/bin/sh
echo "pip is broken" >&2
exit 1
EOF
cat > "$B/pnpm" <<'EOF'
#!/bin/sh
echo "/definitely/not/a/real/store"
EOF
cat > "$B/uv" <<EOF
#!/bin/sh
[ -f "$DEST/hang-uv" ] && exec /bin/sleep 120
exit 1
EOF
chmod +x "$B"/*

echo "$DEST"
