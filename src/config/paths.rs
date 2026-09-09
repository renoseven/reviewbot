//! XDG defaults. One flag, one default value, no implicit search.

use std::path::{Path, PathBuf};

pub fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

/// `$XDG_CONFIG_HOME/reviewbot/reviewbot.toml`, else
/// `~/.config/reviewbot/reviewbot.toml`. The cwd is never consulted.
pub fn default_config_path() -> PathBuf {
    xdg_dir("XDG_CONFIG_HOME", ".config")
        .join("reviewbot")
        .join("reviewbot.toml")
}

/// `$XDG_STATE_HOME/reviewbot/runs`, else `~/.local/state/reviewbot/runs`.
pub fn default_runs_dir() -> PathBuf {
    xdg_dir("XDG_STATE_HOME", ".local/state")
        .join("reviewbot")
        .join("runs")
}

fn xdg_dir(var: &str, fallback: &str) -> PathBuf {
    match std::env::var_os(var) {
        Some(value) if !value.is_empty() => PathBuf::from(value),
        _ => home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(fallback),
    }
}

/// Expand a leading `~` against `$HOME`. Any other `~` is left alone.
pub fn expand_tilde(value: &str) -> PathBuf {
    let Some(rest) = value.strip_prefix('~') else {
        return PathBuf::from(value);
    };
    let Some(home) = home_dir() else {
        return PathBuf::from(value);
    };
    match rest.strip_prefix('/') {
        Some(tail) => home.join(tail),
        None if rest.is_empty() => home,
        None => PathBuf::from(value),
    }
}

/// Best effort absolute form that does not require the path to exist.
pub fn absolute(path: &Path) -> PathBuf {
    if path.is_absolute() {
        return path.to_path_buf();
    }
    std::env::current_dir()
        .map(|cwd| cwd.join(path))
        .unwrap_or_else(|_| path.to_path_buf())
}
