//! Where wukong keeps things: XDG throughout, no hardcoded homes.
//! Config in XDG_CONFIG_HOME, durable data (the store, the database)
//! in XDG_DATA_HOME, the runtime socket under XDG_STATE_HOME so it
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

pub fn home() -> PathBuf {
    HOME.clone()
}

fn xdg(var: &str, fallback: &str) -> PathBuf {
    std::env::var_os(var)
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| home().join(fallback))
        .join("wukong")
}

/// `~/.config/wukong`.
pub fn config_dir() -> PathBuf {
    xdg("XDG_CONFIG_HOME", ".config")
}

/// `~/.local/share/wukong` — the store repo and the database.
pub fn data_dir() -> PathBuf {
    xdg("XDG_DATA_HOME", ".local/share")
}

/// `~/.local/state/wukong` — the daemon socket and pid file.
pub fn state_dir() -> PathBuf {
    xdg("XDG_STATE_HOME", ".local/state")
}

pub fn config_file() -> PathBuf {
    config_dir().join("config.toml")
}

pub fn store_dir() -> PathBuf {
    data_dir().join("store")
}

pub fn db_file() -> PathBuf {
    data_dir().join("wukong.db")
}

pub fn socket_file() -> PathBuf {
    state_dir().join("wukongd.sock")
}

/// A tracked file's identity: its path relative to `$HOME` when it
/// lives there ("~/.zshrc" → ".zshrc"), or an absolute-path mirror
/// under `__abs__` for the rare tracked file outside home.
pub fn store_rel(path: &Path) -> PathBuf {
    match path.strip_prefix(home()) {
        Ok(rel) => rel.to_path_buf(),
        Err(_) => PathBuf::from("__abs__").join(path.strip_prefix("/").unwrap_or(path)),
    }
}

/// The inverse of `store_rel`.
pub fn from_store_rel(rel: &Path) -> PathBuf {
    match rel.strip_prefix("__abs__") {
        Ok(abs) => PathBuf::from("/").join(abs),
        Err(_) => home().join(rel),
    }
}

/// Pretty form for display: `~/…` when under home.
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
