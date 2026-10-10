#!/usr/bin/env bash
# Build a tree of git worktrees covering every classification, for tests.
#
#   tests/fixtures/make_worktrees.sh <dest-dir>
#
# Everything lives under <dest-dir>; HOME and GIT_CONFIG_GLOBAL are isolated so
# the real user config is never read or written. Scan "<dest-dir>/code".
#
# Layout (status with --days 30):
#   code/app             main repo (clone of remote.git, origin/HEAD -> main)
#   code/app-merged      REMOVABLE  clean, merged into origin/main; 20MB ignored node_modules,
#                                   plus a *tracked* dist/ that must not count as an artifact
#   code/app-pushed      REMOVABLE  clean, fully pushed (branch on origin, not merged)
#   code/app-dirty       REVIEW     1 uncommitted change, 1 untracked file
#   code/app-unpushed    REVIEW     1 unpushed commit, not merged
#   code/app-locked      REVIEW     locked worktree
#   code/app-active      ACTIVE     recently used
#   code/app-gone        (deleted)  -> stale metadata in code/app/.git/worktrees/app-gone
#   code/solo-wt         REVIEW     no remote and not merged (repo code/solo has no remote)
#   code/doomed-wt       ORPHAN     parent repo code/doomed was deleted
#   elsewhere/app-outside REMOVABLE outside the scan root; found via code/app/.git/worktrees
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

# Every commit is old.
OLD_ISO="2020-01-01T12:00:00+0000"
OLD_TOUCH="202001011200"
export GIT_AUTHOR_DATE="$OLD_ISO" GIT_COMMITTER_DATE="$OLD_ISO"

q() { "$@" >/dev/null 2>&1; }

commit_file() { # repo file content message
  printf '%s\n' "$3" > "$1/$2"
  git -C "$1" add "$2"
  q git -C "$1" commit -m "$4"
}

mkdir -p "$DEST/code" "$DEST/elsewhere"
CODE="$DEST/code"

# --- remote + main repo -----------------------------------------------------
q git init --bare "$DEST/remote.git"
q git init "$CODE/app"
APP="$CODE/app"
printf 'node_modules/\n' > "$APP/.gitignore"
mkdir -p "$APP/dist"
printf 'tracked build output\n' > "$APP/dist/bundle.js"
printf '# app\n' > "$APP/README.md"
git -C "$APP" add .gitignore dist/bundle.js README.md
q git -C "$APP" commit -m "initial"
git -C "$APP" remote add origin "$DEST/remote.git"
q git -C "$APP" push -u origin main
q git -C "$APP" remote set-head origin main

wt() { # repo path branch [extra args]
  local repo=$1 path=$2 branch=$3
  shift 3
  q git -C "$repo" worktree add "$@" -b "$branch" "$path"
}

# merged + clean + 20MB ignored node_modules
wt "$APP" "$CODE/app-merged" merged
mkdir -p "$CODE/app-merged/node_modules/left-pad"
dd if=/dev/zero of="$CODE/app-merged/node_modules/left-pad/blob.bin" bs=1048576 count=20 2>/dev/null

# pushed but not merged
wt "$APP" "$CODE/app-pushed" pushed
commit_file "$CODE/app-pushed" pushed.txt "pushed work" "pushed work"
q git -C "$CODE/app-pushed" push -u origin pushed

# dirty: one modified tracked file, one untracked file
wt "$APP" "$CODE/app-dirty" dirty
printf 'local edit\n' >> "$CODE/app-dirty/README.md"
printf 'scratch\n' > "$CODE/app-dirty/notes.txt"

# unpushed commit
wt "$APP" "$CODE/app-unpushed" unpushed
commit_file "$CODE/app-unpushed" feature.txt "wip" "unpushed work"

# locked (otherwise clean and merged)
wt "$APP" "$CODE/app-locked" locked
q git -C "$APP" worktree lock "$CODE/app-locked"

# active (gitdir timestamps stay fresh)
wt "$APP" "$CODE/app-active" active

# worktree outside the scan root, discovered via the main repo's records
wt "$APP" "$DEST/elsewhere/app-outside" outside

# deleted worktree folder -> stale metadata
wt "$APP" "$CODE/app-gone" gone
rm -rf "$CODE/app-gone"

# repo without any remote; worktree with its own commit
q git init "$CODE/solo"
commit_file "$CODE/solo" README.md "# solo" "initial"
wt "$CODE/solo" "$CODE/solo-wt" idea
commit_file "$CODE/solo-wt" idea.txt "idea" "an idea"

# orphan: parent repo deleted
q git init "$CODE/doomed"
commit_file "$CODE/doomed" README.md "# doomed" "initial"
wt "$CODE/doomed" "$CODE/doomed-wt" doomed
rm -rf "$CODE/doomed"

# --- back-date gitdir activity for every dormant worktree (last step) --------
for name in app-merged app-pushed app-dirty app-unpushed app-locked app-outside; do
  gd="$APP/.git/worktrees/$name"
  for f in index HEAD logs/HEAD; do
    [ -e "$gd/$f" ] && touch -t "$OLD_TOUCH" "$gd/$f"
  done
done
gd="$CODE/solo/.git/worktrees/solo-wt"
for f in index HEAD logs/HEAD; do
  [ -e "$gd/$f" ] && touch -t "$OLD_TOUCH" "$gd/$f"
done

echo "$DEST"
