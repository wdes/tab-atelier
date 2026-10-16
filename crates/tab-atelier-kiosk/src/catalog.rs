// SPDX-License-Identifier: MPL-2.0

//! The skills catalogue — the Kiosk's own store of reusable agent profiles.
//!
//! A *skill* is a distilled agent: a prompt, a specialty, some conventions, and
//! a running prompt version. The daemon does not own this (it is the operator's
//! view of what their agents have become), so the Kiosk keeps it — one JSON
//! object per skill, the file rewritten on each edit. The append-only JSONL the
//! dashboard branch used earns its keep only with a retire loop that appends
//! data-points; with authoring by hand, a rewritten file is the honest model.
//!
//! The wire shape is exactly what the catalogue UI reads
//! (`assets/kiosk.js`: `catalogView`, `catalogGroups`, `skillProfileModel`):
//! `{skill, name, prompt, specialty, conventions, tools, patterns,
//! promptVersion, usageCount, deleted}` — the metrics and the fresh-vs-resume
//! verdict are **derived elsewhere** and omitted here, so the UI renders its own
//! "no data yet" state rather than a zero the store invented.

use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use tab_atelier_api::schemars::{self, JsonSchema};

use serde::{Deserialize, Serialize};

/// One skill, as the catalogue holds it and as the page reads it.
///
/// `skill` is the fold key and the display name; every other field is optional.
/// `deleted` is the tombstone — emitted only when true, so the normal shape is
/// stable and the page's `deleted === true` check is exact.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Skill {
    /// The fold key — also the display name. Required.
    pub skill: String,
    /// A distilled name, when one exists. Its presence marks the card read-only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub specialty: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub conventions: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub patterns: Vec<String>,
    /// A monotonic number, the optimistic-concurrency token: an edit carries the
    /// version it read, and a mismatch is a conflict.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_version: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage_count: Option<u64>,
    /// The tombstone. `true` hides the skill from `list` unless asked for.
    #[serde(default, skip_serializing_if = "is_false")]
    pub deleted: bool,
}

// `&bool` because that is serde's `skip_serializing_if` signature.
#[allow(clippy::trivially_copy_pass_by_ref)]
const fn is_false(value: &bool) -> bool {
    !*value
}

/// What a `POST /catalog/<skill>/edit` body carries.
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct EditRequest {
    #[serde(default)]
    pub prompt: String,
    #[serde(default)]
    pub specialty: Option<String>,
    #[serde(default)]
    pub conventions: Option<Vec<String>>,
    /// The version the client read; a mismatch is a conflict.
    #[serde(default)]
    pub prompt_version: Option<u32>,
}

/// Why a catalogue operation could not be done.
#[derive(Debug, PartialEq, Eq)]
pub enum CatalogError {
    /// No skill carries that name.
    NotFound,
    /// The `promptVersion` the edit carried is stale — someone else wrote first.
    Conflict,
    /// An edit with no prompt: the prompt is the one thing a skill must have.
    EmptyProfile,
    /// The file cannot be read or written.
    Unavailable,
}

/// The catalogue on disk.
#[derive(Debug, Clone)]
pub struct Catalog {
    /// The JSON Lines file: one skill per line.
    pub path: PathBuf,
}

impl Catalog {
    /// Every skill, tombstones excluded unless `include_deleted`.
    ///
    /// A missing file is an empty catalogue, not an error — a fresh Kiosk has no
    /// skills yet.
    ///
    /// # Errors
    ///
    /// [`CatalogError::Unavailable`] when the file exists but cannot be read or
    /// holds a line that is not a skill.
    pub fn list(&self, include_deleted: bool) -> Result<Vec<Skill>, CatalogError> {
        let mut skills = self.read()?;
        if !include_deleted {
            skills.retain(|skill| !skill.deleted);
        }
        skills.sort_by(|a, b| a.skill.cmp(&b.skill));
        Ok(skills)
    }

    /// Write a skill's profile, creating it if it is new (this is how a skill is
    /// **added**), or replacing it if it exists.
    ///
    /// # Errors
    ///
    /// [`CatalogError::EmptyProfile`] when the prompt is blank;
    /// [`CatalogError::Conflict`] when a `promptVersion` is given and does not
    /// match the stored one.
    pub fn edit(&self, name: &str, body: &EditRequest) -> Result<Skill, CatalogError> {
        if body.prompt.trim().is_empty() {
            return Err(CatalogError::EmptyProfile);
        }
        let mut skills = self.read()?;
        let existing = skills.iter().position(|skill| skill.skill == name);
        // The version check is on the STORED entry: an edit that names a version
        // must be naming the one it read.
        if let Some(index) = existing
            && let Some(client_version) = body.prompt_version
            && skills[index].prompt_version.unwrap_or(0) != client_version
        {
            return Err(CatalogError::Conflict);
        }
        let next_version = existing.map_or(0, |index| skills[index].prompt_version.unwrap_or(0)) + 1;
        let skill = Skill {
            skill: name.to_string(),
            name: None,
            prompt: Some(body.prompt.clone()),
            specialty: body.specialty.clone(),
            conventions: body.conventions.clone().unwrap_or_default(),
            tools: Vec::new(),
            patterns: Vec::new(),
            prompt_version: Some(next_version),
            usage_count: None,
            deleted: false,
        };
        match existing {
            Some(index) => skills[index] = skill.clone(),
            None => skills.push(skill.clone()),
        }
        self.write(&skills)?;
        Ok(skill)
    }

    /// Tombstone (`deleted = true`) or restore (`false`) a skill.
    ///
    /// # Errors
    ///
    /// [`CatalogError::NotFound`] when no skill carries the name.
    pub fn set_deleted(&self, name: &str, deleted: bool) -> Result<Skill, CatalogError> {
        let mut skills = self.read()?;
        let index = skills
            .iter()
            .position(|skill| skill.skill == name)
            .ok_or(CatalogError::NotFound)?;
        skills[index].deleted = deleted;
        let updated = skills[index].clone();
        self.write(&skills)?;
        Ok(updated)
    }

    /// Read and parse every line, in file order.
    fn read(&self) -> Result<Vec<Skill>, CatalogError> {
        let raw = match fs::read_to_string(&self.path) {
            Ok(raw) => raw,
            Err(err) if err.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
            Err(_) => return Err(CatalogError::Unavailable),
        };
        let mut skills = Vec::new();
        for line in raw.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            skills.push(serde_json::from_str(line).map_err(|_| CatalogError::Unavailable)?);
        }
        Ok(skills)
    }

    /// Write every skill, one per line, `0600` and atomically.
    fn write(&self, skills: &[Skill]) -> Result<(), CatalogError> {
        if let Some(dir) = self.path.parent() {
            fs::create_dir_all(dir).map_err(|_| CatalogError::Unavailable)?;
        }
        let mut out = String::new();
        for skill in skills {
            out.push_str(&serde_json::to_string(skill).map_err(|_| CatalogError::Unavailable)?);
            out.push('\n');
        }
        write_atomic(&self.path, &out).map_err(|_| CatalogError::Unavailable)?;
        crate::config::set_owner_only(&self.path).map_err(|_| CatalogError::Unavailable)
    }
}

/// Write `contents` to `path` through a sibling temporary file, then rename.
///
/// A reader sees the old file or the new one, never a half-written one. The
/// temp name is the real file name plus `.tmp` — not `with_extension`, which
/// would turn `catalog.jsonl` into `catalog.tmp` and lose which file it is.
fn write_atomic(path: &Path, contents: &str) -> std::io::Result<()> {
    let name = path.file_name().map_or_else(
        || "catalog.tmp".into(),
        |name| {
            let mut name = name.to_os_string();
            name.push(".tmp");
            name
        },
    );
    let tmp = path.with_file_name(name);
    fs::write(&tmp, contents)?;
    fs::rename(&tmp, path).inspect_err(|_| {
        let _ = fs::remove_file(&tmp);
    })
}

#[cfg(test)]
mod tests {
    use super::{Catalog, CatalogError, EditRequest};
    use std::fs;
    use std::path::PathBuf;

    fn scratch(name: &str) -> (Catalog, PathBuf) {
        let dir = std::env::temp_dir().join(format!("ta-kiosk-catalog-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("make dir");
        (
            Catalog {
                path: dir.join("catalog.jsonl"),
            },
            dir,
        )
    }

    fn body(prompt: &str) -> EditRequest {
        EditRequest {
            prompt: prompt.to_string(),
            ..EditRequest::default()
        }
    }

    /// A missing file is an empty catalogue, not an error.
    #[test]
    fn a_missing_file_is_an_empty_catalogue() {
        let (catalog, _dir) = scratch("missing");
        assert!(catalog.list(false).expect("list").is_empty());
    }

    /// Editing adds a skill; editing again replaces it and bumps the version,
    /// leaving other skills alone.
    #[test]
    fn edit_adds_then_replaces() {
        let (catalog, _dir) = scratch("edit");
        let first = catalog.edit("review", &body("do a review")).expect("add");
        assert_eq!(first.skill, "review");
        assert_eq!(first.prompt_version, Some(1));
        assert!(!first.deleted);

        catalog.edit("audit", &body("do an audit")).expect("add a second");
        let updated = catalog.edit("review", &body("do a better review")).expect("replace");
        assert_eq!(updated.prompt_version, Some(2));

        let listed = catalog.list(false).expect("list");
        assert_eq!(listed.len(), 2, "two skills, not three");
        assert_eq!(
            listed
                .iter()
                .find(|s| s.skill == "review")
                .and_then(|s| s.prompt.clone())
                .as_deref(),
            Some("do a better review")
        );
        assert_eq!(
            listed
                .iter()
                .find(|s| s.skill == "audit")
                .and_then(|s| s.prompt.clone())
                .as_deref(),
            Some("do an audit"),
            "the other skill is untouched"
        );
    }

    /// A blank prompt is refused: a skill with no prompt is not a skill.
    #[test]
    fn a_blank_prompt_is_refused() {
        let (catalog, _dir) = scratch("blank");
        assert_eq!(catalog.edit("x", &body("   ")), Err(CatalogError::EmptyProfile));
        assert!(catalog.list(false).expect("list").is_empty(), "nothing was written");
    }

    /// A stale `promptVersion` is a conflict, so two editors cannot silently
    /// overwrite each other.
    #[test]
    fn a_stale_prompt_version_conflicts() {
        let (catalog, _dir) = scratch("conflict");
        catalog.edit("review", &body("v1")).expect("add");
        // The current version is 1; an edit that claims 0 (or 2) is stale.
        let stale = EditRequest {
            prompt_version: Some(0),
            ..body("v2")
        };
        assert_eq!(catalog.edit("review", &stale), Err(CatalogError::Conflict));
        // The version it actually read (1) is accepted.
        let fresh = EditRequest {
            prompt_version: Some(1),
            ..body("v2")
        };
        assert_eq!(catalog.edit("review", &fresh).expect("edit").prompt_version, Some(2));
    }

    /// The tombstone hides a skill unless asked for, and restore brings it back.
    #[test]
    fn delete_hides_and_restore_returns() {
        let (catalog, _dir) = scratch("delete");
        catalog.edit("review", &body("x")).expect("add");
        assert!(catalog.set_deleted("review", true).expect("delete").deleted);
        assert!(catalog.list(false).expect("list").is_empty(), "hidden by default");
        assert_eq!(catalog.list(true).expect("all").len(), 1, "shown when asked");
        catalog.set_deleted("review", false).expect("restore");
        assert_eq!(catalog.list(false).expect("list").len(), 1, "back");

        assert_eq!(catalog.set_deleted("nope", true), Err(CatalogError::NotFound));
    }

    /// The serialized shape is the one the page reads — camelCase, `deleted`
    /// only when true, and the metrics omitted (the page renders its own).
    #[test]
    fn the_serialized_shape_is_what_the_page_reads() {
        let (catalog, _dir) = scratch("shape");
        let request = EditRequest {
            prompt: "p".to_string(),
            specialty: Some("reviewer".to_string()),
            conventions: Some(vec!["a.md".to_string()]),
            prompt_version: None,
        };
        let skill = catalog.edit("review", &request).expect("add");
        let json = serde_json::to_string(&skill).expect("serialize");
        assert!(json.contains("\"promptVersion\":1"), "camelCase: {json}");
        assert!(json.contains("\"specialty\":\"reviewer\""), "{json}");
        assert!(json.contains("\"conventions\":[\"a.md\"]"), "{json}");
        assert!(!json.contains("deleted"), "not deleted ⇒ the key is absent: {json}");
        assert!(!json.contains("metrics"), "the store does not invent metrics: {json}");
    }

    /// The file is owner-only.
    #[cfg(unix)]
    #[test]
    fn the_store_is_owner_only() {
        use std::os::unix::fs::PermissionsExt as _;
        let (catalog, _dir) = scratch("mode");
        catalog.edit("review", &body("x")).expect("add");
        let mode = fs::metadata(&catalog.path).expect("stat").permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "the catalogue is not world-readable");
    }
}
