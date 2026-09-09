// @licence MPL-2.0 https://mozilla.org/MPL/2.0/

//! `tab-atelier brief [--cwd <dir>] [--json]` — what an agent starting here
//! would be told.
//!
//! The brief is assembled at session start and never shown again, which makes
//! a wrong `baseDir` invisible: the agent simply never mentions the thing you
//! wrote. This prints the same text the hook would inject, so a rule can be
//! checked rather than guessed at.

use std::path::PathBuf;

fn usage() {
    eprintln!(
        "usage: tab-atelier brief [--cwd <dir>] [--json] [--list]\n\n\
         Prints what a Claude session starting in <dir> (default: here) is told.\n\
         --list   show which brief files matched, and from where\n\
         --json   the exact SessionStart payload the hook prints\n\n\
         Project briefs are .md files with front matter, read from:\n  \
         ~/.config/tab-atelier/briefs/   and   /etc/tab-atelier/briefs/\n\n\
         ---\n  \
         baseDir: /mnt/clients/ABCD\n  \
         ---\n  \
         Client ABCD: PHP 7.4. Deploys are manual — never push to production."
    );
}

#[must_use]
pub fn run(args: &[String]) -> i32 {
    let mut cwd: Option<PathBuf> = None;
    let (mut json, mut list) = (false, false);
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--json" => json = true,
            "--list" | "-l" => list = true,
            "--cwd" => {
                i += 1;
                let Some(v) = args.get(i) else {
                    eprintln!("brief: --cwd expects a directory");
                    return 2;
                };
                cwd = Some(PathBuf::from(v));
            }
            "-h" | "--help" => {
                usage();
                return 0;
            }
            other => {
                eprintln!("brief: unknown argument: {other}");
                return 2;
            }
        }
        i += 1;
    }
    let here = cwd.or_else(|| std::env::current_dir().ok()).unwrap_or_default();
    let all = crate::briefs::load_all();
    let selected = crate::briefs::select(&all, &here);

    if list {
        println!("brief directories:");
        for d in crate::briefs::brief_dirs() {
            let state = if d.is_dir() { "" } else { " (absent)" };
            println!("  {}{state}", d.display());
        }
        println!("{} brief file(s) loaded", all.len());
        if selected.is_empty() {
            println!("no project brief matches {}", here.display());
        } else {
            println!("matching {} (least specific first):", here.display());
            for b in &selected {
                println!("  {} — baseDir {:?}", b.source.display(), b.base_dirs);
            }
        }
        return 0;
    }

    let mut text = crate::cli::claude_hook::agent_brief();
    let project = crate::briefs::render(&selected, crate::briefs::MAX_TOTAL);
    if !project.is_empty() {
        text.push_str("\n\n");
        text.push_str(&project);
    }
    if json {
        println!("{}", crate::cli::claude_hook::brief_json(&text));
    } else {
        println!("{text}");
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn args_are_validated() {
        assert_eq!(run(&argv(&["--cwd"])), 2);
        assert_eq!(run(&argv(&["--nope"])), 2);
        assert_eq!(run(&argv(&["--help"])), 0);
        // A directory with no matching brief still prints the built-in one,
        // rather than nothing at all.
        assert_eq!(run(&argv(&["--cwd", "/nonexistent-dir-for-test"])), 0);
    }
}
