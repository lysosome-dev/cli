#!/usr/bin/env bash
# Build a tree of projects with build/dependency artifacts, for `lysosome scan` tests.
#
#   tests/fixtures/make_projects.sh <dest-dir>
#
# Everything lives under <dest-dir>; HOME and GIT_CONFIG_GLOBAL are isolated so
# the real user config is never read or written. Scan "<dest-dir>/code".
#
# Layout (status with --days 30):
#   code/old-web          repo idle since 2020
#     node_modules/       ignored, 5 MiB          REMOVABLE regenerable_expensive
#       linked -> big/    symlink to a 30 MiB folder: never followed or counted
#       pkg/dist/         ignored artifact nested in node_modules: folded into it
#     dist/               ignored, 1 MiB          REMOVABLE regenerable
#     packages/api/node_modules/  ignored, 1 MiB  REMOVABLE regenerable_expensive
#     build/              TRACKED, 2 MiB: never an artifact
#     coverage/           untracked but NOT ignored: never an artifact
#     target -> big/      symlink named like an artifact: never followed or counted
#   code/old-rust         repo idle since 2020
#     target/             ignored, 3 MiB          REMOVABLE regenerable
#   code/fresh-py         repo with a commit made now
#     .venv/              ignored, 2 MiB          ACTIVE regenerable_expensive
#   code/mono             repo idle since 2020 (remote: remote.git), no artifacts itself
#   code/mono-old-wt      dormant merged worktree of mono with ignored node_modules (4 MiB):
#                         only part of the REMOVABLE worktree finding, never an artifact finding
#   code/mono-active-wt   active worktree of mono with ignored node_modules (1 MiB):
#                         ACTIVE artifact finding (project_kind "worktree")
#   code/loose/node_modules   not inside any repo: never counted
#   big/                  30 MiB folder outside the scan root (symlink target)
set -euo pipefail

if [ $# -ne 1 ]; then
  echo "usage: $0 <dest-dir>" >&2
  exit 2
fi
mkdir -p "$1"
DEST="$(cd "$1" && pwd -P)"

export HOME="$DEST/home"
export GIT_CONFIG_GLOBAL="$DEST/gitconfig"
export GIT_CONFIG_NOSYSTEM=1
mkdir -p "$HOME"
cat > "$GIT_CONFIG_GLOBAL" <<'CFG'
[user]
	name = Fixture
	email = fixture@example.com
[init]
	defaultBranch = main
[commit]
	gpgsign = false
[advice]
	detachedHead = false
CFG

OLD_ISO="2020-01-01T12:00:00+0000"
OLD_TOUCH="202001011200"

q() { "$@" >/dev/null 2>&1; }

mib() { # dir name size-in-MiB
  mkdir -p "$1"
  dd if=/dev/zero of="$1/$2" bs=1048576 count="$3" 2>/dev/null
}

old_commit() { # repo message
  GIT_AUTHOR_DATE="$OLD_ISO" GIT_COMMITTER_DATE="$OLD_ISO" q git -C "$1" commit -m "$2"
}

backdate_gitdir() { # gitdir
  for f in index HEAD logs/HEAD; do
    [ -e "$1/$f" ] && touch -t "$OLD_TOUCH" "$1/$f"
  done
  return 0
}

CODE="$DEST/code"
mkdir -p "$CODE" "$DEST/big"
mib "$DEST/big" blob.bin 30

# --- old-web ------------------------------------------------------------------
W="$CODE/old-web"
q git init "$W"
printf 'node_modules/\ndist/\n' > "$W/.gitignore"
mib "$W/build" tracked.bin 2
printf '# web\n' > "$W/README.md"
mkdir -p "$W/packages/api"
printf '{}\n' > "$W/packages/api/package.json"
git -C "$W" add .gitignore README.md build/tracked.bin packages/api/package.json
old_commit "$W" "initial"
mib "$W/node_modules/pkg" index.bin 4
mib "$W/node_modules/pkg/dist" bundle.bin 1
ln -s "$DEST/big" "$W/node_modules/linked"
mib "$W/dist" app.bin 1
mib "$W/packages/api/node_modules/lib" lib.bin 1
mib "$W/coverage" lcov.bin 1
ln -s "$DEST/big" "$W/target"

# --- old-rust -----------------------------------------------------------------
R="$CODE/old-rust"
q git init "$R"
printf 'target/\n' > "$R/.gitignore"
printf 'fn main() {}\n' > "$R/main.rs"
git -C "$R" add .gitignore main.rs
old_commit "$R" "initial"
mib "$R/target/debug" app.bin 3

# --- fresh-py -----------------------------------------------------------------
P="$CODE/fresh-py"
q git init "$P"
printf '.venv/\n' > "$P/.gitignore"
printf 'print(1)\n' > "$P/app.py"
git -C "$P" add .gitignore app.py
q git -C "$P" commit -m "fresh"
mib "$P/.venv/lib" site.bin 2

# --- mono + worktrees ---------------------------------------------------------
q git init --bare "$DEST/remote.git"
M="$CODE/mono"
q git init "$M"
printf 'node_modules/\n' > "$M/.gitignore"
printf '# mono\n' > "$M/README.md"
git -C "$M" add .gitignore README.md
old_commit "$M" "initial"
git -C "$M" remote add origin "$DEST/remote.git"
q git -C "$M" push -u origin main
q git -C "$M" remote set-head origin main
q git -C "$M" worktree add -b old-wt "$CODE/mono-old-wt"
mib "$CODE/mono-old-wt/node_modules/dep" dep.bin 4
q git -C "$M" worktree add -b active-wt "$CODE/mono-active-wt"
mib "$CODE/mono-active-wt/node_modules/dep" dep.bin 1

# --- not a repo ---------------------------------------------------------------
mib "$CODE/loose/node_modules/x" x.bin 1

# --- back-date activity (last step) -------------------------------------------
backdate_gitdir "$W/.git"
backdate_gitdir "$R/.git"
backdate_gitdir "$M/.git"
backdate_gitdir "$M/.git/worktrees/mono-old-wt"

echo "$DEST"
