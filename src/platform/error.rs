#[derive(Debug, thiserror::Error)]
pub enum PlatformError {
    #[error("cannot read {url} as a merge request or pull request URL")]
    UnparsableUrl { url: String },
    #[error("no [[platform]] entry for host {host:?}; add one to the config")]
    UnknownHost { host: String },
    #[error("{operation} against {host} failed: {reason}")]
    Request {
        operation: &'static str,
        host: String,
        reason: String,
    },
    #[error(
        "{host} refused {operation} (HTTP {status}) at {url}: {said}. \
         If it is the token, GitLab needs the `api` scope and GitHub needs \
         `pull_requests: write`. The run stops rather than falling back to a \
         report only"
    )]
    Permission {
        operation: &'static str,
        host: String,
        status: u16,
        url: String,
        /// What the platform put in the body. A 403 is not always the token:
        /// GitHub sends one for a missing `User-Agent` too, and without this
        /// the only readable explanation is thrown away.
        said: String,
    },
    #[error("{operation} against {host} was rejected (HTTP 422): {reason}")]
    Unprocessable {
        operation: &'static str,
        host: String,
        reason: String,
    },
    #[error("{host} does not support {capability}")]
    Unsupported {
        host: String,
        capability: &'static str,
    },
    #[error("{operation} is not implemented yet for {kind}")]
    NotImplemented {
        operation: &'static str,
        kind: &'static str,
    },
}
