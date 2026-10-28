use crate::{
    Issue, absolute,
    policy::{caches, projects, protection},
    pool, scan,
    unix::{Dir, Identity, uid},
    work::{Scheduling, Tracker},
};
use anyhow::{Context, Result, ensure};
use rayon::prelude::*;
use serde::Serialize;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CleanKind {
    Caches,
    Projects,
    All,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CandidateKind {
    NodeDependencies,
    PythonBytecode,
    PythonEnvironment,
    Cache,
}

impl CandidateKind {
    pub fn reason(self) -> &'static str {
        match self {
            Self::NodeDependencies => "project Node dependencies",
            Self::PythonBytecode => "Python bytecode cache",
            Self::PythonEnvironment => "project Python environment",
            Self::Cache => "regenerable user cache",
        }
    }
}

#[derive(Debug, Clone)]
pub struct CleanOptions {
    pub kind: CleanKind,
    pub home: PathBuf,
    pub project_root: Option<PathBuf>,
    pub excludes: Vec<PathBuf>,
    pub jobs: usize,
}

#[derive(Debug, Serialize)]
pub struct Candidate {
    pub path: PathBuf,
    pub kind: CandidateKind,
    pub allocated_bytes: u64,
    pub(crate) identity: Identity,
    pub(crate) parent_identity: Identity,
}

/// Constructed only by `plan`; it cannot be deserialized from untrusted JSON.
#[derive(Debug, Serialize)]
pub struct CleanPlan {
    pub(crate) kind: CleanKind,
    pub(crate) home: PathBuf,
    pub(crate) project_root: Option<PathBuf>,
    pub(crate) excludes: Vec<PathBuf>,
    pub(crate) jobs: usize,
    pub(crate) candidates: Vec<Candidate>,
    pub(crate) issues: Vec<Issue>,
    pub(crate) skipped: Vec<Issue>,
    pub scheduling: Scheduling,
}

impl CleanPlan {
    pub fn candidates(&self) -> &[Candidate] {
        &self.candidates
    }
    pub fn issues(&self) -> &[Issue] {
        &self.issues
    }
    pub fn skipped(&self) -> &[Issue] {
        &self.skipped
    }
    pub fn allocated_bytes(&self) -> u64 {
        self.candidates.iter().map(|c| c.allocated_bytes).sum()
    }
}

struct Discovery<'a> {
    home: &'a Path,
    work: &'a Tracker,
    excludes: &'a [PathBuf],
    raw: Vec<(PathBuf, CandidateKind)>,
    issues: Vec<Issue>,
    skipped: Vec<Issue>,
}

impl<'a> Discovery<'a> {
    fn empty(&self) -> Self {
        Self {
            home: self.home,
            work: self.work,
            excludes: self.excludes,
            raw: vec![],
            issues: vec![],
            skipped: vec![],
        }
    }
    fn merge(mut self, mut other: Self) -> Self {
        self.raw.append(&mut other.raw);
        self.issues.append(&mut other.issues);
        self.skipped.append(&mut other.skipped);
        self
    }

    fn excluded(&self, path: &Path) -> bool {
        self.excludes.iter().any(|p| path.starts_with(p))
    }
    fn issue(&mut self, path: &Path, error: impl ToString) {
        self.issues.push(Issue {
            path: path.into(),
            message: error.to_string(),
        });
    }
    fn skip(&mut self, path: &Path, reason: &str) {
        self.skipped.push(Issue {
            path: path.into(),
            message: reason.into(),
        });
    }

    fn projects(&mut self, dir: Dir, path: &Path, device: u64) {
        self.work.record();
        let names = match dir.names() {
            Ok(names) => names,
            Err(error) => {
                self.issue(path, error);
                return;
            }
        };
        // Recursive parallel iterators feed the same work-stealing queues.
        // Discovering a wide child creates more jobs; idle workers can steal them.
        let found = names
            .par_iter()
            .map(|name| {
                let mut branch = self.empty();
                branch.project_entry(&dir, name, path, device);
                branch
            })
            .reduce(|| self.empty(), Self::merge);
        self.raw.extend(found.raw);
        self.issues.extend(found.issues);
        self.skipped.extend(found.skipped);
    }

    fn project_entry(&mut self, dir: &Dir, name: &std::ffi::OsStr, path: &Path, device: u64) {
        self.work.record();
        let child = path.join(name);
        if self.excluded(&child) {
            self.skip(&child, "explicit exclusion");
            return;
        }
        if protection::project_protected(&child, self.home) {
            self.skip(&child, "installed tool, app, or state directory");
            return;
        }
        let stat = match dir.entry(name) {
            Ok(stat) => stat,
            Err(error) => {
                self.issue(&child, error);
                return;
            }
        };
        if !stat.directory {
            return;
        }
        if stat.identity.dev != device {
            self.skip(&child, "different filesystem");
            return;
        }
        if let Some(kind) = projects::classify(name) {
            if self.excludes.iter().any(|p| p.starts_with(&child)) {
                self.skip(&child, "contains an explicit exclusion");
            } else {
                self.raw.push((child, kind));
            }
        } else {
            match dir.child(name) {
                Ok(opened) => {
                    if opened.stat().map(|s| s.identity).ok() != Some(stat.identity) {
                        self.issue(&child, "directory changed during discovery");
                        return;
                    }
                    self.projects(opened, &child, device);
                }
                Err(error) => self.issue(&child, error),
            }
        }
    }

    /// Expand cache contents into candidates, preserving the cache root itself.
    fn cache_contents(&mut self, dir: Dir, path: &Path) {
        self.work.record();
        let names = match dir.names() {
            Ok(names) => names,
            Err(error) => {
                self.issue(path, error);
                return;
            }
        };
        let found = names
            .par_iter()
            .map(|name| {
                self.work.record();
                let mut branch = self.empty();
                let child = path.join(name);
                if branch.excluded(&child) {
                    branch.skip(&child, "explicit exclusion");
                } else if child == self.home.join("Library/Caches/hermit") {
                    match dir.child(name) {
                        Ok(opened) => branch.cache_contents(opened, &child),
                        Err(error) => branch.issue(&child, error),
                    }
                } else if protection::cache_protected(&child, self.home) {
                    branch.skip(&child, "installed Hermit tools");
                } else if self.excludes.iter().any(|p| p.starts_with(&child)) {
                    branch.skip(&child, "contains an explicit exclusion");
                } else {
                    branch.raw.push((child, CandidateKind::Cache));
                }
                branch
            })
            .reduce(|| self.empty(), Self::merge);
        self.raw.extend(found.raw);
        self.issues.extend(found.issues);
        self.skipped.extend(found.skipped);
    }
}

pub fn plan(options: &CleanOptions) -> Result<CleanPlan> {
    let pool = pool(options.jobs)?;
    let home = absolute(&options.home)?;
    ensure!(home != Path::new("/"), "home cannot be /");
    let home_dir = Dir::open(&home).context("open owned home without following symlinks")?;
    let home_device = home_dir.stat()?.identity.dev;
    ensure!(
        home_dir.stat()?.uid == uid(),
        "home must be owned by the current user"
    );
    let excludes = options
        .excludes
        .iter()
        .map(|p| absolute(p))
        .collect::<Result<Vec<_>>>()?;
    let project_root = options.project_root.as_deref().map(absolute).transpose()?;
    let work = Tracker::new(options.jobs);
    let mut discovery = Discovery {
        work: &work,
        home: &home,
        excludes: &excludes,
        raw: vec![],
        issues: vec![],
        skipped: vec![],
    };
    if options.kind != CleanKind::Caches {
        let root = project_root
            .as_ref()
            .context("project cleanup requires an explicit path")?;
        ensure!(
            root != Path::new("/") && root != Path::new("/Users"),
            "project cleanup requires a bounded user directory"
        );
        ensure!(
            !protection::project_protected(root, &home),
            "project root is a protected installation or state directory"
        );
        ensure!(!discovery.excluded(root), "project root is excluded");
        let dir = Dir::open(root).context("open project root without following symlinks")?;
        let stat = dir.stat()?;
        ensure!(
            stat.uid == uid(),
            "project root must be owned by the current user"
        );
        // The selected root is a boundary, never itself a deletion candidate.
        pool.install(|| discovery.projects(dir, root, stat.identity.dev));
    }
    if options.kind != CleanKind::Projects {
        pool.install(|| {
            let roots = caches::discover(&home, &mut discovery.issues, &work);
            let found = roots
                .par_iter()
                .map(|root| {
                    let mut branch = discovery.empty();
                    if branch.excluded(root) {
                        branch.skip(root, "explicit exclusion");
                        return branch;
                    }
                    match Dir::open(root) {
                        Ok(dir) => match dir.stat() {
                            Ok(stat) if stat.identity.dev == home_device => {
                                branch.cache_contents(dir, root)
                            }
                            Ok(_) => branch.skip(root, "different filesystem"),
                            Err(error) => branch.issue(root, error),
                        },
                        Err(error) => branch.issue(root, error),
                    }
                    branch
                })
                .reduce(|| discovery.empty(), Discovery::merge);
            discovery.raw.extend(found.raw);
            discovery.issues.extend(found.issues);
            discovery.skipped.extend(found.skipped);
        });
    }
    discovery.raw.sort_by(|a, b| a.0.cmp(&b.0));
    discovery.raw.dedup_by(|a, b| a.0 == b.0);
    let mut raw: Vec<(PathBuf, CandidateKind)> = vec![];
    for candidate in discovery.raw {
        if raw
            .last()
            .is_none_or(|(parent, _)| !candidate.0.starts_with(parent))
        {
            raw.push(candidate);
        }
    }
    let measured = pool.install(|| {
        raw.into_par_iter()
            .map(|(path, kind)| {
                let error_path = path.clone();
                let result = (|| -> Result<(Candidate, Vec<Issue>)> {
                    let parent = Dir::open(path.parent().context("candidate has no parent")?)?;
                    let name = path.file_name().context("candidate has no filename")?;
                    let parent_identity = parent.stat()?.identity;
                    let stat = parent.entry(name)?;
                    ensure!(
                        stat.identity.dev == parent_identity.dev,
                        "candidate is a filesystem mount point"
                    );
                    ensure!(
                        stat.uid == uid(),
                        "candidate is not owned by the current user"
                    );
                    let (allocated_bytes, issues) = if stat.directory {
                        let dir = parent.child(name)?;
                        ensure!(
                            dir.stat()?.identity == stat.identity,
                            "candidate changed during planning"
                        );
                        scan::measure(dir, &path, &excludes, &work)
                    } else {
                        (stat.allocated, vec![])
                    };
                    Ok((
                        Candidate {
                            path,
                            kind,
                            allocated_bytes,
                            identity: stat.identity,
                            parent_identity,
                        },
                        issues,
                    ))
                })();
                (error_path, result)
            })
            .collect::<Vec<_>>()
    });
    let mut candidates = vec![];
    for (path, result) in measured {
        match result {
            Ok((candidate, issues)) => {
                candidates.push(candidate);
                discovery.issues.extend(issues);
            }
            Err(error) => discovery.issues.push(Issue {
                path,
                message: format!("candidate measurement failed: {error:#}"),
            }),
        }
    }
    candidates.sort_by(|a, b| {
        b.allocated_bytes
            .cmp(&a.allocated_bytes)
            .then(a.path.cmp(&b.path))
    });
    discovery
        .issues
        .sort_by(|a, b| a.path.cmp(&b.path).then(a.message.cmp(&b.message)));
    discovery
        .skipped
        .sort_by(|a, b| a.path.cmp(&b.path).then(a.message.cmp(&b.message)));
    let Discovery {
        issues, skipped, ..
    } = discovery;
    Ok(CleanPlan {
        kind: options.kind,
        home,
        project_root,
        excludes,
        jobs: options.jobs,
        candidates,
        issues,
        skipped,
        scheduling: work.snapshot(),
    })
}
