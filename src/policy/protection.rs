use std::path::Path;

/// These directories can contain installed runtimes, credentials or app state.
pub(crate) fn project_protected(path: &Path, home: &Path) -> bool {
    let system = [
        "/Applications",
        "/Library",
        "/System",
        "/usr",
        "/opt",
        "/private",
        "/bin",
        "/sbin",
        "/dev",
        "/Volumes",
    ];
    if system.iter().any(|p| path.starts_with(p)) {
        return true;
    }
    if path.starts_with(home.join("Library")) || path.starts_with(home.join("Applications")) {
        return true;
    }
    path.components().any(|part| {
        let name = part.as_os_str().to_string_lossy();
        matches!(
            name.as_ref(),
            ".git"
                | ".hermit"
                | ".cargo"
                | ".rustup"
                | ".local"
                | ".codex"
                | ".grok"
                | ".agents"
                | ".claude"
                | ".config"
                | ".cache"
                | ".npm"
                | ".bun"
                | ".ok"
                | ".aside"
                | ".prime"
        ) || name.ends_with(".app")
            || name.ends_with(".framework")
    })
}

pub(crate) fn cache_protected(path: &Path, home: &Path) -> bool {
    path == home.join("Library/Caches/hermit")
        || path.starts_with(home.join("Library/Caches/hermit/pkg"))
}
