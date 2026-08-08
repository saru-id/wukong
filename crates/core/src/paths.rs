//! Where wukong keeps things: XDG throughout, no hardcoded homes.
//! Config in `XDG_CONFIG_HOME`, durable data (the store, the database)
//! in `XDG_DATA_HOME`, the runtime socket under `XDG_STATE_HOME` so it
//! survives on systems without a runtime dir.

use std::path::{Path, PathBuf};
use std::sync::LazyLock;

/// `$HOME`, resolved through any symlinks. macOS reports filesystem
/// events under real paths (`/private/var/…`) while `$HOME` is often
/// the symlinked form (`/var/…`); canonicalizing once here keeps every
/// path comparison — tracked-file matching most of all — on one form.
static HOME: LazyLock<PathBuf> = LazyLock::new(|| {
    let raw = PathBuf::from(std::env::var_os("HOME").unwrap_or_default());
    std::fs::canonicalize(&raw).unwrap_or(raw)
});

#[must_use]
pub fn home() -> &'static Path {
    &HOME
}

/// Turn user input into a real, canonical path: expand `~/`, make it
/// absolute, resolve symlinks. Everything stored or compared against
/// watcher events must be canonical.
#[must_use]
pub fn resolve_input(path: &str) -> PathBuf {
    let expanded = if let Some(rel) = path.strip_prefix("~/") {
        home().join(rel)
    } else {
        let p = PathBuf::from(path);
        if p.is_absolute() {
            p
        } else {
            std::env::current_dir().unwrap_or_default().join(p)
        }
    };
    canonicalize_lenient(&expanded)
}

/// Canonicalize a path that may not (yet) exist: resolve symlinks on
/// the parent directory and rejoin the file name. Everything that
/// compares against watcher-reported paths must pass through here —
/// macOS reports real paths, and `/etc`, `/var`, and `/tmp` are all
/// symlinks into `/private`.
#[must_use]
pub fn canonicalize_lenient(path: &Path) -> PathBuf {
    if let Ok(canon) = std::fs::canonicalize(path) {
        return canon;
    }
    match (path.parent(), path.file_name()) {
        (Some(dir), Some(name)) => {
            std::fs::canonicalize(dir).map_or_else(|_| path.to_path_buf(), |canon| canon.join(name))
        }
        _ => path.to_path_buf(),
    }
}

/// Create a directory (and parents) readable by the owner only. The
/// config, data, and state trees hold mirrored dotfiles, quarantine
/// evidence, and the socket — none of it is other users' business.
pub fn ensure_private_dir(dir: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt as _;
    match std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
    {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        Err(e) => Err(e),
    }
}

fn xdg(var: &str, fallback: &str) -> PathBuf {
    std::env::var_os(var)
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| home().join(fallback))
        .join("wukong")
}

/// `~/.config/wukong`.
#[must_use]
pub fn config_dir() -> PathBuf {
    xdg("XDG_CONFIG_HOME", ".config")
}

/// `~/.local/share/wukong` — the store repo and the database.
#[must_use]
pub fn data_dir() -> PathBuf {
    xdg("XDG_DATA_HOME", ".local/share")
}

/// `~/.local/state/wukong` — the daemon socket and pid file.
#[must_use]
pub fn state_dir() -> PathBuf {
    xdg("XDG_STATE_HOME", ".local/state")
}

#[must_use]
pub fn config_file() -> PathBuf {
    config_dir().join("config.toml")
}

#[must_use]
pub fn store_dir() -> PathBuf {
    data_dir().join("store")
}

#[must_use]
pub fn db_file() -> PathBuf {
    data_dir().join("wukong.db")
}

#[must_use]
pub fn socket_file() -> PathBuf {
    state_dir().join("wukongd.sock")
}

/// A tracked file's identity: its path relative to `$HOME` when it
/// lives there ("~/.zshrc" → ".zshrc"), or an absolute-path mirror
/// under `__abs__` for the rare tracked file outside home.
#[must_use]
pub fn store_rel(path: &Path) -> PathBuf {
    match path.strip_prefix(home()) {
        Ok(rel) => rel.to_path_buf(),
        Err(_) => PathBuf::from("__abs__").join(path.strip_prefix("/").unwrap_or(path)),
    }
}

/// The inverse of `store_rel`.
#[must_use]
pub fn from_store_rel(rel: &Path) -> PathBuf {
    match rel.strip_prefix("__abs__") {
        Ok(abs) => PathBuf::from("/").join(abs),
        Err(_) => home().join(rel),
    }
}

/// Pretty form for display: `~/…` when under home.
#[must_use]
pub fn display(path: &Path) -> String {
    match path.strip_prefix(home()) {
        Ok(rel) => format!("~/{}", rel.display()),
        Err(_) => path.display().to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn store_rel_round_trips() {
        let zshrc = home().join(".zshrc");
        assert_eq!(from_store_rel(&store_rel(&zshrc)), zshrc);

        let etc = Path::new("/etc/paths.d/dev");
        assert_eq!(from_store_rel(&store_rel(etc)), etc);
        assert!(store_rel(etc).starts_with("__abs__"));
    }
}
