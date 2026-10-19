// SPDX-License-Identifier: MPL-2.0

//! `tab-atelier catalog` — list the skills, retire one, bring one back.
//!
//! The Kiosk already renders the catalogue and its retire/restore buttons. What it could not do is
//! let a shell do it, so "retire the skill that keeps producing bad output" meant opening a browser
//! on the one machine that runs the Kiosk. This is that path.
//!
//! It works on the catalogue **file**, not through the daemon: a skill is the operator's notes
//! about their own fleet, not fleet state, so there is nothing to coordinate and no reason to grow
//! an HTTP surface for it. The Kiosk reads the same file through the same crate.
//!
//! Retire is a tombstone, not a delete — `set_deleted` marks it so the card stops being offered
//! while the prompt it recorded survives. That is deliberate: the thing being retired is the
//! *suggestion* to use a profile, and the reason to keep the text is that a later agent may need to
//! know what the fleet had been told.

/// `tab-atelier catalog <list|retire|restore>`.
#[derive(clap::Parser, Debug)]
#[command(
    name = "tab-atelier catalog",
    about = "List the skills, or retire one from circulation",
    long_about = "The skills catalogue: the distilled profiles the fleet spawns from. `retire` is a \
                  tombstone — the profile stops being offered, and what it recorded is kept.",
    after_help = "Examples:\n  \
                  tab-atelier catalog list\n  \
                  tab-atelier catalog list --all\n  \
                  tab-atelier catalog retire swift-reviewer\n  \
                  tab-atelier catalog restore swift-reviewer"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Verb,
}

#[derive(clap::Subcommand, Debug)]
enum Verb {
    /// The skills on offer.
    List {
        /// Include the retired ones.
        #[arg(long)]
        all: bool,
        /// One skill, as JSON.
        #[arg(long)]
        skill: Option<String>,
    },
    /// Stop offering a skill. A tombstone: nothing is erased.
    Retire {
        /// The skill's name.
        skill: String,
    },
    /// Put a retired skill back on offer.
    Restore {
        /// The skill's name.
        skill: String,
    },
}

/// Where the catalogue lives: the same variable the Kiosk reads, then the state directory.
///
/// Read here rather than through the API because the file is the interface — see the module docs.
fn catalog_path() -> std::path::PathBuf {
    std::env::var("TAB_ATELIER_CATALOG").map_or_else(
        |_| crate::state_dir(&crate::platform::state_base_dir()).join("catalog.jsonl"),
        std::path::PathBuf::from,
    )
}

#[must_use]
pub fn run(args: &[String]) -> i32 {
    let cli = match super::parse::<Cli>("tab-atelier catalog", args) {
        Ok(c) => c,
        Err(code) => return code,
    };
    let catalog = tab_atelier_catalog::Catalog::new(catalog_path());
    match cli.cmd {
        Verb::List { all, skill } => list(&catalog, all, skill.as_deref()),
        Verb::Retire { skill } => set_retired(&catalog, &skill, true),
        Verb::Restore { skill } => set_retired(&catalog, &skill, false),
    }
}

fn list(catalog: &tab_atelier_catalog::Catalog, all: bool, only: Option<&str>) -> i32 {
    let Ok(skills) = catalog.list(all) else {
        // A missing file is not an error: a fresh install has no catalogue yet, and reporting
        // "cannot read" for "there is nothing yet" would send an operator looking for a bug.
        println!("no skills yet ({})", catalog.path.display());
        return 0;
    };
    if let Some(want) = only {
        let Some(found) = skills.iter().find(|s| s.skill == want) else {
            eprintln!("catalog list: no skill named {want}");
            return 1;
        };
        return match serde_json::to_string_pretty(found) {
            Ok(pretty) => {
                println!("{pretty}");
                0
            }
            Err(e) => {
                eprintln!("catalog list: {e}");
                1
            }
        };
    }
    if skills.is_empty() {
        println!("no skills on offer");
        return 0;
    }
    // Name first, then the one-line specialty: enough to recognise a profile, and `--skill` is one
    // flag away for the rest.
    for s in skills {
        let specialty = s.specialty.as_deref().unwrap_or("");
        let version = s.prompt_version.map(|v| format!(" v{v}")).unwrap_or_default();
        let retired = if s.deleted { " (retiré)" } else { "" };
        if specialty.is_empty() {
            println!("{}{version}{retired}", s.skill);
        } else {
            println!("{}{version}{retired}\\t{specialty}", s.skill);
        }
    }
    0
}

/// Retire or restore, and say which of the two things happened.
///
/// Retiring something already retired is a no-op, not an error — a script that retires a list must
/// not stop halfway because one entry was handled last time.
fn set_retired(catalog: &tab_atelier_catalog::Catalog, skill: &str, retire: bool) -> i32 {
    match catalog.set_deleted(skill, retire) {
        Ok(_) => {
            if retire {
                println!("✓ {skill} retiré — la fiche n'est plus proposée, rien n'est effacé");
            } else {
                println!("✓ {skill} de nouveau proposé");
            }
            0
        }
        Err(tab_atelier_catalog::CatalogError::NotFound) => {
            eprintln!("catalog: no skill named {skill} ({})", catalog.path.display());
            1
        }
        Err(e) => {
            eprintln!("catalog: {e}");
            1
        }
    }
}
