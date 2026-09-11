#[derive(Debug, thiserror::Error)]
pub enum ProtocolError {
    #[error("unknown protocol {protocol:?}; known protocols: {known}")]
    Unknown { protocol: String, known: String },
    /// Timeouts, resets, 5xx, 429, empty body, cut-off JSON.
    #[error("{protocol} request failed: {reason}")]
    Transient {
        protocol: &'static str,
        reason: String,
        status: Option<u16>,
    },
    /// 401/403/400/422 and every other 4xx except 429.
    #[error("{protocol} request failed: {reason}")]
    Fatal {
        protocol: &'static str,
        reason: String,
        status: Option<u16>,
    },
    /// A complete JSON body that is the wrong schema. Merge's scoring call
    /// may re-ask; the HTTP layer does not retry this.
    #[error("{protocol} returned something this build cannot read: {reason}")]
    Malformed {
        protocol: &'static str,
        reason: String,
    },
    #[error("{protocol} is not implemented yet")]
    NotImplemented { protocol: &'static str },
}

impl ProtocolError {
    /// Only transient failures may be retried. Everything else fails on the
    /// first attempt: a 401 will not fix itself.
    pub fn is_transient(&self) -> bool {
        matches!(self, ProtocolError::Transient { .. })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_transient_variant_is_retried() {
        assert!(
            ProtocolError::Transient {
                protocol: "openai",
                reason: "HTTP 503".to_string(),
                status: Some(503),
            }
            .is_transient()
        );
        assert!(
            !ProtocolError::Fatal {
                protocol: "openai",
                reason: "HTTP 401".to_string(),
                status: Some(401),
            }
            .is_transient()
        );
        assert!(
            !ProtocolError::Malformed {
                protocol: "openai",
                reason: "response has no output".to_string(),
            }
            .is_transient()
        );
    }
}
