use crate::{
    Issue,
    plan::{CandidateKind, CleanPlan},
    policy::protection,
    unix::{Dir, Identity, uid},
    work::{Scheduling, Tracker},
};
use anyhow::{Context, Result, ensure};
use rayon::prelude::*;
use serde::Serialize;
use std::{
    ffi::OsStr,
    io,
    path::{Path, PathBuf},
    time::Instant,
};

#[derive(Debug, Serialize)]
pub struct CleanReport {
    pub removed_candidates: usize,
    pub removed_entries: u64,
    pub free_space_path: PathBuf,
    pub free_before_bytes: u64,
    pub free_after_bytes: u64,
    pub net_free_change_bytes: i128,
    pub issues: Vec<Issue>,
    pub skipped: Vec<Issue>,
    pub elapsed_seconds: f64,
    pub partial: bool,
    pub scheduling: Scheduling,
}

#[derive(Default)]
struct Removal {
    entries: u64,
    root_removed: bool,
    issues: Vec<Issue>,
}

impl Removal {
    fn merge(mut self, mut other: Self) -> Self {
        self.entries += other.entries;
        self.issues.append(&mut other.issues);
        // root_removed describes this node only, never one of its children.
        self
    }
}

struct Cleaner<'a> {
    plan: &'a CleanPlan,
    work: &'a Tracker,
}

impl Cleaner<'_> {
    fn remove(
        &self,
        parent: &Dir,
        name: &OsStr,
        path: &Path,
        expected: Identity,
        device: u64,
        cache: bool,
    ) -> Removal {
        self.work.record();
        let mut outcome = Removal::default();
        let result = (|| -> Result<()> {
            ensure!(
                !self
                    .plan
                    .excludes
                    .iter()
                    .any(|p| path.starts_with(p) || p.starts_with(path)),
                "explicit exclusion"
            );
            ensure!(
                !protection::cache_protected(path, &self.plan.home),
                "protected Hermit installation"
            );
            if !cache {
                ensure!(
                    !protection::project_protected(path, &self.plan.home),
                    "protected tool or state directory"
                );
            }
            let stat = match parent.entry(name) {
                Ok(stat) => stat,
                Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
                Err(error) => return Err(error.into()),
            };
            ensure!(
                stat.identity == expected,
                "entry identity changed since planning"
            );
            ensure!(
                stat.identity.dev == device,
                "refusing to cross filesystem boundary"
            );
            ensure!(stat.uid == uid(), "entry is not owned by current user");
            if stat.directory {
                let dir = parent.child(name)?;
                ensure!(
                    dir.stat()?.identity == expected,
                    "directory changed while opening"
                );
                ensure!(
                    parent.entry(name)?.identity == expected,
                    "directory moved after opening"
                );
                if cache {
                    dir.cache_access()?;
                }
                // Blocking reduction is the postorder barrier: no parent is unlinked
                // until every child job has completed, including failed children.
                let children = dir
                    .names()?
                    .par_iter()
                    .map(|child_name| {
                        let child = path.join(child_name);
                        match dir.entry(child_name) {
                            Ok(stat) => {
                                self.remove(&dir, child_name, &child, stat.identity, device, cache)
                            }
                            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                                Removal::default()
                            }
                            Err(error) => Removal {
                                issues: vec![Issue {
                                    path: child,
                                    message: error.to_string(),
                                }],
                                ..Removal::default()
                            },
                        }
                    })
                    .reduce(Removal::default, Removal::merge);
                outcome = std::mem::take(&mut outcome).merge(children);
                ensure!(
                    parent.entry(name)?.identity == expected,
                    "directory moved during cleanup"
                );
            }
            parent.unlink(name, stat.directory)?;
            outcome.entries += 1;
            outcome.root_removed = true;
            Ok(())
        })();
        if let Err(error) = result {
            outcome.issues.push(Issue {
                path: path.into(),
                message: format!("{error:#}"),
            });
        }
        outcome
    }
}

/// Apply only an in-process, policy-produced plan. No escalation or retry loops.
pub fn apply(plan: &CleanPlan) -> Result<CleanReport> {
    let start = Instant::now();
    let home = Dir::open(&plan.home).context("reopen home without following symlinks")?;
    ensure!(home.stat()?.uid == uid(), "home ownership changed");
    let before = home.free_bytes()?;
    let work = Tracker::new(plan.jobs);
    let cleaner = Cleaner { plan, work: &work };
    let outcomes = crate::pool(plan.jobs)?.install(|| {
        plan.candidates
            .par_iter()
            .map(|candidate| {
                let result = (|| -> Result<Removal> {
                    let parent =
                        Dir::open(candidate.path.parent().context("candidate has no parent")?)?;
                    ensure!(
                        parent.stat()?.identity == candidate.parent_identity,
                        "candidate parent changed since planning"
                    );
                    if candidate.kind == CandidateKind::Cache {
                        parent.cache_access()?;
                    }
                    Ok(cleaner.remove(
                        &parent,
                        candidate
                            .path
                            .file_name()
                            .context("candidate has no name")?,
                        &candidate.path,
                        candidate.identity,
                        candidate.identity.dev,
                        candidate.kind == CandidateKind::Cache,
                    ))
                })();
                result.unwrap_or_else(|error| Removal {
                    issues: vec![Issue {
                        path: candidate.path.clone(),
                        message: format!("{error:#}"),
                    }],
                    ..Removal::default()
                })
            })
            .collect::<Vec<_>>()
    });
    let mut removed_candidates = 0;
    let mut removed_entries = 0;
    let mut issues = plan.issues.clone();
    for outcome in outcomes {
        removed_candidates += usize::from(outcome.root_removed);
        removed_entries += outcome.entries;
        issues.extend(outcome.issues);
    }
    let after = home.free_bytes()?;
    issues.sort_by(|a, b| a.path.cmp(&b.path));
    let partial = !issues.is_empty();
    Ok(CleanReport {
        removed_candidates,
        removed_entries,
        free_space_path: plan.home.clone(),
        free_before_bytes: before,
        free_after_bytes: after,
        net_free_change_bytes: after as i128 - before as i128,
        issues,
        skipped: plan.skipped.clone(),
        elapsed_seconds: start.elapsed().as_secs_f64(),
        partial,
        scheduling: work.snapshot(),
    })
}
