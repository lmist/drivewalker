# Drivewalker

<p align="center">
  <img src="assets/drivewalker-icon-masked.png" alt="Drivewalker icon" width="128">
</p>

`dw` measures disk hotspots and cleans regenerable project dependencies and user
caches. Native Rust, one binary, no shell-based directory walks or per-folder
subprocesses. Built for macOS; the filesystem core uses Unix directory descriptors.

```sh
cargo install --git https://github.com/lmist/drivewalker --locked

dw scan                         # Home, largest immediate children first
dw scan ~/gts --depth 2          # Measure once, display another level
dw scan ~/gts --drill 8          # Expand the eight largest branches, without rescanning
dw scan ~/gts --drill 0          # Disable automatic hotspot expansion
dw scan --machine               # Disjoint system/home roots + APFS/snapshot context
dw scan ~/gts --json             # Full structured report at the requested depth

dw deep                         # Whole disk, followed down to wherever the space sits
dw deep ~/gts ~/Library          # Same analysis for chosen directories
dw deep --min-size 500M --top 40 # Go deeper: smaller entries, longer rankings

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

## Deep analysis

`dw deep` answers "what is using this disk" in one pass. Without paths it measures
every top-level folder of the Data volume, including system data that `scan
--machine` does not visit, and compares the result with what each volume in the
container reports as used. The report has six parts:

- **Disk**: used space per volume of the container, free space, and the gap
  between measured and used (unreadable folders, snapshots, purgeable space).
- **Where the space is**: a tree of every entry at or above `--min-size`, at any
  depth. A chain of single dominant children is joined on one line, so a 12 GiB
  file nine levels down appears as one row. Each directory ends with a summary of
  what stayed below the threshold and names its three largest such entries.
- **Where it ends up**: a ranked list of space that does not split further at the
  threshold: single files, bundles, and directories of many small entries.
- **By kind**: totals for recognisable directories wherever they occur
  (`node_modules`, Python environments, Cargo `target`, caches, package stores,
  `.git`, Xcode data, simulators, container images, application bundles,
  language toolchains, globally installed packages, installers, Trash), each
  with how to remove it. Only the outermost match is counted, so nested copies
  are not added twice.
- **Largest files** and **Not measured**: the latter groups unreadable locations
  by cause and says whether Full Disk Access or `sudo` would include them.
- **What can go**: the closing list, numbered so a row can be referred to by
  its number. It is a compact tree: the outcome of deleting, then each kind
  with its total and how to remove it, then the paths:

  ```text
   #   GiB
      10.8  Stays freed · nothing recreates it and nothing of yours is lost
       8.2  ├─ Simulator runtimes and devices · xcrun simctl, or Xcode settings
   1   8.2  │  └─ /System/Volumes/Data/System/Library/AssetsV2/com_apple_MobileAsset_iOSSimulatorRuntime
       2.6  └─ Container and VM disk images · prune inside the tool
   2   2.6     └─ ~/Library/Containers/com.docker.docker

      34.6  Grows back · a build, install or launch recreates it
      34.6  └─ Cache folders · dw clean caches, or delete
   3  10.4     ├─ ~/Library/Caches
       6.0     ├─ ~/work
   4   3.8     │  ├─ search/vendor/models/.cache
   5   2.2     │  └─ notes/tmp/cache
   6  18.2     └─ … 3642 smaller locations in ~

      10.9  Your call · applications, tools, history and your own files
       6.4  ├─ Other files and folders · review before deleting
   7   4.3  │  ├─ ~/Pictures/exports
   8   2.1  │  └─ ~/Movies/film.mp4
       4.5  └─ Application bundles · uninstall if unused
   9   4.5     └─ /Applications/Big.app
  ```

  A line with a number is a row to pick; a line without one is a heading and
  its size is the total beneath it. A recognised location of at least
  `--min-size` has its own row and the smaller ones of its kind share one. The
  `--top` largest unrecognised files and folders follow as your call. Rows that
  lie together on disk sit under the folder they share, written once, largest
  first at every level; numbers run top to bottom. Rows never overlap: a
  folder counts only the bytes no other row has claimed, so the sizes add up.
  Regenerable data inside system or package-manager locations (`/Library`,
  `/opt`, `/System`, …) is totalled by kind but not offered, and unrecognised
  contents of those locations are never listed. The home directory is written
  as `~`, sizes share one unit, and paths are shortened in the middle to fit
  `$COLUMNS` or the terminal; piped output keeps them whole. Numbers belong to
  one report; a later run may order rows differently. The list is a report,
  not a plan: `dw clean` still decides what it may delete, and `--json`
  carries the same rows, in the same order, as `analysis.avenues`.

`--min-size` accepts `K`, `M`, `G` or `T` and defaults to 0.5% of the measured
total. `--top N` (1–64, default 20) sets the length of each ranking. Application
bundles, frameworks and photo libraries are reported whole. The analysis reads
only what the scan retained: `ScanReport::deep(DeepOptions)` performs no
filesystem I/O. Because each directory retains its 64 largest children, the
largest-files ranking is exact up to 64 rows.

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
- On macOS a directory's entries and their block accounting are read together
  with `getattrlistbulk`, roughly halving scan time. Mount points, firmlinks and
  entries the kernel does not fully describe are inspected individually, and a
  directory's identity is still confirmed on its opened descriptor. Other
  platforms and filesystems without bulk support use `readdir` and `fstatat`.
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
use drivewalker::{DeepOptions, scan, plan, apply};

let mut usage = scan(&scan_options)?;
usage.expand_hotspots(6, 15); // Uses retained measurements, without filesystem I/O.
let analysis = usage.deep(DeepOptions::default()); // Tree, kinds, largest files, numbered avenues.
let cleanup = plan(&clean_options)?;
// Inspect cleanup.candidates(), cleanup.issues(), cleanup.skipped().
let result = apply(&cleanup)?;
```

`scan` handles traversal/accounting; `deep` derives the culprit tree, kind totals,
rankings and the numbered list from a finished scan; `policy` selects known targets and protects
installations; `plan` discovers, deduplicates, and measures candidates; `clean`
executes an opaque in-process plan; `unix` contains descriptor operations and the
unsafe FFI boundary; `work` records bounded-pool participation; `platform/macos`
provides volume, whole-disk root and snapshot context; `report`
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
Deep tests cover reporting at arbitrary depth after the tree is deleted, threshold
summaries, kind totals without double counting, and grouped unreadable locations.
They also cover the numbered list: its order, shared rows for small locations,
rows that do not overlap, the tree of shared folders, and installed tools never
being offered as regenerable.
A unit test checks that bulk directory reads agree with per-entry inspection.
