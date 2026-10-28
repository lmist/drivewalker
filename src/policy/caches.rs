use crate::{Issue, unix::Dir, work::Tracker};
use rayon::prelude::*;
use std::path::{Path, PathBuf};

const FIXED: &[&str] = &[
    "Library/Caches",
    ".cache",
    ".npm/_cacache",
    ".npm/_npx",
    ".npm/_prebuilds",
    "Library/pnpm/store",
    ".bun/install/cache",
    ".codex/cache",
    ".cargo/registry/cache",
    ".rustup/downloads",
    ".rustup/tmp",
];
const NAMES: &[&str] = &[
    "Cache",
    "Code Cache",
    "GPUCache",
    "DawnCache",
    "DawnGraphiteCache",
    "DawnWebGPUCache",
    "ShaderCache",
    "GrShaderCache",
    "GraphiteDawnCache",
    "GPUPersistentCache",
];
const PRUNE: &[&str] = &[
    "node_modules",
    ".git",
    ".venv",
    "venv",
    "IndexedDB",
    "Local Storage",
    "Session Storage",
    "databases",
    "File System",
    "extensions",
];

fn optional(path: &Path, issues: &mut Vec<Issue>) -> Option<Dir> {
    match Dir::open(path) {
        Ok(dir) => Some(dir),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => {
            issues.push(Issue {
                path: path.into(),
                message: error.to_string(),
            });
            None
        }
    }
}

#[derive(Default)]
struct Found {
    targets: Vec<PathBuf>,
    issues: Vec<Issue>,
}

impl Found {
    fn merge(mut self, mut other: Self) -> Self {
        self.targets.append(&mut other.targets);
        self.issues.append(&mut other.issues);
        self
    }
}

fn support(dir: Dir, path: &Path, depth: usize, work: &Tracker) -> Found {
    work.record();
    let names = match dir.names() {
        Ok(names) => names,
        Err(error) => {
            return Found {
                issues: vec![Issue {
                    path: path.into(),
                    message: error.to_string(),
                }],
                ..Found::default()
            };
        }
    };
    names
        .par_iter()
        .map(|name| {
            work.record();
            let child = path.join(name);
            let mut found = Found::default();
            match dir.entry(name) {
                Ok(stat) if stat.directory => {
                    if NAMES.contains(&name.to_string_lossy().as_ref()) {
                        found.targets.push(child);
                    } else if depth < 7 && !PRUNE.contains(&name.to_string_lossy().as_ref()) {
                        match dir.child(name) {
                            Ok(opened)
                                if opened.stat().map(|s| s.identity).ok()
                                    == Some(stat.identity) =>
                            {
                                return support(opened, &child, depth + 1, work);
                            }
                            Ok(_) => found.issues.push(Issue {
                                path: child,
                                message: "directory changed during discovery".into(),
                            }),
                            Err(error) => found.issues.push(Issue {
                                path: child,
                                message: error.to_string(),
                            }),
                        }
                    }
                }
                Ok(_) => {}
                Err(error) => found.issues.push(Issue {
                    path: child,
                    message: error.to_string(),
                }),
            }
            found
        })
        .reduce(Found::default, Found::merge)
}

pub(crate) fn discover(home: &Path, issues: &mut Vec<Issue>, work: &Tracker) -> Vec<PathBuf> {
    let mut targets = vec![];
    for relative in FIXED {
        let path = home.join(relative);
        if optional(&path, issues).is_some() {
            targets.push(path);
        }
    }
    for (base, suffix) in [
        ("Library/Containers", "Data/Library/Caches"),
        ("Library/Group Containers", "Library/Caches"),
    ] {
        let base = home.join(base);
        if let Some(dir) = optional(&base, issues) {
            match dir.names() {
                Ok(names) => {
                    for name in names {
                        match dir.entry(&name) {
                            Ok(stat) if stat.directory => {}
                            Ok(_) => continue,
                            Err(error) => {
                                issues.push(Issue {
                                    path: base.join(&name),
                                    message: error.to_string(),
                                });
                                continue;
                            }
                        }
                        let path = base.join(name).join(suffix);
                        if optional(&path, issues).is_some() {
                            targets.push(path);
                        }
                    }
                }
                Err(error) => issues.push(Issue {
                    path: base,
                    message: error.to_string(),
                }),
            }
        }
    }
    let base = home.join("Library/Application Support");
    if let Some(dir) = optional(&base, issues) {
        let found = support(dir, &base, 0, work);
        targets.extend(found.targets);
        issues.extend(found.issues);
    }
    targets.sort();
    targets.dedup();
    let mut disjoint: Vec<PathBuf> = vec![];
    for path in targets {
        if disjoint
            .last()
            .is_none_or(|parent| !path.starts_with(parent))
        {
            disjoint.push(path);
        }
    }
    disjoint
}
