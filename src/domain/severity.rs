use std::fmt;

use serde::{Deserialize, Serialize};

/// Band alias for `severity_score`. The score-to-band mapping is
/// `stage::merge`'s job; `domain` only names the four bands.
///
/// Its own four names rather than a second use of `Confidence`'s. The two
/// answer different questions — how much it matters if this is real, against
/// how sure the model is that it is real — and a reader who sees "high" twice
/// on one finding has to work out which "high" is which.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Trivial,
    Minor,
    Major,
    Critical,
}

impl Severity {
    pub const ALL: [Severity; 4] = [
        Severity::Critical,
        Severity::Major,
        Severity::Minor,
        Severity::Trivial,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Severity::Trivial => "trivial",
            Severity::Minor => "minor",
            Severity::Major => "major",
            Severity::Critical => "critical",
        }
    }
}

impl fmt::Display for Severity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}
