// SPDX-License-Identifier: MPL-2.0

//! Per-project briefs: markdown files that reach an agent when it starts in a
//! matching directory.
//!
//! A file in `~/.config/tab-atelier/briefs/` (or `/etc/tab-atelier/briefs/`
//! for the whole machine) with a `baseDir` in its front matter is injected
//! into any session whose working directory is inside that path:
//!
//! ```text
//! ---
//! baseDir: /mnt/clients/ABCD
//! ---
//! Client ABCD: PHP 7.4, no composer update without asking. Deploys are
//! manual — never push to production yourself.
//! ```
//!
//! Selection is the same rule that governs per-project colours: match by cwd
//! prefix. Unlike colours, **every** match applies, ordered from least to most
//! specific, so a note about `/mnt/clients` and a note about
//! `/mnt/clients/ABCD/api` both arrive, with the more specific one last — the
//! position a reader weighs most.
//!
//! Two limits are deliberate. Briefs are read **once, at session start**, from
//! the tab's cwd; an agent that later `cd`s somewhere else is not re-briefed,
//! because context can be added to a session but not withdrawn. And the total
//! is capped: this text is paid for in every session, on every tab, forever.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Most characters of project brief injected into one session.
///
/// Beyond this the most specific matches are kept and the rest dropped with a
/// note, because silently truncating mid-sentence reads like a corrupted
/// instruction.
pub const MAX_TOTAL: usize = 6_000;

/// One brief file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Brief {
    /// Where it came from, for `tab-atelier brief` and error messages.
    pub source: PathBuf,
    /// Directories this applies to. Repeat `baseDir:` for several.
    pub base_dirs: Vec<String>,
    /// Applies everywhere, regardless of cwd (`always: true`).
    pub always: bool,
    /// The markdown below the front matter.
    pub body: String,
}

impl Brief {
    /// Length of the longest `baseDir` that contains `cwd`, or `Some(0)` for
    /// an `always` brief. `None` when it doesn't apply.
    #[must_use]
    pub fn specificity(&self, cwd: &Path) -> Option<usize> {
        let best = self
            .base_dirs
            .iter()
            .filter(|d| cwd.starts_with(Path::new(d.trim_end_matches('/'))))
            .map(String::len)
            .max();
        match best {
            Some(n) => Some(n),
            None if self.always => Some(0),
            None => None,
        }
    }
}

/// Split `---` front matter from the body.
///
/// Deliberately not YAML: `key: value` lines, repeated keys collected, and
/// anything unparseable ignored. A brief is prose with a couple of hints on
/// top; pulling in a parser to read two keys would be a poor trade, and a
/// strict one would reject a file over a stray colon.
#[must_use]
pub fn parse_front_matter(text: &str) -> (BTreeMap<String, Vec<String>>, String) {
    let mut keys: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let trimmed = text.trim_start_matches('\u{feff}');
    let Some(rest) = trimmed.strip_prefix("---") else {
        return (keys, text.trim().to_owned());
    };
    let rest = rest.trim_start_matches(['\r', '\n']);
    // The closing fence must be its own line; a `---` inside prose (a markdown
    // rule, say) must not end the header early.
    let Some(end) = rest
        .lines()
        .position(|l| l.trim_end() == "---" || l.trim_end() == "...")
    else {
        return (keys, text.trim().to_owned());
    };
    for line in rest.lines().take(end) {
        if let Some((k, v)) = line.split_once(':') {
            let (k, v) = (k.trim(), v.trim());
            if !k.is_empty() && !v.is_empty() {
                keys.entry(k.to_ascii_lowercase()).or_default().push(v.to_owned());
            }
        }
    }
    let body: String = rest
        .lines()
        .skip(end + 1)
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_owned();
    (keys, body)
}

/// Parse one file's contents into a [`Brief`].
#[must_use]
pub fn parse_brief(source: PathBuf, text: &str) -> Brief {
    let (keys, body) = parse_front_matter(text);
    let base_dirs = keys
        .get("basedir")
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .map(|d| expand_home(&d))
        .collect();
    let always = keys
        .get("always")
        .and_then(|v| v.first())
        .is_some_and(|v| matches!(v.to_ascii_lowercase().as_str(), "true" | "yes" | "1"));
    Brief {
        source,
        base_dirs,
        always,
        body,
    }
}

fn expand_home(dir: &str) -> String {
    dir.strip_prefix("~/").map_or_else(
        || dir.trim_end_matches('/').to_owned(),
        |rest| {
            std::env::var("HOME").map_or_else(
                |_| dir.to_owned(),
                |h| {
                    format!("{}/{rest}", h.trim_end_matches('/'))
                        .trim_end_matches('/')
                        .to_owned()
                },
            )
        },
    )
}

/// Directories searched, machine-wide first so a user's own briefs sort after
/// (and therefore read as the later word) when equally specific.
#[must_use]
pub fn brief_dirs() -> Vec<PathBuf> {
    vec![
        PathBuf::from("/etc/tab-atelier/briefs"),
        crate::config_dir(&crate::platform::config_dir()).join("briefs"),
    ]
}

/// Read every `.md` file in the brief directories.
///
/// A missing directory is not an error: most machines will never have one.
#[must_use]
pub fn load_all() -> Vec<Brief> {
    let mut out = Vec::new();
    for dir in brief_dirs() {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        let mut paths: Vec<PathBuf> = entries
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|e| e.eq_ignore_ascii_case("md")))
            .collect();
        // Stable order so two runs brief a session identically.
        paths.sort();
        for p in paths {
            if let Ok(text) = std::fs::read_to_string(&p) {
                let b = parse_brief(p, &text);
                if !b.body.is_empty() {
                    out.push(b);
                }
            }
        }
    }
    out
}

/// The briefs that apply in `cwd`, least specific first.
///
/// Ordering matters: the most specific brief lands last, where a reader weighs
/// it most, so `/mnt/clients/ABCD/api` can qualify what `/mnt/clients` said.
#[must_use]
pub fn select<'a>(briefs: &'a [Brief], cwd: &Path) -> Vec<&'a Brief> {
    let mut matched: Vec<(usize, &Brief)> = briefs
        .iter()
        .filter_map(|b| b.specificity(cwd).map(|s| (s, b)))
        .collect();
    matched.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.source.cmp(&b.1.source)));
    matched.into_iter().map(|(_, b)| b).collect()
}

/// Join selected briefs into the text injected at session start.
///
/// Over `cap`, the least specific are dropped whole and a line says so —
/// truncating mid-sentence would read like a corrupted instruction, and an
/// agent has no way to tell the difference.
#[must_use]
pub fn render(selected: &[&Brief], cap: usize) -> String {
    if selected.is_empty() {
        return String::new();
    }
    let mut kept: Vec<&&Brief> = Vec::new();
    let mut total = 0;
    // Most specific first while filling, so the general note is what gets cut.
    for b in selected.iter().rev() {
        let cost = b.body.len() + 2;
        if total + cost > cap && !kept.is_empty() {
            break;
        }
        total += cost;
        kept.push(b);
    }
    let dropped = selected.len() - kept.len();
    kept.reverse();
    let mut out = String::new();
    for b in kept {
        if !out.is_empty() {
            out.push_str("\n\n");
        }
        out.push_str(b.body.trim());
    }
    if dropped > 0 {
        use std::fmt::Write as _;
        let _ = write!(
            out,
            "\n\n({dropped} more project brief(s) omitted — over the {cap}-character budget)"
        );
    }
    out
}

/// The project brief for `cwd`, ready to inject. Empty when nothing matches.
#[must_use]
pub fn for_cwd(cwd: &Path) -> String {
    let all = load_all();
    render(&select(&all, cwd), MAX_TOTAL)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn brief(dirs: &[&str], body: &str, name: &str) -> Brief {
        Brief {
            source: PathBuf::from(name),
            base_dirs: dirs.iter().map(|d| (*d).to_string()).collect(),
            always: false,
            body: body.to_owned(),
        }
    }

    #[test]
    fn front_matter_is_split_from_the_body() {
        let (keys, body) = parse_front_matter("---\nbaseDir: /mnt/clients/ABCD\n---\nUse PHP 7.4.\n");
        assert_eq!(
            keys.get("basedir").map(Vec::as_slice),
            Some(["/mnt/clients/ABCD".to_string()].as_slice())
        );
        assert_eq!(body, "Use PHP 7.4.");
        // Several baseDirs: one brief can cover a client's several checkouts.
        let (keys, _) = parse_front_matter("---\nbaseDir: /a\nbaseDir: /b\n---\ntext");
        assert_eq!(keys.get("basedir").map(Vec::len), Some(2));
        // No front matter at all is still a usable brief.
        let (keys, body) = parse_front_matter("just prose\n");
        assert!(keys.is_empty());
        assert_eq!(body, "just prose");
        // A `---` inside the prose is a horizontal rule, not a second header.
        let (_, body) = parse_front_matter("---\nbaseDir: /x\n---\nfirst\n\n---\n\nsecond\n");
        assert!(body.contains("first") && body.contains("second"), "{body}");
        // An unterminated header is treated as prose rather than swallowing
        // the whole file into keys.
        let (keys, body) = parse_front_matter("---\nbaseDir: /x\nno closing fence\n");
        assert!(keys.is_empty());
        assert!(body.contains("no closing fence"));
    }

    #[test]
    fn a_brief_applies_to_directories_beneath_its_base() {
        let b = parse_brief(PathBuf::from("abcd.md"), "---\nbaseDir: /mnt/clients/ABCD/\n---\nnotes");
        assert_eq!(b.body, "notes");
        assert!(b.specificity(Path::new("/mnt/clients/ABCD")).is_some());
        assert!(b.specificity(Path::new("/mnt/clients/ABCD/api/src")).is_some());
        // A sibling client must not get another client's instructions.
        assert!(b.specificity(Path::new("/mnt/clients/EFGH")).is_none());
        // Prefix match is by path component, not by string: /ABCD-old is a
        // different directory and would be a serious leak.
        assert!(b.specificity(Path::new("/mnt/clients/ABCD-old")).is_none());
    }

    #[test]
    fn matches_compose_with_the_most_specific_last() {
        // A client-wide note and a per-repo note both apply; the repo one
        // lands last, where a reader weighs it most.
        let all = vec![
            brief(&["/mnt/clients/ABCD/api"], "api: run make test", "b.md"),
            brief(&["/mnt/clients"], "all clients: never deploy", "a.md"),
        ];
        let sel = select(&all, Path::new("/mnt/clients/ABCD/api/src"));
        let bodies: Vec<&str> = sel.iter().map(|b| b.body.as_str()).collect();
        assert_eq!(bodies, vec!["all clients: never deploy", "api: run make test"]);
        let text = render(&sel, MAX_TOTAL);
        assert!(text.find("never deploy") < text.find("run make test"));
        // Nothing matching is nothing injected — not an empty header.
        assert!(select(&all, Path::new("/tmp")).is_empty());
        assert!(render(&[], MAX_TOTAL).is_empty());
    }

    #[test]
    fn an_always_brief_applies_everywhere_but_ranks_first() {
        let mut always = brief(&[], "house style: small commits", "z.md");
        always.always = true;
        let all = vec![always, brief(&["/mnt/clients"], "client note", "a.md")];
        let sel = select(&all, Path::new("/mnt/clients/X"));
        assert_eq!(sel.len(), 2);
        assert_eq!(sel[0].body, "house style: small commits", "general first");
        // It still applies where nothing else does.
        assert_eq!(select(&all, Path::new("/tmp")).len(), 1);
    }

    #[test]
    fn the_budget_drops_whole_briefs_starting_with_the_least_specific() {
        let all = vec![
            brief(&["/x"], &"g".repeat(500), "a.md"),
            brief(&["/x/y"], &"s".repeat(500), "b.md"),
        ];
        let sel = select(&all, Path::new("/x/y/z"));
        let text = render(&sel, 600);
        // The specific one survives; the general one is dropped WHOLE, with a
        // note — a half-sentence would read like a corrupted instruction.
        assert!(text.contains(&"s".repeat(500)));
        assert!(!text.contains("ggg"));
        assert!(text.contains("1 more project brief(s) omitted"), "{text}");
        // A single brief over budget is still delivered rather than silently
        // producing nothing at all.
        let big = vec![brief(&["/x"], &"b".repeat(1000), "c.md")];
        let sel = select(&big, Path::new("/x"));
        assert!(render(&sel, 100).contains("bbb"));
    }

    #[test]
    fn tilde_paths_expand_so_a_config_can_be_portable() {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/root".into());
        let b = parse_brief(PathBuf::from("h.md"), "---\nbaseDir: ~/work\n---\nnote");
        assert_eq!(b.base_dirs, vec![format!("{}/work", home.trim_end_matches('/'))]);
        assert!(b.specificity(Path::new(&format!("{home}/work/proj"))).is_some());
    }
}
