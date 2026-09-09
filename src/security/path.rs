//! Path checks for everything the model can ask to read.

use std::path::{Component, Path, PathBuf};

use globset::{Glob, GlobSet, GlobSetBuilder};

use crate::config::{ConfigError, SecuritySettings, Settings};

#[derive(Clone, Debug, thiserror::Error, PartialEq)]
pub enum PathRejection {
    #[error("{path}: absolute paths are not accepted; give a repository relative path")]
    Absolute { path: String },
    #[error("{path}: `..` is not accepted")]
    Traversal { path: String },
    #[error("{path}: empty path")]
    Empty { path: String },
    #[error("{path}: denied by deny_paths")]
    Denied { path: String },
    #[error("{path}: extension is not in allow_extensions ({allowed})")]
    Extension { path: String, allowed: String },
    #[error("{path}: {segment} is a symlink and follow_symlinks is false")]
    Symlink { path: String, segment: String },
    #[error("{path}: resolves outside the worktree root")]
    OutsideWorktree { path: String },
    #[error("{path}: cannot inspect: {reason}")]
    Unreadable { path: String, reason: String },
}

/// The read boundary. Built once at startup from `[security]` plus the
/// directories this run writes to; the config may only append to it.
#[derive(Clone, Debug)]
pub struct PathPolicy {
    deny: GlobSet,
    deny_patterns: Vec<String>,
    allow_extensions: Vec<String>,
    follow_symlinks: bool,
}

impl PathPolicy {
    /// Builtin denials are always present: `.git/**` plus whatever this run
    /// writes (the runs directory and `--out-dir`), expressed relative to
    /// `repo_root` when they land inside it.
    pub fn new(
        settings: &SecuritySettings,
        written_paths: &[PathBuf],
        repo_root: Option<&Path>,
    ) -> Result<Self, globset::Error> {
        let mut patterns = vec![".git".to_string(), ".git/**".to_string()];
        for written in written_paths {
            patterns.extend(deny_patterns_for(written, repo_root));
        }
        patterns.extend(settings.deny_paths.iter().cloned());

        let mut builder = GlobSetBuilder::new();
        for pattern in &patterns {
            builder.add(Glob::new(pattern)?);
        }
        Ok(Self {
            deny: builder.build()?,
            deny_patterns: patterns,
            allow_extensions: settings.allow_extensions.clone(),
            follow_symlinks: settings.follow_symlinks,
        })
    }

    /// The read boundary of one run, assembled from the settings alone.
    ///
    /// Lives here rather than at either caller because there are two: the
    /// stages, and `tool list`, which builds the real tools to print their
    /// contracts. Two assemblies of the same boundary is one of them being
    /// wrong.
    pub fn for_settings(settings: &Settings) -> Result<Self, ConfigError> {
        Self::new(
            &settings.config.security,
            &settings.written_paths(),
            settings.options.worktree.as_deref(),
        )
        .map_err(|error| ConfigError::InvalidGlob {
            field: "[security].deny_paths",
            pattern: String::new(),
            reason: error.to_string(),
        })
    }

    pub fn deny_patterns(&self) -> &[String] {
        &self.deny_patterns
    }

    /// API mode: normalize, deny list, extension whitelist. There is no
    /// symlink or root check because nothing lands on disk.
    pub fn check_repo_path(&self, path: &str) -> Result<String, PathRejection> {
        let normalized = normalize(path)?;
        self.check_deny(&normalized)?;
        self.check_extension(&normalized)?;
        Ok(normalized)
    }

    /// Worktree mode: the same three checks plus the symlink rule, and the
    /// resolved path must still sit under `root`.
    pub fn check_worktree_path(&self, path: &str, root: &Path) -> Result<String, PathRejection> {
        let normalized = self.check_repo_path(path)?;
        self.check_symlinks(&normalized, root)?;
        Ok(normalized)
    }

    /// Whether `deny_paths` covers this path, builtin entries included. The
    /// answer without the rest of the checks, for callers that only need to
    /// name a reason rather than open the file.
    pub fn is_denied(&self, path: &str) -> bool {
        self.deny.is_match(path)
    }

    /// Whether a listing or search pattern aims into denied territory. A glob
    /// is answered with names rather than content, but the names inside a
    /// denied directory are exactly what `deny_paths` is keeping back, so the
    /// pattern is refused before the tree is even asked for.
    pub fn denies_glob(&self, glob: &str) -> bool {
        if self.is_denied(glob) {
            return true;
        }
        let literal = literal_prefix(glob);
        !literal.is_empty()
            && (self.is_denied(&literal) || self.is_denied(&format!("{literal}/**")))
    }

    /// Only the paths the model may see: denied ones are dropped from list
    /// and search results rather than replaced with a placeholder.
    pub fn retain_visible<'a>(&self, paths: impl IntoIterator<Item = &'a str>) -> Vec<String> {
        paths
            .into_iter()
            .filter(|path| !self.is_denied(path))
            .map(|path| path.to_string())
            .collect()
    }

    fn check_deny(&self, path: &str) -> Result<(), PathRejection> {
        if self.is_denied(path) {
            return Err(PathRejection::Denied {
                path: path.to_string(),
            });
        }
        Ok(())
    }

    fn check_extension(&self, path: &str) -> Result<(), PathRejection> {
        let extension = Path::new(path)
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or_default();
        if self
            .allow_extensions
            .iter()
            .any(|allowed| allowed.eq_ignore_ascii_case(extension))
        {
            return Ok(());
        }
        Err(PathRejection::Extension {
            path: path.to_string(),
            allowed: self.allow_extensions.join(", "),
        })
    }

    fn check_symlinks(&self, path: &str, root: &Path) -> Result<(), PathRejection> {
        if self.follow_symlinks {
            return self.check_resolves_inside(path, root);
        }
        let mut walked = root.to_path_buf();
        for segment in path.split('/') {
            walked.push(segment);
            match std::fs::symlink_metadata(&walked) {
                Ok(metadata) if metadata.file_type().is_symlink() => {
                    return Err(PathRejection::Symlink {
                        path: path.to_string(),
                        segment: segment.to_string(),
                    });
                }
                // A path that does not exist yet is not a symlink escape;
                // the read itself will report the missing file.
                Ok(_) | Err(_) => {}
            }
        }
        Ok(())
    }

    fn check_resolves_inside(&self, path: &str, root: &Path) -> Result<(), PathRejection> {
        let target = root.join(path);
        if !target.exists() {
            return Ok(());
        }
        let resolved = target
            .canonicalize()
            .map_err(|e| PathRejection::Unreadable {
                path: path.to_string(),
                reason: e.to_string(),
            })?;
        let root = root.canonicalize().map_err(|e| PathRejection::Unreadable {
            path: path.to_string(),
            reason: e.to_string(),
        })?;
        if resolved.starts_with(&root) {
            Ok(())
        } else {
            Err(PathRejection::OutsideWorktree {
                path: path.to_string(),
            })
        }
    }
}

/// Deny both the directory itself and everything under it, relative to the
/// repository when it lives inside it.
fn deny_patterns_for(written: &Path, repo_root: Option<&Path>) -> Vec<String> {
    let inside = repo_root.and_then(|root| written.strip_prefix(root).ok());
    let base = match inside {
        Some(relative) => relative.to_string_lossy().replace('\\', "/"),
        None => written.to_string_lossy().replace('\\', "/"),
    };
    if base.is_empty() {
        return Vec::new();
    }
    vec![base.clone(), format!("{base}/**")]
}

/// The leading part of a glob that holds no metacharacter, which is the
/// furthest up the tree the pattern can ever reach.
fn literal_prefix(glob: &str) -> String {
    let mut segments = Vec::new();
    for segment in glob.split('/') {
        if segment.contains(['*', '?', '[', ']', '{', '}']) {
            break;
        }
        segments.push(segment);
    }
    segments.join("/")
}

/// Collapse `.`, reject `..` and absolute paths, and give back a plain
/// repository relative path with forward slashes.
fn normalize(path: &str) -> Result<String, PathRejection> {
    if path.is_empty() {
        return Err(PathRejection::Empty {
            path: path.to_string(),
        });
    }
    let candidate = Path::new(path);
    if candidate.is_absolute() || path.starts_with('/') {
        return Err(PathRejection::Absolute {
            path: path.to_string(),
        });
    }
    let mut parts: Vec<String> = Vec::new();
    for component in candidate.components() {
        match component {
            Component::CurDir => {}
            Component::Normal(part) => parts.push(part.to_string_lossy().into_owned()),
            Component::ParentDir => {
                return Err(PathRejection::Traversal {
                    path: path.to_string(),
                });
            }
            Component::RootDir | Component::Prefix(_) => {
                return Err(PathRejection::Absolute {
                    path: path.to_string(),
                });
            }
        }
    }
    if parts.is_empty() {
        return Err(PathRejection::Empty {
            path: path.to_string(),
        });
    }
    Ok(parts.join("/"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy(deny: &[&str], written: &[&str]) -> PathPolicy {
        let settings = SecuritySettings {
            deny_paths: deny.iter().map(|s| s.to_string()).collect(),
            ..SecuritySettings::for_tests()
        };
        let written: Vec<PathBuf> = written.iter().map(PathBuf::from).collect();
        PathPolicy::new(&settings, &written, Some(Path::new("/repo"))).expect("valid globs")
    }

    #[test]
    fn traversal_and_absolute_paths_are_rejected() {
        let policy = policy(&[], &[]);
        assert!(matches!(
            policy.check_repo_path("../etc/passwd"),
            Err(PathRejection::Traversal { .. })
        ));
        assert!(matches!(
            policy.check_repo_path("/etc/passwd"),
            Err(PathRejection::Absolute { .. })
        ));
    }

    #[test]
    fn builtin_denials_survive_an_empty_security_section() {
        let policy = policy(&[], &["/repo/.reviewbot/runs", "/repo/artifacts"]);
        assert!(matches!(
            policy.check_repo_path(".git/config"),
            Err(PathRejection::Denied { .. })
        ));
        assert!(matches!(
            policy.check_repo_path(".reviewbot/runs/abc/report.md"),
            Err(PathRejection::Denied { .. })
        ));
        assert!(matches!(
            policy.check_repo_path("artifacts/report-abc.md"),
            Err(PathRejection::Denied { .. })
        ));
    }

    #[test]
    fn deny_paths_match_directories_and_single_files() {
        let policy = policy(&["secrets/**", "**/*.tfvars", "**/production.toml"], &[]);
        assert!(matches!(
            policy.check_repo_path("secrets/deploy.toml"),
            Err(PathRejection::Denied { .. })
        ));
        assert!(matches!(
            policy.check_repo_path("infra/terraform.tfvars"),
            Err(PathRejection::Denied { .. })
        ));
        assert!(
            matches!(
                policy.check_repo_path("config/production.toml"),
                Err(PathRejection::Denied { .. })
            ),
            "denied even though toml is an allowed extension"
        );
        assert_eq!(
            policy.check_repo_path("./src/main.rs").unwrap(),
            "src/main.rs"
        );
    }

    #[test]
    fn extensions_outside_the_whitelist_are_rejected() {
        let policy = policy(&[], &[]);
        assert!(matches!(
            policy.check_repo_path("deploy/server.pem"),
            Err(PathRejection::Extension { .. })
        ));
        assert!(
            matches!(
                policy.check_repo_path("Makefile"),
                Err(PathRejection::Extension { .. })
            ),
            "known limitation: files without an extension are unreadable"
        );
    }

    /// A listing answers with names, and the names inside a denied directory
    /// are exactly what `deny_paths` is holding back, so a pattern aimed there
    /// is refused rather than answered with a filtered list.
    #[test]
    fn a_glob_aimed_into_denied_territory_is_refused() {
        let policy = policy(&["secrets/**"], &["/repo/.reviewbot/runs"]);
        assert!(policy.denies_glob("secrets/**"));
        assert!(policy.denies_glob("secrets/*.toml"));
        assert!(policy.denies_glob("secrets"));
        assert!(policy.denies_glob(".git/**"));
        assert!(policy.denies_glob(".reviewbot/runs/**"));
        assert!(!policy.denies_glob("src/**/*.rs"));
        assert!(
            !policy.denies_glob("**/*"),
            "a whole-tree glob is fine: the denied paths drop out of the answer"
        );
    }

    #[test]
    fn denied_paths_drop_out_of_listings() {
        let policy = policy(&["secrets/**"], &[]);
        let visible = policy.retain_visible(["src/main.rs", "secrets/key.toml", "Makefile"]);
        assert_eq!(
            visible,
            vec!["src/main.rs".to_string(), "Makefile".to_string()],
            "extension filtering does not apply to listings"
        );
    }
}
