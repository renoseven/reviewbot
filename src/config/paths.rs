//! Defaults under `~/.reviewbot`. One flag, one default value, no implicit search.

use std::path::PathBuf;

use crate::common::paths::home_dir;

fn default_root() -> PathBuf {
    home_dir()
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_live_under_dot_reviewbot() {
        let home = crate::common::paths::home_dir().unwrap_or_else(|| PathBuf::from("."));
        assert_eq!(
            default_config_path(),
            home.join(".reviewbot").join("config.toml")
        );
        assert_eq!(default_runs_dir(), home.join(".reviewbot").join("runs"));
    }
}
