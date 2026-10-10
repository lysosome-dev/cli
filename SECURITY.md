# Security policy

lysosome reads your disk and, in later versions, deletes things on request, so
we treat safety bugs as security bugs.

## Supported versions

Only the latest release gets fixes.

## Reporting a vulnerability

Please **do not open a public issue**. Report privately through
[GitHub's private vulnerability reporting](https://github.com/lysosome-dev/cli/security/advisories/new),
or email scott.havird@gmail.com with "lysosome security" in the subject.

Include the version (`lysosome --version`), your OS, and steps to reproduce.
You should hear back within a few days. We'll agree on a disclosure date with
you once a fix is ready, and credit you in the release notes unless you'd
rather we didn't.

## What counts

Anything that breaks the [safety rules](README.md#safety-rules), for example:

- a scan that writes, moves or deletes anything, or leaves git lock files behind
- a worktree with uncommitted, untracked, locked or unpushed-and-unmerged work
  labelled `REMOVABLE` or given a cleanup command
- tracked source counted as a removable artifact
- following a symlink out of the folders being scanned
- command injection through file, branch or remote names in suggested commands
- a tampered or mismatched release artifact, installer or Homebrew formula

## Release integrity

Release archives are built by GitHub Actions from a tagged commit
(`.github/workflows/release.yml`) and published with SHA-256 checksums
(`*.sha256`, `sha256.sum`). Binaries are not yet code-signed or notarized.
