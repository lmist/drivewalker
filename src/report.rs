use crate::{
    CleanPlan, CleanReport, Issue, ScanReport,
    deep::{Analysis, Avenue, Branch, Effect, Node, branches},
    platform::macos::Volume,
    scan::Usage,
};
use std::{
    fmt::Write,
    path::{Path, PathBuf},
};

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

fn percent(part: u64, whole: u64) -> String {
    if whole == 0 {
        return "    -".into();
    }
    format!("{:>4.1}%", part as f64 * 100.0 / whole as f64)
}

fn tree(out: &mut String, node: &Node, label: String, prefix: &str, analysis: &Analysis) {
    // A directory whose space is almost entirely one child reads better as a
    // single joined line than as a staircase of identical sizes.
    let (mut node, mut label) = (node, label);
    while node.children.len() == 1 && node.unlisted_bytes < analysis.threshold_bytes {
        node = &node.children[0];
        if let Some(name) = node.path.file_name() {
            label = format!("{label}/{}", path(name.as_ref()));
        }
    }
    let _ = writeln!(
        out,
        "{:>11}  {}  {label}",
        bytes(node.allocated_bytes),
        percent(node.allocated_bytes, analysis.total_bytes),
    );
    let remainder = !node.children.is_empty() && node.unlisted_bytes >= analysis.threshold_bytes;
    for (index, child) in node.children.iter().enumerate() {
        let last = index + 1 == node.children.len() && !remainder;
        let name = child
            .path
            .file_name()
            .map(|n| path(n.as_ref()))
            .unwrap_or_else(|| path(&child.path));
        tree(
            out,
            child,
            format!("{prefix}{} {name}", if last { "└─" } else { "├─" }),
            &format!("{prefix}{}  ", if last { " " } else { "│" }),
            analysis,
        );
    }
    if remainder {
        let named: Vec<String> = node
            .unlisted_largest
            .iter()
            .filter_map(|entry| {
                let name = entry.path.file_name()?;
                Some(format!(
                    "{} {}",
                    path(name.as_ref()),
                    bytes(entry.allocated_bytes)
                ))
            })
            .collect();
        let _ = writeln!(
            out,
            "{:>11}  {}  {prefix}└─ … {} smaller items{}{}",
            bytes(node.unlisted_bytes),
            percent(node.unlisted_bytes, analysis.total_bytes),
            node.unlisted_children,
            if named.is_empty() { "" } else { ", largest: " },
            named.join(", ")
        );
    }
}

/// How the closing list is fitted to its reader. The default changes nothing:
/// absolute paths at full length.
#[derive(Debug, Clone, Default)]
pub struct Layout {
    /// Written as `~` at the start of a path.
    pub home: Option<PathBuf>,
    /// Longest line; paths are shortened in the middle to keep to it.
    pub columns: Option<usize>,
}

impl Layout {
    /// Abbreviate `home` and fit the reader's terminal: `$COLUMNS` when set,
    /// else the width of a terminal on standard output. Piped output with no
    /// `$COLUMNS` keeps every path whole.
    pub fn detect(home: PathBuf) -> Self {
        let columns = std::env::var("COLUMNS")
            .ok()
            .and_then(|value| value.trim().parse().ok())
            .filter(|columns| *columns > 0)
            .or_else(crate::unix::terminal_columns);
        Self {
            home: Some(home),
            columns,
        }
    }

    /// The root, the home directory, or a folder above it.
    fn holds_everything(&self, folder: &Path) -> bool {
        folder.parent().is_none()
            || self
                .home
                .as_deref()
                .is_some_and(|home| home.starts_with(folder))
    }

    fn place(&self, value: &Path) -> String {
        match self
            .home
            .as_deref()
            .and_then(|home| value.strip_prefix(home).ok())
        {
            Some(rest) if rest.as_os_str().is_empty() => "~".into(),
            Some(rest) => format!("~/{}", path(rest)),
            None => path(value),
        }
    }
}

/// Shorten in the middle: the end of a path says most about what it is.
fn fit(text: String, width: usize) -> String {
    let length = text.chars().count();
    if length <= width {
        return text;
    }
    let head = width.saturating_sub(1) / 3;
    let tail = width.saturating_sub(1) - head;
    let start: String = text.chars().take(head).collect();
    let end: String = text.chars().skip(length - tail).collect();
    format!("{start}…{end}")
}

/// Fixed left columns of the closing list, and how wide a path may be.
struct Columns<'a> {
    layout: &'a Layout,
    number: usize,
    size: usize,
    scale: u64,
}

impl Columns<'_> {
    fn size(&self, value: u64) -> String {
        let value = value as f64 / self.scale as f64;
        if value > 0.0 && value < 0.05 {
            "<0.1".into()
        } else {
            format!("{value:.1}")
        }
    }

    /// A heading carries a total but no number; a row carries both.
    fn line(&self, out: &mut String, number: Option<usize>, bytes: u64, text: &str) {
        let number = number.map(|n| n.to_string()).unwrap_or_default();
        let _ = writeln!(
            out,
            "{number:>width$}  {:>size$}  {text}",
            self.size(bytes),
            width = self.number,
            size = self.size,
        );
    }

    /// Room left for a path after the columns and `drawn` tree characters.
    fn fit(&self, text: String, drawn: usize) -> String {
        match self.layout.columns {
            Some(columns) => {
                let used = self.number + self.size + 4 + drawn;
                fit(text, columns.saturating_sub(used).max(20))
            }
            None => text,
        }
    }

    fn branches(
        &self,
        out: &mut String,
        branches: &[Branch],
        rows: &[Avenue],
        prefix: &str,
        first_level: bool,
        more_follows: bool,
    ) {
        for (index, branch) in branches.iter().enumerate() {
            let last = index + 1 == branches.len() && !more_follows;
            let label = if first_level {
                self.layout.place(&branch.path)
            } else {
                path(&branch.path)
            };
            let label = self.fit(label, prefix.chars().count() + 3);
            let text = format!("{prefix}{} {label}", if last { "└─" } else { "├─" });
            let number = branch.row.map(|row| rows[row].number);
            self.line(out, number, branch.allocated_bytes, &text);
            let deeper = format!("{prefix}{}  ", if last { " " } else { "│" });
            self.branches(out, &branch.children, rows, &deeper, false, false);
        }
    }
}

/// The numbered list comes last so it is what remains on screen: a tree of
/// outcome, kind and the folders that rows share, with a number on every row.
fn avenues(out: &mut String, analysis: &Analysis, layout: &Layout) {
    let rows = &analysis.avenues;
    if rows.is_empty() {
        return;
    }
    let total = |rows: &[Avenue]| rows.iter().map(|row| row.allocated_bytes).sum::<u64>();
    let outcomes: Vec<&[Avenue]> = rows.chunk_by(|a, b| a.effect == b.effect).collect();
    // One unit for the whole column, chosen by the largest total shown.
    let largest = outcomes.iter().map(|rows| total(rows)).max().unwrap_or(0);
    let (scale, unit) = [(1u64 << 30, "GiB"), (1 << 20, "MiB"), (1 << 10, "KiB")]
        .into_iter()
        .find(|(scale, _)| largest >= *scale)
        .unwrap_or((1, "B"));
    let mut columns = Columns {
        layout,
        number: rows.len().to_string().len(),
        size: unit.len(),
        scale,
    };
    columns.size = columns.size.max(columns.size(largest).len());

    let _ = writeln!(out, "\nWhat can go · pick by number · rows do not overlap");
    let _ = writeln!(
        out,
        "{:>number$}  {unit:>size$}",
        "#",
        number = columns.number,
        size = columns.size
    );
    for (index, outcome) in outcomes.iter().enumerate() {
        if index > 0 {
            let _ = writeln!(out);
        }
        let heading = match outcome[0].effect {
            Effect::StaysFreed => "Stays freed · nothing recreates it and nothing of yours is lost",
            Effect::GrowsBack => "Grows back · a build, install or launch recreates it",
            Effect::YourCall => "Your call · applications, tools, history and your own files",
        };
        columns.line(out, None, total(outcome), heading);
        let kinds: Vec<&[Avenue]> = outcome.chunk_by(|a, b| a.kind == b.kind).collect();
        for (index, kind) in kinds.iter().enumerate() {
            let last = index + 1 == kinds.len();
            let text = format!(
                "{} {} · {}",
                if last { "└─" } else { "├─" },
                kind[0].kind,
                kind[0].how
            );
            columns.line(out, None, total(kind), &text);
            let prefix = if last { "   " } else { "│  " };
            // The smaller locations of a kind share its last row.
            let together = kind.iter().find(|row| row.locations != 1);
            let mut tree = branches(kind);
            // A folder that merely holds everything is not worth a line.
            if tree.len() == 1 && tree[0].row.is_none() && layout.holds_everything(&tree[0].path) {
                let root = tree.remove(0);
                tree = root.children;
                for branch in &mut tree {
                    branch.path = root.path.join(&branch.path);
                }
            }
            columns.branches(out, &tree, kind, prefix, true, together.is_some());
            if let Some(row) = together {
                let place = columns.fit(layout.place(&row.path), prefix.len() + 30);
                let text = format!(
                    "{prefix}└─ … {} {}locations in {place}",
                    row.locations,
                    if kind.len() > 1 { "smaller " } else { "" },
                );
                columns.line(out, Some(row.number), row.allocated_bytes, &text);
            }
        }
    }
}

pub fn deep(
    analysis: &Analysis,
    volumes: &[Volume],
    snapshots: Option<&[String]>,
    report: &ScanReport,
    layout: &Layout,
) -> String {
    let mut out = String::new();
    if let Some(measured) = volumes.iter().find(|v| v.measured) {
        let _ = writeln!(out, "Disk: {} total", bytes(measured.size_bytes));
        for volume in volumes {
            let _ = writeln!(
                out,
                "{:>11}  {}  {}{}",
                bytes(volume.used_bytes),
                percent(volume.used_bytes, measured.size_bytes),
                path(&volume.mount),
                if volume.measured {
                    "  ← measured"
                } else {
                    ""
                }
            );
        }
        let used: u64 = volumes.iter().map(|v| v.used_bytes).sum();
        let other = measured
            .size_bytes
            .saturating_sub(used)
            .saturating_sub(measured.available_bytes);
        if other > 0 {
            let _ = writeln!(
                out,
                "{:>11}  {}  snapshots, purgeable and reserved space",
                bytes(other),
                percent(other, measured.size_bytes)
            );
        }
        let _ = writeln!(
            out,
            "{:>11}  {}  available",
            bytes(measured.available_bytes),
            percent(measured.available_bytes, measured.size_bytes)
        );
        let gap = measured.used_bytes as i128 - analysis.total_bytes as i128;
        let _ = writeln!(
            out,
            "\nMeasured {} in {} entries; {} reports {} used.",
            bytes(analysis.total_bytes),
            analysis.total_entries,
            path(&measured.mount),
            bytes(measured.used_bytes)
        );
        if gap > 0 {
            let _ = writeln!(
                out,
                "{} was not reachable: unreadable folders, snapshots, or paths outside the scanned roots.",
                bytes(gap as u64)
            );
        } else if gap < 0 {
            let _ = writeln!(
                out,
                "Measured exceeds used by {}: APFS clones and compression share blocks between files.",
                bytes((-gap) as u64)
            );
        }
        if let Some(snapshots) = snapshots {
            let _ = writeln!(out, "Local snapshots: {}", snapshots.len());
        }
    } else {
        let _ = writeln!(
            out,
            "Measured {} in {} entries.",
            bytes(analysis.total_bytes),
            analysis.total_entries
        );
    }

    let _ = writeln!(
        out,
        "\nWhere the space is · every entry of {} or more, at any depth",
        bytes(analysis.threshold_bytes)
    );
    let mut small = (0u64, 0usize);
    for root in &analysis.tree {
        if root.allocated_bytes >= analysis.threshold_bytes || analysis.tree.len() == 1 {
            tree(&mut out, root, path(&root.path), "", analysis);
        } else {
            small = (small.0 + root.allocated_bytes, small.1 + 1);
        }
    }
    if small.1 > 0 {
        let _ = writeln!(
            out,
            "{:>11}  {}  … {} smaller roots",
            bytes(small.0),
            percent(small.0, analysis.total_bytes),
            small.1
        );
    }

    if !analysis.concentrations.is_empty() {
        let _ = writeln!(
            out,
            "\nWhere it ends up · space that does not split further"
        );
        for item in &analysis.concentrations {
            let detail = if item.directory && item.items > 0 {
                format!(
                    "  ({} items under {} each)",
                    item.items,
                    bytes(analysis.threshold_bytes)
                )
            } else {
                String::new()
            };
            let _ = writeln!(
                out,
                "{:>11}  {}  {}{detail}",
                bytes(item.allocated_bytes),
                percent(item.allocated_bytes, analysis.total_bytes),
                path(&item.path)
            );
        }
    }

    if !analysis.kinds.is_empty() {
        let _ = writeln!(out, "\nBy kind · added up wherever it occurs");
        for kind in &analysis.kinds {
            let _ = writeln!(
                out,
                "{:>11}  {}  {} · {} location{} · {}",
                bytes(kind.allocated_bytes),
                percent(kind.allocated_bytes, analysis.total_bytes),
                kind.label,
                kind.locations,
                if kind.locations == 1 { "" } else { "s" },
                kind.hint
            );
            // Name the locations that carry the kind; skip incidental crumbs.
            for location in kind
                .largest
                .iter()
                .filter(|l| l.allocated_bytes >= kind.allocated_bytes / 20)
            {
                let _ = writeln!(
                    out,
                    "{:>11}         {}",
                    bytes(location.allocated_bytes),
                    path(&location.path)
                );
            }
        }
    }

    if !analysis.largest_files.is_empty() {
        let _ = writeln!(out, "\nLargest files");
        for file in &analysis.largest_files {
            let _ = writeln!(
                out,
                "{:>11}  {}  {}",
                bytes(file.allocated_bytes),
                percent(file.allocated_bytes, analysis.total_bytes),
                path(&file.path)
            );
        }
    }

    if !analysis.unreadable.is_empty() {
        let total: usize = analysis.unreadable.iter().map(|g| g.locations).sum();
        let _ = writeln!(out, "\nNot measured: {total} locations");
        for group in &analysis.unreadable {
            let _ = writeln!(out, "  {} × {}", group.locations, group.reason);
            if !group.hint.is_empty() {
                let _ = writeln!(out, "    {}", group.hint);
            }
            for example in &group.examples {
                let _ = writeln!(out, "    {}", path(example));
            }
        }
    }
    avenues(&mut out, analysis, layout);
    let _ = writeln!(
        out,
        "\n{:.2}s · {}/{} workers used · allocated blocks · lower --min-size to go deeper",
        report.elapsed_seconds, report.scheduling.workers_used, report.scheduling.jobs
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn list_paths_abbreviate_home_and_shorten_in_the_middle() {
        let layout = Layout {
            home: Some("/Users/me".into()),
            columns: None,
        };
        assert_eq!(layout.place(Path::new("/Users/me")), "~");
        assert_eq!(layout.place(Path::new("/Users/me/a b/c")), "~/a b/c");
        // Only whole components match: this is another user's folder.
        assert_eq!(
            layout.place(Path::new("/Users/merlin/x")),
            "/Users/merlin/x"
        );
        assert_eq!(
            Layout::default().place(Path::new("/Users/me/x")),
            "/Users/me/x"
        );

        assert!(layout.holds_everything(Path::new("/")));
        assert!(layout.holds_everything(Path::new("/Users")));
        assert!(layout.holds_everything(Path::new("/Users/me")));
        assert!(!layout.holds_everything(Path::new("/Users/me/work")));
        assert!(!layout.holds_everything(Path::new("/Applications")));

        assert_eq!(fit("short".into(), 16), "short");
        assert_eq!(
            fit("/a/very/long/path/to/some/folder".into(), 16),
            "/a/ve…ome/folder"
        );
    }
}
