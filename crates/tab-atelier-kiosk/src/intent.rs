// SPDX-License-Identifier: MPL-2.0

//! Intentions: the third pane — an idea a worker circles in a few turns until
//! it can be planned.
//!
//! This module owns the files — where they live, how they are named, what a
//! valid slug is — and nothing else. Who talks to the worker, and how, is the
//! routes' business.
//!
//! # The file is the transcript
//!
//! The conversation between the operator and the diagnostic agent is appended
//! to the same `.md` the intention lives in, as ordinary markdown. A separate
//! log would need its own format, its own reader, and its own way of drifting
//! out of sync with the file; this way the intention is one artefact that can be
//! read, grepped, committed or handed to a planner without a decoder.
//!
//! # Two directories
//!
//! `<root>/` holds intentions being worked on; `<root>/READY-intentions/` the
//! ones a worker judged circumscribed. Promotion is a **move**, not a flag, so
//! the pool a planner reads is a directory listing and cannot disagree with a
//! field inside a file.

use std::path::{Path, PathBuf};

use serde::Serialize;

/// Subdirectory holding the intentions ready to be planned.
pub const READY_DIR: &str = "READY-intentions";

/// Where intentions live when `TAB_ATELIER_INTENTIONS` is unset.
pub const DEFAULT_INTENTIONS_DIR: &str = "intentions";

/// One intention, as much as a list needs to render it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Summary {
    /// File stem — the handle the routes use.
    pub slug: String,
    /// The title from the front matter, or the slug when it has none.
    pub title: String,
    /// Free-form repository name.
    pub repo: String,
    /// True when the file lives under [`READY_DIR`].
    pub ready: bool,
    /// The worker tab driving this intention, once one is running.
    pub tab_id: Option<String>,
}

/// The intentions on disk, under one root directory.
#[derive(Debug, Clone)]
pub struct Intentions {
    root: PathBuf,
}

impl Intentions {
    /// Wrap `root`, creating it and the ready subdirectory if needed.
    ///
    /// # Errors
    /// Fails when a directory cannot be created.
    pub fn new(root: PathBuf) -> Result<Self, String> {
        for dir in [root.clone(), root.join(READY_DIR)] {
            std::fs::create_dir_all(&dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
        }
        Ok(Self { root })
    }

    /// Every intention, the ready ones last so a list rendered in order shows
    /// the ones still moving first.
    ///
    /// # Errors
    /// Fails when a directory cannot be read. A single unreadable *file* is
    /// skipped rather than failing the list — one bad intention must not hide
    /// every other one.
    pub fn list(&self) -> Result<Vec<Summary>, String> {
        let mut out = Vec::new();
        for (dir, ready) in [(self.root.clone(), false), (self.root.join(READY_DIR), true)] {
            let entries = match std::fs::read_dir(&dir) {
                Ok(entries) => entries,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => return Err(format!("read {}: {e}", dir.display())),
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().is_none_or(|ext| ext != "md") {
                    continue;
                }
                let Some(slug) = path.file_stem().and_then(|s| s.to_str()) else {
                    continue;
                };
                let Ok(raw) = std::fs::read_to_string(&path) else {
                    continue;
                };
                let meta = FrontMatter::parse(&raw);
                out.push(Summary {
                    slug: slug.to_string(),
                    title: meta.get("title").map_or_else(|| slug.to_string(), String::clone),
                    repo: meta.get("repo").cloned().unwrap_or_default(),
                    ready,
                    tab_id: meta.get("tab").cloned(),
                });
            }
        }
        out.sort_by(|a, b| (a.ready, &a.slug).cmp(&(b.ready, &b.slug)));
        Ok(out)
    }

    /// The file for `slug`, searched in both directories, and whether it is
    /// ready.
    ///
    /// # Errors
    /// Fails when the slug is not a slug — see [`is_slug`] — or the file does
    /// not exist.
    pub fn path_of(&self, slug: &str) -> Result<(PathBuf, bool), String> {
        if !is_slug(slug) {
            return Err(format!("{slug:?} is not an intention slug"));
        }
        let ready = self.root.join(READY_DIR).join(format!("{slug}.md"));
        if ready.is_file() {
            return Ok((ready, true));
        }
        let wip = self.root.join(format!("{slug}.md"));
        if wip.is_file() {
            return Ok((wip, false));
        }
        Err(format!("no intention named {slug:?}"))
    }

    /// The whole file — front matter and conversation alike.
    ///
    /// # Errors
    /// Fails when the slug is unknown or the file is unreadable.
    pub fn read(&self, slug: &str) -> Result<String, String> {
        let (path, _) = self.path_of(slug)?;
        std::fs::read_to_string(&path).map_err(|e| format!("read {}: {e}", path.display()))
    }

    /// Write a new intention and return its summary.
    ///
    /// The slug is derived from the title, which is predictable to a human but
    /// means two titles can collide. A collision is **not** silently
    /// overwritten — the caller gets `-2`, `-3` and so on — because an intention
    /// carries a conversation, and losing one to a same-named neighbour would be
    /// silent data loss.
    ///
    /// # Errors
    /// Fails when the title yields no usable slug, or the file cannot be written.
    pub fn create(&self, title: &str, repo: &str, pitch: &str) -> Result<Summary, String> {
        let base = slugify(title);
        if base.is_empty() {
            return Err("a title needs at least one letter or digit to name a file".to_string());
        }
        let slug = self.free_slug(&base);
        let path = self.root.join(format!("{slug}.md"));
        let doc = format!(
            "---\ntitle: {title}\nrepo: {repo}\ncreated: {created}\n---\n\n{pitch}\n",
            created = now_iso8601(),
        );
        write_atomic(&path, &doc)?;
        Ok(Summary {
            slug,
            title: title.to_string(),
            repo: repo.to_string(),
            ready: false,
            tab_id: None,
        })
    }

    /// Remember which worker tab drives this intention.
    ///
    /// # Errors
    /// Fails when the slug is unknown or the file cannot be rewritten.
    pub fn set_tab(&self, slug: &str, tab_id: &str) -> Result<(), String> {
        self.edit_front_matter(slug, "tab", tab_id)
    }

    /// Move the intention into [`READY_DIR`].
    ///
    /// A no-op when it is already there, so a worker that promotes twice — or
    /// an operator who clicks twice — does not error on the second.
    ///
    /// # Errors
    /// Fails when the slug is unknown or the move fails.
    pub fn promote(&self, slug: &str) -> Result<(), String> {
        let (path, ready) = self.path_of(slug)?;
        if ready {
            return Ok(());
        }
        let dest = self.root.join(READY_DIR).join(format!("{slug}.md"));
        std::fs::rename(&path, &dest).map_err(|e| format!("move {} -> {}: {e}", path.display(), dest.display()))
    }

    /// Append one turn to the conversation and return the file's new contents.
    ///
    /// `who` is rendered as a heading, so the transcript stays readable as
    /// markdown and greppable as text.
    ///
    /// # Errors
    /// Fails when the slug is unknown or the file cannot be rewritten.
    pub fn append_turn(&self, slug: &str, who: &str, text: &str) -> Result<String, String> {
        use std::fmt::Write as _;
        let (path, _) = self.path_of(slug)?;
        let mut raw = std::fs::read_to_string(&path).map_err(|e| format!("read {}: {e}", path.display()))?;
        if !raw.ends_with('\n') {
            raw.push('\n');
        }
        let _ = write!(raw, "\n### {who}\n\n{}\n", text.trim());
        write_atomic(&path, &raw)?;
        Ok(raw)
    }

    /// Set `key: value` in the front matter, adding the key if absent.
    fn edit_front_matter(&self, slug: &str, key: &str, value: &str) -> Result<(), String> {
        let (path, _) = self.path_of(slug)?;
        let raw = std::fs::read_to_string(&path).map_err(|e| format!("read {}: {e}", path.display()))?;
        let updated = FrontMatter::set(&raw, key, value);
        write_atomic(&path, &updated)
    }

    /// `base`, or `base-2`, `base-3`… until one is free in both directories.
    fn free_slug(&self, base: &str) -> String {
        let taken = |slug: &str| {
            self.root.join(format!("{slug}.md")).exists()
                || self.root.join(READY_DIR).join(format!("{slug}.md")).exists()
        };
        if !taken(base) {
            return base.to_string();
        }
        // Starts at 2 because `base` is `base-1` in spirit; the loop exists for
        // the second collision, not the first.
        for n in 2..10_000 {
            let candidate = format!("{base}-{n}");
            if !taken(&candidate) {
                return candidate;
            }
        }
        base.to_string()
    }
}

/// Whether `s` is a slug this module produced, and safe to join to the root.
///
/// Checked on **read** as well as on create, because the slug arrives from the
/// URL: `../../etc/passwd` and `foo/bar` both have to be refused before they
/// reach a `join`. The set is deliberately the one [`slugify`] can produce —
/// lowercase ASCII, digits, single dashes — so anything a caller invents is
/// rejected rather than normalised.
#[must_use]
pub fn is_slug(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 100
        && !s.starts_with('-')
        && !s.ends_with('-')
        && !s.contains("--")
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// Turn a human title into a slug: lowercase ASCII, digits, single dashes.
///
/// Accents fold to their closest ASCII letter where there is one, so a French
/// title gives a readable filename rather than an empty one. Anything else —
/// punctuation, an emoji, a slash — becomes a dash, which is also what keeps a
/// title from walking out of the directory.
#[must_use]
pub fn slugify(title: &str) -> String {
    let mut out = String::with_capacity(title.len());
    let mut last_dash = false;
    for ch in title.chars() {
        for c in fold_accent(ch).chars() {
            if c.is_ascii_alphanumeric() {
                out.push(c.to_ascii_lowercase());
                last_dash = false;
            } else if !last_dash && !out.is_empty() {
                out.push('-');
                last_dash = true;
            }
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    out.truncate(80);
    while out.ends_with('-') {
        out.pop();
    }
    out
}

/// The ASCII letter an accented one stands for, when there is one.
fn fold_accent(ch: char) -> String {
    match ch {
        'à' | 'á' | 'â' | 'ã' | 'ä' | 'å' | 'À' | 'Á' | 'Â' | 'Ã' | 'Ä' | 'Å' => "a".into(),
        'ç' | 'Ç' => "c".into(),
        'è' | 'é' | 'ê' | 'ë' | 'È' | 'É' | 'Ê' | 'Ë' => "e".into(),
        'ì' | 'í' | 'î' | 'ï' | 'Ì' | 'Í' | 'Î' | 'Ï' => "i".into(),
        'ñ' | 'Ñ' => "n".into(),
        'ò' | 'ó' | 'ô' | 'õ' | 'ö' | 'Ò' | 'Ó' | 'Ô' | 'Õ' | 'Ö' => "o".into(),
        'ù' | 'ú' | 'û' | 'ü' | 'Ù' | 'Ú' | 'Û' | 'Ü' => "u".into(),
        'ý' | 'ÿ' | 'Ý' => "y".into(),
        'œ' | 'Œ' => "oe".into(),
        'æ' | 'Æ' => "ae".into(),
        _ => ch.to_string(),
    }
}

/// A UTC timestamp, `YYYY-MM-DDTHH:MM:SSZ`.
///
/// Hand-rolled rather than pulling a date crate for one line of front matter:
/// the value is read by a human or ignored entirely, and a clock wrong by a
/// second is not a correctness problem here.
fn now_iso8601() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let (days, rem) = (secs / 86_400, secs % 86_400);
    let (hour, minute, second) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    // Civil-from-days (Howard Hinnant's algorithm): no leap-second table, no
    // dependency, correct for every date this program will ever see.
    let z = i64::try_from(days).unwrap_or(i64::MAX) + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day_of_month = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { y + 1 } else { y };
    format!("{year:04}-{month:02}-{day_of_month:02}T{hour:02}:{minute:02}:{second:02}Z")
}

/// Write `contents` to `path` through a temporary file, then rename.
///
/// A worker reads these files while the operator edits them, and a torn write
/// shows as a truncated intention. `rename` is atomic within a directory, so a
/// reader sees the old file or the new one, never half of either.
fn write_atomic(path: &Path, contents: &str) -> Result<(), String> {
    let tmp = path.with_extension("md.tmp");
    std::fs::write(&tmp, contents).map_err(|e| format!("write {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!("rename {} -> {}: {e}", tmp.display(), path.display())
    })
}

/// The `key: value` block at the top of an intention, between `---` fences.
///
/// A deliberately small reader rather than a YAML dependency: the block holds
/// short single-line strings this crate writes itself, and a full parser would
/// accept documents this one should refuse.
struct FrontMatter;

impl FrontMatter {
    /// The bare key/value pairs, or none when there is no front matter.
    fn parse(raw: &str) -> std::collections::BTreeMap<String, String> {
        let mut out = std::collections::BTreeMap::new();
        let mut lines = raw.lines();
        if lines.next().map(str::trim_end) != Some("---") {
            return out;
        }
        for line in lines {
            if line.trim_end() == "---" {
                break;
            }
            if let Some((key, value)) = line.split_once(':') {
                out.insert(key.trim().to_string(), value.trim().to_string());
            }
        }
        out
    }

    /// `raw` with `key` set to `value`, inserted before the closing fence (or
    /// after the opening one when the key is new).
    fn set(raw: &str, key: &str, value: &str) -> String {
        let mut lines: Vec<String> = raw.lines().map(str::to_string).collect();
        if lines.first().map(|l| l.trim_end()) != Some("---") {
            // No front matter at all: the file should not exist in that shape,
            // but adding the block beats dropping the value.
            return format!("---\n{key}: {value}\n---\n\n{raw}");
        }
        // Replace in place when the key is already there — a second `tab:` line
        // would make the file's meaning depend on which one a reader picked.
        let close = lines
            .iter()
            .skip(1)
            .position(|l| l.trim_end() == "---")
            .map_or(lines.len(), |i| i + 1);
        let existing = lines[1..close]
            .iter()
            .position(|l| l.split_once(':').is_some_and(|(k, _)| k.trim() == key));
        let rendered = format!("{key}: {value}");
        match existing {
            Some(offset) => lines[offset + 1] = rendered,
            None => lines.insert(close, rendered),
        }
        let mut out = lines.join("\n");
        if raw.ends_with('\n') {
            out.push('\n');
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::{Intentions, is_slug, slugify};
    use std::path::PathBuf;

    /// A store under a fresh temp directory.
    fn store(name: &str) -> (PathBuf, Intentions) {
        let dir = std::env::temp_dir().join(format!("ta-kiosk-intent-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let intentions = Intentions::new(dir.join("intentions")).expect("new");
        (dir, intentions)
    }

    /// A title becomes a filename. Accents fold rather than vanish, because a
    /// French title slugging to nothing would leave an intention unnamed.
    #[test]
    fn a_title_becomes_a_readable_slug() {
        assert_eq!(slugify("Reprise des devis"), "reprise-des-devis");
        assert_eq!(slugify("Décision : congés d'été"), "decision-conges-d-ete");
        assert_eq!(slugify("  Espaces   multiples  "), "espaces-multiples");
        assert_eq!(slugify("///"), "", "no letters, no slug");
    }

    /// A slug is the shape this module produces and nothing else — which is
    /// also what stops a URL from walking out of the directory.
    #[test]
    fn a_slug_is_the_shape_we_make() {
        assert!(is_slug("conges-ete"));
        assert!(is_slug("a1"));
        for bad in ["", "-x", "x-", "a--b", "A", "a b", "a/b", "a.b", "../etc"] {
            assert!(!is_slug(bad), "{bad:?} must not be a slug");
        }
    }

    /// Create writes a file, lists it, and a colliding title does not clobber.
    #[test]
    fn create_lists_and_never_overwrites() {
        let (_dir, store) = store("create");
        let first_created = store.create("Reprise des devis", "app", "why").expect("create");
        assert_eq!(first_created.slug, "reprise-des-devis");
        assert!(!first_created.ready);

        let first = store.read("reprise-des-devis").expect("read");
        assert!(first.contains("why"), "the pitch is written");

        let second = store.create("Reprise des devis", "app", "again").expect("create");
        assert_eq!(second.slug, "reprise-des-devis-2", "a collision gets a suffix");

        let listed = store.list().expect("list");
        assert_eq!(listed.len(), 2);
        let original = store.read("reprise-des-devis").expect("read");
        assert!(!original.contains("again"), "the first file is not clobbered");
    }

    /// Promote moves the file into the ready directory and is idempotent.
    #[test]
    fn promote_moves_and_is_idempotent() {
        let (_dir, store) = store("promote");
        store.create("One", "app", "x").expect("create");
        store.promote("one").expect("promote");
        assert!(store.list().expect("list")[0].ready, "it is ready now");
        store.promote("one").expect("promote twice is a no-op");
        assert_eq!(store.list().expect("list").len(), 1);
    }

    /// The tab id and the conversation are written to the same file, and only
    /// the file.
    #[test]
    fn turns_and_tab_land_in_the_file() {
        let (_dir, store) = store("turns");
        store.create("One", "app", "pitch").expect("create");
        store.set_tab("one", "tab-7").expect("set_tab");
        store.append_turn("one", "PO", "  what about the API?  ").expect("turn");

        let listed = store.list().expect("list");
        assert_eq!(listed[0].tab_id.as_deref(), Some("tab-7"));
        let body = store.read("one").expect("read");
        assert!(body.contains("### PO"), "the turn is a heading");
        assert!(body.contains("what about the API?"), "trimmed and kept");
    }

    /// An unknown slug is refused, and so is one that is not a slug.
    #[test]
    fn unknown_and_unsafe_slugs_are_refused() {
        let (_dir, store) = store("refuse");
        assert!(store.read("nope").is_err());
        assert!(store.read("../etc/passwd").is_err(), "not a slug");
        assert!(store.path_of("UPPER").is_err());
    }
}
