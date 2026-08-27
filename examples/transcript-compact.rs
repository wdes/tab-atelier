// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Dry-run runner for [`tab_atelier::transcript_compact`].
//!
//! Reads the live tabs from `~/.local/tab-atelier/tabs.json`, resolves each to
//! its Claude Code transcript under `~/.claude/projects/`, and reports what the
//! trim layers WOULD reclaim. Never writes a transcript — pure analysis.
//!
//!   cargo run --release --no-default-features --features headless \
//!     --example transcript-compact -- [--report | --tab NAME | --examples NAME]

use std::collections::HashMap;
use std::path::PathBuf;

use tab_atelier::transcript_compact as tc;

fn home() -> PathBuf {
    std::env::var_os("HOME").map_or_else(|| PathBuf::from("/root"), PathBuf::from)
}

fn mb(n: u64) -> String {
    format!("{:.1}M", n as f64 / 1_048_576.0)
}

/// (tab name, transcript path) for every live tab with a Claude session file.
fn live_tabs() -> Vec<(String, PathBuf)> {
    let tj = match std::fs::read_to_string(home().join(".local/tab-atelier/tabs.json")) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("read tabs.json: {e}");
            std::process::exit(1);
        }
    };
    let v: serde_json::Value = match serde_json::from_str(&tj) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("parse tabs.json: {e}");
            std::process::exit(1);
        }
    };

    let mut idx: HashMap<String, PathBuf> = HashMap::new();
    let proj = home().join(".claude/projects");
    if let Ok(dirs) = std::fs::read_dir(&proj) {
        for d in dirs.flatten() {
            if let Ok(files) = std::fs::read_dir(d.path()) {
                for f in files.flatten() {
                    let p = f.path();
                    if p.extension().and_then(|x| x.to_str()) == Some("jsonl")
                        && let Some(stem) = p.file_stem().and_then(|s| s.to_str())
                    {
                        idx.insert(stem.to_string(), p.clone());
                    }
                }
            }
        }
    }
    let mut out = Vec::new();
    for t in v["tabs"].as_array().cloned().unwrap_or_default() {
        if let Some(sid) = t.get("agent_session_id").and_then(|x| x.as_str())
            && let Some(p) = idx.get(sid)
        {
            let name = t.get("name").and_then(|n| n.as_str()).unwrap_or("?").to_string();
            out.push((name, p.clone()));
        }
    }
    out
}

fn solo_layers() -> Vec<(&'static str, tc::Config)> {
    vec![
        (
            "A dedup",
            tc::Config {
                dedup: true,
                ..tc::Config::default()
            },
        ),
        (
            "B drop-file-history",
            tc::Config {
                drop_file_history: true,
                ..tc::Config::default()
            },
        ),
        (
            "C drop-attachments",
            tc::Config {
                drop_attachments: true,
                ..tc::Config::default()
            },
        ),
        (
            "D strip-thinking(6)",
            tc::Config {
                keep_thinking: Some(6),
                ..tc::Config::default()
            },
        ),
        (
            "E cap-tools(8K)",
            tc::Config {
                tool_cap: Some(8192),
                ..tc::Config::default()
            },
        ),
        (
            "F drop-images(6)",
            tc::Config {
                keep_images: Some(6),
                ..tc::Config::default()
            },
        ),
    ]
}

fn report() {
    let tabs = live_tabs();
    println!(
        "Dry-run over {} live-tab transcripts. NO files are modified.",
        tabs.len()
    );
    println!("(debug build — serde_json is unoptimized here; a --release build scans ~10× faster.)\n");

    let solos = solo_layers();
    let presets = tc::presets();
    // One combined config list so each file is scanned in a SINGLE pass.
    let all_cfgs: Vec<tc::Config> = solos
        .iter()
        .map(|(_, c)| *c)
        .chain(presets.iter().map(|(_, c)| *c))
        .collect();
    let n_solo = solos.len();
    let mut solo_tot: Vec<u64> = vec![0; solos.len()];
    let mut preset_saved: Vec<u64> = vec![0; presets.len()];
    let mut orig_total = 0u64;
    let mut per_tab: Vec<(u64, String, u64, u64)> = Vec::new();

    let n = tabs.len();
    let start = std::time::Instant::now();
    let mut done_bytes = 0u64;
    for (idx, (name, path)) in tabs.iter().enumerate() {
        let text = std::fs::read_to_string(path).unwrap_or_default();
        done_bytes += text.len() as u64;
        // Live progress on stderr (kept off stdout so a `> report.txt` stays clean).
        eprint!(
            "\r  [{:>2}/{n}] {:<24} {:>6}  ({:.0} MB/s)      ",
            idx + 1,
            &name.chars().take(24).collect::<String>(),
            mb(text.len() as u64),
            done_bytes as f64 / 1_048_576.0 / start.elapsed().as_secs_f64().max(0.001),
        );
        let _ = std::io::Write::flush(&mut std::io::stderr());

        let records = tc::parse(&text);
        let orig_size = tc::size(&records);
        orig_total += orig_size;

        // Single pass scoring all solo + preset configs at once.
        let stats = tc::measure_batch(&records, &all_cfgs);
        for i in 0..n_solo {
            solo_tot[i] += stats[i].total();
        }
        for i in 0..presets.len() {
            preset_saved[i] += stats[n_solo + i].total();
        }
        let bsaved = stats[n_solo + 1].total(); // presets[1] == balanced
        per_tab.push((bsaved, name.clone(), orig_size, orig_size - bsaved));
    }
    eprintln!(
        "\r  scanned {n} tabs in {:.1}s{:<30}",
        start.elapsed().as_secs_f64(),
        ""
    );

    // Resume-safety spot-check: run the REAL apply + validate (the mutating,
    // re-parenting path) on the 3 biggest tabs under the most aggressive preset.
    let mut biggest: Vec<&(String, PathBuf)> = tabs.iter().collect();
    biggest.sort_by_key(|(_, p)| std::cmp::Reverse(std::fs::metadata(p).map_or(0, |m| m.len())));
    let aggressive = presets[3].1;
    let mut spot = Vec::new();
    for (name, path) in biggest.iter().take(3) {
        let text = std::fs::read_to_string(path).unwrap_or_default();
        let records = tc::parse(&text); // parse once
        let orig_uuids = tc::uuid_set(&records);
        let (out, _) = tc::apply(records, &aggressive); // consumes it
        let probs = tc::validate(&orig_uuids, &out);
        spot.push((name.clone(), probs.first().cloned()));
    }

    println!("── original total: {} ──", mb(orig_total));
    println!("── each layer ALONE (independent reclaim) ──");
    for (i, (label, _)) in solos.iter().enumerate() {
        let v = solo_tot[i];
        println!(
            "  {label:<22}{:>8}   ({:4.1}%)",
            mb(v),
            100.0 * v as f64 / orig_total as f64
        );
    }

    println!("\n── preset combinations (cumulative) ──");
    let layers = ["A,B,C", "A,B,C,D", "A,B,C,D,E", "A,B,C,D,E,F"];
    println!("  {:<12}{:>10}{:>10}{:>7}   layers", "preset", "new size", "saved", "%");
    for (i, (pname, _)) in presets.iter().enumerate() {
        let saved = preset_saved[i];
        println!(
            "  {pname:<12}{:>10}{:>10}{:>6.1}%   {}",
            mb(orig_total - saved),
            mb(saved),
            100.0 * saved as f64 / orig_total as f64,
            layers.get(i).unwrap_or(&"")
        );
    }
    println!("  (savings are a conservative floor — see `measure` docs)");

    println!("\n── resume-safety spot-check: real apply+validate, aggressive preset, 3 biggest tabs ──");
    for (name, prob) in &spot {
        match prob {
            None => println!("  {name:<22} ✓ every parent link intact"),
            Some(p) => println!("  {name:<22} ⚠ {p}"),
        }
    }

    per_tab.sort_by_key(|b| std::cmp::Reverse(b.0));
    println!("\n── top 10 tabs, balanced preset (orig → new) ──");
    for (saved, name, orig, new) in per_tab.iter().take(10) {
        println!(
            "  {name:<22}{:>8} → {:<8} saved {} ({:.0}%)",
            mb(*orig),
            mb(*new),
            mb(*saved),
            100.0 * *saved as f64 / *orig as f64
        );
    }
}

/// Per-layer breakdown + a validated before/after for one tab.
fn one_tab(query: &str, examples: bool) {
    let Some((name, path)) = live_tabs().into_iter().find(|(n, _)| n == query) else {
        eprintln!("no live tab named {query:?}");
        std::process::exit(1);
    };
    let text = std::fs::read_to_string(&path).unwrap_or_default();
    let orig_uuids = tc::uuid_set(&tc::parse(&text));
    let orig_size = tc::size(&tc::parse(&text));

    println!("tab {name:?}  ({})  {}\n", mb(orig_size), path.display());
    for (pname, cfg) in tc::presets() {
        let (out, st) = tc::apply(tc::parse(&text), &cfg);
        let new = tc::size(&out);
        let probs = tc::validate(&orig_uuids, &out);
        let safe = if probs.is_empty() {
            "✓ resume-safe"
        } else {
            &format!("⚠ {}", probs[0])
        };
        println!(
            "  {pname:<11}{:>8} → {:<8} saved {} ({:.0}%)   {safe}",
            mb(orig_size),
            mb(new),
            mb(orig_size - new),
            100.0 * (orig_size - new) as f64 / orig_size as f64
        );
        println!(
            "     layers: dedup {} · file-history {} · attachments {} · thinking {} · cap {} · images {}",
            mb(st.dedup),
            mb(st.file_history),
            mb(st.attachments),
            mb(st.thinking),
            mb(st.cap),
            mb(st.images)
        );
    }

    if examples {
        println!("\n── concrete items the trim touches (real, from this tab) ──");
        show_examples(&text);
    }
}

/// Print a few real records the layers act on, with sizes, so the effect is
/// tangible. Read-only.
fn show_examples(text: &str) {
    let recs = tc::parse(text);
    let mut shown_dup = false;
    let mut shown_think = false;
    let mut shown_cap = false;
    let mut shown_att: HashMap<String, u64> = HashMap::new();
    for r in &recs {
        let Some(v) = &r.value else { continue };
        if v.get("type").and_then(|t| t.as_str()) == Some("attachment")
            && let Some(k) = v.get("attachment").and_then(|a| a.get("type")).and_then(|t| t.as_str())
        {
            *shown_att.entry(k.to_string()).or_insert(0) += r.raw.len() as u64 + 1;
        }
        if !shown_dup && let Some(tur) = v.get("toolUseResult") {
            let tur_len = serde_json::to_string(tur).map_or(0, |s| s.len());
            let has_block = v
                .get("message")
                .and_then(|m| m.get("content"))
                .and_then(|c| c.as_array())
                .is_some_and(|a| {
                    a.iter()
                        .any(|b| b.get("type").and_then(|t| t.as_str()) == Some("tool_result"))
                });
            if has_block && tur_len > 2000 {
                println!(
                    "  • A dedup: a {tur_len} B `toolUseResult` alongside the same text in the tool_result block → dropped"
                );
                shown_dup = true;
            }
        }
        if let Some(blocks) = v
            .get("message")
            .and_then(|m| m.get("content"))
            .and_then(|c| c.as_array())
        {
            for b in blocks {
                let bt = b.get("type").and_then(|t| t.as_str()).unwrap_or("");
                if !shown_think && bt == "thinking" {
                    let n = serde_json::to_string(b).map_or(0, |s| s.len());
                    let head: String = b
                        .get("thinking")
                        .and_then(|t| t.as_str())
                        .unwrap_or("")
                        .chars()
                        .take(90)
                        .collect();
                    println!(
                        "  • D thinking: a {n} B block — “{}…” → stripped on old turns",
                        head.replace('\n', " ")
                    );
                    shown_think = true;
                }
                if !shown_cap && bt == "tool_result" {
                    let n = b
                        .get("content")
                        .map_or(0, |c| serde_json::to_string(c).map_or(0, |s| s.len()));
                    if n > 8192 {
                        println!("  • E cap: a {n} B tool output → head+tail 8 KB with a trimmed marker");
                        shown_cap = true;
                    }
                }
            }
        }
    }
    if !shown_att.is_empty() {
        let mut kinds: Vec<_> = shown_att.into_iter().collect();
        kinds.sort_by_key(|b| std::cmp::Reverse(b.1));
        let top: Vec<String> = kinds.iter().take(5).map(|(k, v)| format!("{k} {}", mb(*v))).collect();
        println!("  • C attachments dropped, by kind: {}", top.join(" · "));
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("--tab") => one_tab(args.get(1).map_or("", String::as_str), false),
        Some("--examples") => one_tab(args.get(1).map_or("", String::as_str), true),
        _ => report(),
    }
}
