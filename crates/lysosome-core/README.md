# lysosome-core

The engine behind [`lysosome`](https://github.com/lysosome-dev/cli), the disk
cleaner for the agentic coding era. It finds what dev tools and coding agents
leave behind (dormant git worktrees, git-ignored build and dependency
folders, package caches, local AI models, Docker and Xcode data), sizes it,
and labels each item by how safe it is to remove. It returns data and never
prints; the [`lysosome`](https://crates.io/crates/lysosome) crate is the CLI.

Scanning is read-only: every git call uses `git --no-optional-locks`,
symlinks are never followed, an artifact folder only counts when
`git check-ignore` confirms it, and anything uncertain is `REVIEW`, never
`REMOVABLE`. Cleanup (the `clean` module) defaults to a dry run, uses each
tool's own cleanup command or the Trash, and re-checks every item first.

```rust,no_run
use lysosome_core::detector::ScanContext;
use lysosome_core::detectors::{project_detectors, run};

let home = std::path::PathBuf::from(std::env::var_os("HOME").expect("HOME"));
let ctx = ScanContext::new(vec![home.join("code")], home);
let scan = run(&project_detectors(), &ctx);
for f in &scan.report.findings {
    println!("{:?} {:>12} {}", f.status, f.size_bytes, f.path.display());
}
```

`detectors::all_detectors()` adds the machine-wide scanners (caches, AI
models, Docker, Xcode, Android, editors). `Report` serializes to the
versioned JSON contract in
[`schema/lysosome.schema.json`](https://github.com/lysosome-dev/cli/blob/main/schema/lysosome.schema.json).

The API follows the CLI's needs and may change between minor versions
before 1.0. Licensed under MIT.
