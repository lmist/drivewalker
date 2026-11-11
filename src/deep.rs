//! Deep analysis of one retained measurement: where the space really sits.
//! Everything here is derived from the scan's directory index, so it performs
//! no filesystem I/O and describes the tree as it was when it was measured.
use crate::{
    Issue,
    scan::{DirectorySnapshot, EntrySummary, ScanReport},
};
use serde::Serialize;
use std::{
    cmp::Reverse,
    collections::{BTreeMap, BinaryHeap},
    path::{Path, PathBuf},
};

/// What becomes of the space once an entry is deleted.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum Effect {
    /// Nothing recreates it and nothing of the owner's is lost.
    StaysFreed,
    /// A build, install or launch recreates it.
    GrowsBack,
    /// Only the owner can say whether it is still needed.
    YourCall,
}

impl Effect {
    pub fn label(self) -> &'static str {
        match self {
            Self::StaysFreed => "stays freed",
            Self::GrowsBack => "grows back",
            Self::YourCall => "your call",
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct DeepOptions {
    /// Smallest entry that gets its own line. `None` uses 0.5% of the measured total.
    pub min_bytes: Option<u64>,
    /// Rows in each ranked list: concentrations, kinds, largest files and the
    /// unrecognised files and folders offered as the owner's call.
    pub top: usize,
}

impl Default for DeepOptions {
    fn default() -> Self {
        Self {
            min_bytes: None,
            top: 20,
        }
    }
}

/// One entry of at least the threshold size, with every such entry beneath it.
#[derive(Debug, Serialize)]
pub struct Node {
    pub path: PathBuf,
    pub allocated_bytes: u64,
    pub entries: u64,
    pub directory: bool,
    /// Space inside this directory that no listed child accounts for.
    pub unlisted_bytes: u64,
    pub unlisted_children: usize,
    /// The largest entries that fell below the threshold, to name the remainder.
    pub unlisted_largest: Vec<EntrySummary>,
    pub children: Vec<Node>,
}

/// Space that cannot be split further at the threshold: a single file, a
/// bundle, or a directory's many entries that are each below the threshold.
#[derive(Debug, Clone, Serialize)]
pub struct Concentration {
    pub path: PathBuf,
    pub allocated_bytes: u64,
    pub directory: bool,
    /// Entries sharing these bytes; zero for a file or an unopened bundle.
    pub items: usize,
}

/// Directories of one recognisable kind, added up wherever they occur.
#[derive(Debug, Serialize)]
pub struct Kind {
    pub label: &'static str,
    pub hint: &'static str,
    pub allocated_bytes: u64,
    pub locations: usize,
    pub largest: Vec<EntrySummary>,
}

/// One numbered row of what could be deleted. Rows never overlap, so their
/// sizes add up.
#[derive(Debug, Clone, Serialize)]
pub struct Avenue {
    /// Position in the list, from 1. Meaningful within this report only.
    pub number: usize,
    pub item: &'static str,
    /// The location itself, or the folder that contains all of `locations`.
    pub path: PathBuf,
    /// Bytes that no other row accounts for.
    pub allocated_bytes: u64,
    pub effect: Effect,
    pub how: &'static str,
    /// Locations added together in this row; one for a single path.
    pub locations: usize,
}

#[derive(Debug, Serialize)]
pub struct Unreadable {
    pub reason: String,
    pub hint: &'static str,
    pub locations: usize,
    pub examples: Vec<PathBuf>,
}

#[derive(Debug, Serialize)]
pub struct Analysis {
    pub total_bytes: u64,
    pub total_entries: u64,
    pub threshold_bytes: u64,
    pub tree: Vec<Node>,
    pub concentrations: Vec<Concentration>,
    pub kinds: Vec<Kind>,
    pub largest_files: Vec<EntrySummary>,
    pub unreadable: Vec<Unreadable>,
    /// What could be deleted, numbered, ordered by effect and then by size.
    pub avenues: Vec<Avenue>,
}

/// Bundles are presented as one item; their internals are rarely actionable.
fn bundle(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|s| s.to_str()),
        Some("app" | "framework" | "photoslibrary" | "xcarchive")
    )
}

/// A recognisable kind of directory or file: how it is totalled, how one
/// location of it is named in the numbered list, and what deleting it costs.
struct Spec {
    label: &'static str,
    hint: &'static str,
    item: &'static str,
    effect: Effect,
    how: &'static str,
}

const fn spec(
    label: &'static str,
    hint: &'static str,
    item: &'static str,
    effect: Effect,
    how: &'static str,
) -> Spec {
    Spec {
        label,
        hint,
        item,
        effect,
        how,
    }
}

const SIMULATORS: usize = 12;
const INSTALLERS: usize = 13;
const TELEMETRY: usize = 14;
const TOOLCHAINS: usize = 15;
const GLOBAL_PACKAGES: usize = 16;

/// `classify` returns positions in this table.
const KINDS: &[Spec] = &[
    spec(
        "Node dependencies (node_modules)",
        "regenerable: dw clean projects PATH",
        "Node dependencies",
        Effect::GrowsBack,
        "dw clean projects PATH",
    ),
    spec(
        "Python environments and bytecode",
        "regenerable: dw clean projects PATH",
        "Python env or bytecode",
        Effect::GrowsBack,
        "dw clean projects PATH",
    ),
    spec(
        "Rust build output (target)",
        "regenerable: cargo clean in each project",
        "Rust build output",
        Effect::GrowsBack,
        "cargo clean",
    ),
    spec(
        "JS/Swift/Gradle build and tool caches",
        "regenerable by the next build",
        "Build or tool cache",
        Effect::GrowsBack,
        "delete; the next build recreates it",
    ),
    spec(
        "Cache folders",
        "regenerable; dw clean caches covers the known user caches",
        "Cache folder",
        Effect::GrowsBack,
        "dw clean caches, or delete",
    ),
    spec(
        "Package manager stores",
        "re-downloaded on demand; prune with the package manager",
        "Package store",
        Effect::GrowsBack,
        "prune with the package manager",
    ),
    spec(
        "Git repository data (.git)",
        "history; shrink with git gc or shallow clones",
        "Git history",
        Effect::YourCall,
        "git gc, or re-clone shallow",
    ),
    spec(
        "Xcode build data",
        "regenerable; Xcode rebuilds it",
        "Xcode build data",
        Effect::GrowsBack,
        "delete; Xcode rebuilds it",
    ),
    spec(
        "Container and VM disk images",
        "prune images and volumes inside the tool",
        "Container or VM image",
        Effect::StaysFreed,
        "prune inside the tool",
    ),
    spec(
        "Local AI models",
        "delete models you no longer run",
        "Local AI model",
        Effect::YourCall,
        "delete models you no longer run",
    ),
    spec(
        "Application bundles",
        "uninstall apps you do not use",
        "Application",
        Effect::YourCall,
        "uninstall if unused",
    ),
    spec(
        "Trash",
        "empty the Trash",
        "Trash",
        Effect::StaysFreed,
        "empty the Trash",
    ),
    spec(
        "Simulator runtimes and devices",
        "remove unused ones with xcrun simctl or in Xcode",
        "Simulators",
        Effect::StaysFreed,
        "xcrun simctl, or Xcode settings",
    ),
    spec(
        "Installers and staged updates",
        "delete once the app is installed",
        "Installer",
        Effect::StaysFreed,
        "delete once installed",
    ),
    spec(
        "Telemetry backlogs",
        "unsent usage events; delete, and turn the tool's metrics off to stop it",
        "Telemetry backlog",
        Effect::GrowsBack,
        "delete; turn its metrics off",
    ),
    spec(
        "Language toolchains",
        "remove versions you no longer use with their version manager",
        "Toolchain",
        Effect::YourCall,
        "remove with its version manager",
    ),
    spec(
        "Globally installed packages",
        "installed tools; uninstall the ones you do not use",
        "Global packages",
        Effect::YourCall,
        "uninstall with the package manager",
    ),
];

/// Disk images and packages left where they were downloaded or staged.
fn installer(directory: &Path, file: &Path) -> bool {
    matches!(
        file.extension().and_then(|s| s.to_str()),
        Some("dmg" | "pkg" | "mpkg")
    ) && directory
        .components()
        .any(|part| matches!(part.as_os_str().to_str(), Some("Caskroom" | "Downloads")))
}

/// Folders the operating system or a package manager owns. They are measured
/// and classified, but their unrecognised contents are never offered.
fn system(path: &Path) -> bool {
    [
        "/System",
        "/Library",
        "/private",
        "/usr",
        "/bin",
        "/sbin",
        "/opt",
        "/Applications",
        "/cores",
    ]
    .iter()
    .any(|owned| path.starts_with(owned))
}

fn has_child(snapshot: &DirectorySnapshot, names: &[&str]) -> bool {
    snapshot.children.iter().any(|child| {
        child
            .path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| names.contains(&n))
    })
}

fn classify(path: &Path, snapshot: &DirectorySnapshot) -> Option<usize> {
    let name = path.file_name()?.to_str()?;
    let above = |levels: usize| {
        path.ancestors()
            .nth(levels)
            .and_then(Path::file_name)
            .and_then(|n| n.to_str())
            .unwrap_or("")
    };
    let (parent, grandparent) = (above(1), above(2));
    let lower = name.to_ascii_lowercase();
    Some(match name {
        // Packages installed as tools are not a project's regenerable dependencies.
        "node_modules" if parent == "lib" || parent == "global" || grandparent == "global" => {
            GLOBAL_PACKAGES
        }
        "node_modules" => 0,
        ".venv" | ".venvs" | "venv" | "__pycache__" => 1,
        // `target` is a common word; require Cargo's own markers inside it.
        "target"
            if has_child(
                snapshot,
                &["CACHEDIR.TAG", ".rustc_info.json", "debug", "release"],
            ) =>
        {
            2
        }
        ".next" | ".nuxt" | ".turbo" | ".svelte-kit" | ".parcel-cache" | ".vite" | ".build"
        | ".gradle" | ".tox" | ".mypy_cache" | ".pytest_cache" | ".ruff_cache" | ".zig-cache"
        | ".terraform" => 3,
        "store" if parent == "pnpm" => 5,
        "registry" | "git" if parent == ".cargo" => 5,
        "mod" if parent == "pkg" => 5,
        "repository" if parent == ".m2" => 5,
        ".git" => 6,
        "DerivedData" | "iOS DeviceSupport" => 7,
        "CoreSimulator" => SIMULATORS,
        "com.docker.docker" | ".docker" | ".colima" | ".lima" | ".orbstack" => 8,
        ".ollama" | ".lmstudio" | "OptGuideOnDeviceModel" => 9,
        ".Trash" | ".Trashes" => 11,
        "in_progress" if parent == "com.docker.install" => INSTALLERS,
        "eventsData" if parent == ".dolt" => TELEMETRY,
        _ if name.contains("SimulatorRuntime") => SIMULATORS,
        // One installed version each: rustup, ghcup, mise, nvm and the *env managers.
        _ if matches!(
            (grandparent, parent),
            (".rustup", "toolchains")
                | (".ghcup", "ghc")
                | ("mise", "installs")
                | ("versions", "node")
                | (".pyenv" | ".rbenv" | ".nodenv" | ".goenv", "versions")
        ) =>
        {
            TOOLCHAINS
        }
        // A data folder may merely be named like a bundle; a bundle has contents.
        _ if name.ends_with(".app") && has_child(snapshot, &["Contents", "Wrapper"]) => 10,
        _ if lower == "caches" || lower.ends_with("cache") || lower == "cacheddata" => 4,
        _ => return None,
    })
}

fn branch(report: &ScanReport, entry: &EntrySummary, threshold: u64) -> Node {
    let mut node = Node {
        path: entry.path.clone(),
        allocated_bytes: entry.allocated_bytes,
        entries: entry.entries,
        directory: entry.directory,
        unlisted_bytes: 0,
        unlisted_children: 0,
        unlisted_largest: vec![],
        children: vec![],
    };
    if !entry.directory {
        return node;
    }
    node.unlisted_bytes = entry.allocated_bytes;
    if bundle(&entry.path) {
        return node;
    }
    if let Some(snapshot) = report.directories.get(&entry.path) {
        // Retained children are sorted largest first.
        node.children = snapshot
            .children
            .iter()
            .take_while(|child| child.allocated_bytes >= threshold)
            .map(|child| branch(report, child, threshold))
            .collect();
        let listed: u64 = node.children.iter().map(|c| c.allocated_bytes).sum();
        node.unlisted_bytes = entry.allocated_bytes.saturating_sub(listed);
        node.unlisted_children = snapshot.total_children - node.children.len();
        node.unlisted_largest = snapshot
            .children
            .iter()
            .skip(node.children.len())
            .take(3)
            .filter(|child| child.allocated_bytes > 0)
            .cloned()
            .collect();
    }
    node
}

fn concentrate(node: &Node, threshold: u64, out: &mut Vec<Concentration>) {
    let exclusive = if node.directory {
        node.unlisted_bytes
    } else {
        node.allocated_bytes
    };
    // A leaf always qualifies; an expanded directory only if its remainder is large.
    if node.children.is_empty() || exclusive >= threshold {
        out.push(Concentration {
            path: node.path.clone(),
            allocated_bytes: exclusive,
            directory: node.directory,
            items: node.unlisted_children,
        });
    }
    for child in &node.children {
        concentrate(child, threshold, out);
    }
}

/// Every recognised location, per kind, largest first.
fn locate(report: &ScanReport) -> Vec<Vec<EntrySummary>> {
    let mut found: Vec<Vec<EntrySummary>> = KINDS.iter().map(|_| vec![]).collect();
    // Paths iterate parent-first with descendants contiguous, so remembering
    // the last match is enough to count only the outermost directory.
    let mut covered: Option<&Path> = None;
    for (path, snapshot) in &report.directories {
        if covered.is_some_and(|outer| path.starts_with(outer)) {
            continue;
        }
        if let Some(kind) = classify(path, snapshot) {
            covered = Some(path);
            found[kind].push(EntrySummary {
                path: path.clone(),
                allocated_bytes: snapshot.allocated_bytes,
                entries: snapshot.entries,
                directory: true,
            });
            continue;
        }
        found[INSTALLERS].extend(
            snapshot
                .children
                .iter()
                .filter(|child| !child.directory && installer(path, &child.path))
                .cloned(),
        );
    }
    for locations in &mut found {
        locations.sort_by(|a, b| {
            b.allocated_bytes
                .cmp(&a.allocated_bytes)
                .then(a.path.cmp(&b.path))
        });
    }
    found
}

fn kinds(found: Vec<Vec<EntrySummary>>, top: usize, floor: u64) -> Vec<Kind> {
    let mut kinds: Vec<Kind> = found
        .into_iter()
        .zip(KINDS)
        .filter(|(locations, _)| !locations.is_empty())
        .map(|(locations, spec)| Kind {
            label: spec.label,
            hint: spec.hint,
            allocated_bytes: locations.iter().map(|l| l.allocated_bytes).sum(),
            locations: locations.len(),
            largest: locations.into_iter().take(3).collect(),
        })
        // A kind below a thousandth of the total explains nothing.
        .filter(|kind| kind.allocated_bytes > floor)
        .collect();
    kinds.sort_by(|a, b| {
        b.allocated_bytes
            .cmp(&a.allocated_bytes)
            .then(a.label.cmp(b.label))
    });
    kinds.truncate(top);
    kinds
}

/// The deepest folder that contains every one of `locations`.
fn common(locations: &[&EntrySummary]) -> PathBuf {
    let first = &locations[0].path;
    let mut shared = first.parent().unwrap_or(first).to_owned();
    for location in &locations[1..] {
        while !location.path.starts_with(&shared) && shared.pop() {}
    }
    shared
}

fn leaves<'a>(node: &'a Node, out: &mut Vec<&'a Node>) {
    if node.children.is_empty() {
        out.push(node);
    }
    for child in &node.children {
        leaves(child, out);
    }
}

/// The numbered list of what could go. A recognised location of at least the
/// threshold gets its own row and the smaller ones of its kind share one.
/// The largest unrecognised files and folders outside system locations follow
/// as the owner's call, counting only bytes no recognised row already claims.
fn avenues(
    found: &[Vec<EntrySummary>],
    tree: &[Node],
    threshold: u64,
    floor: u64,
    top: usize,
) -> Vec<Avenue> {
    let mut rows = vec![];
    let mut claimed: BTreeMap<&Path, u64> = BTreeMap::new();
    for (locations, spec) in found.iter().zip(KINDS) {
        let row = |path: PathBuf, allocated_bytes: u64, locations: usize| Avenue {
            number: 0,
            item: spec.item,
            path,
            allocated_bytes,
            effect: spec.effect,
            how: spec.how,
            locations,
        };
        // What regenerates inside a system or package-manager location is
        // that owner's to clear: totalled by kind, but not offered here.
        let offered: Vec<&EntrySummary> = locations
            .iter()
            .filter(|l| spec.effect != Effect::GrowsBack || !system(&l.path))
            .collect();
        let large = offered.partition_point(|l| l.allocated_bytes >= threshold);
        let (large, small) = offered.split_at(large);
        rows.extend(
            large
                .iter()
                .map(|l| row(l.path.clone(), l.allocated_bytes, 1)),
        );
        let rest: u64 = small.iter().map(|l| l.allocated_bytes).sum();
        if rest > floor {
            let path = match small {
                [only] => only.path.clone(),
                _ => common(small),
            };
            rows.push(row(path, rest, small.len()));
        }
        claimed.extend(
            locations
                .iter()
                .map(|l| (l.path.as_path(), l.allocated_bytes)),
        );
    }

    let mut unrecognised = vec![];
    // A scanned root is a boundary, never itself something to delete.
    for child in tree.iter().flat_map(|root| &root.children) {
        leaves(child, &mut unrecognised);
    }
    let mut own: Vec<Avenue> = unrecognised
        .into_iter()
        .filter(|node| !system(&node.path))
        .filter(|node| !node.path.ancestors().any(|a| claimed.contains_key(a)))
        .filter_map(|node| {
            let inside: u64 = claimed
                .range(node.path.as_path()..)
                .take_while(|(path, _)| path.starts_with(&node.path))
                .map(|(_, bytes)| bytes)
                .sum();
            let allocated_bytes = node.allocated_bytes.saturating_sub(inside);
            (allocated_bytes >= threshold).then(|| Avenue {
                number: 0,
                item: if node.directory { "Folder" } else { "File" },
                path: node.path.clone(),
                allocated_bytes,
                effect: Effect::YourCall,
                how: "review before deleting",
                locations: 1,
            })
        })
        .collect();
    own.sort_by(|a, b| {
        b.allocated_bytes
            .cmp(&a.allocated_bytes)
            .then(a.path.cmp(&b.path))
    });
    own.truncate(top);
    rows.extend(own);

    rows.sort_by(|a, b| {
        a.effect
            .cmp(&b.effect)
            .then(b.allocated_bytes.cmp(&a.allocated_bytes))
            .then(a.path.cmp(&b.path))
    });
    for (index, row) in rows.iter_mut().enumerate() {
        row.number = index + 1;
    }
    rows
}

fn largest_files(report: &ScanReport, top: usize) -> Vec<EntrySummary> {
    // Every directory retains its largest children, so any file in the global
    // top `top` (at most the retention limit) is present in the index.
    let top = top.min(crate::scan::RETAINED_CHILDREN);
    let mut heap = BinaryHeap::with_capacity(top + 1);
    for snapshot in report.directories.values() {
        for child in snapshot.children.iter().filter(|c| !c.directory) {
            heap.push(Reverse((child.allocated_bytes, &child.path)));
            if heap.len() > top {
                heap.pop();
            }
        }
    }
    let mut files: Vec<EntrySummary> = heap
        .into_iter()
        .filter(|Reverse((bytes, _))| *bytes > 0)
        .map(|Reverse((bytes, path))| EntrySummary {
            path: path.clone(),
            allocated_bytes: bytes,
            entries: 1,
            directory: false,
        })
        .collect();
    files.sort_by(|a, b| {
        b.allocated_bytes
            .cmp(&a.allocated_bytes)
            .then(a.path.cmp(&b.path))
    });
    files
}

fn unreadable(issues: &[Issue]) -> Vec<Unreadable> {
    // "open X without following symlinks: Permission denied" and a bare
    // "Permission denied" are the same reason: group by the underlying error.
    let mut groups: BTreeMap<&str, Vec<&Path>> = BTreeMap::new();
    for issue in issues {
        let reason = issue.message.rsplit(": ").next().unwrap_or(&issue.message);
        groups.entry(reason).or_default().push(&issue.path);
    }
    let mut groups: Vec<Unreadable> = groups
        .into_iter()
        .map(|(reason, paths)| Unreadable {
            reason: reason.into(),
            hint: if reason.contains("os error 1)") {
                "protected by macOS privacy controls; grant the terminal Full Disk Access to measure these"
            } else if reason.contains("os error 13)") {
                "owned by root or another user; run dw with sudo to measure these"
            } else if reason.contains("os error 11)") {
                "online-only cloud items; they hold no local data until downloaded"
            } else {
                ""
            },
            locations: paths.len(),
            examples: paths.iter().take(3).map(|p| p.to_path_buf()).collect(),
        })
        .collect();
    groups.sort_by(|a, b| b.locations.cmp(&a.locations).then(a.reason.cmp(&b.reason)));
    groups
}

impl ScanReport {
    /// Follow every branch down until nothing beneath it reaches the threshold,
    /// then rank where the bytes concentrate, what kinds of data they are and
    /// which single files are largest. Uses retained measurements only.
    pub fn deep(&self, options: DeepOptions) -> Analysis {
        let total_bytes: u64 = self.roots.iter().map(|r| r.allocated_bytes).sum();
        let threshold = options.min_bytes.unwrap_or(total_bytes / 200).max(1);
        let mut tree: Vec<Node> = self
            .roots
            .iter()
            .map(|root| {
                branch(
                    self,
                    &EntrySummary {
                        path: root.path.clone(),
                        allocated_bytes: root.allocated_bytes,
                        entries: root.entries,
                        directory: root.directory,
                    },
                    threshold,
                )
            })
            .collect();
        tree.sort_by(|a, b| {
            b.allocated_bytes
                .cmp(&a.allocated_bytes)
                .then(a.path.cmp(&b.path))
        });
        let mut concentrations = vec![];
        for root in &tree {
            concentrate(root, threshold, &mut concentrations);
        }
        concentrations.sort_by(|a, b| {
            b.allocated_bytes
                .cmp(&a.allocated_bytes)
                .then(a.path.cmp(&b.path))
        });
        concentrations.truncate(options.top);
        let found = locate(self);
        let floor = total_bytes / 1000;
        let avenues = avenues(&found, &tree, threshold, floor, options.top);
        Analysis {
            total_bytes,
            total_entries: self.roots.iter().map(|r| r.entries).sum(),
            threshold_bytes: threshold,
            tree,
            concentrations,
            kinds: kinds(found, options.top, floor),
            largest_files: largest_files(self, options.top),
            unreadable: unreadable(&self.issues),
            avenues,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_locations_and_installers_are_recognised_by_path() {
        assert!(system(Path::new("/Library/Caches")));
        assert!(system(Path::new("/opt/homebrew/lib/node_modules")));
        assert!(system(Path::new("/System/Volumes/Data/System/Library")));
        assert!(!system(Path::new("/Users/me/Library/Caches")));

        let cask = Path::new("/opt/homebrew/Caskroom/word/16.1");
        assert!(installer(cask, Path::new("Word_Installer.pkg")));
        assert!(installer(
            Path::new("/Users/me/Downloads"),
            Path::new("App.dmg")
        ));
        assert!(!installer(
            Path::new("/Users/me/Documents"),
            Path::new("App.dmg")
        ));
        assert!(!installer(
            Path::new("/Users/me/Downloads"),
            Path::new("notes.txt")
        ));
    }
}
