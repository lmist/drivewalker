# Drivewalker

<p align="center">
  <img src="assets/drivewalker-icon-masked.png" alt="Drivewalker icon" width="128">
</p>

`dw` measures disk hotspots and cleans regenerable project dependencies and user
caches. Native Rust, one binary, no shell-based directory walks or per-folder
subprocesses. Built for macOS; the filesystem core uses Unix directory descriptors.

```sh
cargo install --git https://github.com/oxfern/drivewalker --locked

dw scan                         # Home, largest immediate children first
dw scan ~/gts --depth 2          # Measure once, display another level
dw scan ~/gts --drill 8          # Expand the eight largest branches, without rescanning
dw scan ~/gts --drill 0          # Disable automatic hotspot expansion
dw scan --machine               # Disjoint system/home roots + APFS/snapshot context
dw scan ~/gts --json             # Full structured report at the requested depth

dw clean caches                 # Preview known user caches
dw clean projects ~/gts          # Preview Node/Python dependency and bytecode folders
dw clean all ~/gts               # User caches + dependencies under this project root
dw clean projects ~/gts --apply  # Explicitly perform cleanup
dw clean caches --apply

dw scan ~/gts --exclude ~/gts/archive --jobs 4
dw clean projects ~/gts --exclude ~/gts/active-app/node_modules
```

For a local checkout, use `cargo install --path . --locked`.

Global flags: `--json`, repeatable `--exclude PATH`, `--jobs 1..64` (default 8).
Relative paths, including exclusions, resolve against the working directory.
`scan --limit N` limits displayed children per directory. The JSON `roots` tree
is untruncated at the requested display depth. Adaptive `hotspots` are separately
bounded and include an `omitted_children` count.
Display depth does not limit measurement: calculating a directory's size requires
visiting its descendants. `scan --depth 0 --drill 0` returns root totals only.

## Adaptive work distribution

Discovery, measurement, and deletion split at directory boundaries and share
Rayon's work-stealing queues within one pool per operation. Idle workers can
steal child jobs from a large branch. Even one large cleanup candidate, or a
single flat directory full of files, can use multiple workers. Parent removal
waits for every child job to finish. Exclusions, ownership/device checks, and
symlink protections still apply to each deletion job. The same `--jobs` limit
applies throughout; subtrees do not create additional pools or OS threads.

Project discovery prunes matching dependency folders before measurement, so
discovery and measurement do not recursively walk the same tree. Cache discovery
also parallelizes the bounded Application Support traversal. JSON reports expose
`scheduling.jobs` and `scheduling.workers_used`; the latter counts workers that
actually participated, not a promise that every run needs every worker. Small
trees and filesystem contention may limit the benefit of additional workers.

Scanning retains directory totals and the largest 64 immediate children per
directory, independently of display depth. The default `--drill 6` uses a
best-first frontier: expand the largest currently exposed directory, add its
child directories, then select the next largest branch. Application bundles and
frameworks are not automatically expanded. All directory totals remain indexed,
including directories outside the local top 64. No additional filesystem walks
are needed. Retention costs memory proportional to directory count and at most
64 child summaries per directory; large scans trade memory for reusable results.

The library's `ScanReport::drill(expansions, limit)` returns another view of the
original measurement without filesystem I/O, even if the files have since changed
or disappeared. `expand_hotspots` attaches that view to a report for serialization.
The internal directory index stays in memory and is not included in JSON; JSON
contains the selected expansions, not a reloadable snapshot of the whole index.

## Cleanup scope

Project rules match exactly `node_modules`, `__pycache__`, `.venv`, `.venvs`, and
`venv`. Matching folders are pruned from discovery and measured once. The selected
project root itself is preserved. Project cleanup requires an explicit path.
Git data, app bundles, Hermit and other known installed-tool/state directories are
protected, including when found under a project root. Protected `.claude` folders
also exclude any worktrees stored inside them; ordinary project worktrees remain
eligible. This is intentionally more conservative than deleting every folder
named `node_modules` beneath home.

Cache rules cover `~/Library/Caches`, `~/.cache`, npm's `_cacache`, `_npx`, and
`_prebuilds`, pnpm's `~/Library/pnpm/store`, Bun downloads, Cargo registry archives,
Rustup download staging, and `~/.codex/cache`. The macOS discovery policy also
includes accessible container/group-container cache folders and known
Chromium/Electron cache names beneath Application Support, with bounded discovery.
Cache roots remain in place. `~/Library/Caches/hermit/pkg` is always protected.
Credentials, browser storage, agent sessions, installed packages, Docker volumes,
and source folders are outside the cache policy.

An exclusion within a cleanup candidate protects that whole candidate. Preview
does not change permissions or delete files. `--apply` constructs a fresh plan
and executes it; JSON output is a report, not an executable plan file.

## Filesystem guarantees and limits

- Directory descent uses `openat` with `O_NOFOLLOW` for every component. Symlink
  targets are never traversed. Symlinks inside selected caches/dependencies can be
  unlinked without affecting their targets. Symlinked roots are rejected.
- Cleanup rechecks device/inode identity, ownership, exclusions, and policy before
  deletion. Changed candidates are refused. Disjoint candidates are processed by
  a bounded worker pool. There is one deletion pass, without retries or sudo.
- Read-only, user-owned **cache directories** may gain owner read/write/search
  permissions through their opened descriptor. Files and project dependency
  directories are never chmodded; shared package-file permissions stay intact.
- New siblings created after planning are outside the plan. Contents created
  inside a planned directory before execution may be removed. Live writes during
  deletion can leave a nonempty directory; this is reported as partial cleanup.
- Handles prevent symlink redirection, not all concurrent rename races. An opened
  directory moved during cleanup may have contents removed before the move is
  detected. Avoid running cleanup alongside directory migrations.
- Traversal stays on each root's filesystem. Scans count allocated blocks and
  deduplicate hard links across scan roots. Attribution of shared hard links to
  individual children may vary with parallel traversal. Plan estimates deduplicate
  within each candidate, not between candidates. APFS clones, snapshots, open files,
  compression, and concurrent writes mean estimated bytes differ from recovered
  space. Cleanup reports actual free space before/after on the home filesystem;
  changes on other filesystems are not reflected in that measurement.
- Permission failures are reported. The tool does not bypass macOS privacy
  controls, stop applications, delete snapshots, or alter installed toolchains.

Exit codes: **0** successful report/cleanup, **1** fatal error, **2** partial result
(or invalid CLI arguments). Errors and progress go to stderr; JSON goes to stdout.

## Library and structure

```rust,ignore
use drivewalker::{scan, plan, apply};

let mut usage = scan(&scan_options)?;
usage.expand_hotspots(6, 15); // Uses retained measurements, without filesystem I/O.
let cleanup = plan(&clean_options)?;
// Inspect cleanup.candidates(), cleanup.issues(), cleanup.skipped().
let result = apply(&cleanup)?;
```

`scan` handles traversal/accounting; `policy` selects known targets and protects
installations; `plan` discovers, deduplicates, and measures candidates; `clean`
executes an opaque in-process plan; `unix` contains descriptor operations and the
unsafe FFI boundary; `work` records bounded-pool participation; `platform/macos` provides volume/snapshot context; `report`
formats human output; `main` handles CLI arguments and exit status.

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

Safety tests use temporary fixtures inside the checkout, never the real home or
real caches. They cover symlink and ancestor replacement, Hermit protection,
exclusions, late siblings, partial failures, permissions, and block accounting.
Adaptive tests cover parallel discovery in a single workspace, parallel deletion
inside one large/flat target, parent/child ordering, partial sibling failures,
worker limits, bounded retained rankings, and drill-down after deleting a fixture.
