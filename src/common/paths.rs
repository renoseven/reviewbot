//! Home, `~`, and XDG directory roots. Binary-specific defaults stay in
//! `config`: they name `reviewbot.toml`, not paths in general.

use std::path::PathBuf;

pub fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

pub fn xdg_dir(var: &str, fallback: &str) -> PathBuf {
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
