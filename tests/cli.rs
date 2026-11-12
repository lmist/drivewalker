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
    assert_eq!(avenues[0]["kind"], "Node dependencies (node_modules)");
    assert_eq!(avenues[0]["effect"], "grows_back");
    assert_eq!(avenues[1]["kind"], "Other files and folders");
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
    // The numbered list closes the report as a tree: outcome, kind with how
    // to remove it, then numbered paths with the home directory as `~`.
    let list = text.split("What can go").nth(1).expect("numbered list");
    assert!(list.contains("Grows back · "), "{text}");
    assert!(list.contains("Your call · "), "{text}");
    assert!(
        list.contains("Node dependencies (node_modules) · dw clean projects PATH"),
        "{text}"
    );
    let numbered = |text: &str, number: &str| {
        text.lines()
            .find(|line| line.split_whitespace().next() == Some(number))
            .map(str::to_owned)
    };
    let first = numbered(list, "1").expect("row one");
    assert!(first.ends_with("└─ ~/tree/app/node_modules"), "{text}");
    let second = numbered(list, "2").expect("row two");
    assert!(second.ends_with("└─ ~/tree/a/b/c/d/big.bin"), "{text}");

    // A narrow reader gets paths shortened in the middle, never longer rows.
    let narrow = Command::new(env!("CARGO_BIN_EXE_dw"))
        .env("HOME", temp.path())
        .env("COLUMNS", "34")
        .args(["deep", path, "--min-size", "0.125M"])
        .output()
        .unwrap();
    let narrow = String::from_utf8(narrow.stdout).unwrap();
    let narrow = narrow.split("What can go").nth(1).expect("numbered list");
    for number in ["1", "2"] {
        let row = numbered(narrow, number).expect("numbered row");
        assert!(row.chars().count() <= 34, "{narrow}");
        assert!(row.contains('…'), "{narrow}");
    }

    assert_eq!(
        run(&["deep", path, "--min-size", "lots"]).status.code(),
        Some(2)
    );
    assert_eq!(run(&["deep", path, "--top", "0"]).status.code(), Some(2));
}

#[test]
fn cli_shows_the_logo_with_the_top_level_help() {
    let run = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_dw"))
            .args(args)
            .output()
            .unwrap()
    };
    let light = "(===============)";

    // `dw --help`: the logo above the help, without colour codes when piped.
    let help = run(&["--help"]);
    assert!(help.status.success());
    let text = String::from_utf8(help.stdout).unwrap();
    let logo = text.find(light).expect("logo");
    assert!(logo < text.find("Usage: dw").expect("usage"), "{text}");
    assert!(!text.contains('\x1b'), "{text}");

    // `dw` alone is still a usage error, now answered with the same help.
    let bare = run(&[]);
    assert_eq!(bare.status.code(), Some(2));
    assert!(bare.stdout.is_empty());
    let text = String::from_utf8(bare.stderr).unwrap();
    assert!(text.contains(light) && text.contains("Usage: dw"), "{text}");

    // A subcommand's help stays plain.
    let deep = run(&["deep", "--help"]);
    assert!(deep.status.success());
    assert!(!String::from_utf8(deep.stdout).unwrap().contains(light));
}
