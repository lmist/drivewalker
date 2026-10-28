//! Drivewalker: measured disk usage and preview-first, policy-bound cleanup.
#![cfg(unix)]

pub mod clean;
pub mod plan;
pub mod platform;
pub mod policy;
pub mod report;
pub mod scan;
mod unix;
mod work;

use serde::Serialize;
use std::path::PathBuf;

pub use clean::{CleanReport, apply};
pub use plan::{Candidate, CandidateKind, CleanKind, CleanOptions, CleanPlan, plan};
pub use scan::{ScanOptions, ScanReport, scan};
pub use work::Scheduling;

#[derive(Debug, Clone, Serialize)]
pub struct Issue {
    pub path: PathBuf,
    pub message: String,
}

pub(crate) fn absolute(path: &std::path::Path) -> anyhow::Result<PathBuf> {
    use std::path::Component;
    let path = if path.is_absolute() {
        path.to_owned()
    } else {
        std::env::current_dir()?.join(path)
    };
    let mut result = PathBuf::new();
    for part in path.components() {
        match part {
            Component::ParentDir => anyhow::bail!(
                "parent components (..) are not accepted: {}",
                path.display()
            ),
            Component::CurDir => {}
            _ => result.push(part.as_os_str()),
        }
    }
    Ok(result)
}

pub(crate) fn pool(jobs: usize) -> anyhow::Result<rayon::ThreadPool> {
    anyhow::ensure!((1..=64).contains(&jobs), "jobs must be between 1 and 64");
    Ok(rayon::ThreadPoolBuilder::new().num_threads(jobs).build()?)
}
