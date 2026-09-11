//! Which step of a run something belongs to.

use std::fmt;

use serde::{Deserialize, Serialize};

/// One of the six steps every run walks, in the order they are walked.
///
/// One identity rather than a number travelling beside a name: the number
/// is what a progress row prints, the name is the checkpoint file and what a
/// person reads, and while they were two arguments every call site could pair
/// them wrongly and no reader would notice. The order is the declaration
/// order, so a stage can be compared with another — which is what makes
/// "finished up to here" sayable. Which step does what, and when, is still
/// not decided here.
///
/// The serialized form is the name, so `meta.json` and the traces of runs
/// recorded before this type existed still read back.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "lowercase")]
#[repr(u8)]
pub enum Stage {
    Input = 1,
    Plan = 2,
    Review = 3,
    Merge = 4,
    Report = 5,
    Publish = 6,
}

impl Stage {
    /// Every stage, in order. A watcher shows what is still coming with this;
    /// the sequence a run actually walks is still `review`'s to state.
    pub const ALL: [Self; 6] = [
        Self::Input,
        Self::Plan,
        Self::Review,
        Self::Merge,
        Self::Report,
        Self::Publish,
    ];

    /// Its place in the run, counted from 1 for a progress row rather than
    /// from 0 for an array. Checkpoint files use the name, not this.
    pub fn number(self) -> u8 {
        self as u8
    }

    /// The word for it, used in checkpoint names, traces, reports and on
    /// screen. One spelling everywhere, so none of them can drift.
    pub fn name(self) -> &'static str {
        match self {
            Self::Input => "input",
            Self::Plan => "plan",
            Self::Review => "review",
            Self::Merge => "merge",
            Self::Report => "report",
            Self::Publish => "publish",
        }
    }

    /// The stage before this one, if any. What "everything up to here" means
    /// when this one turns out not to have finished after all.
    pub fn previous(self) -> Option<Self> {
        Self::ALL
            .into_iter()
            .rev()
            .find(|candidate| *candidate < self)
    }

    /// This stage and every stage before it.
    pub fn through(self) -> impl Iterator<Item = Self> {
        Self::ALL.into_iter().filter(move |stage| *stage <= self)
    }
}

impl fmt::Display for Stage {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.name())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The names are a storage format: `meta.json` and the traces of runs
    /// recorded before this enum existed hold these exact strings.
    #[test]
    fn a_stage_serializes_as_its_name() {
        assert_eq!(
            serde_json::to_string(&Stage::Report).expect("serialize"),
            "\"report\""
        );
        assert_eq!(
            serde_json::from_str::<Stage>("\"publish\"").expect("deserialize"),
            Stage::Publish
        );
    }

    /// The comparison is the order a run walks them in. The numbers are what
    /// a progress row prints; checkpoint files use the name.
    #[test]
    fn the_order_is_the_declaration_order() {
        assert_eq!(
            Stage::ALL.map(Stage::number),
            [1, 2, 3, 4, 5, 6],
            "the numbers are for progress rows"
        );
        assert!(Stage::Input < Stage::Publish);
        assert!(Stage::ALL.is_sorted());
    }

    #[test]
    fn a_stage_knows_what_came_before_it() {
        assert_eq!(Stage::Input.previous(), None);
        assert_eq!(Stage::Review.previous(), Some(Stage::Plan));
        assert_eq!(
            Stage::Merge.through().collect::<Vec<_>>(),
            vec![Stage::Input, Stage::Plan, Stage::Review, Stage::Merge]
        );
    }
}
