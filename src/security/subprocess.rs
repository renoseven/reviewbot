//! What an external command is allowed to inherit and how far it may run.

/// Environment whitelist. Anything that looks like a credential is dropped
/// even if the whitelist would have let it through.
#[derive(Clone, Debug)]
pub struct EnvPolicy {
    allow: Vec<String>,
}

impl Default for EnvPolicy {
    fn default() -> Self {
        Self {
            allow: ["PATH", "HOME", "LANG", "LC_ALL", "TMPDIR", "TZ"]
                .iter()
                .map(|s| s.to_string())
                .collect(),
        }
    }
}

impl EnvPolicy {
    pub fn new(allow: Vec<String>) -> Self {
        Self { allow }
    }

    /// Keep the whitelisted names, then strip every `*_API_KEY` / `*_TOKEN`.
    pub fn apply<I, K, V>(&self, environment: I) -> Vec<(String, String)>
    where
        I: IntoIterator<Item = (K, V)>,
        K: AsRef<str>,
        V: AsRef<str>,
    {
        environment
            .into_iter()
            .filter(|(name, _)| self.allow.iter().any(|a| a == name.as_ref()))
            .filter(|(name, _)| !is_credential_name(name.as_ref()))
            .map(|(name, value)| (name.as_ref().to_string(), value.as_ref().to_string()))
            .collect()
    }
}

fn is_credential_name(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    upper.ends_with("_API_KEY")
        || upper.ends_with("_TOKEN")
        || upper == "API_KEY"
        || upper == "TOKEN"
}

/// Resource ceilings for one external command. Enforcing them is the
/// command tool's job; this only says what they are.
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    pub timeout_ms: u64,
    pub max_output_bytes: u64,
}

impl Limits {
    pub fn new(timeout_ms: u64, max_output_bytes: u64) -> Self {
        Self {
            timeout_ms,
            max_output_bytes,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credentials_never_reach_a_subprocess() {
        let policy = EnvPolicy::default();
        let kept = policy.apply([
            ("PATH", "/usr/bin"),
            ("HOME", "/home/ci"),
            ("DEEPSEEK_API_KEY", "secret"),
            ("GITLAB_TOKEN", "secret"),
            ("CI_JOB_ID", "42"),
        ]);
        assert_eq!(
            kept,
            vec![
                ("PATH".to_string(), "/usr/bin".to_string()),
                ("HOME".to_string(), "/home/ci".to_string()),
            ]
        );
    }

    #[test]
    fn a_whitelisted_name_that_looks_like_a_credential_is_still_dropped() {
        let policy = EnvPolicy::new(vec!["CUSTOM_TOKEN".to_string()]);
        assert!(policy.apply([("CUSTOM_TOKEN", "secret")]).is_empty());
    }
}
