use drivewalker::{CleanKind, CleanOptions, ScanOptions, apply, plan, scan};
use std::{
    fs,
    os::unix::fs::{MetadataExt, PermissionsExt, symlink},
    path::{Path, PathBuf},
};

fn fixture() -> tempfile::TempDir {
    // Use the checkout to avoid macOS's symlinked /var temporary-directory path.
    tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap()
}

fn put(path: impl AsRef<Path>) {
    let path = path.as_ref();
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, vec![42; 8192]).unwrap();
}

fn options(home: &Path, kind: CleanKind) -> CleanOptions {
    CleanOptions {
        kind,
        home: home.into(),
        project_root: Some(home.join("projects")),
        excludes: vec![],
        jobs: 2,
    }
}

#[test]
fn preview_is_read_only_and_apply_preserves_source_and_installations() {
    let fixture = fixture();
    let home = fixture.path();
    for file in [
        "projects/app/node_modules/pkg/index.js",
        "projects/app/.venv/bin/python",
        "projects/app/src/main.py",
        "projects/app/.hermit/pkg/tool/node_modules/keep",
        "projects/app/Editor.app/Contents/node_modules/keep",
        "projects/app/.git/objects/keep",
    ] {
        put(home.join(file));
    }
    let plan = plan(&options(home, CleanKind::Projects)).unwrap();
    assert_eq!(plan.candidates().len(), 2);
    assert!(home.join("projects/app/node_modules/pkg/index.js").exists());
    assert!(plan.issues().is_empty(), "{:?}", plan.issues());
    let report = apply(&plan).unwrap();
    assert!(!report.partial, "{:?}", report.issues);
    assert!(!home.join("projects/app/node_modules").exists());
    assert!(!home.join("projects/app/.venv").exists());
    for file in [
        "projects/app/src/main.py",
        "projects/app/.hermit/pkg/tool/node_modules/keep",
        "projects/app/Editor.app/Contents/node_modules/keep",
        "projects/app/.git/objects/keep",
    ] {
        assert!(home.join(file).exists(), "{file}");
    }
}

#[test]
fn cache_roots_and_hermit_tools_survive() {
    let fixture = fixture();
    let home = fixture.path();
    for file in [
        "Library/Caches/hermit/pkg/tool/keep",
        "Library/Caches/hermit/downloads/remove",
        ".cache/app/remove",
        ".codex/cache/remove",
        ".codex/sessions/keep",
        ".codex/packages/keep",
        "Library/Application Support/Browser/Default/Code Cache/remove",
        "Library/Application Support/Browser/Default/Local Storage/keep",
        "Library/Containers/app/Data/Library/Caches/remove",
        "Library/Group Containers/app/Library/Caches/remove",
    ] {
        put(home.join(file));
    }
    let plan = plan(&options(home, CleanKind::Caches)).unwrap();
    assert!(plan.issues().is_empty(), "{:?}", plan.issues());
    let report = apply(&plan).unwrap();
    assert!(!report.partial, "{:?}", report.issues);
    for file in [
        "Library/Caches/hermit/pkg/tool/keep",
        ".codex/sessions/keep",
        ".codex/packages/keep",
        "Library/Application Support/Browser/Default/Local Storage/keep",
    ] {
        assert!(home.join(file).exists(), "{file}");
    }
    for dir in [
        ".cache",
        ".codex/cache",
        "Library/Caches",
        "Library/Application Support/Browser/Default/Code Cache",
        "Library/Containers/app/Data/Library/Caches",
        "Library/Group Containers/app/Library/Caches",
    ] {
        assert!(home.join(dir).is_dir(), "{dir}");
    }
    assert!(!home.join("Library/Caches/hermit/downloads").exists());
}

#[test]
fn symlinks_never_redirect_discovery_or_deletion() {
    let fixture = fixture();
    let home = fixture.path();
    put(home.join("outside/keep"));
    put(home.join("projects/app/node_modules/remove"));
    symlink(
        home.join("outside"),
        home.join("projects/app/node_modules/link"),
    )
    .unwrap();
    symlink(home.join("outside"), home.join("projects/linked-project")).unwrap();
    let plan = plan(&options(home, CleanKind::Projects)).unwrap();
    let report = apply(&plan).unwrap();
    assert!(!report.partial, "{:?}", report.issues);
    assert!(home.join("outside/keep").exists());
    assert!(home.join("projects/linked-project").is_symlink());
}

#[test]
fn replaced_candidate_is_refused() {
    let fixture = fixture();
    let home = fixture.path();
    put(home.join("outside/keep"));
    put(home.join("projects/app/node_modules/remove"));
    let plan = plan(&options(home, CleanKind::Projects)).unwrap();
    fs::rename(
        home.join("projects/app/node_modules"),
        home.join("original"),
    )
    .unwrap();
    symlink(home.join("outside"), home.join("projects/app/node_modules")).unwrap();
    let report = apply(&plan).unwrap();
    assert!(report.partial);
    assert!(home.join("outside/keep").exists());
    assert!(home.join("original/remove").exists());
}

#[test]
fn replaced_ancestor_is_refused() {
    let fixture = fixture();
    let home = fixture.path();
    put(home.join("outside/node_modules/keep"));
    put(home.join("projects/app/node_modules/remove"));
    let plan = plan(&options(home, CleanKind::Projects)).unwrap();
    fs::rename(home.join("projects/app"), home.join("original")).unwrap();
    symlink(home.join("outside"), home.join("projects/app")).unwrap();
    assert!(apply(&plan).unwrap().partial);
    assert!(home.join("outside/node_modules/keep").exists());
}

#[test]
fn excludes_protect_entire_dependency_candidate() {
    let fixture = fixture();
    let home = fixture.path();
    put(home.join("projects/app/node_modules/keep"));
    put(home.join("projects/app/__pycache__/remove"));
    let mut options = options(home, CleanKind::Projects);
    options
        .excludes
        .push(home.join("projects/app/node_modules/keep"));
    let plan = plan(&options).unwrap();
    assert_eq!(plan.candidates().len(), 1);
    assert!(!apply(&plan).unwrap().partial);
    assert!(home.join("projects/app/node_modules/keep").exists());
}

#[test]
fn late_cache_siblings_survive_and_apply_is_repeatable() {
    let fixture = fixture();
    let home = fixture.path();
    put(home.join(".cache/old/remove"));
    let plan = plan(&options(home, CleanKind::Caches)).unwrap();
    put(home.join(".cache/new/keep"));
    assert!(!apply(&plan).unwrap().partial);
    assert!(home.join(".cache/new/keep").exists());
    assert!(!apply(&plan).unwrap().partial);
}

#[test]
fn cache_directory_permissions_can_be_repaired_without_chmodding_files() {
    let fixture = fixture();
    let home = fixture.path();
    put(home.join(".cache/pkg/remove"));
    put(home.join("outside/keep"));
    fs::hard_link(home.join("outside/keep"), home.join(".cache/pkg/linked")).unwrap();
    fs::set_permissions(home.join("outside/keep"), fs::Permissions::from_mode(0o444)).unwrap();
    fs::set_permissions(home.join(".cache/pkg"), fs::Permissions::from_mode(0o555)).unwrap();
    let plan = plan(&options(home, CleanKind::Caches)).unwrap();
    assert!(!apply(&plan).unwrap().partial);
    assert_eq!(
        fs::metadata(home.join("outside/keep")).unwrap().mode() & 0o777,
        0o444
    );
    assert!(home.join("outside/keep").exists());
}

#[test]
fn scan_counts_blocks_once_and_does_not_follow_links() {
    let fixture = fixture();
    let root = fixture.path();
    put(root.join("tree/a"));
    fs::hard_link(root.join("tree/a"), root.join("tree/b")).unwrap();
    put(root.join("outside/big"));
    symlink(root.join("outside"), root.join("tree/link")).unwrap();
    let result = scan(&ScanOptions {
        roots: vec![root.join("tree")],
        excludes: vec![],
        depth: 2,
        jobs: 2,
    })
    .unwrap();
    assert!(result.issues.is_empty());
    let expected = fs::symlink_metadata(root.join("tree")).unwrap().blocks() * 512
        + fs::metadata(root.join("tree/a")).unwrap().blocks() * 512
        + fs::symlink_metadata(root.join("tree/link"))
            .unwrap()
            .blocks()
            * 512;
    assert_eq!(result.roots[0].allocated_bytes, expected);
    assert_eq!(result.roots[0].entries, 4);
}

#[test]
fn rejects_ambiguous_roots_and_symlink_components() {
    let fixture = fixture();
    let home = fixture.path();
    fs::create_dir(home.join("projects")).unwrap();
    let mut options = options(home, CleanKind::Projects);
    options.project_root = None;
    assert!(plan(&options).is_err());
    options.project_root = Some(PathBuf::from("/"));
    assert!(plan(&options).is_err());
    options.project_root = Some(home.join("projects/../projects"));
    assert!(plan(&options).is_err());
    symlink(home.join("projects"), home.join("alias")).unwrap();
    options.project_root = Some(home.join("alias"));
    assert!(plan(&options).is_err());
    assert!(
        scan(&ScanOptions {
            roots: vec![home.into(), home.join("projects")],
            excludes: vec![],
            depth: 1,
            jobs: 2
        })
        .is_err()
    );
}

#[test]
fn protected_project_permissions_produce_a_partial_result_without_chmod() {
    let fixture = fixture();
    let home = fixture.path();
    put(home.join("projects/app/node_modules/keep"));
    let target = home.join("projects/app/node_modules");
    fs::set_permissions(&target, fs::Permissions::from_mode(0o555)).unwrap();
    let plan = plan(&options(home, CleanKind::Projects)).unwrap();
    let result = apply(&plan).unwrap();
    let mode = fs::metadata(&target).unwrap().mode() & 0o777;
    // Restore fixture permissions before any assertions or automatic teardown.
    fs::set_permissions(&target, fs::Permissions::from_mode(0o755)).unwrap();
    assert!(result.partial);
    assert_eq!(result.removed_candidates, 0);
    assert_eq!(mode, 0o555);
    assert!(target.join("keep").exists());
}

#[test]
fn symlinked_cache_root_is_reported_and_outside_data_survives() {
    let fixture = fixture();
    let home = fixture.path();
    put(home.join("outside/keep"));
    symlink(home.join("outside"), home.join(".cache")).unwrap();
    let plan = plan(&options(home, CleanKind::Caches)).unwrap();
    assert!(!plan.issues().is_empty());
    assert!(apply(&plan).unwrap().partial);
    assert!(home.join("outside/keep").exists());
}

#[test]
fn failed_scan_roots_do_not_discard_successful_roots() {
    let fixture = fixture();
    let home = fixture.path();
    put(home.join("tree/file"));
    let result = scan(&ScanOptions {
        roots: vec![home.join("tree"), home.join("missing")],
        excludes: vec![],
        depth: 1,
        jobs: 2,
    })
    .unwrap();
    assert_eq!(result.roots.len(), 1);
    assert_eq!(result.issues.len(), 1);
    assert_eq!(result.issues[0].path, home.join("missing"));
}

#[test]
fn combined_plan_is_disjoint_and_preserves_excluded_caches() {
    let fixture = fixture();
    let home = fixture.path();
    put(home.join("projects/app/node_modules/remove"));
    put(home.join(".cache/nested/node_modules/keep"));
    let mut options = options(home, CleanKind::All);
    options.project_root = Some(home.into());
    options
        .excludes
        .push(home.join(".cache/nested/node_modules/keep"));
    let plan = plan(&options).unwrap();
    assert_eq!(plan.candidates().len(), 1);
    let result = apply(&plan).unwrap();
    assert!(!result.partial, "{:?}", result.issues);
    assert!(home.join(".cache/nested/node_modules/keep").exists());
    assert!(!home.join("projects/app/node_modules").exists());
}

#[test]
fn cache_discovery_ignores_container_metadata_files() {
    let fixture = fixture();
    let home = fixture.path();
    put(home.join("Library/Containers/.DS_Store"));
    put(home.join("Library/Group Containers/.DS_Store"));
    put(home.join("Library/Containers/app/Data/Library/Caches/remove"));
    let plan = plan(&options(home, CleanKind::Caches)).unwrap();
    assert!(plan.issues().is_empty(), "{:?}", plan.issues());
    assert_eq!(plan.candidates().len(), 1);
    assert!(!apply(&plan).unwrap().partial);
    assert!(home.join("Library/Containers/.DS_Store").exists());
}
