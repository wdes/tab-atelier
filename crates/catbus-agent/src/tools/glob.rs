// SPDX-License-Identifier: MPL-2.0

//! Glob patterns, shared by the tools that accept one.
//!
//! `FileTree`'s `glob` action and `Grep`'s `glob` filter ask the same question — does this path
//! match this pattern — and they have to answer it the same way, or the file an agent finds with
//! one is a file it cannot search with the other.
//!
//! The matcher is [`globset`], the one ripgrep uses for `-g`, with `literal_separator` set so a
//! single `*` stops at a path separator the way it does in ripgrep and `.gitignore` — `**` is what
//! crosses directories, and `{a,b}` is an alternation. (globset's default lets `*` cross separators,
//! which is not what anyone means by a glob.)
//!
//! A pattern is matched against **both** the path relative to the searched root and the file name
//! alone. That is the forgiving reading, and it is deliberate: `*.rs` is what people write meaning
//! "every Rust file", and matching only the full path would make it select nothing in a subdirectory
//! — the opposite of what was asked, and silently. `src/**/*.rs` still works, because the path form
//! is tried as well. `*` does not cross a separator, so a bare `*.rs` cannot leak into a
//! subdirectory through the path form; it reaches those files through the name form, which is
//! exactly the intent.

use std::path::Path;

/// Compile `pattern`, or explain why it cannot be.
///
/// The error names the escaping, because a glob's metacharacters are exactly the ones a path
/// contains: someone searching for a literal `[` has to be told that `[` opens a class.
pub fn matcher(pattern: &str) -> Result<globset::GlobMatcher, String> {
    globset::GlobBuilder::new(pattern)
        // `*` stops at a separator, `**` crosses one — which is what everyone means by a glob, and
        // what ripgrep and `.gitignore` do. This is *not* globset's default: without it a single `*`
        // silently crosses directories, so `src/*.rs` would match `src/deep/nested/main.rs` and a
        // pattern written to keep a search shallow would quietly search the whole tree.
        .literal_separator(true)
        .build()
        .map(|glob| glob.compile_matcher())
        .map_err(|why| {
            format!(
                "`{pattern}` is not a valid glob: {why}. `*` matches within one path segment, `**` \
                 crosses separators, and `{{a,b}}` is an alternation — escape `*`, `?`, `[` and `{{` \
                 with a backslash to match the characters themselves."
            )
        })
}

/// Whether `relative` matches, by its path or by its file name.
#[must_use]
pub fn matches(matcher: &globset::GlobMatcher, relative: &Path) -> bool {
    if matcher.is_match(relative) {
        return true;
    }
    relative
        .file_name()
        .is_some_and(|name| matcher.is_match(Path::new(name)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn matched(pattern: &str, path: &str) -> bool {
        let matcher = matcher(pattern).expect("a valid glob");
        matches(&matcher, &PathBuf::from(path))
    }

    /// A bare extension glob selects files in subdirectories, which is what it is written to mean.
    /// Matching only the full path would make `*.rs` miss `src/main.rs` — silently, and so a search
    /// would report "no matches" over a directory full of matches.
    #[test]
    fn a_bare_glob_matches_by_file_name() {
        assert!(matched("*.rs", "main.rs"));
        assert!(matched("*.rs", "src/main.rs"));
        assert!(matched("*.rs", "src/deep/nested/main.rs"));
        assert!(!matched("*.rs", "src/main.ts"));
        // And it still cannot cross a separator through the path form: a `.rs` directory name must
        // not drag in everything below it.
        assert!(!matched("*.rs", "weird.rs/inside.txt"));
    }

    /// A leading `**/` is the idiom for "at any depth", and it works because the path form is tried
    /// as well as the name form.
    #[test]
    fn a_double_star_crosses_directories() {
        assert!(matched("**/*.rs", "src/main.rs"));
        assert!(matched("**/*.rs", "a/b/c/d.rs"));
        // The path form with a single `*` stops at the separator, so a nested file is not matched
        // by a pattern that did not ask to cross one — unless the name form catches it, which it
        // does not here because the pattern is anchored to the path's shape.
        assert!(matched("src/*.rs", "src/main.rs"));
        assert!(!matched("src/*.rs", "src/deep/main.rs"));
    }

    /// An alternation is the shorthand for "these extensions", which is the other thing a caller
    /// reaches for.
    #[test]
    fn braces_are_an_alternation() {
        assert!(matched("*.{rs,toml}", "Cargo.toml"));
        assert!(matched("*.{rs,toml}", "src/main.rs"));
        assert!(!matched("*.{rs,toml}", "README.md"));
    }

    /// A glob that cannot compile is refused with a message that teaches the escaping, because the
    /// characters that break a glob are the ones a path contains.
    #[test]
    fn a_bad_glob_is_refused_with_an_explanation() {
        let why = matcher("[unclosed").expect_err("must not compile");
        assert!(why.contains("not a valid glob"), "{why}");
        assert!(why.contains("escape"), "the message must teach the escaping: {why}");
    }

    /// An exact path and an exact name both work, so a caller with a precise target is not forced
    /// into a wildcard.
    #[test]
    fn an_exact_name_matches() {
        assert!(matched("main.rs", "src/main.rs"));
        assert!(matched("src/main.rs", "src/main.rs"));
        assert!(!matched("main.rs", "src/other.rs"));
    }
}
