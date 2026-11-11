use crate::unix;
use serde::Serialize;
use std::{
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    process::Command,
};

#[derive(Debug, Serialize)]
pub struct OverviewCommand {
    pub command: String,
    pub output: String,
    pub success: bool,
}

/// Read-only APFS and local-snapshot context, without a filesystem tree walk.
pub fn overview() -> Vec<OverviewCommand> {
    [
        ("df", vec!["-h", "/", "/System/Volumes/Data"]),
        ("diskutil", vec!["apfs", "list"]),
        ("tmutil", vec!["listlocalsnapshots", "/"]),
    ]
    .into_iter()
    .map(|(command, args)| {
        let result = Command::new(command).args(&args).output();
        let (success, output) = match result {
            Ok(output) => (
                output.status.success(),
                format!(
                    "{}{}",
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                ),
            ),
            Err(error) => (false, error.to_string()),
        };
        OverviewCommand {
            command: format!("{command} {}", args.join(" ")),
            output,
            success,
        }
    })
    .collect()
}

pub const DATA_VOLUME: &str = "/System/Volumes/Data";

/// One volume of the container holding the measured files. APFS volumes share
/// the container's free space, so `size_bytes` repeats across its volumes.
#[derive(Debug, Clone, Serialize)]
pub struct Volume {
    pub device: String,
    pub mount: PathBuf,
    pub size_bytes: u64,
    pub used_bytes: u64,
    pub available_bytes: u64,
    /// The scanned roots live on this volume.
    pub measured: bool,
}

/// `/dev/disk3s1s1` and `/dev/disk3s5` both belong to container `disk3`.
fn container(device: &str) -> Option<&str> {
    let name = device.strip_prefix("/dev/")?;
    let digits = name.strip_prefix("disk")?;
    let end = digits
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(digits.len());
    (end > 0).then(|| &name[..4 + end])
}

/// Volumes sharing a container with the filesystem that holds `path`, largest
/// first. Uses mount-table statistics only; nothing is walked or spawned.
pub fn volumes(path: &Path) -> Vec<Volume> {
    let Ok(mounts) = unix::mounts() else {
        return vec![];
    };
    // The longest mount point prefixing the path holds it, except that
    // firmlinked paths (/Users, /Applications, /usr/local) are the same
    // directories as their counterparts beneath the Data volume.
    let data = Path::new(DATA_VOLUME);
    let on_data = path.strip_prefix("/").is_ok_and(|relative| {
        std::fs::symlink_metadata(path)
            .ok()
            .zip(std::fs::symlink_metadata(data.join(relative)).ok())
            .is_some_and(|(a, b)| a.dev() == b.dev() && a.ino() == b.ino())
    });
    let holder = if on_data && mounts.iter().any(|m| m.path == data) {
        data.to_owned()
    } else {
        mounts
            .iter()
            .filter(|m| path.starts_with(&m.path))
            .max_by_key(|m| m.path.as_os_str().len())
            .map(|m| m.path.clone())
            .unwrap_or_default()
    };
    let Some(home) = mounts.iter().find(|m| m.path == holder) else {
        return vec![];
    };
    let group = container(&home.device);
    let mut volumes: Vec<Volume> = mounts
        .iter()
        .filter(|m| {
            m.path == holder
                || (group.is_some()
                    && m.filesystem == home.filesystem
                    && container(&m.device) == group)
        })
        .map(|m| Volume {
            device: m.device.clone(),
            mount: m.path.clone(),
            size_bytes: m.size,
            // Mount-table block counts describe the shared container; ask
            // the volume itself, and fall back to the table elsewhere.
            used_bytes: unix::volume_used(&m.path).unwrap_or(m.used),
            available_bytes: m.available,
            measured: m.path == holder,
        })
        .collect();
    volumes.sort_by(|a, b| b.used_bytes.cmp(&a.used_bytes).then(a.mount.cmp(&b.mount)));
    volumes
}

/// Disjoint roots that together cover the whole Data volume. Each top-level
/// folder is addressed by its firmlinked name (`/Users`) when that is the same
/// directory, otherwise by its real location beneath the Data volume. The
/// sealed system volume is not walked: its size comes from [`volumes`].
pub fn machine_roots() -> Vec<PathBuf> {
    let data = Path::new(DATA_VOLUME);
    let Ok(volume) = std::fs::symlink_metadata(data) else {
        return vec![];
    };
    let Ok(entries) = std::fs::read_dir(data) else {
        return vec![];
    };
    let mut roots = vec![];
    for entry in entries.flatten() {
        let real = entry.path();
        let Ok(meta) = std::fs::symlink_metadata(&real) else {
            continue;
        };
        // Skip files, symlinks and anything mounted over a top-level folder.
        if !meta.is_dir() || meta.dev() != volume.dev() {
            continue;
        }
        let alias = Path::new("/").join(entry.file_name());
        let same = std::fs::symlink_metadata(&alias)
            .is_ok_and(|other| other.dev() == meta.dev() && other.ino() == meta.ino());
        roots.push(if same { alias } else { real });
    }
    roots.sort();
    roots
}

/// Names of local APFS snapshots; they hold space no directory walk can see.
pub fn local_snapshots() -> Option<Vec<String>> {
    let output = Command::new("tmutil")
        .args(["listlocalsnapshots", "/"])
        .output()
        .ok()?;
    output.status.success().then(|| {
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter(|line| line.starts_with("com.apple."))
            .map(str::to_owned)
            .collect()
    })
}
