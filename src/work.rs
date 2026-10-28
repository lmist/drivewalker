//! All recursive jobs join the caller's Rayon pool. No subtree creates threads.
use serde::Serialize;
use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Debug, Clone, Copy, Serialize)]
pub struct Scheduling {
    pub jobs: usize,
    pub workers_used: u32,
}

pub(crate) struct Tracker {
    jobs: usize,
    workers: AtomicU64,
}

impl Tracker {
    pub fn new(jobs: usize) -> Self {
        Self {
            jobs,
            workers: AtomicU64::new(0),
        }
    }

    pub fn record(&self) {
        if let Some(worker) = rayon::current_thread_index() {
            debug_assert!(worker < self.jobs);
            self.workers.fetch_or(1u64 << worker, Ordering::Relaxed);
        }
    }

    pub fn snapshot(&self) -> Scheduling {
        Scheduling {
            jobs: self.jobs,
            workers_used: self.workers.load(Ordering::Relaxed).count_ones(),
        }
    }
}
