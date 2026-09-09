//! Turn a browser URL into host, project and number. The host picks the
//! `[[platform]]` entry; HTTP goes to that row's `base_url`, never to the
//! HTML page. Nothing is guessed from the rest of the URL.

use super::PlatformError;

/// One merge request or pull request.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ChangeRef {
    pub host: String,
    pub project: String,
    pub number: u64,
}

impl ChangeRef {
    /// Understands the two public shapes:
    /// `https://host/group/project/-/merge_requests/128` and
    /// `https://host/owner/repo/pull/128`.
    pub fn parse(url: &str) -> Result<Self, PlatformError> {
        let rest = url
            .strip_prefix("https://")
            .or_else(|| url.strip_prefix("http://"))
            .ok_or_else(|| PlatformError::UnparsableUrl {
                url: url.to_string(),
            })?;
        let (host, path) = rest
            .split_once('/')
            .ok_or_else(|| PlatformError::UnparsableUrl {
                url: url.to_string(),
            })?;
        let segments: Vec<&str> = path.trim_end_matches('/').split('/').collect();
        let marker = segments
            .iter()
            .position(|s| *s == "merge_requests" || *s == "pull" || *s == "pulls")
            .ok_or_else(|| PlatformError::UnparsableUrl {
                url: url.to_string(),
            })?;
        let number: u64 = segments
            .get(marker + 1)
            .and_then(|s| s.parse().ok())
            .ok_or_else(|| PlatformError::UnparsableUrl {
                url: url.to_string(),
            })?;
        let project_end = if segments.get(marker.wrapping_sub(1)) == Some(&"-") {
            marker - 1
        } else {
            marker
        };
        let project = segments[..project_end].join("/");
        if project.is_empty() {
            return Err(PlatformError::UnparsableUrl {
                url: url.to_string(),
            });
        }
        Ok(Self {
            host: host.to_string(),
            project,
            number,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gitlab_merge_request_urls_parse() {
        let parsed = ChangeRef::parse("https://gitlab.com/acme/app/-/merge_requests/128").unwrap();
        assert_eq!(
            parsed,
            ChangeRef {
                host: "gitlab.com".to_string(),
                project: "acme/app".to_string(),
                number: 128,
            }
        );
    }

    #[test]
    fn nested_gitlab_groups_keep_their_full_path() {
        let parsed =
            ChangeRef::parse("https://git.example.com/group/sub/app/-/merge_requests/7").unwrap();
        assert_eq!(parsed.project, "group/sub/app");
        assert_eq!(parsed.host, "git.example.com");
    }

    #[test]
    fn github_pull_request_urls_parse() {
        let parsed = ChangeRef::parse("https://github.com/acme/app/pull/42").unwrap();
        assert_eq!(parsed.project, "acme/app");
        assert_eq!(parsed.number, 42);
    }

    #[test]
    fn anything_else_is_rejected_rather_than_guessed() {
        assert!(ChangeRef::parse("https://gitlab.com/acme/app").is_err());
        assert!(ChangeRef::parse("./local.diff").is_err());
    }
}
