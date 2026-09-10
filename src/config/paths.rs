//! Defaults under `~/.reviewbot`. One flag, one default value, no implicit search.

use std::path::{Path, PathBuf};

fn default_root() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".reviewbot")
}

/// `~/.reviewbot/config.toml`. The cwd is never consulted.
pub fn default_config_path() -> PathBuf {
    default_root().join("config.toml")
}

/// `~/.reviewbot/runs`.
pub fn default_runs_dir() -> PathBuf {
    default_root().join("runs")
}

/// Expand a leading `~` / `~/`. Other paths are left alone.
pub fn expand_user(path: &Path) -> PathBuf {
    PathBuf::from(shellexpand::tilde(&path.to_string_lossy()).as_ref())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_live_under_dot_reviewbot() {
        let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
        assert_eq!(
            default_config_path(),
            home.join(".reviewbot").join("config.toml")
        );
        assert_eq!(default_runs_dir(), home.join(".reviewbot").join("runs"));
        assert_eq!(
            expand_user(Path::new("~/.reviewbot/config.toml")),
            default_config_path()
        );
        assert_eq!(
            expand_user(Path::new("/abs/config.toml")),
            Path::new("/abs/config.toml")
        );
    }
}
