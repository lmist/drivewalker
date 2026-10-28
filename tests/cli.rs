use std::{fs, process::Command};

#[test]
fn cli_preview_json_apply_and_argument_validation() {
    let temp = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap();
    let root = temp.path().join("projects");
    let dependencies = root.join("app/node_modules");
    fs::create_dir_all(&dependencies).unwrap();
    fs::write(dependencies.join("package.js"), "fixture").unwrap();
    let run = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_dw"))
            .env("HOME", temp.path())
            .args(args)
            .output()
            .unwrap()
    };
    let preview = run(&["clean", "projects", root.to_str().unwrap(), "--json"]);
    assert!(
        preview.status.success(),
        "{}",
        String::from_utf8_lossy(&preview.stderr)
    );
    let json: serde_json::Value = serde_json::from_slice(&preview.stdout).unwrap();
    assert_eq!(json["candidates"].as_array().unwrap().len(), 1);
    assert!(dependencies.exists());
    let cleaned = run(&[
        "clean",
        "projects",
        root.to_str().unwrap(),
        "--apply",
        "--json",
    ]);
    assert!(
        cleaned.status.success(),
        "{}",
        String::from_utf8_lossy(&cleaned.stderr)
    );
    let json: serde_json::Value = serde_json::from_slice(&cleaned.stdout).unwrap();
    assert_eq!(json["removed_candidates"], 1);
    assert!(!dependencies.exists());
    assert!(!run(&["clean", "projects"]).status.success());
    assert!(
        !run(&["clean", "caches", root.to_str().unwrap()])
            .status
            .success()
    );
    assert!(
        !run(&["scan", root.to_str().unwrap(), "--jobs", "0"])
            .status
            .success()
    );
    fs::create_dir_all(root.join("branch/deep")).unwrap();
    fs::write(root.join("branch/deep/file"), "retained fixture").unwrap();
    let expanded = run(&[
        "scan",
        root.to_str().unwrap(),
        "--depth",
        "0",
        "--drill",
        "2",
        "--json",
    ]);
    assert!(expanded.status.success());
    let json: serde_json::Value = serde_json::from_slice(&expanded.stdout).unwrap();
    assert_eq!(json["hotspots"].as_array().unwrap().len(), 2);
    assert!(json["roots"][0]["children"].as_array().unwrap().is_empty());
    let plain = run(&["scan", root.to_str().unwrap(), "--drill", "0", "--json"]);
    assert!(plain.status.success());
    let json: serde_json::Value = serde_json::from_slice(&plain.stdout).unwrap();
    assert!(json["hotspots"].as_array().unwrap().is_empty());
}
