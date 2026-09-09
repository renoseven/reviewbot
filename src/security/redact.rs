//! Replace credentials with placeholders, keeping the kind and the position
//! but never the value.

use regex::Regex;

pub struct Redactor {
    patterns: Vec<(Regex, &'static str)>,
    values: Vec<String>,
}

impl Default for Redactor {
    fn default() -> Self {
        Self::new()
    }
}

impl Redactor {
    pub fn new() -> Self {
        let patterns = vec![
            (
                Regex::new(r"-----BEGIN [A-Z ]*PRIVATE KEY-----[\s\S]*?-----END [A-Z ]*PRIVATE KEY-----")
                    .expect("valid pattern"),
                "<redacted:private-key>",
            ),
            (
                Regex::new(r"(?i)authorization\s*:\s*bearer\s+\S+").expect("valid pattern"),
                "<redacted:authorization-header>",
            ),
            (
                Regex::new(r"sk-[A-Za-z0-9_-]{8,}").expect("valid pattern"),
                "<redacted:api-key>",
            ),
            (
                Regex::new(r"glpat-[A-Za-z0-9_-]{8,}").expect("valid pattern"),
                "<redacted:token>",
            ),
            (
                Regex::new(r"gh[pousr]_[A-Za-z0-9]{16,}").expect("valid pattern"),
                "<redacted:token>",
            ),
            (
                Regex::new(r"github_pat_[A-Za-z0-9_]{20,}").expect("valid pattern"),
                "<redacted:token>",
            ),
            (
                // The `.env` shape: NAME=value, where the name says it is a credential.
                Regex::new(
                    r#"(?im)^(\s*(?:export\s+)?[A-Za-z0-9_]*(?:API_KEY|APIKEY|TOKEN|SECRET|PASSWORD)[A-Za-z0-9_]*\s*[:=]\s*)\S+"#,
                )
                .expect("valid pattern"),
                "${1}<redacted:credential>",
            ),
        ];
        Self {
            patterns,
            values: Vec::new(),
        }
    }

    /// Also blot out a value read at startup, so a credential that matches no
    /// pattern still never reaches the model or a trace.
    pub fn hide_value(&mut self, value: &str) {
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
        let redactor = Redactor::new();
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
        let redactor = Redactor::new();
        let text = "-----BEGIN RSA PRIVATE KEY-----\nMIIabc\n-----END RSA PRIVATE KEY-----";
        assert_eq!(redactor.redact(text), "<redacted:private-key>");
    }

    #[test]
    fn a_value_read_at_startup_is_hidden_even_without_a_matching_shape() {
        let mut redactor = Redactor::new();
        redactor.hide_value("plain-looking-credential");
        let redacted = redactor.redact("token is plain-looking-credential here");
        assert_eq!(redacted, "token is <redacted:credential> here");
    }

    #[test]
    fn ordinary_code_is_left_alone() {
        let redactor = Redactor::new();
        let code = "let total = count * 2; // sk- is not a key here";
        assert_eq!(redactor.redact(code), code);
    }
}
