//! Replace credentials with placeholders, keeping the kind and the position
//! but never the value.

use regex::{Error as RegexError, Regex};

#[derive(Debug, thiserror::Error)]
#[error("a builtin redaction pattern is invalid: {0}")]
pub struct PatternError(RegexError);

const PATTERNS: &[(&str, &str)] = &[
    (
        r"-----BEGIN [A-Z ]*PRIVATE KEY-----[\s\S]*?-----END [A-Z ]*PRIVATE KEY-----",
        "<redacted:private-key>",
    ),
    (
        r"(?i)authorization\s*:\s*bearer\s+\S+",
        "<redacted:authorization-header>",
    ),
    (r"sk-[A-Za-z0-9_-]{8,}", "<redacted:api-key>"),
    (r"glpat-[A-Za-z0-9_-]{8,}", "<redacted:token>"),
    (r"gh[pousr]_[A-Za-z0-9]{16,}", "<redacted:token>"),
    (r"github_pat_[A-Za-z0-9_]{20,}", "<redacted:token>"),
    (
        r#"(?im)^(\s*(?:export\s+)?[A-Za-z0-9_]*(?:API_KEY|APIKEY|TOKEN|SECRET|PASSWORD)[A-Za-z0-9_]*\s*[:=]\s*)\S+"#,
        "${1}<redacted:credential>",
    ),
];

pub struct Redactor {
    patterns: Vec<(Regex, &'static str)>,
    values: Vec<String>,
}

impl Redactor {
    pub fn new() -> Result<Self, PatternError> {
        let patterns = PATTERNS
            .iter()
            .map(|(source, replacement)| {
                Regex::new(source)
                    .map(|regex| (regex, *replacement))
                    .map_err(PatternError)
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self {
            patterns,
            values: Vec::new(),
        })
    }

    /// Secrets known at construction, so a credential that matches no
    /// pattern still never reaches the model or a trace.
    pub fn with_secrets(
        secrets: impl IntoIterator<Item = impl AsRef<str>>,
    ) -> Result<Self, PatternError> {
        let mut redactor = Self::new()?;
        for secret in secrets {
            redactor.hide_value(secret.as_ref());
        }
        Ok(redactor)
    }

    fn hide_value(&mut self, value: &str) {
        if value.len() >= 8 {
            self.values.push(value.to_string());
        }
    }

    pub fn redact(&self, text: &str) -> String {
        let mut out = text.to_string();
        for value in &self.values {
            out = out.replace(value.as_str(), "<redacted:credential>");
        }
        for (pattern, replacement) in &self.patterns {
            out = pattern.replace_all(&out, *replacement).into_owned();
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_credential_shapes_are_replaced() {
        let redactor = Redactor::new().expect("patterns");
        let text = concat!(
            "key = sk-abcdefgh12345678\n",
            "Authorization: Bearer abc.def.ghi\n",
            "GITLAB_TOKEN=glpat-ABCDEFGH12345678\n",
        );
        let redacted = redactor.redact(text);
        assert!(!redacted.contains("sk-abcdefgh12345678"), "{redacted}");
        assert!(!redacted.contains("abc.def.ghi"), "{redacted}");
        assert!(!redacted.contains("glpat-ABCDEFGH12345678"), "{redacted}");
        assert!(redacted.contains("<redacted:"), "{redacted}");
    }

    #[test]
    fn private_key_blocks_are_replaced_whole() {
        let redactor = Redactor::new().expect("patterns");
        let text = "-----BEGIN RSA PRIVATE KEY-----\nMIIabc\n-----END RSA PRIVATE KEY-----";
        assert_eq!(redactor.redact(text), "<redacted:private-key>");
    }

    #[test]
    fn a_value_read_at_startup_is_hidden_even_without_a_matching_shape() {
        let redactor = Redactor::with_secrets(["plain-looking-credential"]).expect("patterns");
        let redacted = redactor.redact("token is plain-looking-credential here");
        assert_eq!(redacted, "token is <redacted:credential> here");
    }

    #[test]
    fn ordinary_code_is_left_alone() {
        let redactor = Redactor::new().expect("patterns");
        let code = "let total = count * 2; // sk- is not a key here";
        assert_eq!(redactor.redact(code), code);
    }
}
