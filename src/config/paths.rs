//! XDG defaults. One flag, one default value, no implicit search.

use std::path::PathBuf;

use crate::common::paths::xdg_dir;

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
