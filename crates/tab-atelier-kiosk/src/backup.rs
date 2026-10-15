// SPDX-License-Identifier: MPL-2.0

//! Backing up and restoring everything the Kiosk owns.
//!
//! The package never deletes the state directory — not even on purge — but a
//! machine move, a reinstall on new hardware, or a mistake at the terminal
//! still loses data unless it can be taken out and put back. This is that:
//! a copy of the whole state directory, and a copy back.
//!
//! A plain recursive copy, not an archive, and deliberately: the crate carries
//! no compression or tar dependency, and a directory of readable files is a
//! backup an operator can inspect, diff, and salvage by hand when the tool
//! itself is the thing that broke.

use std::fs;
use std::path::{Path, PathBuf};

/// Why a backup or restore could not be done.
#[derive(Debug, PartialEq, Eq)]
pub enum BackupError {
    /// A path is wrong — missing, unreadable, or a dangerous arrangement such
    /// as a backup into the directory being backed up.
    BadPath(String),
    /// The destination already holds data and `force` was not given.
    NotEmpty(String),
    /// A read or write failed partway.
    Io(String),
}

/// Copy every file under `from` into `to`, recreating the directory tree.
///
/// Returns how many files were copied. Missing `from` is zero files, not an
/// error — an empty state directory is a valid thing to back up.
///
/// # Errors
///
/// [`BackupError::Io`] when a directory cannot be created or a file copied.
fn copy_tree(from: &Path, to: &Path) -> Result<usize, BackupError> {
    if !from.is_dir() {
        return Ok(0);
    }
    let mut copied = 0;
    let entries = fs::read_dir(from).map_err(|e| BackupError::Io(format!("read {}: {e}", from.display())))?;
    for entry in entries.flatten() {
        let source = entry.path();
        let name = entry.file_name();
        let dest = to.join(&name);
        if source.is_dir() {
            fs::create_dir_all(&dest).map_err(|e| BackupError::Io(format!("create {}: {e}", dest.display())))?;
            copied += copy_tree(&source, &dest)?;
        } else {
            fs::copy(&source, &dest)
                .map_err(|e| BackupError::Io(format!("copy {} -> {}: {e}", source.display(), dest.display())))?;
            copied += 1;
        }
    }
    Ok(copied)
}

/// Whether a directory holds anything at all.
fn is_empty_dir(dir: &Path) -> bool {
    // Unreadable counts as nothing to protect: a restore into a directory we
    // cannot list should try, and fail on the write if the permission really is
    // missing, rather than refuse with a wrong reason.
    fs::read_dir(dir).map_or(true, |mut entries| entries.next().is_none())
}

/// Write a backup of `state` into `dest`.
///
/// `dest` is created if missing; it may hold an earlier backup (this
/// overwrites file-by-file, which is what a re-run should do). It must not be
/// `state` or inside it — a backup nested in what it backs up would recurse
/// into itself and never finish.
///
/// # Errors
///
/// [`BackupError::BadPath`] for a nested destination; [`BackupError::Io`] for a
/// failed read or write.
pub fn backup(state: &Path, dest: &Path) -> Result<usize, BackupError> {
    if dest == state || dest.starts_with(state) {
        return Err(BackupError::BadPath(format!(
            "the backup destination {} is inside the directory being backed up",
            dest.display()
        )));
    }
    fs::create_dir_all(dest).map_err(|e| BackupError::Io(format!("create {}: {e}", dest.display())))?;
    copy_tree(state, dest)
}

/// Restore a backup from `src` into `state`.
///
/// Refuses a non-empty `state` without `force`, because restoring over live data
/// is how a backup becomes the second accident. With `force`, files are copied
/// over in place — anything the backup does not carry is left alone.
///
/// # Errors
///
/// [`BackupError::NotEmpty`] when `state` holds data and `force` is not set;
/// [`BackupError::Io`] for a failed read or write.
pub fn restore(state: &Path, src: &Path, force: bool) -> Result<usize, BackupError> {
    if !src.is_dir() {
        return Err(BackupError::BadPath(format!("no backup at {}", src.display())));
    }
    if state.is_dir() && !is_empty_dir(state) && !force {
        return Err(BackupError::NotEmpty(format!(
            "{} already holds data — pass --force to restore over it",
            state.display()
        )));
    }
    fs::create_dir_all(state).map_err(|e| BackupError::Io(format!("create {}: {e}", state.display())))?;
    copy_tree(src, state)
}

/// The state directory this process uses, read from the environment the same
/// way [`crate::Config`] reads it.
#[must_use]
pub fn state_dir() -> PathBuf {
    std::env::var("TAB_ATELIER_KIOSK_STATE").map_or_else(|_| PathBuf::from(crate::DEFAULT_STATE_DIR), PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::{BackupError, backup, copy_tree, restore};
    use std::fs;
    use std::path::PathBuf;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ta-kiosk-backup-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("make scratch");
        dir
    }

    /// A backup captures every file, and a restore puts them all back — the
    /// whole point: a state directory can be taken out and returned intact.
    #[test]
    fn backup_then_restore_round_trips_every_file() {
        let root = scratch("round");
        let state = root.join("state");
        fs::create_dir_all(state.join("outbox")).expect("state");
        fs::write(state.join("decisions.jsonl"), "{\"id\":\"a\"}\n").expect("write");
        fs::write(state.join("token"), "secret\n").expect("write");
        fs::write(state.join("outbox/a1.md"), "# note\n").expect("write");

        let dest = root.join("backup");
        assert_eq!(backup(&state, &dest).expect("backup"), 3);

        // Wipe the original, as a reinstall would.
        fs::remove_dir_all(&state).expect("wipe");
        assert_eq!(restore(&state, &dest, false).expect("restore"), 3);

        assert_eq!(fs::read_to_string(state.join("token")).expect("token"), "secret\n");
        assert_eq!(
            fs::read_to_string(state.join("outbox/a1.md")).expect("note"),
            "# note\n"
        );
        assert!(state.join("decisions.jsonl").is_file());
    }

    /// A backup into its own directory is refused — it would recurse forever.
    #[test]
    fn a_nested_backup_is_refused() {
        let root = scratch("nested");
        let err = backup(&root, &root.join("inside")).unwrap_err();
        assert!(matches!(err, BackupError::BadPath(_)), "{err:?}");
    }

    /// Restoring over live data needs `--force`, and taking a backup of an
    /// empty directory is fine.
    #[test]
    fn restore_is_guarded_and_empty_state_backs_up() {
        let root = scratch("guard");
        let state = root.join("state");
        let dest = root.join("backup");
        fs::create_dir_all(&state).expect("state");
        fs::write(state.join("keep.md"), "live\n").expect("write");
        assert_eq!(backup(&state, &dest).expect("backup empty dest"), 1);

        // The backup now exists; restore without force sees live data and refuses.
        assert!(matches!(
            restore(&state, &dest, false).unwrap_err(),
            BackupError::NotEmpty(_)
        ));
        // With force it goes through.
        assert_eq!(restore(&state, &dest, true).expect("forced restore"), 1);

        // A missing source, and an empty source, are handled.
        assert!(matches!(
            restore(&state, &root.join("nope"), true),
            Err(BackupError::BadPath(_))
        ));
        let empty = root.join("empty");
        fs::create_dir_all(&empty).expect("empty");
        assert_eq!(copy_tree(&empty, &dest).expect("copy empty"), 0);
    }
}
