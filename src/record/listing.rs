//! Listing, inspecting and pruning run directories. None of this talks to a
//! model or a platform.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde::{Deserialize, Serialize};

use crate::domain::Confidence;

use super::{LocalStorage, Meta, RecordError, Recorder, Storage, layout};

/// Comment counts by band, matching `summary.json` so `run show` can
/// reuse the file without asking `publish` for the type.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct CommentCount {
    pub band: Confidence,
    pub count: usize,
}

/// How many newest runs `run prune` keeps when `--keep` is omitted.
/// Written into the command line, not the config: changing it must not
/// invalidate fingerprints. Zero means delete every run.
pub const DEFAULT_KEEP: usize = 0;

/// After this many runs, `review` names `run prune` once. Not deletion,
/// and not the prune default: that default is zero.
pub const WARN_AFTER_RUNS: usize = 10;

/// One row of `run list`. Missing `meta.json` still shows the directory
/// name so a half-written run is visible.
#[derive(Clone, Debug, Serialize)]
pub struct RunRow {
    pub run_id: String,
    pub input: String,
    pub completed_stages: Vec<String>,
    pub spent: f64,
    pub currency: String,
    pub updated_at: u64,
}

/// What `run show` prints: the list row plus comment counts and the
/// paths a person actually opens.
#[derive(Clone, Debug, Serialize)]
pub struct RunShow {
    pub run_id: String,
    pub input: String,
    pub model: String,
    pub completed_stages: Vec<String>,
    pub comments: Vec<CommentCount>,
    pub spent: f64,
    pub budget: Option<f64>,
    pub currency: String,
    pub report: PathBuf,
    pub summary: PathBuf,
    pub traces: PathBuf,
}

/// What `run prune` did, or would do.
#[derive(Clone, Debug, Serialize)]
pub struct PruneReport {
    pub runs_dir: PathBuf,
    pub keep: usize,
    pub dry_run: bool,
    pub kept: Vec<String>,
    pub deleted: Vec<String>,
}

/// Immediate child directories of `runs_dir`. A missing directory is empty,
/// not an error: `run list` on a fresh machine still names the path.
pub fn list_run_dirs(runs_dir: &Path) -> Result<Vec<PathBuf>, RecordError> {
    let entries = match fs::read_dir(runs_dir) {
        Ok(entries) => entries,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(source) => {
            return Err(RecordError::Io {
                path: runs_dir.to_path_buf(),
                source,
            });
        }
    };
    let mut dirs = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|source| RecordError::Io {
            path: runs_dir.to_path_buf(),
            source,
        })?;
        if entry
            .file_type()
            .map_err(|source| RecordError::Io {
                path: entry.path(),
                source,
            })?
            .is_dir()
        {
            dirs.push(entry.path());
        }
    }
    Ok(dirs)
}

pub fn count_runs(runs_dir: &Path) -> Result<usize, RecordError> {
    Ok(list_run_dirs(runs_dir)?.len())
}

/// Newest first by directory mtime. Success and failure sit in one queue.
pub fn runs_by_mtime(runs_dir: &Path) -> Result<Vec<PathBuf>, RecordError> {
    let mut dirs = list_run_dirs(runs_dir)?;
    dirs.sort_by_key(|directory| std::cmp::Reverse(mtime(directory)));
    Ok(dirs)
}

pub fn list_runs(runs_dir: &Path) -> Result<Vec<RunRow>, RecordError> {
    let mut rows = Vec::new();
    for directory in runs_by_mtime(runs_dir)? {
        rows.push(row_from_dir(&directory)?);
    }
    Ok(rows)
}

pub fn show_run(runs_dir: &Path, run_id: &str) -> Result<RunShow, RecordError> {
    let directory = runs_dir.join(run_id);
    if !directory.is_dir() {
        return Err(RecordError::RunNotFound {
            run_id: run_id.to_string(),
            runs_dir: runs_dir.to_path_buf(),
        });
    }
    let row = row_from_dir(&directory)?;
    let storage = LocalStorage::open(directory.clone());
    let meta = Recorder::peek_meta(&storage)?;
    let (model, budget, comments) = match &meta {
        Some(meta) => (
            meta.model.clone(),
            budget_ceiling(meta),
            comments_from(&storage, meta),
        ),
        None => (
            String::new(),
            None,
            Confidence::ALL
                .iter()
                .map(|band| CommentCount {
                    band: *band,
                    count: 0,
                })
                .collect(),
        ),
    };
    Ok(RunShow {
        run_id: row.run_id,
        input: row.input,
        model,
        completed_stages: row.completed_stages,
        comments,
        spent: row.spent,
        budget,
        currency: row.currency,
        report: directory.join(layout::REPORT),
        summary: directory.join(layout::SUMMARY),
        traces: directory.join("traces"),
    })
}

/// Keep the newest `keep` run directories; delete the rest, report.md
/// included. `--dry-run` only names what would go.
pub fn prune_runs(runs_dir: &Path, keep: usize, dry_run: bool) -> Result<PruneReport, RecordError> {
    let ranked = runs_by_mtime(runs_dir)?;
    let kept: Vec<String> = ranked
        .iter()
        .take(keep)
        .filter_map(|path| {
            path.file_name()
                .map(|name| name.to_string_lossy().into_owned())
        })
        .collect();
    let deleted: Vec<String> = ranked
        .iter()
        .skip(keep)
        .filter_map(|path| {
            path.file_name()
                .map(|name| name.to_string_lossy().into_owned())
        })
        .collect();
    if !dry_run {
        for path in ranked.iter().skip(keep) {
            fs::remove_dir_all(path).map_err(|source| RecordError::Io {
                path: path.clone(),
                source,
            })?;
        }
    }
    Ok(PruneReport {
        runs_dir: runs_dir.to_path_buf(),
        keep,
        dry_run,
        kept,
        deleted,
    })
}

fn row_from_dir(directory: &Path) -> Result<RunRow, RecordError> {
    let run_id = directory
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let storage = LocalStorage::open(directory.to_path_buf());
    match Recorder::peek_meta(&storage)? {
        Some(meta) => Ok(RunRow {
            run_id: meta.run_id,
            input: meta.input.source,
            completed_stages: meta.completed_stages,
            spent: meta.spent,
            currency: meta.currency,
            updated_at: meta.updated_at,
        }),
        None => Ok(RunRow {
            run_id,
            input: String::new(),
            completed_stages: Vec::new(),
            spent: 0.0,
            currency: String::new(),
            updated_at: 0,
        }),
    }
}

fn comments_from(storage: &LocalStorage, _meta: &Meta) -> Vec<CommentCount> {
    if let Ok(Some(bytes)) = storage.read(layout::SUMMARY)
        && let Ok(summary) = serde_json::from_slice::<serde_json::Value>(&bytes)
        && let Some(comments) = summary.get("comments")
        && let Ok(parsed) = serde_json::from_value::<Vec<CommentCount>>(comments.clone())
    {
        return parsed;
    }
    Confidence::ALL
        .iter()
        .map(|band| CommentCount {
            band: *band,
            count: 0,
        })
        .collect()
}

fn budget_ceiling(meta: &Meta) -> Option<f64> {
    if meta.budget_limit < 0.0 {
        None
    } else {
        Some(meta.budget_limit)
    }
}

fn mtime(path: &Path) -> SystemTime {
    path.metadata()
        .and_then(|meta| meta.modified())
        .unwrap_or(SystemTime::UNIX_EPOCH)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, SystemTime};

    fn touch_dir(path: &Path, secs: u64) {
        fs::create_dir_all(path).expect("dir");
        let time = SystemTime::UNIX_EPOCH + Duration::from_secs(secs);
        let file = fs::File::open(path).expect("open dir");
        file.set_modified(time).expect("mtime");
    }

    #[test]
    fn prune_keeps_the_newest_and_deletes_the_rest_by_mtime() {
        let root = tempfile::tempdir().expect("temp");
        let runs = root.path().join("runs");
        fs::create_dir_all(runs.join("old")).expect("old");
        fs::write(runs.join("old").join(layout::REPORT), b"old").expect("report");
        fs::create_dir_all(runs.join("mid")).expect("mid");
        fs::create_dir_all(runs.join("new")).expect("new");
        // mtime last: writing a file would otherwise make `old` the newest.
        touch_dir(&runs.join("old"), 100);
        touch_dir(&runs.join("mid"), 200);
        touch_dir(&runs.join("new"), 300);

        let dry = prune_runs(&runs, 2, true).expect("dry-run");
        assert!(dry.dry_run);
        assert_eq!(dry.kept, vec!["new", "mid"]);
        assert_eq!(dry.deleted, vec!["old"]);
        assert!(runs.join("old").join(layout::REPORT).is_file());

        let done = prune_runs(&runs, 2, false).expect("prune");
        assert!(!done.dry_run);
        assert!(!runs.join("old").exists());
        assert!(runs.join("new").is_dir());
        assert!(runs.join("mid").is_dir());
    }

    #[test]
    fn prune_keep_zero_deletes_every_run() {
        let root = tempfile::tempdir().expect("temp");
        let runs = root.path().join("runs");
        fs::create_dir_all(runs.join("old")).expect("old");
        fs::create_dir_all(runs.join("new")).expect("new");
        touch_dir(&runs.join("old"), 100);
        touch_dir(&runs.join("new"), 200);

        let done = prune_runs(&runs, DEFAULT_KEEP, false).expect("prune");
        assert_eq!(done.keep, 0);
        assert!(done.kept.is_empty());
        assert_eq!(done.deleted.len(), 2);
        assert!(!runs.join("old").exists());
        assert!(!runs.join("new").exists());
    }

    #[test]
    fn a_missing_runs_directory_lists_as_empty() {
        let rows = list_runs(Path::new("/nonexistent/reviewbot-runs")).expect("empty");
        assert!(rows.is_empty());
        assert_eq!(
            count_runs(Path::new("/nonexistent/reviewbot-runs")).unwrap(),
            0
        );
    }
}
