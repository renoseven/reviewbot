use std::fmt;

use serde::{Deserialize, Serialize};

/// Band alias for `confidence_score`. The score-to-band mapping is
/// `stage::merge`'s job; `domain` only names the four bands.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Confidence {
    Low,
    Medium,
    High,
    Certain,
}

impl Confidence {
    pub const ALL: [Confidence; 4] = [
        Confidence::Certain,
        Confidence::High,
        Confidence::Medium,
        Confidence::Low,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Confidence::Low => "low",
            Confidence::Medium => "medium",
            Confidence::High => "high",
            Confidence::Certain => "certain",
        }
    }
}

impl fmt::Display for Confidence {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}
