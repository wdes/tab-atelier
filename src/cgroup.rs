// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Per-tab cgroup v2 resource limits (used by both binaries).
//!
//! ## Why
//!
//! Every tab is a child of one process (the `tab-atelier-headless.service`,
//! or the GUI app), so a unit-level `MemoryMax=` would limit *all tabs
//! together*. To cap
//! a single tab's memory / CPU / task count we put each tab's shell in
//! its own cgroup under the service's **delegated** subtree and write
//! that cgroup's `memory.max` / `cpu.max` / `pids.max`.
//!
//! ## Requirements
//!
//! - cgroup v2 (Debian 13 default).
//! - `Delegate=yes` on the unit, so systemd hands the service write
//!   access to its own cgroup subtree (and turns off the read-only
//!   `/sys/fs/cgroup` that `ProtectControlGroups=` would otherwise set
//!   for the owned subtree).
//!
//! ## cgroup v2 "no internal processes" rule
//!
//! A non-root cgroup may hold *either* processes *or* child cgroups
//! with controllers enabled — not both. The daemon's own PID lives in
//! the delegated cgroup, so before creating per-tab children we move
//! the daemon into a `supervisor/` leaf and enable the controllers on
//! the parent. Only then can each tab get its own limited cgroup.
//!
//! ## Safety / degradation
//!
//! Everything here is best-effort. If delegation isn't set up (running
//! outside systemd, cgroup v1, missing `Delegate=`, denied writes), the
//! init disables limiting and [`apply`] becomes a no-op — tabs still
//! spawn normally, just unlimited. Nothing here can fail a tab spawn.
//!
//! ## Shared by both binaries
//!
//! The GUI (`tab-atelier`) and the headless daemon (`tab-atelier-headless`)
//! both use the `init` + `apply`/`reapply` here, so per-tab limits behave
//! identically whichever binary owns the tab. The tab-LIFECYCLE helpers below
//! (`ensure_tab`/`kill_tab`/`reap_stale_tabs`/`prepare_tab_cgroup`/
//! `move_pid_to_tab_cgroup`) are the headless daemon's own teardown + nftables
//! wiring and are gated `not(feature = "gui")` — the GUI drives its tabs a
//! different way and would only get dead-code warnings for them.

#![cfg(target_os = "linux")]

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use log::{debug, info, warn};

use crate::TabResourceLimits;

const CGROUP_ROOT: &str = "/sys/fs/cgroup";

/// The delegated cgroup directory under which we create per-tab
/// children, resolved once at [`init`]. `None` when delegation isn't
/// available, which makes [`apply`] a no-op.
static DELEGATED_BASE: OnceLock<Option<PathBuf>> = OnceLock::new();

/// Read this process's cgroup v2 path from `/proc/self/cgroup`.
///
/// The v2 line is `0::/system.slice/tab-atelier-headless.service`; we
/// join the relative part onto [`CGROUP_ROOT`]. Returns `None` on a
/// cgroup v1 / hybrid layout (no `0::` line) or a read error.
fn own_cgroup_dir() -> Option<PathBuf> {
    let content = std::fs::read_to_string("/proc/self/cgroup").ok()?;
    for line in content.lines() {
        // Format: hierarchy-ID:controller-list:path. v2 is `0::<path>`.
        if let Some(rel) = line.strip_prefix("0::") {
            let rel = rel.trim_start_matches('/');
            return Some(Path::new(CGROUP_ROOT).join(rel));
        }
    }
    None
}

/// Set up cgroup delegation for per-tab limits. Idempotent; call once at
/// daemon startup before any tab spawns. Safe to call when limits are
/// never used — it only does work if it can.
///
/// `wanted` is true when at least one global/per-tab limit could apply;
/// when false we skip the whole dance so an unconfigured daemon doesn't
/// touch cgroups at all.
pub fn init(wanted: bool) {
    let resolved = if wanted { setup() } else { None };
    let _ = DELEGATED_BASE.set(resolved);
}

/// Attempt the delegation setup, returning the base dir for per-tab
/// cgroups on success.
fn setup() -> Option<PathBuf> {
    let base = own_cgroup_dir()?;
    if !base.is_dir() {
        debug!("cgroup: own cgroup {} not a directory; limits disabled", base.display());
        return None;
    }
    // Probe writability of the delegated subtree by trying to create a
    // throwaway child cgroup. If delegation isn't set up this fails with
    // EROFS/EACCES and we disable limiting cleanly.
    let probe = base.join("tab-atelier.probe");
    if std::fs::create_dir(&probe).is_err() {
        debug!(
            "cgroup: {} not writable (no Delegate=yes / cgroup v1?); per-tab limits disabled",
            base.display()
        );
        return None;
    }
    let _ = std::fs::remove_dir(&probe);

    // Move our own (multi-threaded) process into a `supervisor` leaf so
    // the delegated cgroup itself holds no processes — required before
    // enabling controllers on it for the per-tab children.
    let supervisor = base.join("supervisor");
    let _ = std::fs::create_dir(&supervisor);
    if write_cgroup(&supervisor.join("cgroup.procs"), &std::process::id().to_string()).is_err() {
        warn!("cgroup: could not move daemon into supervisor leaf; per-tab limits disabled");
        return None;
    }
    // Enable the controllers we hand to children. Best-effort per
    // controller — a kernel without one just means that axis won't take.
    for ctrl in ["+memory", "+cpu", "+pids"] {
        let _ = write_cgroup(&base.join("cgroup.subtree_control"), ctrl);
    }
    info!("cgroup: per-tab resource limits enabled under {}", base.display());
    Some(base)
}

/// Apply `limits` to a tab's process tree by moving `pid` into a fresh
/// per-tab cgroup with the requested ceilings written. Best-effort and
/// silent-ish: a failure logs at debug and leaves the tab unlimited.
///
/// No-op when limiting is disabled or `limits` is empty.
pub fn apply(tab_id: &str, pid: u32, limits: &TabResourceLimits) {
    if limits.is_empty() {
        return;
    }
    let Some(Some(base)) = DELEGATED_BASE.get() else {
        return;
    };
    // Per-tab cgroup name — sanitise the id to a safe path component.
    let dir = base.join(format!("tab-{}", sanitize_id(tab_id)));
    if std::fs::create_dir_all(&dir).is_err() {
        debug!("cgroup: could not create {}; tab {tab_id} unlimited", dir.display());
        return;
    }
    if let Some(bytes) = limits.memory_max_bytes() {
        let _ = write_cgroup(&dir.join("memory.max"), &bytes.to_string());
    }
    if let Some(line) = limits.cpu_max_line() {
        let _ = write_cgroup(&dir.join("cpu.max"), &line);
    }
    if let Some(tasks) = limits.tasks_max {
        let _ = write_cgroup(&dir.join("pids.max"), &tasks.to_string());
    }
    // Move the tab's shell into its cgroup last, so the limits are in
    // force before it can fork. Descendants inherit the cgroup.
    if write_cgroup(&dir.join("cgroup.procs"), &pid.to_string()).is_err() {
        debug!("cgroup: could not move pid {pid} (tab {tab_id}) into {}", dir.display());
    } else {
        debug!(
            "cgroup: applied limits to tab {tab_id} (pid {pid}) at {}",
            dir.display()
        );
    }
}

/// Re-apply `limits` to a tab's EXISTING cgroup at runtime (a `tab-atelier
/// limit …` / `POST /tabs/<id>/limits` change on a *live* tab).
///
/// Unlike [`apply`] — spawn-time, a no-op when empty — this writes EVERY axis
/// unconditionally, resetting any that's now unset back to `max`, so *lifting*
/// a limit actually takes effect (writing only the `Some` axes would leave a
/// previously-set ceiling in place). Creates the tab cgroup if the tab predates
/// limiting, and re-adds the pid (a no-op if it's already there). No-op only
/// when delegation is off. Same code drives both the GUI and headless.
pub fn reapply(tab_id: &str, pid: u32, limits: &TabResourceLimits) {
    let Some(Some(base)) = DELEGATED_BASE.get() else {
        return;
    };
    let dir = base.join(format!("tab-{}", sanitize_id(tab_id)));
    if std::fs::create_dir_all(&dir).is_err() {
        debug!("cgroup: could not create {}; tab {tab_id} left as-is", dir.display());
        return;
    }
    let _ = write_cgroup(
        &dir.join("memory.max"),
        &limits
            .memory_max_bytes()
            .map_or_else(|| "max".to_string(), |b| b.to_string()),
    );
    let _ = write_cgroup(
        &dir.join("cpu.max"),
        &limits.cpu_max_line().unwrap_or_else(|| "max 100000".to_string()),
    );
    let _ = write_cgroup(
        &dir.join("pids.max"),
        &limits.tasks_max.map_or_else(|| "max".to_string(), |t| t.to_string()),
    );
    let _ = write_cgroup(&dir.join("cgroup.procs"), &pid.to_string());
    debug!("cgroup: re-applied limits to tab {tab_id} (pid {pid})");
}

/// Create a tab's cgroup (empty) and return its path **relative to the
/// cgroup v2 mount** (e.g. `system.slice/tab-atelier-headless.service/tab-<id>`)
/// for nftables' `socket cgroupv2` match. The path is deterministic and
/// needs no pid, so the caller can install nft rules against it **before**
/// the tab's shell is spawned — there is no unconfined window. Pair with
/// [`move_pid_to_tab_cgroup`] once the pid exists. `None` when delegation
/// isn't set up. Idempotent.
#[must_use]
#[cfg(not(feature = "gui"))]
pub fn prepare_tab_cgroup(tab_id: &str) -> Option<String> {
    let Some(Some(base)) = DELEGATED_BASE.get() else {
        return None;
    };
    let dir = base.join(format!("tab-{}", sanitize_id(tab_id)));
    std::fs::create_dir_all(&dir).ok()?;
    dir.strip_prefix(CGROUP_ROOT)
        .ok()
        .map(|rel| rel.to_string_lossy().into_owned())
}

#[cfg(not(feature = "gui"))]
/// Move `pid` into the tab's (already [`prepare_tab_cgroup`]d) cgroup, so
/// the nft rules keyed on it take effect. Best-effort: `false` if delegation
/// is off or the write fails.
pub fn move_pid_to_tab_cgroup(tab_id: &str, pid: u32) -> bool {
    let Some(Some(base)) = DELEGATED_BASE.get() else {
        return false;
    };
    let dir = base.join(format!("tab-{}", sanitize_id(tab_id)));
    write_cgroup(&dir.join("cgroup.procs"), &pid.to_string()).is_ok()
}

#[cfg(not(feature = "gui"))]
/// Ensure `tab_id` has its own cgroup and `pid` is in it. Idempotent.
///
/// EVERY tab gets its own cgroup — even one with no resource limits — so its
/// whole process subtree can be reliably torn down via [`kill_tab`] and reaped
/// on the next startup ([`reap_stale_tabs`]). Without this a limit-less tab's
/// shell stays in the daemon's `supervisor` leaf, so its orphaned children
/// (a `claude --resume …` that ignored SIGHUP) couldn't be killed by cgroup
/// without also killing the daemon. Best-effort no-op when delegation is off.
pub fn ensure_tab(tab_id: &str, pid: u32) {
    if prepare_tab_cgroup(tab_id).is_some() {
        move_pid_to_tab_cgroup(tab_id, pid);
    }
}

#[cfg(not(feature = "gui"))]
/// Kill a tab's ENTIRE process subtree and remove its cgroup. Best-effort.
///
/// Writes `1` to the tab cgroup's `cgroup.kill` — a cgroup-v2 atomic SIGKILL
/// of every process in the subtree, immune to the SIGHUP-survival that lets
/// `claude` (Node) and its detached tool/MCP subprocesses orphan. Then removes
/// the (now-empty) cgroup dir. No-op when delegation is off or the cgroup is
/// already gone. Returns `true` if the kill was issued.
pub fn kill_tab(tab_id: &str) -> bool {
    let Some(Some(base)) = DELEGATED_BASE.get() else {
        return false;
    };
    let dir = base.join(format!("tab-{}", sanitize_id(tab_id)));
    let killed = write_cgroup(&dir.join("cgroup.kill"), "1").is_ok();
    // rmdir can lose the race with the kernel reaping the killed procs; if so
    // the next spawn reuses the dir and reap_stale_tabs cleans it eventually.
    let _ = std::fs::remove_dir(&dir);
    killed
}

#[cfg(not(feature = "gui"))]
/// On startup, kill + remove any `tab-*` cgroups left over from a PRIOR run.
///
/// An unclean stop (crash, SIGKILL, or a `claude` that survived SIGHUP) leaves
/// orphaned process trees sitting in their old per-tab cgroups (reparented to
/// pid 1 but still cgroup members). Call this after [`init`] and BEFORE
/// respawning tabs, so a fresh `claude --resume <id>` can't run alongside a
/// still-live copy of the same session — the root cause of the duplicate
/// ghost sessions. Skips the `supervisor` leaf (that's us). Best-effort.
pub fn reap_stale_tabs() {
    let Some(Some(base)) = DELEGATED_BASE.get() else {
        return;
    };
    let Ok(entries) = std::fs::read_dir(base) else {
        return;
    };
    let mut n = 0u32;
    for e in entries.flatten() {
        let name = e.file_name();
        // Only our per-tab cgroups; never the `supervisor` leaf (the daemon).
        if !name.to_string_lossy().starts_with("tab-") {
            continue;
        }
        let dir = e.path();
        let _ = write_cgroup(&dir.join("cgroup.kill"), "1");
        let _ = std::fs::remove_dir(&dir);
        n += 1;
    }
    if n > 0 {
        info!("cgroup: reaped {n} stale tab cgroup(s) (orphans from a prior run)");
    }
}

/// Sanitise a tab id into a safe single path component.
fn sanitize_id(tab_id: &str) -> String {
    tab_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Write a single value to a cgroup control file. cgroup files want the
/// value with no trailing newline required, but a newline is accepted.
fn write_cgroup(path: &Path, value: &str) -> std::io::Result<()> {
    std::fs::write(path, value.as_bytes())
}
