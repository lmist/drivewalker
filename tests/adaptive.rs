use drivewalker::{CleanKind, CleanOptions, ScanOptions, apply, plan, scan};
use std::{fs, os::unix::fs::PermissionsExt, path::Path};

fn fixture() -> tempfile::TempDir {
    tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap()
}

fn put(path: impl AsRef<Path>) {
    let path = path.as_ref();
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, vec![1; 8192]).unwrap();
}

fn options(home: &Path, jobs: usize) -> CleanOptions {
    CleanOptions {
        kind: CleanKind::Projects,
        home: home.into(),
        project_root: Some(home.join("projects")),
        excludes: vec![],
        jobs,
    }
}

#[test]
fn discovery_divides_one_large_workspace_and_matches_serial_policy() {
    let temp = fixture();
    let home = temp.path();
    for branch in 0..96 {
        for leaf in 0..4 {
            put(home.join(format!(
                "projects/workspace/branch{branch}/leaf{leaf}/source.rs"
            )));
        }
    }
    // Use no cleanup candidates: worker participation here can only come from
    // discovery, not the already-parallel candidate measurement phase.
    let parallel = plan(&options(home, 4)).unwrap();
    let serial = plan(&options(home, 1)).unwrap();
    assert!(parallel.candidates().is_empty());
    assert!(parallel.issues().is_empty());
    assert!(serial.issues().is_empty());
    assert_eq!(serial.scheduling.workers_used, 1);
    assert!(parallel.scheduling.workers_used > 1);
    assert!(parallel.scheduling.workers_used <= 4);

    put(home.join("projects/workspace/app/node_modules/remove"));
    put(home.join("projects/workspace/.hermit/pkg/node_modules/keep"));
    let parallel = plan(&options(home, 4)).unwrap();
    let serial = plan(&options(home, 1)).unwrap();
    let paths = |p: &drivewalker::CleanPlan| {
        p.candidates()
            .iter()
            .map(|c| c.path.clone())
            .collect::<Vec<_>>()
    };
    assert_eq!(paths(&parallel), paths(&serial));
    assert_eq!(parallel.allocated_bytes(), serial.allocated_bytes());
    assert_eq!(parallel.candidates().len(), 1);
    assert!(
        home.join("projects/workspace/.hermit/pkg/node_modules/keep")
            .exists()
    );
}

#[test]
fn one_large_cleanup_target_splits_internally_and_joins_before_parent_removal() {
    let temp = fixture();
    let home = temp.path();
    let target = home.join("projects/app/node_modules");
    for package in 0..96 {
        for file in 0..8 {
            put(target.join(format!("pkg{package}/lib/file{file}")));
        }
    }
    put(home.join("projects/app/source.rs"));
    let plan = plan(&options(home, 4)).unwrap();
    assert_eq!(plan.candidates().len(), 1);
    let result = apply(&plan).unwrap();
    assert!(!result.partial, "{:?}", result.issues);
    assert_eq!(result.removed_candidates, 1);
    assert!(result.scheduling.workers_used > 1);
    assert!(result.scheduling.workers_used <= 4);
    assert!(
        !target.exists(),
        "parent removal must await every child task"
    );
    assert!(home.join("projects/app/source.rs").exists());
}

#[test]
fn one_flat_target_can_share_file_deletions() {
    let temp = fixture();
    let home = temp.path();
    let target = home.join("projects/app/node_modules");
    for file in 0..512 {
        put(target.join(format!("file{file}")));
    }
    let result = apply(&plan(&options(home, 2)).unwrap()).unwrap();
    assert!(!result.partial, "{:?}", result.issues);
    assert_eq!(result.removed_candidates, 1);
    assert_eq!(result.scheduling.workers_used, 2);
    assert!(!target.exists());
}

#[test]
fn parallel_failures_keep_parent_but_do_not_stop_other_children() {
    let temp = fixture();
    let home = temp.path();
    let target = home.join("projects/app/node_modules");
    put(target.join("readonly/keep"));
    for package in 0..32 {
        put(target.join(format!("writable{package}/remove")));
    }
    fs::set_permissions(target.join("readonly"), fs::Permissions::from_mode(0o555)).unwrap();
    let result = apply(&plan(&options(home, 4)).unwrap()).unwrap();
    fs::set_permissions(target.join("readonly"), fs::Permissions::from_mode(0o755)).unwrap();
    assert!(result.partial);
    assert_eq!(result.removed_candidates, 0);
    assert!(target.join("readonly/keep").exists());
    assert!(result.removed_entries > 0);
    for package in 0..32 {
        assert!(!target.join(format!("writable{package}")).exists());
    }
}

#[test]
fn adaptive_drill_uses_original_measurements_after_tree_is_gone() {
    let temp = fixture();
    let root = temp.path().join("tree");
    for file in 0..32 {
        put(root.join(format!("large/deep/file{file}")));
    }
    put(root.join("small/file"));
    for file in 0..48 {
        put(root.join(format!("excluded/file{file}")));
    }
    let mut report = scan(&ScanOptions {
        roots: vec![root.clone()],
        excludes: vec![root.join("excluded")],
        depth: 0,
        jobs: 4,
    })
    .unwrap();
    assert!(report.roots[0].children.is_empty());
    assert!(report.issues.is_empty());
    fs::remove_dir_all(&root).unwrap();
    report.expand_hotspots(2, 5);
    assert_eq!(report.hotspots.len(), 2);
    assert_eq!(report.hotspots[0].path, root.join("large"));
    assert_eq!(report.hotspots[1].path, root.join("large/deep"));
    assert_eq!(report.hotspots[1].children.len(), 5);
    assert_eq!(report.hotspots[1].omitted_children, 27);
    let again = report.drill(2, 5);
    assert_eq!(again[1].allocated_bytes, report.hotspots[1].allocated_bytes);
    assert!(report.drill(0, 10).is_empty());
    assert!(report.scheduling.workers_used <= 4);
}

#[test]
fn retained_rankings_are_bounded_but_smaller_directories_remain_expandable() {
    let temp = fixture();
    let root = temp.path().join("tree");
    for file in 0..100 {
        put(root.join(format!("wide/file{file}")));
    }
    // Zero-byte directories rank below the 100 allocated files; their records
    // must still exist even though they fall outside the 64 retained children.
    for folder in 0..4 {
        let path = root.join(format!("wide/small{folder}/empty"));
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, []).unwrap();
    }
    let report = scan(&ScanOptions {
        roots: vec![root.clone()],
        excludes: vec![],
        depth: 0,
        jobs: 2,
    })
    .unwrap();
    let hotspots = report.drill(10, usize::MAX);
    assert_eq!(hotspots.len(), 5);
    assert_eq!(
        hotspots[0].children.len(),
        drivewalker::scan::RETAINED_CHILDREN
    );
    assert_eq!(hotspots[0].omitted_children, 40);
    assert!(hotspots.iter().any(|h| h.path == root.join("wide/small3")));
}
