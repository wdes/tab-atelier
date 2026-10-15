// SPDX-License-Identifier: MPL-2.0

//! The outbox: the reports the Kiosk lists, and the intentions it records.
//!
//! A report is any markdown file in the outbox. The reports tab derives its own
//! facets from the filename, so the Kiosk hands over only what the filesystem
//! knows — name, path, mtime — and invents nothing.

use std::fs;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;

/// One report, as the reports tab reads it.
///
/// Only the three fields the filesystem has. The tab folds repo and task out of
/// the name itself, so inventing them here would be a second opinion it ignores.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Report {
    /// File name, e.g. `audit-01.md`.
    pub name: String,
    /// Path as the journal writes it, relative to the Kiosk's state directory —
    /// the form `/decisions/file?path=` serves.
    pub path: String,
    /// Last modified, Unix seconds. `0` when the filesystem will not say.
    pub mtime: u64,
}

/// Why an intention could not be recorded.
#[derive(Debug, PartialEq, Eq)]
pub enum IntentError {
    /// Nothing to record: a lone heading is not an intention.
    Empty,
    /// The outbox could not be written.
    Unavailable,
}

/// Lists the markdown reports in the outbox, newest first.
///
/// A missing or unreadable outbox is an empty list, not an error: a fresh kiosk
/// has no reports yet, which is a state and not a fault.
#[must_use]
pub fn list(outbox: &Path) -> Vec<Report> {
    let Ok(entries) = fs::read_dir(outbox) else {
        return Vec::new();
    };
    let mut reports: Vec<Report> = entries
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            let name = path.file_name()?.to_str()?.to_string();
            if !path.is_file() || !is_markdown(&name) {
                return None;
            }
            let mtime = entry
                .metadata()
                .ok()
                .and_then(|meta| meta.modified().ok())
                .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
                .map_or(0, |since| since.as_secs());
            Some(Report {
                path: journal_path(outbox, &name)?,
                name,
                mtime,
            })
        })
        .collect();
    reports.sort_by(|a, b| b.mtime.cmp(&a.mtime).then_with(|| a.name.cmp(&b.name)));
    reports
}

/// Records an intention as `intent-<ts>.md` in the outbox, and returns it.
///
/// # Errors
///
/// [`IntentError::Empty`] for content that is only whitespace — the tab mirrors
/// the same rule, so an empty body here means a caller that skipped it.
pub fn write_intent(outbox: &Path, content: &str) -> Result<Report, IntentError> {
    if content.trim().is_empty() {
        return Err(IntentError::Empty);
    }
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| IntentError::Unavailable)?
        .as_secs();
    let name = unique_name(outbox, seconds);
    fs::write(outbox.join(&name), content).map_err(|_| IntentError::Unavailable)?;
    Ok(Report {
        path: journal_path(outbox, &name).ok_or(IntentError::Unavailable)?,
        name,
        mtime: seconds,
    })
}

/// Whether a file name is a report. Markdown only: the outbox holds other
/// things, and the tab renders only markdown.
fn is_markdown(name: &str) -> bool {
    Path::new(name)
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("md") || ext.eq_ignore_ascii_case("markdown"))
}

/// The path a report is addressed by, relative to the outbox's parent — the
/// form `/decisions/file` resolves against the journal's directory.
fn journal_path(outbox: &Path, name: &str) -> Option<String> {
    Some(format!("{}/{name}", outbox.file_name()?.to_str()?))
}

/// `intent-<seconds>.md`, or `-1`, `-2` … when two land in the same second.
fn unique_name(outbox: &Path, seconds: u64) -> String {
    let base = format!("intent-{seconds}");
    let mut candidate = format!("{base}.md");
    let mut suffix = 1;
    while outbox.join(&candidate).exists() {
        candidate = format!("{base}-{suffix}.md");
        suffix += 1;
    }
    candidate
}

#[cfg(test)]
mod tests {
    use super::{IntentError, is_markdown, list, write_intent};
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::time::{SystemTime, UNIX_EPOCH};

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ta-kiosk-reports-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("outbox")).expect("make outbox");
        dir.join("outbox")
    }

    /// Only markdown files, newest first, and a path the journal can serve.
    #[test]
    fn list_reads_markdown_newest_first() {
        let outbox = scratch("list");
        fs::write(outbox.join("old.md"), "x").expect("write");
        fs::write(outbox.join("new.md"), "y").expect("write");
        fs::write(outbox.join("notes.txt"), "z").expect("write"); // not a report
        fs::create_dir_all(outbox.join("subdir.md")).expect("a directory named like a report");

        // `old.md` gets an older mtime; `new.md` the newer.
        let old = UNIX_EPOCH + std::time::Duration::from_secs(1_000_000);
        let file = fs::File::options()
            .write(true)
            .open(outbox.join("old.md"))
            .expect("reopen");
        file.set_modified(old).expect("set mtime");

        let reports = list(&outbox);
        let names: Vec<&str> = reports.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, ["new.md", "old.md"], "markdown only, newest first");
        assert_eq!(
            reports[1].path, "outbox/old.md",
            "path is how /decisions/file addresses it"
        );
        assert!(reports.iter().all(|r| r.mtime > 0));
    }

    /// A missing outbox is an empty board, not an error.
    #[test]
    fn list_on_a_missing_outbox_is_empty() {
        assert!(list(Path::new("/nonexistent/ta-kiosk-outbox")).is_empty());
    }

    /// An intention is written as `intent-<ts>.md`, addressed like any report,
    /// and empty content is refused.
    #[test]
    fn write_intent_records_a_server_named_artifact() {
        let outbox = scratch("intent");
        let report = write_intent(&outbox, "# Intention\n\nDo the thing.\n").expect("write");
        assert!(report.name.starts_with("intent-"), "server-named: {}", report.name);
        assert!(is_markdown(&report.name), "server-named: {}", report.name);
        assert_eq!(report.path, format!("outbox/{}", report.name));
        assert_eq!(
            fs::read_to_string(outbox.join(&report.name)).expect("read back"),
            "# Intention\n\nDo the thing.\n"
        );

        // A second within the same second does not clobber the first.
        let again = write_intent(&outbox, "# Intention\n\nAgain.\n").expect("write");
        assert_ne!(again.name, report.name);

        // A lone heading is not an intention.
        assert_eq!(write_intent(&outbox, "   \n  "), Err(IntentError::Empty));
        assert!(SystemTime::now() > UNIX_EPOCH);
    }
}
