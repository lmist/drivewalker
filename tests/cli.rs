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

#[test]
fn cli_deep_reports_json_text_and_rejects_bad_sizes() {
    let temp = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap();
    let root = temp.path().join("tree");
    let big = root.join("a/b/c/d/big.bin");
    fs::create_dir_all(big.parent().unwrap()).unwrap();
    fs::write(&big, vec![1u8; 1 << 20]).unwrap();
    fs::create_dir_all(root.join("app/node_modules")).unwrap();
    fs::write(root.join("app/node_modules/dep.js"), vec![1u8; 1 << 18]).unwrap();
    let run = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_dw"))
            .env("HOME", temp.path())
            .args(args)
            .output()
            .unwrap()
    };
    let path = root.to_str().unwrap();

    let json = run(&["deep", path, "--min-size", "128K", "--top", "5", "--json"]);
    assert!(
        json.status.success(),
        "{}",
        String::from_utf8_lossy(&json.stderr)
    );
    let value: serde_json::Value = serde_json::from_slice(&json.stdout).unwrap();
    let analysis = &value["analysis"];
    assert_eq!(analysis["threshold_bytes"], 128 * 1024);
    assert_eq!(
        analysis["largest_files"][0]["path"].as_str().unwrap(),
        big.to_str().unwrap()
    );
    assert_eq!(analysis["kinds"][0]["locations"], 1);
    let avenues = analysis["avenues"].as_array().unwrap();
    assert_eq!(avenues[0]["number"], 1);
    assert_eq!(avenues[0]["item"], "Node dependencies");
    assert_eq!(avenues[0]["effect"], "grows_back");
    assert_eq!(avenues[1]["item"], "File");
    assert_eq!(avenues[1]["path"].as_str().unwrap(), big.to_str().unwrap());
    assert_eq!(avenues[1]["effect"], "your_call");
    assert!(value["issues"].as_array().unwrap().is_empty());

    let text = run(&["deep", path, "--min-size", "0.125M"]);
    assert!(text.status.success());
    let text = String::from_utf8(text.stdout).unwrap();
    // A single-child chain is joined on one line down to the file itself.
    assert!(text.contains("a/b/c/d/big.bin"), "{text}");
    assert!(text.contains("Node dependencies (node_modules)"), "{text}");
    assert!(text.contains("Largest files"), "{text}");
    // The numbered list closes the report: one bordered table per outcome,
    // with the home directory written as `~`.
    let list = text.split("What can go").nth(1).expect("numbered list");
    assert!(list.contains("\nGrows back · "), "{text}");
    assert!(list.contains("\nYour call · "), "{text}");
    assert!(list.contains("│ # │ Item"), "{text}");
    let first = list.lines().find(|line| line.starts_with("│ 1 │"));
    let first = first.expect("row one");
    assert!(first.contains("Node dependencies"), "{text}");
    assert!(first.contains("~/tree/app/node_modules"), "{text}");

    // A narrow reader gets paths shortened in the middle, never a wider table.
    let narrow = Command::new(env!("CARGO_BIN_EXE_dw"))
        .env("HOME", temp.path())
        .env("COLUMNS", "56")
        .args(["deep", path, "--min-size", "0.125M"])
        .output()
        .unwrap();
    let narrow = String::from_utf8(narrow.stdout).unwrap();
    let table: Vec<&str> = narrow
        .lines()
        .filter(|line| line.starts_with(['│', '┌', '├', '└']))
        .collect();
    assert!(!table.is_empty(), "{narrow}");
    assert!(
        table.iter().all(|line| line.chars().count() <= 56),
        "{narrow}"
    );
    assert!(table.iter().any(|line| line.contains('…')), "{narrow}");

    assert_eq!(
        run(&["deep", path, "--min-size", "lots"]).status.code(),
        Some(2)
    );
    assert_eq!(run(&["deep", path, "--top", "0"]).status.code(), Some(2));
}
