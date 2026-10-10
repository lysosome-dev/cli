#!/usr/bin/env bash
# Generates the man pages and shell completions that cargo-dist copies into
# release archives (see `include` in dist-workspace.toml). CI runs this before
# `dist build`; run it yourself before a local `dist build`.
set -euo pipefail

cd "$(dirname "$0")/.."
out=target/dist-extras
rm -rf "$out"
mkdir -p "$out/man" "$out/completions"

cargo build --locked --quiet -p lysosome
bin=target/debug/lysosome

"$bin" __generate-man "$out/man"
"$bin" completions bash > "$out/completions/lysosome.bash"
"$bin" completions zsh > "$out/completions/_lysosome"
"$bin" completions fish > "$out/completions/lysosome.fish"
"$bin" completions powershell > "$out/completions/_lysosome.ps1"
"$bin" completions elvish > "$out/completions/lysosome.elv"

echo "wrote $(find "$out" -type f | wc -l | tr -d ' ') files to $out"
