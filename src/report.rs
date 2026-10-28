use crate::{CleanPlan, CleanReport, Issue, ScanReport, scan::Usage};
use std::fmt::Write;

fn path(value: &std::path::Path) -> String {
    value
        .to_string_lossy()
        .chars()
        .flat_map(char::escape_debug)
        .collect()
}

pub fn bytes(value: u64) -> String {
    let units = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut size = value as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit < units.len() - 1 {
        size /= 1024.0;
        unit += 1;
    }
    format!("{size:.1} {}", units[unit])
}

fn issues(out: &mut String, title: &str, issues: &[Issue]) {
    if !issues.is_empty() {
        let _ = writeln!(out, "\n{title}: {}", issues.len());
        for issue in issues.iter().take(10) {
            let _ = writeln!(out, "  {}: {}", path(&issue.path), issue.message);
        }
        if issues.len() > 10 {
            let _ = writeln!(out, "  … use --json for all paths");
        }
    }
}

pub fn scan(report: &ScanReport, limit: usize) -> String {
    fn rows(out: &mut String, node: &Usage, indent: usize, limit: usize) {
        let _ = writeln!(
            out,
            "{:>12}  {}{}",
            bytes(node.allocated_bytes),
            "  ".repeat(indent),
            path(&node.path)
        );
        for child in node.children.iter().take(limit) {
            rows(out, child, indent + 1, limit);
        }
    }
    let mut out = String::new();
    for root in &report.roots {
        rows(&mut out, root, 0, limit);
    }
    for hotspot in &report.hotspots {
        let _ = writeln!(
            out,
            "\nExpand: {} ({})",
            path(&hotspot.path),
            bytes(hotspot.allocated_bytes)
        );
        for child in &hotspot.children {
            let _ = writeln!(
                out,
                "{:>12}  {}",
                bytes(child.allocated_bytes),
                path(&child.path)
            );
        }
        if hotspot.omitted_children > 0 {
            let _ = writeln!(out, "  … {} more entries", hotspot.omitted_children);
        }
    }
    issues(&mut out, "Incomplete measurements", &report.issues);
    let _ = writeln!(
        out,
        "\n{:.2}s · {}/{} workers used · allocated blocks · symlink targets and other filesystems excluded",
        report.elapsed_seconds, report.scheduling.workers_used, report.scheduling.jobs
    );
    out
}

pub fn plan(plan: &CleanPlan, limit: usize) -> String {
    let mut out = format!(
        "Preview: {} candidates, {} allocated\n",
        plan.candidates().len(),
        bytes(plan.allocated_bytes())
    );
    for candidate in plan.candidates().iter().take(limit) {
        let _ = writeln!(
            out,
            "{:>12}  {}  ({})",
            bytes(candidate.allocated_bytes),
            path(&candidate.path),
            candidate.kind.reason()
        );
    }
    if plan.candidates().len() > limit {
        let _ = writeln!(out, "  … use --json for all candidates");
    }
    issues(&mut out, "Discovery/measurement errors", plan.issues());
    let _ = writeln!(
        out,
        "{}/{} workers used",
        plan.scheduling.workers_used, plan.scheduling.jobs
    );
    let _ = writeln!(
        out,
        "\n{} protected or excluded paths. Nothing deleted. Add --apply to clean.",
        plan.skipped().len()
    );
    out
}

pub fn clean(report: &CleanReport) -> String {
    let mut out = format!(
        "Removed {} candidates ({} entries).\nFree: {} → {} ({:+.2} GiB net) on {}\n",
        report.removed_candidates,
        report.removed_entries,
        bytes(report.free_before_bytes),
        bytes(report.free_after_bytes),
        report.net_free_change_bytes as f64 / 1073741824.0,
        path(&report.free_space_path)
    );
    issues(&mut out, "Partial cleanup", &report.issues);
    let _ = writeln!(
        out,
        "{} protected/excluded paths · {:.2}s · {}/{} workers used",
        report.skipped.len(),
        report.elapsed_seconds,
        report.scheduling.workers_used,
        report.scheduling.jobs
    );
    out
}
