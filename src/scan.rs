use crate::{
    Issue, absolute, pool,
    unix::{Dir, Stat},
    work::{Scheduling, Tracker},
};
use anyhow::{Context, Result, ensure};
use rayon::prelude::*;
use serde::Serialize;
use std::{
    cmp::Reverse,
    collections::{BTreeMap, BinaryHeap, HashMap, HashSet},
    path::{Path, PathBuf},
    sync::{
        Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::Instant,
};

#[derive(Debug, Clone)]
pub struct ScanOptions {
    pub roots: Vec<PathBuf>,
    pub excludes: Vec<PathBuf>,
    pub depth: usize,
    pub jobs: usize,
}

#[derive(Debug, Serialize)]
pub struct Usage {
    pub path: PathBuf,
    pub allocated_bytes: u64,
    pub entries: u64,
    pub children: Vec<Usage>,
    pub directory: bool,
}

#[derive(Debug, Serialize)]
pub struct ScanReport {
    pub roots: Vec<Usage>,
    pub issues: Vec<Issue>,
    pub elapsed_seconds: f64,
    pub scheduling: Scheduling,
    pub hotspots: Vec<Hotspot>,
    #[serde(skip)]
    pub(crate) directories: BTreeMap<PathBuf, DirectorySnapshot>,
    pub accounting: &'static str,
}

/// Maximum retained immediate children per directory. All directory totals are
/// retained separately, including directories outside this local ranking.
pub const RETAINED_CHILDREN: usize = 64;

#[derive(Debug, Clone, Serialize)]
pub struct EntrySummary {
    pub path: PathBuf,
    pub allocated_bytes: u64,
    pub entries: u64,
    pub directory: bool,
}

#[derive(Debug)]
pub(crate) struct DirectorySnapshot {
    pub(crate) allocated_bytes: u64,
    pub(crate) entries: u64,
    pub(crate) children: Vec<EntrySummary>,
    pub(crate) total_children: usize,
}

#[derive(Debug, Serialize)]
pub struct Hotspot {
    pub path: PathBuf,
    pub allocated_bytes: u64,
    pub entries: u64,
    pub children: Vec<EntrySummary>,
    pub omitted_children: usize,
}

impl ScanReport {
    /// Best-first expansion of the largest measured branches. This method does
    /// no filesystem I/O: results remain usable after directories change/vanish.
    /// The caller receives the original measurement, not refreshed live values.
    pub fn drill(&self, expansions: usize, limit: usize) -> Vec<Hotspot> {
        let roots: HashSet<&Path> = self.roots.iter().map(|r| r.path.as_path()).collect();
        let mut children: HashMap<&Path, Vec<&Path>> = HashMap::new();
        let mut frontier = BinaryHeap::new();
        for (path, snapshot) in &self.directories {
            if roots.contains(path.as_path()) {
                continue;
            }
            if matches!(
                path.extension().and_then(|s| s.to_str()),
                Some("app" | "framework")
            ) {
                continue;
            }
            if let Some(parent) = path.parent() {
                children.entry(parent).or_default().push(path);
                if roots.contains(parent) && snapshot.total_children > 0 {
                    frontier.push((snapshot.allocated_bytes, Reverse(path.as_path())));
                }
            }
        }
        let mut result = Vec::new();
        while result.len() < expansions {
            let Some((_, Reverse(path))) = frontier.pop() else {
                break;
            };
            let snapshot = &self.directories[path];
            let visible: Vec<_> = snapshot.children.iter().take(limit).cloned().collect();
            result.push(Hotspot {
                path: path.into(),
                allocated_bytes: snapshot.allocated_bytes,
                entries: snapshot.entries,
                omitted_children: snapshot.total_children - visible.len(),
                children: visible,
            });
            if let Some(paths) = children.get(path) {
                for child in paths {
                    let snapshot = &self.directories[*child];
                    if snapshot.total_children > 0 {
                        frontier.push((snapshot.allocated_bytes, Reverse(*child)));
                    }
                }
            }
        }
        result
    }

    pub fn expand_hotspots(&mut self, expansions: usize, limit: usize) {
        self.hotspots = self.drill(expansions, limit);
    }
}

struct Walker<'a> {
    excludes: &'a [PathBuf],
    work: &'a Tracker,
    directories: Option<Mutex<BTreeMap<PathBuf, DirectorySnapshot>>>,
    issues: Mutex<Vec<Issue>>,
    seen: Mutex<HashSet<(u64, u64)>>,
    progress: Option<&'a AtomicU64>,
}

impl Walker<'_> {
    fn issue(&self, path: &Path, error: impl ToString) {
        self.issues.lock().unwrap().push(Issue {
            path: path.into(),
            message: error.to_string(),
        });
    }

    fn walk(&self, dir: Dir, path: &Path, depth: usize, device: u64) -> Usage {
        self.work.record();
        let mut usage = Usage {
            path: path.into(),
            allocated_bytes: 0,
            entries: 1,
            children: vec![],
            directory: true,
        };
        match dir.stat() {
            Ok(stat) => usage.allocated_bytes = stat.allocated,
            Err(error) => {
                self.issue(path, error);
                return usage;
            }
        }
        let listing = match dir.list() {
            Ok(listing) => listing,
            Err(error) => {
                self.issue(path, error);
                return usage;
            }
        };
        let results: Vec<Usage> = listing
            .par_iter()
            .filter_map(|listed| {
                self.work.record();
                let name = &listed.name;
                let child = path.join(name);
                if self
                    .excludes
                    .iter()
                    .any(|exclude| child.starts_with(exclude))
                {
                    return None;
                }
                // Most entries arrive already described by the directory read;
                // the rest are inspected individually, never through a link.
                let inspect = || match dir.entry(name) {
                    Ok(stat) => Some(stat),
                    Err(error) => {
                        self.issue(&child, error);
                        None
                    }
                };
                let mut stat = match listed.known {
                    Some(known) => Stat::known(known),
                    None => inspect()?,
                };
                if stat.identity.dev != device {
                    return None;
                }
                if stat.directory {
                    let opened = match dir.child(name) {
                        Ok(opened) => opened,
                        Err(error) => {
                            self.issue(&child, error);
                            return None;
                        }
                    };
                    let actual = opened.stat().map(|s| s.identity).ok();
                    if actual != Some(stat.identity) && listed.known.is_some() {
                        // The listing and the opened directory disagree; decide
                        // from a direct inspection, as for an undescribed entry.
                        stat = inspect()?;
                        if stat.identity.dev != device {
                            return None;
                        }
                    }
                    if !stat.directory || actual != Some(stat.identity) {
                        self.issue(&child, "directory changed while scanning");
                        return None;
                    }
                    Some(self.walk(opened, &child, depth.saturating_sub(1), device))
                } else {
                    // Count multiply linked files once; most files need no hash-table entry.
                    let duplicate = stat.links > 1
                        && !self
                            .seen
                            .lock()
                            .unwrap()
                            .insert((stat.identity.dev, stat.identity.ino));
                    Some(Usage {
                        path: child,
                        allocated_bytes: if duplicate { 0 } else { stat.allocated },
                        entries: 1,
                        children: vec![],
                        directory: false,
                    })
                }
            })
            .collect();
        if let Some(progress) = self.progress {
            progress.fetch_add(results.len() as u64, Ordering::Relaxed);
        }
        if let Some(directories) = &self.directories {
            let allocated_bytes = results.iter().fold(usage.allocated_bytes, |total, child| {
                total.saturating_add(child.allocated_bytes)
            });
            let entries = 1 + results.iter().map(|child| child.entries).sum::<u64>();
            let mut children: Vec<_> = results
                .iter()
                .map(|child| EntrySummary {
                    path: child.path.clone(),
                    allocated_bytes: child.allocated_bytes,
                    entries: child.entries,
                    directory: child.directory,
                })
                .collect();
            let total_children = children.len();
            // Linear selection bounds retained file metadata without sorting a
            // giant flat directory just to keep its 64 largest entries.
            let order = |a: &EntrySummary, b: &EntrySummary| {
                b.allocated_bytes
                    .cmp(&a.allocated_bytes)
                    .then(a.path.cmp(&b.path))
            };
            if children.len() > RETAINED_CHILDREN {
                children.select_nth_unstable_by(RETAINED_CHILDREN, order);
                children.truncate(RETAINED_CHILDREN);
            }
            children.sort_by(order);
            directories.lock().unwrap().insert(
                path.into(),
                DirectorySnapshot {
                    allocated_bytes,
                    entries,
                    children,
                    total_children,
                },
            );
        }
        for child in results {
            usage.allocated_bytes = usage.allocated_bytes.saturating_add(child.allocated_bytes);
            usage.entries += child.entries;
            if depth > 0 {
                usage.children.push(child);
            }
        }
        usage.children.sort_by(|a, b| {
            b.allocated_bytes
                .cmp(&a.allocated_bytes)
                .then(a.path.cmp(&b.path))
        });
        usage
    }
}

pub fn scan(options: &ScanOptions) -> Result<ScanReport> {
    scan_with_progress(options, None)
}

/// Like [`scan`], and adds every measured entry to `progress` as directories
/// complete, so a caller can show activity from another thread.
pub fn scan_with_progress(
    options: &ScanOptions,
    progress: Option<&AtomicU64>,
) -> Result<ScanReport> {
    ensure!(
        !options.roots.is_empty(),
        "at least one scan root is required"
    );
    ensure!(options.depth <= 16, "display depth must be at most 16");
    let start = Instant::now();
    let mut roots = options
        .roots
        .iter()
        .map(|p| absolute(p))
        .collect::<Result<Vec<_>>>()?;
    roots.sort();
    roots.dedup();
    ensure!(
        roots
            .iter()
            .all(|p| p != Path::new("/") && p != Path::new("/System/Volumes/Data")),
        "scan explicit disjoint roots instead of / or /System/Volumes/Data"
    );
    for (i, root) in roots.iter().enumerate() {
        ensure!(
            !roots
                .iter()
                .enumerate()
                .any(|(j, other)| i != j && root.starts_with(other)),
            "overlapping scan roots are not allowed"
        );
    }
    let excludes = options
        .excludes
        .iter()
        .map(|p| absolute(p))
        .collect::<Result<Vec<_>>>()?;
    for path in &roots {
        ensure!(
            !excludes.iter().any(|p| path.starts_with(p)),
            "scan root is excluded: {}",
            path.display()
        );
    }
    let work = Tracker::new(options.jobs);
    let walker = Walker {
        work: &work,
        directories: Some(Mutex::new(BTreeMap::new())),
        excludes: &excludes,
        issues: Mutex::new(vec![]),
        seen: Mutex::new(HashSet::new()),
        progress,
    };
    let roots = pool(options.jobs)?.install(|| {
        roots
            .par_iter()
            .filter_map(|path| {
                let result = (|| -> Result<Usage> {
                    let dir = Dir::open(path).with_context(|| {
                        format!("open {} without following symlinks", path.display())
                    })?;
                    let device = dir.stat()?.identity.dev;
                    Ok(walker.walk(dir, path, options.depth, device))
                })();
                match result {
                    Ok(usage) => Some(usage),
                    Err(error) => {
                        walker.issue(path, format!("{error:#}"));
                        None
                    }
                }
            })
            .collect::<Vec<_>>()
    });
    let mut issues = walker.issues.into_inner().unwrap();
    issues.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(ScanReport {
        roots,
        issues,
        elapsed_seconds: start.elapsed().as_secs_f64(),
        scheduling: work.snapshot(),
        hotspots: vec![],
        directories: walker.directories.unwrap().into_inner().unwrap(),
        accounting: "Allocated filesystem blocks; hard links counted once across roots. APFS clones, snapshots, compression, and live writes can differ from df. Other filesystems and symlink targets are not traversed.",
    })
}

pub(crate) fn measure(
    dir: Dir,
    path: &Path,
    excludes: &[PathBuf],
    work: &Tracker,
) -> (u64, Vec<Issue>) {
    let device = match dir.stat() {
        Ok(stat) => stat.identity.dev,
        Err(error) => {
            return (
                0,
                vec![Issue {
                    path: path.into(),
                    message: error.to_string(),
                }],
            );
        }
    };
    let walker = Walker {
        work,
        directories: None,
        excludes,
        issues: Mutex::new(vec![]),
        seen: Mutex::new(HashSet::new()),
        progress: None,
    };
    let bytes = walker.walk(dir, path, 0, device).allocated_bytes;
    (bytes, walker.issues.into_inner().unwrap())
}
