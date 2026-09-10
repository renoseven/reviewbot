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

/// Which engine this call uses. Exactly one, so an enum rather than a bit.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SearchKind {
    Regex,
    Keyword,
}

/// One path the listing named. `bytes` is `None` when this platform did not
/// hand a size over for free, not when the size is unknown forever.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct File {
    pub path: String,
    pub bytes: Option<u64>,
}

impl File {
    pub fn new(path: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            bytes: None,
        }
    }
}

/// The platform side of "what is in this repository". Paths are ordinary
/// repository relative strings; the path checks happen in `tool`, which is
/// the only way the model reaches this.
pub trait RepoSource: Send + Sync {
    /// Paths matching `glob`. `complete` is false when the platform could
    /// not hand over the whole tree, which the model has to be told.
    fn list_files(&self, glob: &str) -> Result<Listing, PlatformError>;

    fn read_file(&self, path: &str, lines: Option<LineRange>) -> Result<String, PlatformError>;

    /// A file's byte size without fetching its body.
    fn size(&self, path: &str) -> Result<u64, PlatformError>;

    fn search(
        &self,
        kind: SearchKind,
        query: &str,
        glob: Option<&str>,
    ) -> Result<Vec<SearchHit>, PlatformError>;

    /// Body already in hand from an earlier call. Hands it over, and never
    /// issues a request for it. `None` means this optimisation does not
    /// apply, not that the file is absent.
    fn cached_body(&self, _path: &str) -> Option<String> {
        None
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Listing {
    pub files: Vec<File>,
    pub complete: bool,
}

impl Listing {
    pub fn paths(&self) -> impl Iterator<Item = &str> {
        self.files.iter().map(|file| file.path.as_str())
    }
}

#[cfg(test)]
pub(crate) mod contract {
    use std::collections::BTreeMap;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use crate::platform::{Capabilities, PlatformError};

    use super::{File, LineRange, Listing, RepoSource, SearchHit, SearchKind};

    /// Counts reads separately from sizes so a test can tell "asked for the
    /// body" from "asked how big it is".
    pub(crate) struct Fake {
        pub files: Vec<File>,
        pub bodies: BTreeMap<String, String>,
        pub reads: AtomicUsize,
        pub sizes: AtomicUsize,
        pub supported: Capabilities,
    }

    impl Fake {
        pub(crate) fn keyword_only() -> Self {
            Self {
                files: vec![File {
                    path: "src/parse.c".to_string(),
                    bytes: Some(12),
                }],
                bodies: BTreeMap::from([(
                    "src/parse.c".to_string(),
                    "int parse(void);\n".to_string(),
                )]),
                reads: AtomicUsize::new(0),
                sizes: AtomicUsize::new(0),
                supported: Capabilities::KEYWORD_SEARCH,
            }
        }

        pub(crate) fn without_sizes() -> Self {
            Self {
                files: vec![File::new("src/parse.c"), File::new("docs/readme.md")],
                bodies: BTreeMap::new(),
                reads: AtomicUsize::new(0),
                sizes: AtomicUsize::new(0),
                supported: Capabilities::empty(),
            }
        }

        pub(crate) fn reads(&self) -> usize {
            self.reads.load(Ordering::SeqCst)
        }
    }

    impl RepoSource for Fake {
        fn list_files(&self, _glob: &str) -> Result<Listing, PlatformError> {
            Ok(Listing {
                files: self.files.clone(),
                complete: true,
            })
        }

        fn read_file(&self, path: &str, lines: Option<LineRange>) -> Result<String, PlatformError> {
            self.reads.fetch_add(1, Ordering::SeqCst);
            let body = self
                .bodies
                .get(path)
                .cloned()
                .ok_or_else(|| PlatformError::Request {
                    operation: "reading a repository file",
                    host: "test".to_string(),
                    reason: format!("{path} is not in this fake"),
                })?;
            Ok(match lines {
                Some(range) => range.slice(&body),
                None => body,
            })
        }

        fn size(&self, path: &str) -> Result<u64, PlatformError> {
            self.sizes.fetch_add(1, Ordering::SeqCst);
            self.files
                .iter()
                .find(|file| file.path == path)
                .and_then(|file| file.bytes)
                .or_else(|| self.bodies.get(path).map(|body| body.len() as u64))
                .ok_or_else(|| PlatformError::Request {
                    operation: "reading a file size",
                    host: "test".to_string(),
                    reason: format!("{path} is not in this fake"),
                })
        }

        fn search(
            &self,
            kind: SearchKind,
            _query: &str,
            _glob: Option<&str>,
        ) -> Result<Vec<SearchHit>, PlatformError> {
            let supported = match kind {
                SearchKind::Regex => Capabilities::REGEX_SEARCH,
                SearchKind::Keyword => Capabilities::KEYWORD_SEARCH,
            };
            if !self.supported.contains(supported) {
                return Err(PlatformError::Unsupported {
                    host: "test".to_string(),
                    capability: match kind {
                        SearchKind::Regex => "regular expression search",
                        SearchKind::Keyword => "keyword search",
                    },
                });
            }
            Ok(Vec::new())
        }
    }

    pub(crate) fn regex_search_on_keyword_only_is_an_error(source: &dyn RepoSource) {
        let error = source
            .search(SearchKind::Regex, "token", None)
            .expect_err("regex is not this engine");
        assert!(
            matches!(error, PlatformError::Unsupported { .. }),
            "{error}"
        );
    }

    pub(crate) fn listing_without_sizes_has_none(source: &dyn RepoSource) {
        let listing = source.list_files("**/*").expect("listed");
        assert!(
            listing.files.iter().all(|file| file.bytes.is_none()),
            "{listing:?}"
        );
    }

    pub(crate) fn size_does_not_read_the_body(
        source: &dyn RepoSource,
        path: &str,
        body_reads: impl Fn() -> usize,
    ) {
        let before = body_reads();
        source.size(path).expect("sized");
        assert_eq!(body_reads(), before, "size must not fetch the body");
    }
}

#[cfg(test)]
mod tests {
    use super::contract::{
        Fake, listing_without_sizes_has_none, regex_search_on_keyword_only_is_an_error,
        size_does_not_read_the_body,
    };
    use super::*;

    #[test]
    fn size_does_not_fetch_a_body() {
        let fake = Fake::keyword_only();
        size_does_not_read_the_body(&fake, "src/parse.c", || fake.reads());
        assert_eq!(fake.reads(), 0);
        assert_eq!(fake.size("src/parse.c").expect("sized"), 12);
    }

    #[test]
    fn regex_search_against_a_keyword_only_source_is_an_error() {
        regex_search_on_keyword_only_is_an_error(&Fake::keyword_only());
    }

    #[test]
    fn a_listing_from_a_source_with_no_sizes_has_none() {
        listing_without_sizes_has_none(&Fake::without_sizes());
    }
}
