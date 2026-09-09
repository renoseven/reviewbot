//! Where a credential comes from, and the value once it is in memory.
//!
//! The config file never holds the credential: it names an environment
//! variable or a path. This module is what that pointer becomes.

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};

/// A credential value. Never serialized, never printed, never fingerprinted.
#[derive(Clone)]
pub struct Secret(String);

impl Secret {
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl From<String> for Secret {
    fn from(value: String) -> Self {
        Secret(value)
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(<redacted>)")
    }
}

impl fmt::Display for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<redacted>")
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SecretError {
    #[error("{field} is empty; give an environment variable name or a path")]
    Empty { field: String },
    #[error("{field} looks like the credential itself; use an environment variable name or a path")]
    Inline { field: String },
    #[error("{field} points at {path}, which is inside the repository under review")]
    InsideRepo { field: String, path: PathBuf },
    #[error("cannot read {field} from {origin}: {reason}")]
    Unreadable {
        field: String,
        origin: String,
        reason: String,
    },
}

/// The two accepted forms of `api_key` / `api_token`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SecretSource {
    Env(String),
    File(PathBuf),
}

impl SecretSource {
    /// Values starting with `/`, `./` or `~` are paths; anything else is an
    /// environment variable name. A value that looks like the credential
    /// itself is rejected outright.
    pub fn parse(field: &str, value: &str) -> Result<Self, SecretError> {
        if value.is_empty() {
            return Err(SecretError::Empty {
                field: field.to_string(),
            });
        }
        if value.starts_with('/') || value.starts_with("./") || value.starts_with('~') {
            return Ok(SecretSource::File(PathBuf::from(
                shellexpand::tilde(value).as_ref(),
            )));
        }
        if looks_like_secret(value) {
            return Err(SecretError::Inline {
                field: field.to_string(),
            });
        }
        Ok(SecretSource::Env(value.to_string()))
    }

    pub fn describe(&self) -> String {
        match self {
            SecretSource::Env(name) => format!("environment variable {name}"),
            SecretSource::File(path) => format!("file {}", path.display()),
        }
    }

    /// Read the credential into memory. `repo_root`, when known, keeps
    /// credential files from living inside the repository under review.
    pub fn read(&self, field: &str, repo_root: Option<&Path>) -> Result<Secret, SecretError> {
        match self {
            SecretSource::Env(name) => {
                std::env::var(name)
                    .map(Secret)
                    .map_err(|_| SecretError::Unreadable {
                        field: field.to_string(),
                        origin: self.describe(),
                        reason: "environment variable is not set".to_string(),
                    })
            }
            SecretSource::File(path) => self.read_file(field, path, repo_root),
        }
    }

    fn read_file(
        &self,
        field: &str,
        path: &Path,
        repo_root: Option<&Path>,
    ) -> Result<Secret, SecretError> {
        if let Some(root) = repo_root
            && path.starts_with(root)
        {
            return Err(SecretError::InsideRepo {
                field: field.to_string(),
                path: path.to_path_buf(),
            });
        }
        let unreadable = |reason: String| SecretError::Unreadable {
            field: field.to_string(),
            origin: self.describe(),
            reason,
        };
        let metadata = std::fs::metadata(path).map_err(|e| unreadable(e.to_string()))?;
        check_mode_600(&metadata).map_err(unreadable)?;
        let raw = std::fs::read_to_string(path).map_err(|e| unreadable(e.to_string()))?;
        Ok(Secret(raw.trim_end_matches(['\n', '\r']).to_string()))
    }
}

#[cfg(unix)]
fn check_mode_600(metadata: &std::fs::Metadata) -> Result<(), String> {
    use std::os::unix::fs::MetadataExt;
    let mode = metadata.mode() & 0o777;
    if mode == 0o600 {
        Ok(())
    } else {
        Err(format!("permissions are {mode:o}, must be 600"))
    }
}

#[cfg(not(unix))]
fn check_mode_600(_metadata: &std::fs::Metadata) -> Result<(), String> {
    Ok(())
}

/// A value that is plainly the credential rather than a pointer to it.
fn looks_like_secret(value: &str) -> bool {
    const PREFIXES: [&str; 6] = ["sk-", "sk_", "ghp_", "gho_", "glpat-", "github_pat_"];
    if PREFIXES.iter().any(|p| value.starts_with(p)) {
        return true;
    }
    // `DEEPSEEK_API_KEY` and friends: the CI shape, never a credential.
    if value
        .chars()
        .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
    {
        return false;
    }
    value.len() >= 32 && shannon_bits_per_char(value) > 3.5
}

fn shannon_bits_per_char(value: &str) -> f64 {
    let mut counts: BTreeMap<char, usize> = BTreeMap::new();
    for c in value.chars() {
        *counts.entry(c).or_default() += 1;
    }
    let total = value.chars().count() as f64;
    -counts
        .values()
        .map(|&n| {
            let p = n as f64 / total;
            p * p.log2()
        })
        .sum::<f64>()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_var_names_are_not_mistaken_for_credentials() {
        assert_eq!(
            SecretSource::parse("api_key", "DEEPSEEK_API_KEY").unwrap(),
            SecretSource::Env("DEEPSEEK_API_KEY".to_string())
        );
        assert_eq!(
            SecretSource::parse("api_token", "A_VERY_LONG_ENVIRONMENT_VARIABLE_NAME_HERE").unwrap(),
            SecretSource::Env("A_VERY_LONG_ENVIRONMENT_VARIABLE_NAME_HERE".to_string())
        );
    }

    #[test]
    fn inline_credentials_are_rejected() {
        assert!(SecretSource::parse("api_key", "sk-abc123").is_err());
        assert!(
            SecretSource::parse("api_key", "glpat-xyzXYZ0123456789abcdef").is_err(),
            "known token prefixes are rejected"
        );
        assert!(
            SecretSource::parse("api_key", "9f3aQ7zK1mW8pL2xR6tY4bN0vC5hJ8dS").is_err(),
            "long high entropy strings are rejected"
        );
    }

    #[test]
    fn paths_are_recognized_by_their_first_character() {
        assert!(matches!(
            SecretSource::parse("api_key", "./local.key").unwrap(),
            SecretSource::File(_)
        ));
        assert!(matches!(
            SecretSource::parse("api_key", "/etc/reviewbot/openai.key").unwrap(),
            SecretSource::File(_)
        ));
        let SecretSource::File(path) = SecretSource::parse("api_key", "~/.reviewbot/key").unwrap()
        else {
            panic!("tilde is a path");
        };
        assert!(
            path.ends_with(".reviewbot/key"),
            "tilde expands: {}",
            path.display()
        );
        assert!(!path.starts_with("~"), "{}", path.display());
    }
}
