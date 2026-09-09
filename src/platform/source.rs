//! Reading the repository through the platform API, always by `head_sha`.
//! reviewbot never clones.

use super::PlatformError;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LineRange {
    pub first: u32,
    pub last: u32,
}

impl LineRange {
    /// The lines this range names, 1 based and inclusive. A range that runs
    /// past the end of the file simply stops there.
    pub fn slice(&self, text: &str) -> String {
        text.lines()
            .enumerate()
            .filter(|(index, _)| {
                let number = *index as u32 + 1;
                number >= self.first && number <= self.last
            })
            .map(|(_, line)| line)
            .collect::<Vec<_>>()
            .join("\n")
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SearchHit {
    pub path: String,
    pub line: u32,
    pub text: String,
}

/// The platform side of "what is in this repository". Paths are ordinary
/// repository relative strings; the path checks happen in `tool`, which is
/// the only way the model reaches this.
pub trait RepoSource: Send + Sync {
    /// Paths matching `glob`. `complete` is false when the platform could
    /// not hand over the whole tree, which the model has to be told.
    fn list_files(&self, glob: &str) -> Result<Listing, PlatformError>;

    fn read_file(&self, path: &str, lines: Option<LineRange>) -> Result<String, PlatformError>;

    fn search(&self, query: &str, glob: Option<&str>) -> Result<Vec<SearchHit>, PlatformError>;
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Listing {
    pub paths: Vec<String>,
    pub complete: bool,
}
