// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Per-tab network metering — the unprivileged, universal half.
//!
//! Counts a tab's **active outbound connections** by matching the socket
//! inodes its process subtree owns (`/proc/<pid>/fd`) against the kernel's
//! connection tables (`/proc/net/{tcp,tcp6,udp,udp6}`). No privilege, no
//! nftables — works in both the desktop GUI and the headless service.
//!
//! Byte counts (TX, allowed vs denied) come from nftables counters instead
//! (privileged, headless — see `net_nft`); this module is just connections.
//!
//! The expensive scans (the process tree, every `/proc/.../fd`, the four
//! net tables) are done **once per refresh for all tabs** by
//! [`connection_counts`], not per-tab, so cost is O(processes + sockets)
//! regardless of tab count. Callers should still throttle it (a few seconds
//! between refreshes) rather than run it on every UI tick.

#![cfg(target_os = "linux")]

use std::collections::{HashMap, HashSet};

/// Count a tab's active connections in one `/proc/net/{tcp,udp}` table.
///
/// Counts entries whose socket inode is in `inodes` **and** that have a real
/// remote endpoint (a non-zero remote port — so listeners and unconnected
/// sockets don't inflate the number). Pure over the table text so it's
/// unit-testable.
#[must_use]
pub fn count_connections<S: std::hash::BuildHasher>(table: &str, inodes: &HashSet<u64, S>) -> usize {
    table
        .lines()
        .skip(1) // header row
        .filter(|line| {
            let f: Vec<&str> = line.split_whitespace().collect();
            // sl(0) local(1) rem(2) st(3) … uid(7) timeout(8) inode(9)
            if f.len() < 10 {
                return false;
            }
            // Remote endpoint present? `IP:PORT` in hex; "…:0000" ⇒ none.
            let has_remote = f[2].rsplit(':').next().is_some_and(|port| port != "0000");
            if !has_remote {
                return false;
            }
            f[9].parse::<u64>().is_ok_and(|inode| inodes.contains(&inode))
        })
        .count()
}

/// Socket inodes held by `pid` — reads `/proc/<pid>/fd/*`, whose symlink
/// targets look like `socket:[12345]`. Silently yields nothing for a pid
/// whose `fd` dir can't be read (gone, or not ours).
fn socket_inodes(pid: u32, out: &mut HashSet<u64>) {
    let Ok(entries) = std::fs::read_dir(format!("/proc/{pid}/fd")) else {
        return;
    };
    for entry in entries.flatten() {
        if let Ok(target) = std::fs::read_link(entry.path())
            && let Some(inode) = target.to_str().and_then(parse_socket_link)
        {
            out.insert(inode);
        }
    }
}

/// `socket:[12345]` → `12345`.
fn parse_socket_link(link: &str) -> Option<u64> {
    link.strip_prefix("socket:[")?.strip_suffix(']')?.parse().ok()
}

/// Build a `pid -> ppid` map from `/proc/<pid>/stat` for every process. Used
/// to gather a tab's descendant pids from its shell's root pid.
fn ppid_map() -> HashMap<u32, u32> {
    let mut map = HashMap::new();
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return map;
    };
    for entry in entries.flatten() {
        let Some(pid) = entry.file_name().to_str().and_then(|s| s.parse::<u32>().ok()) else {
            continue;
        };
        if let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat"))
            && let Some(ppid) = parse_ppid(&stat)
        {
            map.insert(pid, ppid);
        }
    }
    map
}

/// Extract ppid (the 4th field) from a `/proc/<pid>/stat` line. The 2nd
/// field — `comm` — is parenthesised and may contain spaces/`)`, so split
/// after the LAST `)`.
fn parse_ppid(stat: &str) -> Option<u32> {
    let after = &stat[stat.rfind(')')? + 1..];
    // after = " S <ppid> ..." → state is the 1st token, ppid the 2nd.
    after.split_whitespace().nth(1)?.parse().ok()
}

/// All descendants of `root` (inclusive), per the `pid -> ppid` map.
fn descendants(root: u32, ppids: &HashMap<u32, u32>) -> Vec<u32> {
    let mut want = HashSet::from([root]);
    // Iterate to a fixed point — a child whose parent is already wanted gets
    // added; repeat until no growth. Process trees are shallow so this
    // converges in a couple of passes.
    loop {
        let before = want.len();
        for (&pid, &ppid) in ppids {
            if want.contains(&ppid) {
                want.insert(pid);
            }
        }
        if want.len() == before {
            break;
        }
    }
    want.into_iter().collect()
}

/// Active outbound connection count per tab, keyed by tab id. `roots` is
/// each tab's shell root pid. Reads the four net tables + the process tree
/// once for the whole batch.
#[must_use]
pub fn connection_counts(roots: &[(String, u32)]) -> HashMap<String, usize> {
    let mut result = HashMap::new();
    if roots.is_empty() {
        return result;
    }
    let tables: String = ["tcp", "tcp6", "udp", "udp6"]
        .iter()
        .filter_map(|t| std::fs::read_to_string(format!("/proc/net/{t}")).ok())
        .collect::<Vec<_>>()
        .join("\n");
    let ppids = ppid_map();
    for (id, root) in roots {
        let mut inodes = HashSet::new();
        for pid in descendants(*root, &ppids) {
            socket_inodes(pid, &mut inodes);
        }
        // Each of the four tables has its own header line; `count_connections`
        // skips one. We joined them, so re-split per original table would be
        // cleaner — but the joined header lines simply never match an inode,
        // so they're harmless. Count across the joined text.
        result.insert(id.clone(), count_connections(&tables, &inodes));
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    // Two real-ish /proc/net/tcp rows: one ESTABLISHED to 8.8.8.8:443
    // (inode 111), one LISTEN (inode 222, remote :0000).
    const TABLE: &str = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n   0: 0100007F:8B1E 08080808:01BB 01 00000000:00000000 00:00000000 00000000  1000        0 111 1 0000 0\n   1: 00000000:1F90 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 222 1 0000 0";

    #[test]
    fn counts_only_connected_sockets_we_own() {
        let mut ours = HashSet::new();
        ours.insert(111u64); // the established one
        ours.insert(222u64); // the listener
        // Listener excluded (no remote), so only the established counts.
        assert_eq!(count_connections(TABLE, &ours), 1);
    }

    #[test]
    fn ignores_inodes_not_ours() {
        let ours = HashSet::from([999u64]);
        assert_eq!(count_connections(TABLE, &ours), 0);
    }

    #[test]
    fn empty_inode_set_is_zero() {
        assert_eq!(count_connections(TABLE, &HashSet::new()), 0);
    }

    #[test]
    fn parse_socket_link_works() {
        assert_eq!(parse_socket_link("socket:[12345]"), Some(12345));
        assert_eq!(parse_socket_link("pipe:[1]"), None);
        assert_eq!(parse_socket_link("/dev/null"), None);
    }

    #[test]
    fn parse_ppid_handles_paren_comm() {
        // comm with spaces and a `)` inside must not fool the parser.
        let stat = "1234 (weird )name) S 1000 1234 1234 0 -1 ...";
        assert_eq!(parse_ppid(stat), Some(1000));
    }

    #[test]
    fn descendants_follows_the_tree() {
        // 1 -> 2 -> 3, plus unrelated 9 -> 8.
        let ppids = HashMap::from([(2u32, 1u32), (3, 2), (8, 9)]);
        let mut got = descendants(1, &ppids);
        got.sort_unstable();
        assert_eq!(got, vec![1, 2, 3]);
    }
}
