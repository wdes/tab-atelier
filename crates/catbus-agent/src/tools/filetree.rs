// SPDX-License-Identifier: MPL-2.0

//! `FileTree` — list a directory to a bounded depth.
//!
//! The observation tool for a session that has no shell. `Read` shows one file's
//! contents but not what exists beside it, so an agent with only `Read` and
//! `Write` has no way to find out what a directory holds or where to put a new
//! file. That is the gap this fills, and it exists so the pair can be the whole
//! tool set for a plain-file task (see [`super::MINIMAL_TOOLS`]).
//!
//! # What is skipped
//!
//! Exactly two things, and both are reported rather than silent:
//!
//! 1. **Version-control directories** ([`VCS_DIRS`]). Machine state that no one
//!    reads, and `.git` is the one directory guaranteed to dwarf the project it
//!    sits in. Skipped even outside a repository, because a stray `.git` is
//!    still not something to walk.
//! 2. **Whatever the project's own ignore rules say**, via the [`ignore`]
//!    crate — the same one ripgrep uses, so a file that `rg` hides is a file
//!    this hides. That means `.gitignore` in the tree, `.gitignore` in parent
//!    directories, `.git/info/exclude`, and the user's **global** ignore file
//!    (`core.excludesFile`, or `$XDG_CONFIG_HOME/git/ignore` when unset).
//!
//! Using the project's ignore rules instead of a hard-coded noise list is the
//! point: `node_modules` and `target` were the wrong target. They are already
//! gitignored in almost every repository, so a list is redundant where it is
//! right and wrong where the project disagrees — a vendored `target/` that is
//! committed, or a Python project with no `node_modules` at all. The ignore
//! rules are also *correct by construction*: they are the project's own
//! statement about what is not source. Dotfiles are **not** skipped, since
//! `.github/`, `.env.example` and `.eslintrc` are exactly the kind of thing an
//! agent is asked about; VCS directories are the only name-based exception, and
//! they are enumerated rather than pattern-matched.
//!
//! A consequence worth knowing: because the global ignore file is honoured, a
//! pattern in one operator's `~/.config/git/ignore` hides a file for *their*
//! agent and not for a colleague's. That is the same asymmetry `rg` already has,
//! and the output names the sources it consulted so a missing file is
//! explicable.
//!
//! # What is bounded
//!
//! Output goes into a model's context and a directory can be arbitrarily large:
//!
//! * `depth` is required and capped at [`MAX_DEPTH`], and a request beyond it is
//!   refused rather than clamped. Silently showing less than was asked for is
//!   how an agent concludes a directory is empty when it is not.
//! * `max_entries` bounds the total, and truncation is reported for the same
//!   reason.
//! * Symlinked directories are printed but never descended, so a link back up
//!   the tree cannot loop the walk.
//! * Unreadable directories are counted and reported — otherwise a permission
//!   error is indistinguishable from an empty directory.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Deepest the walk will go. Refused above, not clamped — see the module docs.
pub const MAX_DEPTH: usize = 6;

/// Entries returned when the caller does not say.
pub const DEFAULT_MAX_ENTRIES: usize = 200;

/// Largest `max_entries` accepted. A number above this is a mistake or an
/// attempt to dump a whole tree into the context window, and either way the
/// operator should hear about it.
pub const MAX_ENTRIES_CEILING: usize = 2000;

/// Version-control state, always skipped.
///
/// An enumerated list rather than a pattern: these are the only directories
/// whose *contents* are not meant to be read, and naming them keeps every other
/// dotfile visible. `.git` is the important one — it is the single directory
/// most likely to be larger than the project it belongs to.
const VCS_DIRS: &[&str] = &[".git", ".hg", ".svn", ".bzr"];

/// One entry to render.
struct Row {
    /// Path relative to the listed root, exactly as printed. Stored rather than
    /// derived at print time so the sort and the output cannot disagree about
    /// what a row's path is.
    rel: String,
    kind: Kind,
}

enum Kind {
    Dir,
    File,
    /// A symlink, printed with its target and never descended.
    Symlink(String),
}

/// Totals carried alongside the rendered tree.
#[derive(Default)]
struct Stats {
    dirs: usize,
    files: usize,
    links: usize,
    /// Directories skipped by [`VCS_DIRS`].
    vcs_skipped: usize,
    /// Directories the walk could not read.
    unreadable: usize,
}

pub async fn run(input: &serde_json::Value, cwd: &Path) -> Result<String, String> {
    let path = input
        .get("path")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "missing path".to_string())?;
    let depth = input.get("depth").and_then(serde_json::Value::as_u64).ok_or_else(|| {
        "missing depth: pass how many levels deep to list, e.g. depth 1 for just this \
             directory's contents"
            .to_string()
    })?;
    let max_entries = input
        .get("max_entries")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(DEFAULT_MAX_ENTRIES as u64);
    // Both default to false, and are read as booleans rather than "truthy": a
    // string `"false"` must not turn them on, since the whole point of both is
    // that widening the listing is something the caller asked for on purpose.
    let show_ignored = input
        .get("show_ignored")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    let show_vcs = input
        .get("show_vcs")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);

    // Refused rather than clamped, so the model cannot believe it saw a whole
    // directory when it saw part of one. The message states the limit, which
    // makes the retry obvious instead of mysterious.
    let depth = usize::try_from(depth).map_err(|_| format!("depth is too large (max {MAX_DEPTH})"))?;
    if depth == 0 {
        return Err("depth must be at least 1".to_string());
    }
    if depth > MAX_DEPTH {
        return Err(format!(
            "depth {depth} is deeper than the {MAX_DEPTH}-level limit; ask for {MAX_DEPTH} or less"
        ));
    }
    let max_entries = usize::try_from(max_entries).unwrap_or(MAX_ENTRIES_CEILING);
    if max_entries == 0 {
        return Err("max_entries must be at least 1".to_string());
    }
    if max_entries > MAX_ENTRIES_CEILING {
        return Err(format!(
            "max_entries {max_entries} is above the {MAX_ENTRIES_CEILING} ceiling"
        ));
    }

    let root = super::resolve(cwd, path);
    // Off the async runtime: the walk is bounded by `max_entries`, but it is
    // still blocking filesystem work, and this process runs a single-threaded
    // reactor — calling it inline would stall the socket for the whole walk.
    let rendered = tokio::task::spawn_blocking(move || {
        let root = normalize(&root);
        // `symlink_metadata` so the root itself can be a symlink without us
        // following it into somewhere unexpected.
        let meta = std::fs::symlink_metadata(&root).map_err(|e| format!("{}: {e}", root.display()))?;
        if !meta.is_dir() {
            return Err(format!("{}: not a directory", root.display()));
        }
        Ok(render(&root, depth, max_entries, Options { show_ignored, show_vcs }))
    })
    .await
    .map_err(|e| format!("listing failed: {e}"))??;

    Ok(rendered)
}

/// What the caller asked to be shown.
///
/// A struct rather than two more `bool` parameters: they are adjacent, both
/// default to false, and mixing them up would produce a listing that is wrong in
/// a way that still looks plausible — `show_vcs` accidentally set would silently
/// dump a whole `.git` into the context window.
#[derive(Debug, Clone, Copy, Default)]
struct Options {
    /// Include entries the project's ignore rules exclude.
    show_ignored: bool,
    /// Include version-control directories.
    show_vcs: bool,
}

/// Walk and format.
fn render(root: &Path, max_depth: usize, max_entries: usize, options: Options) -> String {
    let (mut rows, mut stats, truncated) = collect(root, max_depth, max_entries, options);
    // Sorted by path, in byte order — the order `sort` and `rg --files` produce,
    // so the listing matches what the model has seen from those tools. Two
    // useful consequences fall out for free rather than needing a custom
    // comparator: it is stable between runs (the walker's own order is already
    // by name, but this pins it), and a directory is immediately followed by its
    // own children, since every child's path shares its prefix and `/` sorts
    // before any name character.
    rows.sort_by(|a, b| a.rel.cmp(&b.rel));
    stats.dirs = rows.iter().filter(|r| matches!(r.kind, Kind::Dir)).count();
    stats.files = rows.iter().filter(|r| matches!(r.kind, Kind::File)).count();
    stats.links = rows.iter().filter(|r| matches!(r.kind, Kind::Symlink(_))).count();
    format_listing(root, &rows, &stats, truncated, options)
}

/// Walk the tree and collect what will be printed.
///
/// Separate from [`format_listing`] so neither outgrows a readable length, and so
/// the interesting failure modes — a truncated walk, an unreadable directory —
/// are values passed between them rather than flags threaded through formatting.
fn collect(root: &Path, max_depth: usize, max_entries: usize, options: Options) -> (Vec<Row>, Stats, bool) {
    // Counted inside the filter closure, which runs during iteration and so
    // needs interior mutability. An `Arc<AtomicUsize>` rather than a plain
    // atomic: `filter_entry` takes the closure by value, so the count has to
    // outlive the builder for the totals line to read it.
    let pruned = Arc::new(AtomicUsize::new(0));
    let pruned_in_filter = Arc::clone(&pruned);

    let mut builder = ignore::WalkBuilder::new(root);
    builder
        .max_depth(Some(max_depth))
        // Dotfiles are shown: `.github/`, `.env.example` and `.eslintrc` are
        // things an agent gets asked about. VCS directories are excluded by the
        // filter below instead, which prunes the subtree rather than hiding a
        // name — the distinction `hidden` cannot make.
        .hidden(false)
        .follow_links(false)
        // The ignore sources, named explicitly so this does not depend on the
        // crate's defaults: the project's `.gitignore` files, `.git/info/exclude`,
        // and the operator's global ignore file. All gated on `show_ignored`
        // rather than left on and filtered afterwards, because an ignored
        // *directory* has to be descended into to list its contents — deciding
        // per entry after the fact would only ever show ignored files, never
        // what is inside an ignored directory such as `node_modules/`.
        .git_ignore(!options.show_ignored)
        .git_exclude(!options.show_ignored)
        .git_global(!options.show_ignored)
        // `.ignore` is the crate's own format, used by tools that want ignore
        // rules without a repository. Covered by the same flag, since it is the
        // same request: "show me what is normally hidden".
        .ignore(!options.show_ignored)
        // Honour ignore files in parent directories too, so a cwd inside a repo
        // still sees the repo's rules — and stop honouring them, for the same
        // reason as above, when the caller wants everything.
        .parents(!options.show_ignored)
        // ...and apply those rules even outside a repository. A directory with
        // a `.gitignore` but no `.git` is a deliberate statement about its own
        // contents, and honouring it can only make the listing more accurate.
        .require_git(false)
        // Deterministic collection order, so which entries survive the
        // `max_entries` cut does not vary between runs. The comparator is
        // byte-order by name; the display order is set by the explicit sort in
        // `render`, which is why this only has to be *stable*, not *correct*.
        .sort_by_file_name(std::cmp::Ord::cmp)
        .filter_entry(move |entry| {
            // The root itself is never pruned: listing `.git` on purpose should
            // work, even though the walk would never descend into it by accident.
            if entry.depth() == 0 {
                return true;
            }
            let is_vcs = entry.file_name().to_str().is_some_and(|name| VCS_DIRS.contains(&name));
            if is_vcs {
                if options.show_vcs {
                    return true;
                }
                pruned_in_filter.fetch_add(1, Ordering::Relaxed);
            }
            !is_vcs
        });

    let mut rows = Vec::new();
    let mut truncated = false;
    let mut unreadable = 0_usize;

    for item in builder.build() {
        match item {
            Ok(entry) => {
                if entry.depth() == 0 {
                    continue;
                }
                // Stop at the budget rather than collecting everything and
                // slicing: "there is more" is known one entry past the limit,
                // without walking the rest of a potentially enormous tree.
                if rows.len() >= max_entries {
                    truncated = true;
                    break;
                }
                rows.push(row_for(&entry, root));
            }
            // A permission error on a subdirectory arrives here. Counted rather
            // than dropped: an unreadable directory reported as absent is the
            // exact confusion this tool exists to prevent.
            Err(_) => unreadable += 1,
        }
    }

    let stats = Stats {
        dirs: 0,
        files: 0,
        links: 0,
        vcs_skipped: pruned.load(Ordering::Relaxed),
        unreadable,
    };
    (rows, stats, truncated)
}

/// Compose the printed listing from what [`collect`] found.
///
/// Uses `write!` rather than `push_str(&format!(..))`: the same output without
/// building and dropping a `String` per line.
fn format_listing(root: &Path, rows: &[Row], stats: &Stats, truncated: bool, options: Options) -> String {
    // A generous initial size: a listing is a handful of short lines, and
    // starting from scratch would reallocate a few times on a large tree.
    let mut out = String::with_capacity(64 + rows.len() * 32);
    // `writeln!` on a `String` cannot fail, so the results are discarded rather
    // than unwrapped — an infallible call has nothing to report.
    let _ = writeln!(out, "{}", root.display());
    // Flat paths relative to the root, one per line. Every line is then usable
    // verbatim as the `path` of a `Read` or `Write`, which is the property that
    // matters most: a path the model has to *derive* from indentation is a path
    // it can get wrong, and a wrong path costs a whole failed round trip — far
    // more than the bytes an ancestor prefix repeats.
    for row in rows {
        let rel = &row.rel;
        match &row.kind {
            Kind::Dir => {
                let _ = writeln!(out, "{rel}/");
            }
            Kind::File => {
                let _ = writeln!(out, "{rel}");
            }
            Kind::Symlink(target) => {
                let _ = writeln!(out, "{rel} -> {target}");
            }
        }
    }

    // A separator, so the totals are not read as one more entry in a listing
    // where every other line is a path.
    let _ = writeln!(out, "---");
    let _ = writeln!(
        out,
        "{} {}, {} {}, {} {}",
        stats.dirs,
        plural(stats.dirs, "directory", "directories"),
        stats.files,
        plural(stats.files, "file", "files"),
        stats.links,
        plural(stats.links, "symlink", "symlinks")
    );
    if let Some(note) = ignore_note(root, options) {
        let _ = writeln!(out, "{note}");
    }
    if stats.vcs_skipped > 0 {
        let _ = writeln!(
            out,
            "{} version-control {} skipped ({})",
            stats.vcs_skipped,
            plural(stats.vcs_skipped, "directory", "directories"),
            VCS_DIRS.join(", ")
        );
    }
    if stats.unreadable > 0 {
        let _ = writeln!(
            out,
            "{} {} could not be read (permissions?)",
            stats.unreadable,
            plural(stats.unreadable, "entry", "entries")
        );
    }
    if truncated {
        let _ = writeln!(
            out,
            "… more entries not shown, raise max_entries (ceiling {MAX_ENTRIES_CEILING})"
        );
    }
    out
}

/// One [`Row`], with its path made relative to the listed root.
fn row_for(entry: &ignore::DirEntry, root: &Path) -> Row {
    let path = entry.path();
    let rel = path.strip_prefix(root).unwrap_or(path).to_string_lossy().into_owned();
    // `path_is_symlink` rather than `file_type`: with `follow_links(false)` a
    // link is reported as itself, and this stays unambiguous about that.
    let kind = if entry.path_is_symlink() {
        // A broken link still lists, showing "?" for its target — disappearing
        // it would misrepresent the directory.
        let target = std::fs::read_link(path).map_or_else(|_| "?".to_string(), |p| p.display().to_string());
        Kind::Symlink(target)
    } else if entry.file_type().is_some_and(|t| t.is_dir()) {
        Kind::Dir
    } else {
        Kind::File
    };
    Row { rel, kind }
}

/// A line naming the ignore sources that exist at or above the root, or saying
/// they were deliberately not applied.
///
/// Reported because an ignored file is otherwise indistinguishable from a
/// missing one, and the ignore rules may live in a file the operator never
/// opened — most of all the *global* one, which is per-machine and so makes the
/// same directory list differently for two people. Only presence is checked;
/// counting what each rule matched would mean walking the tree twice.
fn ignore_note(root: &Path, options: Options) -> Option<String> {
    let mut sources = Vec::new();
    if root.join(".gitignore").is_file() {
        sources.push(".gitignore".to_string());
    }
    // The parent-directory rules only matter when the root is inside a repo.
    let mut dir = root.parent();
    while let Some(parent) = dir {
        if parent.join(".git").exists() {
            sources.push("repo .gitignore".to_string());
            break;
        }
        dir = parent.parent();
    }
    if let Some(global) = global_ignore_file().filter(|path| path.is_file()) {
        sources.push(format!("global {}", global.display()));
    }

    // When the caller asked for everything, an unqualified "ignore rules
    // applied" would be actively misleading — it would have the model believe a
    // file it can now see is somehow exempt, or that a file still missing was
    // ignored when the real reason is that it does not exist.
    if options.show_ignored {
        return Some(if sources.is_empty() {
            "show_ignored: no ignore rules existed to skip".to_string()
        } else {
            format!("show_ignored: ignoring ignore rules ({})", sources.join(", "))
        });
    }
    if sources.is_empty() {
        return None;
    }
    Some(format!("ignore rules applied: {}", sources.join(", ")))
}

/// The operator's global ignore file, following the same rule as `ignore`:
/// `core.excludesFile` is not read here (that needs the config parser), so this
/// mirrors the fallback the walker itself uses.
fn global_ignore_file() -> Option<PathBuf> {
    let config_home = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))?;
    Some(config_home.join("git").join("ignore"))
}

/// Singular or plural, spelled out because "directory" is not "directory" + "s".
const fn plural<'a>(n: usize, one: &'a str, many: &'a str) -> &'a str {
    if n == 1 { one } else { many }
}

/// Drop `.` components so a listing's header reads `/tmp/work` rather than
/// `/tmp/work/.` — the model sees this line and may copy it into a later `path`,
/// so it should be the path a person would write.
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        if !matches!(component, std::path::Component::CurDir) {
            out.push(component);
        }
    }
    if out.as_os_str().is_empty() {
        PathBuf::from(".")
    } else {
        out
    }
}

/// The tool spec sent to the model.
#[must_use]
pub fn spec() -> serde_json::Value {
    serde_json::json!({
        "name": "FileTree",
        "description": "List a directory's contents, deepest `depth` levels, without running a shell. \
            Output is one path per line relative to the listed directory, sorted, with directories \
            suffixed `/` and symlinks shown as `name -> target`; a `---` line then totals. Each \
            listed path can be passed straight to Read or Write without modification. \
            Version-control directories (`.git`, `.hg`, `.svn`, …) are skipped by default, and so is \
            anything the project's own ignore rules exclude — `.gitignore` at any level, \
            `.git/info/exclude`, and the user's global ignore file — so a file that `rg` hides is \
            hidden here too. Dotfiles other than those are shown. Each call reports which ignore \
            sources applied, so a file you expected but cannot see is explicable. \
            Pass `show_ignored` to include ignored paths (an ignored directory's contents then \
            appear too), and `show_vcs` to include version-control directories — both are usually \
            noise, but they are the only way to inspect a build output tree or a repository's own \
            state, and `show_ignored` is also how you confirm a file is missing rather than ignored.",
        "input_schema": {
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Directory to list. Absolute, or relative to the agent's working directory. Use \".\" for the working directory itself."
                },
                "depth": {
                    "type": "integer",
                    "description": format!("How many levels deep to list. 1 lists only this directory's immediate contents. Must be between 1 and {MAX_DEPTH}."),
                    "minimum": 1,
                    "maximum": MAX_DEPTH
                },
                "max_entries": {
                    "type": "integer",
                    "description": format!("Cap on entries returned (default {DEFAULT_MAX_ENTRIES}, ceiling {MAX_ENTRIES_CEILING}). Truncation is reported.")
                },
                "show_ignored": {
                    "type": "boolean",
                    "description": "Include paths the project's ignore rules exclude, and descend into ignored directories. Default false. Useful to inspect build output, vendored code, or to check whether a file is missing or merely ignored."
                },
                "show_vcs": {
                    "type": "boolean",
                    "description": "Include version-control directories (`.git`, `.hg`, …) and their contents. Default false. Usually only wanted when the repository's own state is the question."
                }
            },
            "required": ["path", "depth"]
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// A tree with the shapes that matter: nesting, a VCS directory, an ignored
    /// file, a symlink, dotfiles, and enough entries to hit a budget.
    ///
    /// Fixture names are conventional source filenames on purpose. The walker
    /// consults the operator's *global* ignore file, which an in-process test
    /// cannot control (that needs `env::set_var`, which edition 2024 made
    /// `unsafe`), so a fixture named in a way a plausible global rule would
    /// match would make these tests pass on one machine and fail on another.
    /// `README.md`, `main.rs` and friends collide with no sensible global rule;
    /// the global behaviour is asserted in the integration suite instead, where
    /// the child's environment is set explicitly. The VCS name below is
    /// deliberately real (`.git`) because that case must *always* be skipped,
    /// whatever any ignore file says.
    fn sample() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("src/deep/deeper")).unwrap();
        fs::write(root.join("README.md"), "hi").unwrap();
        fs::write(root.join("src/main.rs"), "fn main() {}").unwrap();
        fs::write(root.join("src/lib.rs"), "// lib").unwrap();
        fs::write(root.join("src/deep/deeper/leaf.txt"), "leaf").unwrap();
        // A VCS directory, which must be skipped unconditionally.
        fs::create_dir_all(root.join(".git")).unwrap();
        fs::write(root.join(".git/HEAD"), "ref").unwrap();
        // Dotfiles that are *not* VCS: shown, because an agent gets asked about
        // `.github/` and `.env.example`.
        fs::create_dir_all(root.join(".github/workflows")).unwrap();
        fs::write(root.join(".github/workflows/ci.yml"), "on: push").unwrap();
        fs::write(root.join(".editorconfig"), "root = true").unwrap();
        // A link to a directory *inside* the tree: following it would loop.
        std::os::unix::fs::symlink(root.join("src"), root.join("srclink")).unwrap();
        dir
    }

    fn run_at(dir: &Path, depth: u64) -> String {
        let input = serde_json::json!({ "path": ".", "depth": depth });
        run_with(&input, dir)
    }

    fn run_with(input: &serde_json::Value, dir: &Path) -> String {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(run(input, dir))
            .unwrap()
    }

    /// The listing's path lines, without the header, totals or any note.
    ///
    /// Everything after the `---` separator is prose about the walk, so tests
    /// about *content* read only what came before it.
    fn entries(out: &str) -> Vec<&str> {
        out.lines().skip(1).take_while(|l| *l != "---").collect()
    }

    #[test]
    fn output_is_flat_paths_relative_to_the_listed_directory() {
        // The property that matters most: every line is usable verbatim as the
        // `path` of a Read or Write. Nothing is indented, nothing needs the
        // header joined onto it, and it matches what `sort` and `rg --files`
        // print so the model sees a familiar shape.
        let dir = sample();
        // Depth 4, because the deepest path asserted below is at level 4
        // (`src/deep/deeper/leaf.txt`). Depth 3 would legitimately omit it, so a
        // shallower walk here would look like a formatting bug.
        let out = run_at(dir.path(), 4);
        let lines = entries(&out);
        assert!(lines.contains(&"README.md"), "{out}");
        assert!(lines.contains(&"src/main.rs"), "{out}");
        assert!(lines.contains(&"src/deep/deeper/leaf.txt"), "{out}");
        for line in &lines {
            assert!(!line.starts_with(' '), "a path line is indented:\n{out}");
            assert!(!line.starts_with('/'), "paths should be relative:\n{out}");
        }
    }

    #[test]
    fn a_listed_path_is_accepted_by_read_unchanged() {
        // The round trip the format exists for: take a line from FileTree's
        // output and hand it straight to the same resolver `Read` uses. If the
        // two ever disagreed, the pair would be useless together.
        let dir = sample();
        let out = run_at(dir.path(), 4);
        let listed = entries(&out)
            .into_iter()
            .find(|l| l.ends_with("leaf.txt"))
            .expect("leaf.txt should be listed");
        let resolved = super::super::resolve(dir.path(), listed);
        assert!(resolved.is_file(), "{listed:?} did not resolve to a file: {resolved:?}");
        assert_eq!(fs::read_to_string(resolved).unwrap(), "leaf");
    }

    #[test]
    fn directories_carry_a_trailing_slash_and_nothing_else_does() {
        let dir = sample();
        let out = run_at(dir.path(), 2);
        assert!(entries(&out).contains(&"src/"), "{out}");
        assert!(
            !entries(&out).contains(&"src"),
            "a directory without its marker:\n{out}"
        );
        assert!(entries(&out).contains(&"README.md"), "{out}");
    }

    #[test]
    fn sorting_matches_sort_one_and_puts_a_directory_before_its_children() {
        // Byte-order sorting is chosen because it gives two properties for free:
        // it is what `sort` does, and a child always follows its own directory
        // because `/` sorts before any name character.
        let dir = sample();
        let out = run_at(dir.path(), 3);
        let lines = entries(&out);
        let mut sorted = lines.clone();
        sorted.sort_unstable();
        assert_eq!(lines, sorted, "output is not in byte-sorted order:\n{out}");

        let src = lines.iter().position(|l| *l == "src/").unwrap();
        assert!(
            lines[src + 1].starts_with("src/"),
            "a directory should be followed by its own children:\n{out}"
        );
    }

    #[test]
    fn depth_one_lists_only_immediate_children() {
        let dir = sample();
        let out = run_at(dir.path(), 1);
        assert!(entries(&out).contains(&"src/"), "{out}");
        assert!(
            !entries(&out).contains(&"src/main.rs"),
            "depth 1 showed a grandchild:\n{out}"
        );
    }

    #[test]
    fn each_depth_reveals_exactly_one_more_level() {
        let dir = sample();
        let two = run_at(dir.path(), 2);
        assert!(entries(&two).contains(&"src/main.rs"), "{two}");
        assert!(!entries(&two).contains(&"src/deep/deeper/leaf.txt"), "{two}");

        let three = run_at(dir.path(), 3);
        assert!(entries(&three).contains(&"src/deep/deeper/"), "{three}");
        assert!(!entries(&three).contains(&"src/deep/deeper/leaf.txt"), "{three}");

        let four = run_at(dir.path(), 4);
        assert!(entries(&four).contains(&"src/deep/deeper/leaf.txt"), "{four}");
    }

    #[test]
    fn vcs_directories_are_skipped_and_the_skip_is_reported() {
        // The one name-based rule, and the only directory whose contents are
        // never worth reading. Reported as a count, because an unannounced skip
        // reads as "this does not exist".
        let dir = sample();
        let out = run_at(dir.path(), 4);
        assert!(!entries(&out).contains(&".git/"), ".git should be skipped:\n{out}");
        assert!(!out.contains("HEAD"), "a file inside .git leaked:\n{out}");
        assert!(out.contains("version-control"), "the skip should be reported:\n{out}");
    }

    #[test]
    fn every_name_in_the_vcs_list_is_actually_skipped() {
        // Iterates the constant rather than naming one directory, so a typo in
        // the list cannot pass unnoticed: `contain` matches on the exact string,
        // so `.bzr` misspelled as `.bazr` would simply never skip anything, and
        // the only symptom would be a stray directory in someone's listing.
        //
        // The fixture is built from each string in the constant rather than from
        // a lowercased copy of it, because the skip compares names exactly: an
        // entry whose spelling later diverges in case from what a tool creates
        // would leave this test passing while the real skip did nothing.
        for name in VCS_DIRS {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path();
            fs::create_dir_all(root.join(name)).unwrap();
            fs::write(root.join(name).join("inside"), "state").unwrap();
            fs::write(root.join("kept.py"), "real source").unwrap();

            let out = run_at(root, 2);
            assert!(
                !entries(&out).contains(&format!("{name}/").as_str()),
                "{name} should be skipped:\n{out}"
            );
            assert!(
                !out.contains("inside"),
                "a file inside {name} leaked into the listing:\n{out}"
            );
            assert!(
                entries(&out).contains(&"kept.py"),
                "{name} must not take real source with it:\n{out}"
            );
            // And shown again when the caller asks, so the skip is a default
            // and not an inability.
            let shown = run_with(&serde_json::json!({ "path": ".", "depth": 2, "show_vcs": true }), root);
            assert!(
                entries(&shown).contains(&format!("{name}/").as_str()),
                "show_vcs should reveal {name}:\n{shown}"
            );
        }
    }

    #[test]
    fn the_vcs_list_stays_small() {
        // A guard on intent, not just behaviour: this list is the *only*
        // name-based rule, and it exists for machine state that dwarfs its
        // project. Every entry added here is a directory an agent can no longer
        // see by default, so growth should be a deliberate decision — which is
        // why the count is asserted rather than merely commented.
        assert_eq!(VCS_DIRS, [".git", ".hg", ".svn", ".bzr"]);
    }

    #[test]
    fn dotfiles_that_are_not_vcs_are_shown() {
        // `.github/`, `.editorconfig` and the like are things an agent is asked
        // about, so `hidden(false)` is load-bearing — and the VCS rule above is
        // a name list rather than "anything starting with a dot" precisely so
        // these survive.
        let dir = sample();
        let out = run_at(dir.path(), 3);
        assert!(entries(&out).contains(&".editorconfig"), "{out}");
        assert!(entries(&out).contains(&".github/"), "{out}");
        assert!(entries(&out).contains(&".github/workflows/ci.yml"), "{out}");
    }

    #[test]
    fn a_project_ignore_file_hides_what_it_names() {
        // The behaviour that replaced the hard-coded `node_modules`/`target`
        // list: the project says what is not source, and this obeys.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("node_modules/pkg")).unwrap();
        fs::write(root.join("node_modules/pkg/index.js"), "// js").unwrap();
        fs::create_dir_all(root.join("target")).unwrap();
        fs::write(root.join("target/junk"), "junk").unwrap();
        fs::write(root.join("app.py"), "print('hi')").unwrap();
        fs::write(root.join(".gitignore"), "node_modules/\ntarget/\n").unwrap();

        let out = run_at(root, 3);
        assert!(
            !entries(&out).contains(&"node_modules/"),
            "gitignored dir shown:\n{out}"
        );
        assert!(!entries(&out).contains(&"target/"), "gitignored dir shown:\n{out}");
        assert!(entries(&out).contains(&"app.py"), "a tracked file was hidden:\n{out}");
        assert!(out.contains(".gitignore"), "the applied rule should be named:\n{out}");
    }

    #[test]
    fn an_ignored_file_is_hidden_and_a_similar_name_is_not() {
        // Pattern specificity: `*.log` must not take `catalog.md` with it.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("debug.log"), "noise").unwrap();
        fs::write(root.join("catalog.md"), "keep").unwrap();
        fs::write(root.join(".gitignore"), "*.log\n").unwrap();

        let out = run_at(root, 1);
        assert!(
            !entries(&out).contains(&"debug.log"),
            "an ignored file was shown:\n{out}"
        );
        assert!(
            entries(&out).contains(&"catalog.md"),
            "an unrelated file was hidden:\n{out}"
        );
    }

    #[test]
    fn without_an_ignore_file_nothing_is_hidden_by_name() {
        // The old hard-coded list would have dropped `node_modules` here. With
        // the project's own rules in charge, a directory nobody ignored is
        // simply listed — which is the intended change, not a regression.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("node_modules")).unwrap();
        fs::write(root.join("node_modules/index.js"), "// js").unwrap();
        let out = run_at(root, 2);
        assert!(
            entries(&out).contains(&"node_modules/"),
            "with no ignore rules, nothing should be hidden by name:\n{out}"
        );
    }

    #[test]
    fn a_symlink_is_shown_and_never_followed() {
        let dir = sample();
        let out = run_at(dir.path(), 4);
        assert!(out.contains("srclink -> "), "symlink should show its target:\n{out}");
        // Following it would revisit `src` and duplicate its children; counting
        // them is the check.
        assert_eq!(
            entries(&out).iter().filter(|l| l.ends_with("lib.rs")).count(),
            1,
            "the symlink was followed:\n{out}"
        );
    }

    #[test]
    fn totals_count_what_was_shown() {
        let dir = sample();
        let out = run_at(dir.path(), 4);
        // At depth 4: src/, src/deep/, src/deep/deeper/, .github/,
        // .github/workflows/ = 5 directories. README.md, .editorconfig, lib.rs,
        // main.rs, leaf.txt, ci.yml = 6 files. One symlink. .git is skipped.
        assert!(out.contains("5 directories"), "directory count wrong:\n{out}");
        assert!(out.contains("6 files"), "file count wrong:\n{out}");
        assert!(out.contains("1 symlink"), "symlink count wrong:\n{out}");
        assert!(!out.contains("1 symlinks"), "bad pluralisation:\n{out}");
    }

    #[test]
    fn the_entry_budget_truncates_and_says_so() {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..20 {
            fs::write(dir.path().join(format!("f{i:02}.txt")), "x").unwrap();
        }
        let input = serde_json::json!({ "path": ".", "depth": 1, "max_entries": 5 });
        let out = run_with(&input, dir.path());
        assert_eq!(entries(&out).len(), 5, "budget not enforced:\n{out}");
        assert!(out.contains("not shown"), "truncation must be reported:\n{out}");
        assert!(out.contains("max_entries"), "the fix should be named:\n{out}");
    }

    #[test]
    fn too_deep_a_request_is_refused_not_clamped() {
        let dir = sample();
        let input = serde_json::json!({ "path": ".", "depth": MAX_DEPTH + 1 });
        let err = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(run(&input, dir.path()))
            .unwrap_err();
        assert!(err.contains("deeper than"), "{err}");
        assert!(
            err.contains(&MAX_DEPTH.to_string()),
            "the limit should be stated: {err}"
        );
    }

    #[test]
    fn depth_is_required_and_zero_is_refused() {
        let dir = sample();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let missing = serde_json::json!({ "path": "." });
        let err = runtime.block_on(run(&missing, dir.path())).unwrap_err();
        assert!(err.contains("missing depth"), "{err}");

        let zero = serde_json::json!({ "path": ".", "depth": 0 });
        let err = runtime.block_on(run(&zero, dir.path())).unwrap_err();
        assert!(err.contains("at least 1"), "{err}");
    }

    #[test]
    fn an_absurd_entry_budget_is_refused() {
        let dir = sample();
        let input = serde_json::json!({ "path": ".", "depth": 1, "max_entries": MAX_ENTRIES_CEILING + 1 });
        let err = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(run(&input, dir.path()))
            .unwrap_err();
        assert!(err.contains("ceiling"), "{err}");
    }

    #[test]
    fn a_file_or_a_missing_path_is_an_error_not_an_empty_listing() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("plain.txt");
        fs::write(&file, "x").unwrap();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        let input = serde_json::json!({ "path": "plain.txt", "depth": 1 });
        let err = runtime.block_on(run(&input, dir.path())).unwrap_err();
        assert!(err.contains("not a directory"), "{err}");

        let input = serde_json::json!({ "path": "nope", "depth": 1 });
        let err = runtime.block_on(run(&input, dir.path())).unwrap_err();
        assert!(err.contains("nope"), "the missing path should be named: {err}");
    }

    #[test]
    fn an_empty_directory_says_so_with_zeroes() {
        let dir = tempfile::tempdir().unwrap();
        let out = run_at(dir.path(), 2);
        assert!(out.contains("0 directories"), "{out}");
        assert!(out.contains("0 files"), "{out}");
    }

    #[test]
    fn the_header_is_the_directory_not_the_path_as_typed() {
        // The model reads this line and may reuse it, so a trailing `/.` would
        // be copied into the next request.
        let dir = sample();
        let input = serde_json::json!({ "path": "./", "depth": 1 });
        let out = run_with(&input, dir.path());
        let header = out.lines().next().unwrap();
        assert!(!header.ends_with("/."), "header kept a dot component: {header}");
        assert!(!header.ends_with("//"), "header kept a double slash: {header}");
        assert_eq!(Path::new(header), dir.path());
    }

    #[test]
    fn results_are_stable_across_runs() {
        // Deterministic order is what makes two listings comparable, and what
        // makes the `max_entries` cut reproducible.
        let dir = sample();
        let first = run_at(dir.path(), 3);
        for _ in 0..5 {
            assert_eq!(first, run_at(dir.path(), 3), "listing order is not stable");
        }
    }

    #[test]
    fn the_spec_advertises_the_same_limits_the_code_enforces() {
        // Drift here would make the model ask for something always refused.
        let spec = spec();
        let depth = &spec["input_schema"]["properties"]["depth"];
        assert_eq!(depth["maximum"], MAX_DEPTH);
        assert_eq!(depth["minimum"], 1);
        assert_eq!(spec["input_schema"]["required"], serde_json::json!(["path", "depth"]));
    }

    #[test]
    fn the_format_promise_is_made_in_the_spec_the_model_reads() {
        // The description is the only thing telling the model what the output
        // looks like, so the two claims callers depend on — relative paths, and
        // that a listed path is directly reusable — must be in it.
        let spec = spec();
        let description = spec["description"].as_str().unwrap();
        assert!(description.contains("one path per line"), "{description}");
        assert!(description.contains("relative"), "{description}");
        assert!(description.contains("straight to Read or Write"), "{description}");
    }

    #[test]
    fn show_ignored_includes_what_the_project_excludes() {
        // The escape hatch, and the way to tell "missing" from "ignored".
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("node_modules/pkg")).unwrap();
        fs::write(root.join("node_modules/pkg/index.js"), "// js").unwrap();
        fs::write(root.join("app.py"), "print('hi')").unwrap();
        fs::write(root.join(".gitignore"), "node_modules/\n").unwrap();

        let input = serde_json::json!({ "path": ".", "depth": 3, "show_ignored": true });
        let out = run_with(&input, root);
        let lines = entries(&out);
        // The directory *and* its contents: an ignored directory has to be
        // descended into, which is why the flag gates the walker rather than
        // filtering entries after the fact.
        assert!(lines.contains(&"node_modules/"), "{out}");
        assert!(lines.contains(&"node_modules/pkg/index.js"), "{out}");
        assert!(lines.contains(&"app.py"), "{out}");
        // And the note flips, so the listing cannot be mistaken for an
        // unqualified one where the file was genuinely absent.
        assert!(
            out.contains("show_ignored"),
            "the note should say rules were not applied:\n{out}"
        );
        assert!(
            !out.contains("ignore rules applied"),
            "an unqualified claim would contradict the listing:\n{out}"
        );
    }

    #[test]
    fn show_vcs_includes_version_control_directories() {
        let dir = sample();
        let input = serde_json::json!({ "path": ".", "depth": 2, "show_vcs": true });
        let out = run_with(&input, dir.path());
        assert!(entries(&out).contains(&".git/"), "{out}");
        assert!(entries(&out).contains(&".git/HEAD"), "{out}");
        // Nothing was pruned, so there is no skip line to report.
        assert!(!out.contains("version-control"), "no skip happened:\n{out}");
    }

    #[test]
    fn the_two_flags_are_independent() {
        // `show_vcs` must not imply `show_ignored` or the reverse: they answer
        // different questions, and an agent asking for one should not silently
        // get the other.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join(".git")).unwrap();
        fs::write(root.join(".git/HEAD"), "ref").unwrap();
        fs::create_dir_all(root.join("build")).unwrap();
        fs::write(root.join("build/out.bin"), "x").unwrap();
        fs::write(root.join(".gitignore"), "build/\n").unwrap();

        let only_vcs = run_with(&serde_json::json!({ "path": ".", "depth": 2, "show_vcs": true }), root);
        assert!(entries(&only_vcs).contains(&".git/"), "{only_vcs}");
        assert!(
            !entries(&only_vcs).contains(&"build/"),
            "show_vcs must not also show ignored paths:\n{only_vcs}"
        );

        let only_ignored = run_with(
            &serde_json::json!({ "path": ".", "depth": 2, "show_ignored": true }),
            root,
        );
        assert!(entries(&only_ignored).contains(&"build/"), "{only_ignored}");
        assert!(
            !entries(&only_ignored).contains(&".git/"),
            "show_ignored must not also show VCS directories:\n{only_ignored}"
        );
    }

    #[test]
    fn defaults_are_the_quiet_ones() {
        // Both flags absent, and both set to explicit false, must behave the
        // same — an agent that passes `"show_vcs": false` should get exactly
        // what it gets by omitting it.
        let dir = sample();
        let omitted = run_with(&serde_json::json!({ "path": ".", "depth": 2 }), dir.path());
        let explicit = run_with(
            &serde_json::json!({ "path": ".", "depth": 2, "show_vcs": false, "show_ignored": false }),
            dir.path(),
        );
        assert_eq!(omitted, explicit);
        assert!(!entries(&omitted).contains(&".git/"), "{omitted}");
    }

    #[test]
    fn a_non_boolean_flag_value_is_not_treated_as_true() {
        // The callers for these are widening the listing on purpose, so a
        // malformed value must fall back to the quiet default rather than flip
        // the flag on. `"false"` read as truthy is the classic way this goes
        // wrong, and `show_vcs` on by accident dumps a whole `.git`.
        let dir = sample();
        let out = run_with(
            &serde_json::json!({ "path": ".", "depth": 2, "show_vcs": "false", "show_ignored": 1 }),
            dir.path(),
        );
        assert!(!entries(&out).contains(&".git/"), "a string turned show_vcs on:\n{out}");
        assert!(!out.contains("show_ignored"), "a number turned show_ignored on:\n{out}");
    }

    #[test]
    fn listing_a_vcs_directory_directly_still_works() {
        // The root is never pruned: asking for `.git` explicitly is a deliberate
        // request, and refusing it would make the tool unable to answer a
        // question it can plainly see the answer to.
        let dir = sample();
        let input = serde_json::json!({ "path": ".git", "depth": 1 });
        let out = run_with(&input, dir.path());
        assert!(entries(&out).contains(&"HEAD"), "listing .git directly failed:\n{out}");
    }
}
