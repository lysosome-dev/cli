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
#     packages/inner/     nested independent repo (ignored by old-web), idle since 2020
#       target/           ignored, 1 MiB          REMOVABLE regenerable (owned by inner)
#     .venv/src/mylib/.git  ignored .venv holding an editable git checkout:
#                         REVIEW (tier review, no cleanup) — it may hold local work
#   code/old-rust         repo idle since 2020
#     target/             ignored, 3 MiB          REMOVABLE regenerable
#   code/old-edited       repo committed in 2020 but with a tracked file edited now
#     node_modules/       ignored, 1 MiB          ACTIVE (uncommitted edits count as activity)
#   code/old-scratch      repo committed in 2020; old untracked scratch/ folder with a file
#     node_modules/       edited now: ignored, 1 MiB, ACTIVE
#   code/fresh-py         repo with a commit made now
#     .venv/              ignored, 2 MiB          ACTIVE regenerable_expensive
#   code/mono             repo idle since 2020 (remote: remote.git), no artifacts itself
#   code/mono-old-wt      dormant merged worktree of mono with ignored node_modules (4 MiB):
#                         only part of the REMOVABLE worktree finding, never an artifact finding
#   code/mono-old-wt/vendor/lib   idle repo nested in the dormant worktree, ignored dist/ (1 MiB):
#                         counted in the worktree's size only, never an artifact finding
#   code/mono-active-wt   active worktree of mono with ignored node_modules (1 MiB):
#                         ACTIVE artifact finding (project_kind "worktree")
#   code/mono-active-wt/vendor/lib2  idle repo nested in the active worktree with ignored
#                         dist/ (1 MiB): reported once, as the worktree's ACTIVE artifact
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
printf 'node_modules/\ndist/\n.venv/\npackages/inner/\n' > "$W/.gitignore"
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
I="$W/packages/inner"
q git init "$I"
printf 'target/\n' > "$I/.gitignore"
git -C "$I" add .gitignore
old_commit "$I" "inner"
mib "$I/target" t.bin 1
mkdir -p "$W/.venv/src"
q git init "$W/.venv/src/mylib"
printf 'local edits\n' > "$W/.venv/src/mylib/lib.py"

# --- old-edited ---------------------------------------------------------------
E="$CODE/old-edited"
q git init "$E"
printf 'node_modules/\n' > "$E/.gitignore"
printf '# edited\n' > "$E/README.md"
git -C "$E" add .gitignore README.md
old_commit "$E" "initial"
mib "$E/node_modules/x" x.bin 1
printf 'work in progress\n' >> "$E/README.md"

# --- old-scratch --------------------------------------------------------------
X="$CODE/old-scratch"
q git init "$X"
printf 'node_modules/\n' > "$X/.gitignore"
git -C "$X" add .gitignore
old_commit "$X" "initial"
mib "$X/node_modules/x" x.bin 1
mkdir -p "$X/scratch"
printf 'old notes\n' > "$X/scratch/old.txt"
touch -t "$OLD_TOUCH" "$X/scratch/old.txt"
printf 'edited today\n' > "$X/scratch/notes.txt"
touch -t "$OLD_TOUCH" "$X/scratch"

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
printf 'node_modules/\nvendor/\n' > "$M/.gitignore"
printf '# mono\n' > "$M/README.md"
git -C "$M" add .gitignore README.md
old_commit "$M" "initial"
git -C "$M" remote add origin "$DEST/remote.git"
q git -C "$M" push -u origin main
q git -C "$M" remote set-head origin main
q git -C "$M" worktree add -b old-wt "$CODE/mono-old-wt"
mib "$CODE/mono-old-wt/node_modules/dep" dep.bin 4
L="$CODE/mono-old-wt/vendor/lib"
q git init "$L"
printf 'dist/\n' > "$L/.gitignore"
git -C "$L" add .gitignore
old_commit "$L" "vendored"
mib "$L/dist" lib.bin 1
q git -C "$M" worktree add -b active-wt "$CODE/mono-active-wt"
mib "$CODE/mono-active-wt/node_modules/dep" dep.bin 1
L2="$CODE/mono-active-wt/vendor/lib2"
q git init "$L2"
printf 'dist/\n' > "$L2/.gitignore"
git -C "$L2" add .gitignore
old_commit "$L2" "vendored"
mib "$L2/dist" lib.bin 1

# --- not a repo ---------------------------------------------------------------
mib "$CODE/loose/node_modules/x" x.bin 1

# --- back-date activity (last step) -------------------------------------------
# old-web's untracked (not ignored) coverage/ and target symlink are old too,
# so the uncommitted-changes check sees no recent edits.
touch -t "$OLD_TOUCH" "$W/coverage/lcov.bin" "$W/coverage"
touch -h -t "$OLD_TOUCH" "$W/target"
backdate_gitdir "$W/.git"
backdate_gitdir "$R/.git"
backdate_gitdir "$E/.git"
backdate_gitdir "$X/.git"
backdate_gitdir "$I/.git"
backdate_gitdir "$L2/.git"
backdate_gitdir "$M/.git"
backdate_gitdir "$M/.git/worktrees/mono-old-wt"
backdate_gitdir "$L/.git"

echo "$DEST"
