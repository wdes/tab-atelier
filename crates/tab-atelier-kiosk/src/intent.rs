// SPDX-License-Identifier: MPL-2.0

//! Intentions: the third Kiosk pane.
//!
//! An intention is a markdown file a worker circles in a few turns until it can
//! be planned. This module owns the files — where they live, how they are named,
//! what a valid slug is — and nothing else. Who talks to the worker, and how,
//! belongs to the routes.
//!
//! # Why the file is the transcript
//!
//! The conversation between the PO and the diagnostic agent is appended to the
//! same `.md` the intention lives in, as ordinary markdown. A separate log
//! would need its own format, its own reader and its own way of going out of
//! sync with the file; this way the intention is one artifact that can be read,
//! grepped, committed or handed to a planner without a decoder.
//!
//! # Two directories
//!
//! `<root>/` holds intentions being worked on, `<root>/READY-intentions/` the
//! ones a worker judged circumscribed. Promotion is a **move** rather than a
//! flag, so the pool a planner reads is a directory listing and cannot disagree
//! with a field inside a file.

use std::path::{Path, PathBuf};

/// Subdirectory holding the intentions that are ready to be planned.
pub const READY_DIR: &str = "READY-intentions";

/// Where intentions live when `--intentions-dir` is unset.
pub const DEFAULT_INTENTIONS_DIR: &str = "Dev/intentions";

/// One intention, as much as the list needs to render it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Summary {
    /// File stem — the handle routes use.
    pub slug: String,
    pub title: String,
    pub repo: String,
    /// True when the file lives under [`READY_DIR`].
    pub ready: bool,
    /// The worker tab driving this intention, once one is running.
    pub tab_id: Option<String>,
}

/// Default location, `$HOME`-relative so a test can point `HOME` elsewhere.
///
/// # Errors
/// Fails when `$HOME` is unset — the only case where there is no sensible
/// default, and guessing a directory would write files somewhere surprising.
pub fn default_dir() -> Result<PathBuf, String> {
    let home = std::env::var("HOME").map_err(|_| "HOME is unset; pass --intentions-dir".to_string())?;
    Ok(Path::new(&home).join(DEFAULT_INTENTIONS_DIR))
}

/// The intentions on disk, under one root directory.
#[derive(Debug, Clone)]
pub struct Intentions {
    root: PathBuf,
}

impl Intentions {
    /// Wrap `root`, creating it if needed.
    ///
    /// # Errors
    /// Fails when the directory cannot be created.
    pub fn new(root: PathBuf) -> Result<Self, String> {
        for dir in [root.clone(), root.join(READY_DIR)] {
            std::fs::create_dir_all(&dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
        }
        Ok(Self { root })
    }

    /// The directory being managed.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Every intention, those ready to plan last so a list rendered in order
    /// shows the ones still moving first.
    ///
    /// # Errors
    /// Fails when a directory cannot be read. A single unreadable file is
    /// skipped rather than failing the list: one bad intention must not hide
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

    /// The file for `slug`, searched in both directories.
    ///
    /// # Errors
    /// Fails when the slug is not a slug — see [`is_slug`] — or the file is
    /// unreadable.
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

    /// The whole file, front matter and conversation alike.
    ///
    /// # Errors
    /// Fails when the slug is unknown or the file is unreadable.
    pub fn read(&self, slug: &str) -> Result<String, String> {
        let (path, _) = self.path_of(slug)?;
        std::fs::read_to_string(&path).map_err(|e| format!("read {}: {e}", path.display()))
    }

    /// Write a new intention and return its summary.
    ///
    /// The slug is derived from the title, which makes it predictable to a
    /// human but means two titles can collide. A collision is **not** silently
    /// overwritten — the caller gets `-2`, `-3` and so on — because an intention
    /// carries a conversation and losing one to a same-named neighbour would be
    /// silent data loss.
    ///
    /// # Errors
    /// Fails when the title yields no usable slug, or the file cannot be
    /// written.
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
    /// A no-op when it is already there, so a worker that promotes twice — or a
    /// PO who clicks twice — does not error on the second.
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
        // Starts at 2 because `base` itself is `base-1` in spirit; the loop
        // exists for the second collision, not the first.
        for n in 2..10_000 {
            let candidate = format!("{base}-{n}");
            if !taken(&candidate) {
                return candidate;
            }
        }
        base.to_string()
    }
}

/// Is `s` a slug this module produced, and safe to join to the root?
///
/// Checked on **read** as well as on create, because the slug arrives from the
/// URL: `../../etc/passwd` and `foo/bar` both have to be refused before they
/// reach a `join`. The set is deliberately the one [`slugify`] can produce —
/// lowercase ASCII, digits and single dashes — so anything a caller invents is
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
/// Accents are folded to their closest ASCII letter where there is one, so a
/// French title gives a readable filename rather than an empty one. Anything
/// else — punctuation, an emoji, a slash — becomes a dash, which is also what
/// keeps a title from walking out of the directory.
#[must_use]
pub fn slugify(title: &str) -> String {
    let mut out = String::with_capacity(title.len());
    let mut last_dash = false;
    for ch in title.chars() {
        let folded = fold_accent(ch);
        for c in folded.chars() {
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
/// the value is read by a human or ignored entirely, and a wrong-by-a-second
/// clock is not a correctness problem here.
fn now_iso8601() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let (days, rem) = (secs / 86_400, secs % 86_400);
    let (secs_of_day, minute_of_hour, second_of_minute) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    // Civil-from-days, Howard Hinnant's algorithm: no leap-second table, no
    // dependency, and correct for every date this program will ever see.
    // `days` is a count of days since 1970, so it fits an i64 by
    // construction; the cast is the sign-correct one rather than `as`.
    let z = days.cast_signed() + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day_of_month = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { y + 1 } else { y };
    format!("{year:04}-{month:02}-{day_of_month:02}T{secs_of_day:02}:{minute_of_hour:02}:{second_of_minute:02}Z")
}

/// Write `contents` to `path` through a temporary file, then rename.
///
/// A worker reads these files while the PO edits them, and a torn write would
/// show as a truncated intention. `rename` is atomic within a directory, so a
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
            // but adding the block is better than dropping the value.
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

    fn store() -> (tempfile::TempDir, Intentions) {
        let dir = tempfile::tempdir().expect("tempdir");
        let intentions = Intentions::new(dir.path().join("intentions")).expect("new");
        (dir, intentions)
    }

    /// A title becomes a filename. Accents fold rather than vanish, because a
    /// French title slugging to nothing would leave an intention unnamed.
    #[test]
    fn a_title_becomes_a_readable_slug() {
        assert_eq!(slugify("Reprise des devis"), "reprise-des-devis");
        assert_eq!(slugify("Décision : congés d'été"), "decision-conges-d-ete");
        assert_eq!(slugify("  Espaces   multiples  "), "espaces-multiples");
        assert_eq!(slugify("Résumé & synthèse — volet 2"), "resume-synthese-volet-2");
        assert_eq!(slugify("!"), "");
    }

    /// The slug check is the one thing standing between a URL and a `join`.
    #[test]
    fn traversal_and_separators_are_not_slugs() {
        for bad in [
            "../../etc/passwd",
            "foo/bar",
            "foo bar",
            "Foo",
            "-leading",
            "trailing-",
            "double--dash",
            "",
            "a".repeat(101).as_str(),
        ] {
            assert!(!is_slug(bad), "{bad:?} must not be accepted as a slug");
        }
        for good in ["a", "reprise-des-devis", "volet-2", "x9"] {
            assert!(is_slug(good), "{good:?} must be accepted");
        }
    }

    /// Creating, listing and reading round-trip.
    #[test]
    fn an_intention_is_created_listed_and_read_back() {
        let (_dir, store) = store();
        let summary = store
            .create(
                "Congés d'été",
                "/home/mox2/Dev/kalpin-back",
                "Ouvrir les congés aux gars.",
            )
            .expect("create");
        assert_eq!(summary.slug, "conges-d-ete");
        assert_eq!(summary.title, "Congés d'été");
        assert!(!summary.ready);

        let listed = store.list().expect("list");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].repo, "/home/mox2/Dev/kalpin-back");

        let raw = store.read("conges-d-ete").expect("read");
        assert!(raw.contains("Ouvrir les congés aux gars."), "pitch is in the file");
        assert!(raw.starts_with("---\ntitle: Congés d'été\n"), "front matter first");
    }

    /// Two intentions with the same title must not overwrite each other: one of
    /// them carries a conversation by then.
    #[test]
    fn a_second_title_of_the_same_name_gets_its_own_file() {
        let (_dir, store) = store();
        let first = store.create("Devis", "repo-a", "un").expect("first");
        let second = store.create("Devis", "repo-b", "deux").expect("second");
        assert_eq!(first.slug, "devis");
        assert_eq!(second.slug, "devis-2", "a collision earns a suffix");

        assert!(store.read("devis").expect("read first").contains("un"));
        assert!(store.read("devis-2").expect("read second").contains("deux"));
        assert_eq!(store.list().expect("list").len(), 2);
    }

    /// Promotion is a move between the two directories, and asking twice is not
    /// an error — a worker and a PO can both ask.
    #[test]
    fn promoting_moves_the_file_and_is_idempotent() {
        let (_dir, store) = store();
        store.create("Cloisonnement", "repo", "pitch").expect("create");
        let (before, ready) = store.path_of("cloisonnement").expect("path");
        assert!(!ready, "starts in the working directory");

        store.promote("cloisonnement").expect("promote");
        let (after, ready) = store.path_of("cloisonnement").expect("path");
        assert!(ready, "now under READY-intentions");
        assert!(!before.exists(), "the old file is gone, not copied");
        assert!(after.ends_with("READY-intentions/cloisonnement.md"));

        store.promote("cloisonnement").expect("promoting twice is fine");
        assert_eq!(store.list().expect("list").len(), 1, "still exactly one");
        assert!(store.list().expect("list")[0].ready);
    }

    /// The conversation is appended to the same file, and the front matter
    /// survives the rewrites — that is what makes the file readable by a human
    /// and by the next step alike.
    #[test]
    fn turns_append_to_the_file_without_losing_the_front_matter() {
        let (_dir, store) = store();
        store.create("Catalogue", "repo", "présentation").expect("create");

        store
            .append_turn("catalogue", "Vous", "Quel est le périmètre ?")
            .expect("ask");
        store
            .append_turn("catalogue", "Worker", "Trois écrans, dont un à faire.")
            .expect("answer");

        let raw = store.read("catalogue").expect("read");
        assert!(raw.starts_with("---\ntitle: Catalogue\n"), "front matter intact");
        assert!(raw.contains("### Vous\n\nQuel est le périmètre ?"));
        assert!(raw.contains("### Worker\n\nTrois écrans, dont un à faire."));
        assert!(raw.ends_with('\n'), "file stays newline-terminated");
    }

    /// The worker's tab id lands in the front matter, and setting it twice
    /// replaces the line rather than adding a second one.
    #[test]
    fn the_worker_tab_is_recorded_once() {
        let (_dir, store) = store();
        store.create("X", "repo", "p").expect("create");
        store.set_tab("x", "aaa-111").expect("set");
        store.set_tab("x", "bbb-222").expect("replace");

        let raw = store.read("x").expect("read");
        assert_eq!(raw.matches("tab:").count(), 1, "one line, not two");
        assert!(raw.contains("tab: bbb-222"), "the latest wins");
        assert_eq!(store.list().expect("list")[0].tab_id.as_deref(), Some("bbb-222"));
    }

    /// An unknown slug is an error, not an empty file.
    #[test]
    fn reading_something_that_is_not_there_fails() {
        let (_dir, store) = store();
        assert!(store.read("fantome").is_err());
        assert!(store.path_of("../../etc/passwd").is_err());
    }

    /// A title with nothing sluggable is refused rather than creating `.md`.
    #[test]
    fn a_title_that_slugs_to_nothing_is_refused() {
        let (_dir, store) = store();
        assert!(store.create("!!!", "repo", "p").is_err());
        assert!(store.list().expect("list").is_empty());
    }
}
