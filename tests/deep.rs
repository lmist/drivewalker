use drivewalker::{
    DeepOptions, Effect, ScanOptions,
    deep::{Avenue, Node},
    scan,
};
use std::{fs, path::Path};

fn fixture() -> tempfile::TempDir {
    tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap()
}

fn put(path: impl AsRef<Path>, bytes: usize) {
    let path = path.as_ref();
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, vec![1u8; bytes]).unwrap();
}

fn find<'a>(nodes: &'a [Node], path: &Path) -> Option<&'a Node> {
    nodes.iter().find_map(|node| {
        if node.path == path {
            Some(node)
        } else {
            find(&node.children, path)
        }
    })
}

const KIB: usize = 1024;

fn measured(root: &Path) -> drivewalker::ScanReport {
    scan(&ScanOptions {
        roots: vec![root.to_owned()],
        excludes: vec![],
        depth: 0,
        jobs: 4,
    })
    .unwrap()
}

#[test]
fn deep_follows_space_to_any_depth_without_touching_the_filesystem() {
    let temp = fixture();
    let root = temp.path().join("tree");
    let big = root.join("a/b/c/d/e/f/g/big.bin");
    put(&big, 2048 * KIB);
    put(root.join("a/b/side.bin"), 600 * KIB);
    for file in 0..40 {
        put(root.join(format!("crumbs/file{file}")), 4 * KIB);
    }
    let report = measured(&root);
    // Display depth was zero and the tree is gone: the analysis uses only
    // what the scan retained.
    assert!(report.roots[0].children.is_empty());
    fs::remove_dir_all(&root).unwrap();

    let analysis = report.deep(DeepOptions {
        min_bytes: Some(256 * KIB as u64),
        top: 10,
    });
    assert_eq!(analysis.total_bytes, report.roots[0].allocated_bytes);
    assert_eq!(analysis.threshold_bytes, 256 * KIB as u64);
    assert_eq!(analysis.tree.len(), 1);

    let leaf = find(&analysis.tree, &big).expect("eight levels down is still reported");
    assert!(!leaf.directory);
    assert!(leaf.allocated_bytes >= 2048 * KIB as u64);
    assert!(find(&analysis.tree, &root.join("a/b/side.bin")).is_some());

    // Entries below the threshold are summarised, never listed individually.
    assert!(find(&analysis.tree, &root.join("crumbs")).is_none());
    let top = &analysis.tree[0];
    assert_eq!(top.unlisted_children, 1);
    assert_eq!(top.unlisted_largest[0].path, root.join("crumbs"));
    assert_eq!(
        top.allocated_bytes,
        top.unlisted_bytes + top.children.iter().map(|c| c.allocated_bytes).sum::<u64>()
    );

    assert_eq!(analysis.largest_files[0].path, big);
    assert_eq!(analysis.largest_files[1].path, root.join("a/b/side.bin"));
    assert_eq!(analysis.concentrations[0].path, big);
    assert!(analysis.unreadable.is_empty());
}

#[test]
fn kinds_add_up_scattered_directories_and_count_nested_matches_once() {
    let temp = fixture();
    let root = temp.path().join("tree");
    put(root.join("one/node_modules/pkg/index.js"), 300 * KIB);
    put(
        root.join("one/node_modules/pkg/node_modules/inner/index.js"),
        200 * KIB,
    );
    put(
        root.join("two/deep/er/node_modules/pkg/index.js"),
        400 * KIB,
    );
    put(root.join("rusty/target/debug/app"), 500 * KIB);
    // A folder merely named `target` is not build output.
    put(root.join("plain/target/notes.txt"), 500 * KIB);
    put(root.join("Tool.app/Contents/MacOS/tool"), 700 * KIB);
    put(
        root.join("Tool.app/Contents/Resources/node_modules/x"),
        64 * KIB,
    );
    let report = measured(&root);
    let analysis = report.deep(DeepOptions {
        min_bytes: Some(128 * KIB as u64),
        top: 20,
    });

    let kind = |label: &str| {
        analysis
            .kinds
            .iter()
            .find(|k| k.label.starts_with(label))
            .unwrap_or_else(|| panic!("missing kind {label}"))
    };
    let node = kind("Node dependencies");
    assert_eq!(
        node.locations, 2,
        "nested and bundled copies are not recounted"
    );
    assert!(node.allocated_bytes >= 900 * KIB as u64);
    assert!(node.allocated_bytes < 1100 * KIB as u64);
    assert_eq!(
        node.largest[0].path,
        root.join("one/node_modules"),
        "largest location first"
    );
    let rust = kind("Rust build output");
    assert_eq!(rust.locations, 1);
    assert_eq!(rust.largest[0].path, root.join("rusty/target"));
    assert_eq!(kind("Application bundles").locations, 1);

    // Bundles are reported whole instead of being opened up.
    let app = find(&analysis.tree, &root.join("Tool.app")).unwrap();
    assert!(app.children.is_empty());
    assert_eq!(app.unlisted_bytes, app.allocated_bytes);
}

#[test]
fn default_threshold_scales_with_the_measured_total() {
    let temp = fixture();
    let root = temp.path().join("tree");
    put(root.join("big"), 4096 * KIB);
    put(root.join("tiny"), 4 * KIB);
    let report = measured(&root);
    let analysis = report.deep(DeepOptions::default());
    assert_eq!(analysis.threshold_bytes, analysis.total_bytes / 200);
    assert!(find(&analysis.tree, &root.join("big")).is_some());
    assert!(find(&analysis.tree, &root.join("tiny")).is_none());
}

#[test]
fn unreadable_locations_are_grouped_by_reason() {
    use std::os::unix::fs::PermissionsExt;
    let temp = fixture();
    let root = temp.path().join("tree");
    put(root.join("open/file"), 4 * KIB);
    for name in ["locked1", "locked2"] {
        put(root.join(name).join("file"), 4 * KIB);
        fs::set_permissions(root.join(name), fs::Permissions::from_mode(0o000)).unwrap();
    }
    let report = measured(&root);
    for name in ["locked1", "locked2"] {
        fs::set_permissions(root.join(name), fs::Permissions::from_mode(0o755)).unwrap();
    }
    let analysis = report.deep(DeepOptions::default());
    assert_eq!(analysis.unreadable.len(), 1);
    assert_eq!(analysis.unreadable[0].locations, 2);
    assert!(analysis.unreadable[0].reason.contains("os error 13"));
    assert!(analysis.unreadable[0].hint.contains("sudo"));
}

fn row<'a>(rows: &'a [Avenue], path: &Path) -> &'a Avenue {
    rows.iter()
        .find(|row| row.path == path)
        .unwrap_or_else(|| panic!("no row for {}", path.display()))
}

#[test]
fn avenues_number_what_can_go_by_outcome_without_overlap() {
    let temp = fixture();
    let root = temp.path().join("tree");
    put(root.join("one/node_modules/pkg/index.js"), 600 * KIB);
    put(root.join("two/node_modules/pkg/index.js"), 40 * KIB);
    put(root.join("three/node_modules/pkg/index.js"), 40 * KIB);
    put(root.join(".Trash/old.bin"), 300 * KIB);
    put(root.join("Tool.app/Contents/MacOS/tool"), 500 * KIB);
    put(root.join("films/film.mp4"), 900 * KIB);
    for file in 0..30 {
        put(root.join(format!("work/notes{file}.txt")), 8 * KIB);
    }
    put(root.join("work/app/node_modules/dep.js"), 40 * KIB);
    put(root.join("Downloads/Setup.dmg"), 200 * KIB);
    let report = measured(&root);
    let analysis = report.deep(DeepOptions {
        min_bytes: Some(128 * KIB as u64),
        top: 10,
    });
    let rows = &analysis.avenues;

    // Numbered from one, by outcome and then by size.
    let listed: Vec<(usize, &str, Effect)> = rows
        .iter()
        .map(|row| (row.number, row.item, row.effect))
        .collect();
    assert_eq!(
        listed,
        [
            (1, "Trash", Effect::StaysFreed),
            (2, "Installer", Effect::StaysFreed),
            (3, "Node dependencies", Effect::GrowsBack),
            (4, "Node dependencies", Effect::GrowsBack),
            (5, "File", Effect::YourCall),
            (6, "Application", Effect::YourCall),
            (7, "Folder", Effect::YourCall),
        ]
    );
    assert_eq!(rows[0].path, root.join(".Trash"));
    assert_eq!(rows[1].path, root.join("Downloads/Setup.dmg"));
    assert_eq!(rows[4].path, root.join("films/film.mp4"));
    assert_eq!(rows[5].path, root.join("Tool.app"));
    assert!(rows.iter().all(|row| !row.how.is_empty()));

    // A location of at least the threshold has its own row; the smaller ones
    // of its kind share one, placed at the folder that contains them all.
    assert_eq!(
        (rows[2].path.clone(), rows[2].locations),
        (root.join("one/node_modules"), 1)
    );
    assert_eq!((rows[3].path.clone(), rows[3].locations), (root.clone(), 3));
    assert!(rows[3].allocated_bytes >= 120 * KIB as u64);

    // An unrecognised folder counts only what no other row has claimed, and a
    // folder holding nothing but recognised data is not listed again.
    let work = row(rows, &root.join("work"));
    let whole = find(&analysis.tree, &root.join("work")).unwrap();
    assert!(work.allocated_bytes >= 240 * KIB as u64);
    assert!(work.allocated_bytes + 40 * KIB as u64 <= whole.allocated_bytes);
    for folder in ["Downloads", "one", "films"] {
        assert!(rows.iter().all(|row| row.path != root.join(folder)));
    }
    assert!(rows.iter().map(|row| row.allocated_bytes).sum::<u64>() <= analysis.total_bytes);
}

#[test]
fn installed_tools_are_the_owners_call_never_regenerable() {
    let temp = fixture();
    let root = temp.path().join("tree");
    put(root.join("prefix/lib/node_modules/tool/cli.js"), 300 * KIB);
    put(
        root.join(".bun/install/global/node_modules/tool/cli.js"),
        300 * KIB,
    );
    put(root.join(".rustup/toolchains/stable/bin/rustc"), 300 * KIB);
    put(
        root.join(".nvm/versions/node/v24/lib/node_modules/npm/cli.js"),
        300 * KIB,
    );
    // A data folder merely named like a bundle is not an application.
    put(root.join("support/com.vendor.app/data.bin"), 300 * KIB);
    let report = measured(&root);
    let analysis = report.deep(DeepOptions {
        min_bytes: Some(128 * KIB as u64),
        top: 10,
    });
    let rows = &analysis.avenues;

    assert!(rows.iter().all(|row| row.effect == Effect::YourCall));
    for path in [
        "prefix/lib/node_modules",
        ".bun/install/global/node_modules",
    ] {
        assert_eq!(row(rows, &root.join(path)).item, "Global packages");
    }
    for path in [".rustup/toolchains/stable", ".nvm/versions/node/v24"] {
        assert_eq!(row(rows, &root.join(path)).item, "Toolchain");
    }
    assert_eq!(
        row(rows, &root.join("support/com.vendor.app")).item,
        "Folder"
    );
    assert!(
        analysis
            .kinds
            .iter()
            .all(|kind| !kind.label.starts_with("Node dependencies")
                && !kind.label.starts_with("Application"))
    );
}
