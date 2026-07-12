// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Hot swap — upgrade the running binary while every tab's shell stays
//! alive.
//!
//! ## How
//!
//! A normal restart re-forks every shell and replays saved output text.
//! A hot swap instead `exec()`s the new binary **in place**: the process
//! keeps its pid, so the tab shells remain our children (their SIGCHLD
//! reaping, process groups, and controlling TTYs are untouched), and any
//! file descriptor whose `CLOEXEC` flag we clear survives the exec.
//!
//! Sequence, on the process that is being replaced ([`exec_swap`]):
//!
//! 1. Freeze the PTY readers ([`frozen`], checked by `PtyTap::read`) so
//!    bytes the shells emit from here on stay in the kernel PTY buffers
//!    — the post-exec process reads them into its fresh parser, so
//!    nothing is ever lost.
//! 2. Persist state as usual (tabs.json + per-tab output/uptime), which
//!    the caller does before invoking us.
//! 3. For every tab: write its raw PTY-ring bytes to a sidecar file
//!    (so remote-viewer scrollback survives), clear `CLOEXEC` on a dup
//!    of the PTY **master** fd, and record `(tab id, fd number, child
//!    pid)` in a JSON manifest.
//! 4. Remove the agent reaper's provenance record — a hot swap is a
//!    clean handover, and the reaper would otherwise see "recorded
//!    agents still alive at boot" and SIGKILL the fleet it just
//!    inherited.
//! 5. `exec()` the (freshly installed) binary at our own path, passing
//!    `--handoff <manifest>` on argv. On exec failure everything is
//!    rolled back and the old process keeps running.
//!
//! On the new binary's side ([`adopt_from_args`] at boot): validate the
//! manifest (`handoff_version` + writer pid must equal ours — after an
//! exec the pid is unchanged, and a stale manifest from some earlier
//! crash can never match), take ownership of each inherited fd, restore
//! `CLOEXEC`, and stash everything in a registry keyed by tab id. The
//! tab restore paths then call [`take_adopted`] and, on a hit, wrap the
//! live master fd in an [`AdoptedPty`] instead of forking a new shell.
//! Grid contents are restored through the existing saved-output replay;
//! the carried ring bytes re-seed the tab's `PtyRing`.
//!
//! [`AdoptedPty`] mirrors `alacritty_terminal`'s Unix `Pty` (same poller
//! registration, same SIGCHLD-pipe + `waitpid` child-exit detection,
//! same drop semantics) — alacritty's own `Pty` can't be constructed
//! from an existing fd because it insists on owning a `std::process::
//! Child`, which cannot be rebuilt from a bare pid.

use std::sync::atomic::{AtomicBool, Ordering};

/// Set by `POST /upgrade`; polled by both binaries' owner loops right
/// next to `SHUTDOWN_REQUESTED`. The loop that observes it persists
/// state and calls [`exec_swap`].
pub static UPGRADE_REQUESTED: AtomicBool = AtomicBool::new(false);

/// While true, `PtyTap::read` reports `WouldBlock` without touching the
/// PTY: the readers are parked for the handoff so unread bytes stay in
/// the kernel buffers for the post-exec process.
static FREEZE: AtomicBool = AtomicBool::new(false);

pub fn request_upgrade() {
    UPGRADE_REQUESTED.store(true, Ordering::SeqCst);
}

#[must_use]
pub fn upgrade_requested() -> bool {
    UPGRADE_REQUESTED.load(Ordering::SeqCst)
}

pub fn clear_upgrade_request() {
    UPGRADE_REQUESTED.store(false, Ordering::SeqCst);
}

/// Whether the PTY readers are parked for a handoff. Read by
/// `PtyTap::read` on every PTY read; a relaxed load keeps that hot path
/// free.
#[must_use]
pub fn frozen() -> bool {
    FREEZE.load(Ordering::Relaxed)
}

#[cfg(unix)]
pub use unix::*;

/// Windows stubs — ConPTY handles can't be handed across an exec (and
/// there is no exec), so hot swap is Unix-only. These keep the shared
/// call sites (boot restore, boot loader) free of `cfg` noise.
#[cfg(not(unix))]
mod stubs {
    #[must_use]
    pub fn adoptable(_id: &str) -> bool {
        false
    }

    #[must_use]
    pub fn adopt_from_args() -> usize {
        0
    }

    #[must_use]
    pub fn adopted_ids() -> Vec<String> {
        Vec::new()
    }

    pub fn close_unclaimed() {}

    #[must_use]
    pub fn reexec_target_ok() -> bool {
        false
    }
}

#[cfg(not(unix))]
pub use stubs::*;

#[cfg(unix)]
mod unix {
    use std::collections::HashMap;
    use std::fs::File;
    use std::io::{self, ErrorKind, Read};
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
    use std::os::unix::net::UnixStream;
    use std::os::unix::process::{CommandExt, ExitStatusExt};
    use std::path::{Path, PathBuf};
    use std::sync::atomic::Ordering;
    use std::sync::{LazyLock, Mutex};

    use alacritty_terminal::event::{OnResize, WindowSize};
    use alacritty_terminal::tty::{ChildEvent, EventedPty, EventedReadWrite};
    use log::{info, warn};
    use polling::{Event, PollMode, Poller};
    use serde::{Deserialize, Serialize};

    use super::FREEZE;

    /// argv flag carrying the manifest path into the exec'd binary.
    /// argv (unlike the environment) is never inherited by the tab
    /// shells, so nothing leaks into user sessions.
    pub const HANDOFF_ARG: &str = "--handoff";

    /// Manifest schema version. Bump on any incompatible change; an
    /// unknown version is rejected (fds closed, tabs respawn fresh).
    const HANDOFF_VERSION: u32 = 1;

    /// Poller token for the PTY master fd. Must match
    /// `alacritty_terminal::tty::PTY_READ_WRITE_TOKEN` (crate-private
    /// upstream), which the `EventLoop` dispatches on.
    const TOKEN_PTY: usize = 0;
    /// Poller token for the SIGCHLD pipe — upstream's
    /// `PTY_CHILD_EVENT_TOKEN`.
    const TOKEN_CHILD: usize = 1;

    #[derive(Serialize, Deserialize)]
    struct Manifest {
        handoff_version: u32,
        /// `std::process::id()` of the writer. exec keeps the pid, so
        /// the adopter requires it to equal its own — a manifest left
        /// behind by a crashed swap can never validate in a fresh
        /// process, whose fd table wouldn't match the recorded numbers.
        pid: u32,
        from_version: String,
        tabs: Vec<ManifestTab>,
    }

    #[derive(Serialize, Deserialize)]
    struct ManifestTab {
        id: String,
        /// Raw fd number of the (CLOEXEC-cleared) dup of the PTY master.
        fd: RawFd,
        /// The tab's shell pid — still our child after the exec.
        pid: u32,
        /// Sidecar file with the tab's raw PTY-ring bytes, if any.
        ring: Option<PathBuf>,
    }

    /// Everything the swapping process supplies per tab.
    pub struct HandoffSource {
        pub id: String,
        /// Dup of the PTY master fd (kept `CLOEXEC` until the swap).
        pub master: File,
        pub pid: u32,
        /// Raw ring bytes (`PtyRing::since(0)`), carried so viewer
        /// scrollback survives.
        pub ring: Vec<u8>,
    }

    /// One adopted tab, claimed from the registry by the restore path.
    pub struct Adopted {
        pub master: OwnedFd,
        pub pid: u32,
        pub ring: Vec<u8>,
    }

    static ADOPTED: LazyLock<Mutex<HashMap<String, Adopted>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

    fn registry() -> std::sync::MutexGuard<'static, HashMap<String, Adopted>> {
        ADOPTED.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Whether a handoff entry is waiting for this tab id (without
    /// claiming it). The boot restore uses this to skip the paths that
    /// assume a fresh shell (agent auto-resume, net-off respawn).
    #[must_use]
    pub fn adoptable(id: &str) -> bool {
        registry().contains_key(id)
    }

    /// Claim the adopted PTY for `id`. Consumed once — a later respawn
    /// of the same tab misses and forks fresh, as it should.
    #[must_use]
    pub fn take_adopted(id: &str) -> Option<Adopted> {
        registry().remove(id)
    }

    /// Tab ids still waiting in the registry. The headless boot passes
    /// these to the cgroup reaper as "not stale — do not kill".
    #[must_use]
    pub fn adopted_ids() -> Vec<String> {
        registry().keys().cloned().collect()
    }

    /// Close any handoff fds the boot restore never claimed (tab gone
    /// from tabs.json, or its restore failed). Leaving them open would
    /// silently wedge the orphaned shell once its PTY buffer fills.
    pub fn close_unclaimed() {
        let mut reg = registry();
        for (id, a) in reg.drain() {
            warn!(
                "hotswap: handoff for tab {id} (pid {pid}) unclaimed — closing its PTY",
                pid = a.pid
            );
            // Dropping the OwnedFd closes the master: the shell gets
            // SIGHUP from the kernel, same as a closed tab.
        }
    }

    /// Scan argv for `--handoff <path>` and adopt the manifest it names.
    ///
    /// Called once at boot by both binaries. Returns the number of tabs
    /// adopted (0 when argv carries no handoff).
    #[must_use]
    pub fn adopt_from_args() -> usize {
        let mut args = std::env::args();
        while let Some(a) = args.next() {
            if a == HANDOFF_ARG
                && let Some(path) = args.next()
            {
                return adopt_manifest(Path::new(&path));
            }
        }
        0
    }

    /// Parse + validate a handoff manifest and take ownership of the
    /// inherited fds. Invalid manifests are removed and yield 0.
    #[must_use]
    pub fn adopt_manifest(path: &Path) -> usize {
        let cleanup = |m: Option<&Manifest>| {
            if let Some(m) = m {
                for t in &m.tabs {
                    if let Some(r) = &t.ring {
                        let _ = std::fs::remove_file(r);
                    }
                }
            }
            let _ = std::fs::remove_file(path);
        };
        let Ok(raw) = std::fs::read_to_string(path) else {
            return 0;
        };
        let Ok(manifest) = serde_json::from_str::<Manifest>(&raw) else {
            warn!("hotswap: unreadable handoff manifest at {} — ignoring", path.display());
            cleanup(None);
            return 0;
        };
        if manifest.handoff_version != HANDOFF_VERSION || manifest.pid != std::process::id() {
            warn!(
                "hotswap: stale handoff manifest (version {v}, writer pid {p}, our pid {me}) — discarding",
                v = manifest.handoff_version,
                p = manifest.pid,
                me = std::process::id(),
            );
            cleanup(Some(&manifest));
            return 0;
        }
        let mut reg = registry();
        let mut n = 0usize;
        for t in &manifest.tabs {
            if t.fd < 3 {
                continue;
            }
            // Take ownership of the inherited fd. Sound: the manifest is
            // proven ours (same pid ⇒ written by the image this process
            // exec'd from), each fd appears once, and nothing else in
            // this fresh image knows these numbers.
            #[allow(unsafe_code)]
            let master = unsafe { OwnedFd::from_raw_fd(t.fd) };
            // An fd the exec didn't actually deliver (double adoption,
            // hand-edited manifest) fails fcntl — skip it.
            if rustix::io::fcntl_setfd(&master, rustix::io::FdFlags::CLOEXEC).is_err() {
                warn!(
                    "hotswap: tab {} fd {} is not inheritable — respawning fresh",
                    t.id, t.fd
                );
                continue;
            }
            let ring = t.ring.as_ref().and_then(|p| std::fs::read(p).ok()).unwrap_or_default();
            reg.insert(
                t.id.clone(),
                Adopted {
                    master,
                    pid: t.pid,
                    ring,
                },
            );
            n += 1;
        }
        drop(reg);
        info!(
            "hotswap: adopted {n} live tab(s) handed off by v{} (pid {})",
            manifest.from_version, manifest.pid
        );
        cleanup(Some(&manifest));
        n
    }

    /// The path to re-exec: our own binary, with the ` (deleted)`
    /// suffix `/proc/self/exe` grows when the file was replaced (dpkg
    /// upgrade) stripped, so we launch the freshly installed one.
    fn reexec_path() -> PathBuf {
        let exe = std::env::current_exe().unwrap_or_else(|_| {
            std::env::args_os()
                .next()
                .map_or_else(|| PathBuf::from("/proc/self/exe"), PathBuf::from)
        });
        let s = exe.to_string_lossy();
        s.strip_suffix(" (deleted)").map_or_else(|| exe.clone(), PathBuf::from)
    }

    /// Whether the re-exec target currently exists — the cheap sanity
    /// check `POST /upgrade` runs before arming the swap.
    #[must_use]
    pub fn reexec_target_ok() -> bool {
        reexec_path().is_file()
    }

    /// Current argv minus any previous `--handoff <path>` pair, so
    /// repeated swaps don't accumulate flags.
    fn passthrough_args() -> Vec<std::ffi::OsString> {
        let mut out = Vec::new();
        let mut args = std::env::args_os().skip(1);
        while let Some(a) = args.next() {
            if a == HANDOFF_ARG {
                let _ = args.next();
                continue;
            }
            out.push(a);
        }
        out
    }

    /// Replace this process with the binary at our own path, handing it
    /// every tab's live PTY.
    ///
    /// Returns **only on failure** (exec succeeded ⇒ this process image
    /// no longer exists); on failure all side effects are rolled back
    /// and the caller keeps running.
    pub fn exec_swap(sources: &[HandoffSource]) -> io::Error {
        // Park the PTY readers, then give in-flight reads a moment to
        // finish so the ring dumps below are byte-consistent with what
        // alacritty consumed. Bytes arriving after this stay in the
        // kernel PTY buffers and are read by the post-exec process.
        FREEZE.store(true, Ordering::SeqCst);
        std::thread::sleep(std::time::Duration::from_millis(30));

        let state_base = crate::platform::state_base_dir();
        let dir = crate::state_dir(&state_base);
        let _ = std::fs::create_dir_all(&dir);
        let manifest_path = dir.join("handoff.json");

        let mut tabs = Vec::with_capacity(sources.len());
        let mut ring_files = Vec::new();
        for (i, s) in sources.iter().enumerate() {
            let ring = if s.ring.is_empty() {
                None
            } else {
                let p = dir.join(format!("handoff-{i}.ring"));
                if std::fs::write(&p, &s.ring).is_ok() {
                    ring_files.push(p.clone());
                    Some(p)
                } else {
                    None
                }
            };
            tabs.push(ManifestTab {
                id: s.id.clone(),
                fd: s.master.as_raw_fd(),
                pid: s.pid,
                ring,
            });
        }
        let manifest = Manifest {
            handoff_version: HANDOFF_VERSION,
            pid: std::process::id(),
            from_version: env!("CARGO_PKG_VERSION").to_string(),
            tabs,
        };

        let rollback = |err: io::Error| {
            for s in sources {
                let _ = rustix::io::fcntl_setfd(&s.master, rustix::io::FdFlags::CLOEXEC);
            }
            for p in &ring_files {
                let _ = std::fs::remove_file(p);
            }
            let _ = std::fs::remove_file(&manifest_path);
            FREEZE.store(false, Ordering::SeqCst);
            err
        };

        let json = match serde_json::to_string(&manifest) {
            Ok(j) => j,
            Err(e) => return rollback(io::Error::other(e)),
        };
        if let Err(e) = std::fs::write(&manifest_path, json) {
            return rollback(e);
        }

        // Let the fds outlive the exec.
        for s in sources {
            if let Err(e) = rustix::io::fcntl_setfd(&s.master, rustix::io::FdFlags::empty()) {
                return rollback(e.into());
            }
        }

        // A hot swap is a clean handover: drop the agent reaper's
        // provenance record so the new image's boot reap doesn't SIGKILL
        // the (alive, wanted) agents it is about to inherit.
        let _ = std::fs::remove_file(crate::agent_reaper::record_path(&state_base));

        let exe = reexec_path();
        info!("hotswap: exec {} with {} live tab(s)", exe.display(), sources.len());
        let err = std::process::Command::new(&exe)
            .args(passthrough_args())
            .arg(HANDOFF_ARG)
            .arg(&manifest_path)
            .exec();
        warn!("hotswap: exec of {} failed: {err} — rolling back", exe.display());
        rollback(err)
    }

    /// A live PTY inherited across the exec, driving the same
    /// `alacritty_terminal::EventLoop` interface as the Unix `Pty` it
    /// replaces. See the module docs for why upstream's type can't be
    /// rebuilt from an fd.
    pub struct AdoptedPty {
        file: File,
        /// SIGCHLD wakeup pipe (read end), poller token [`TOKEN_CHILD`].
        signals: UnixStream,
        sig_id: signal_hook::SigId,
        pid: rustix::process::Pid,
    }

    impl AdoptedPty {
        /// Wrap an adopted master fd. Verifies the child is still alive
        /// (and still ours) first — `None` means the caller should fork
        /// a fresh shell instead. `ws` is pushed to the PTY so a grid
        /// that changed size across the swap `SIGWINCH`es the shell.
        #[must_use]
        pub fn adopt(master: OwnedFd, pid: u32, ws: WindowSize) -> Option<Self> {
            let pid = rustix::process::Pid::from_raw(i32::try_from(pid).ok()?)?;
            // Register the SIGCHLD pipe BEFORE probing liveness, so an
            // exit in the gap between the two still lands a wakeup byte
            // and `next_child_event` reports it — probe-then-register
            // would lose exactly that window's SIGCHLD forever.
            let (sender, recv) = UnixStream::pair().ok()?;
            let sig_id = signal_hook::low_level::pipe::register(signal_hook::consts::SIGCHLD, sender).ok()?;
            recv.set_nonblocking(true).ok()?;
            // Anything but "alive and ours" (exited-and-reaped-just-now,
            // or ECHILD) means the caller should fork fresh.
            if !matches!(
                rustix::process::waitpid(Some(pid), rustix::process::WaitOptions::NOHANG),
                Ok(None)
            ) {
                signal_hook::low_level::unregister(sig_id);
                return None;
            }
            let file = File::from(master);
            // The open PTY description was already non-blocking in the
            // old process (shared across the dup + exec), but make it
            // explicit — the event loop depends on `WouldBlock`.
            let set_nonblocking = || -> Option<()> {
                let flags = rustix::fs::fcntl_getfl(&file).ok()?;
                rustix::fs::fcntl_setfl(&file, flags | rustix::fs::OFlags::NONBLOCK).ok()
            };
            if set_nonblocking().is_none() {
                signal_hook::low_level::unregister(sig_id);
                return None;
            }
            let _ = rustix::termios::tcsetwinsize(&file, winsize(ws));
            Some(Self {
                file,
                signals: recv,
                sig_id,
                pid,
            })
        }

        /// Dup of the master, kept by the owner for the *next* handoff.
        #[must_use]
        pub fn master_copy(&self) -> Option<File> {
            self.file.try_clone().ok()
        }
    }

    const fn winsize(ws: WindowSize) -> rustix::termios::Winsize {
        rustix::termios::Winsize {
            ws_row: ws.num_lines,
            ws_col: ws.num_cols,
            ws_xpixel: ws.num_cols * ws.cell_width,
            ws_ypixel: ws.num_lines * ws.cell_height,
        }
    }

    impl Drop for AdoptedPty {
        fn drop(&mut self) {
            // Mirror upstream `Pty::drop`: hang up the child, then reap
            // it. (Runs on the PTY event-loop thread after `Msg::
            // Shutdown`, same as upstream.)
            let _ = rustix::process::kill_process(self.pid, rustix::process::Signal::HUP);
            signal_hook::low_level::unregister(self.sig_id);
            let _ = rustix::process::waitpid(Some(self.pid), rustix::process::WaitOptions::empty());
        }
    }

    // `EventedReadWrite::register` is `unsafe fn` upstream (the
    // implementor guarantees the sources outlive their registration);
    // same narrow allow as `PtyTap`.
    #[allow(unsafe_code)]
    impl EventedReadWrite for AdoptedPty {
        type Reader = File;
        type Writer = File;

        unsafe fn register(
            &mut self,
            poll: &std::sync::Arc<Poller>,
            mut interest: Event,
            mode: PollMode,
        ) -> io::Result<()> {
            interest.key = TOKEN_PTY;
            // Safety: identical contract to upstream `Pty::register` —
            // both fds live in `self`, which outlives the registration
            // (deregistered by the event loop before drop).
            unsafe {
                poll.add_with_mode(&self.file, interest, mode)?;
                poll.add_with_mode(&self.signals, Event::readable(TOKEN_CHILD), PollMode::Level)
            }
        }

        fn reregister(&mut self, poll: &std::sync::Arc<Poller>, mut interest: Event, mode: PollMode) -> io::Result<()> {
            interest.key = TOKEN_PTY;
            poll.modify_with_mode(&self.file, interest, mode)?;
            poll.modify_with_mode(&self.signals, Event::readable(TOKEN_CHILD), PollMode::Level)
        }

        fn deregister(&mut self, poll: &std::sync::Arc<Poller>) -> io::Result<()> {
            poll.delete(&self.file)?;
            poll.delete(&self.signals)
        }

        fn reader(&mut self) -> &mut File {
            &mut self.file
        }

        fn writer(&mut self) -> &mut File {
            &mut self.file
        }
    }

    impl EventedPty for AdoptedPty {
        fn next_child_event(&mut self) -> Option<ChildEvent> {
            // Drain one wakeup byte, mirroring upstream. Every SIGCHLD in
            // the process fans out to every registered pipe; the waitpid
            // below is scoped to OUR child, so foreign exits are `None`.
            let mut buf = [0u8; 1];
            if let Err(err) = self.signals.read(&mut buf) {
                if err.kind() != ErrorKind::WouldBlock {
                    warn!("hotswap: error reading SIGCHLD pipe: {err}");
                }
                return None;
            }
            match rustix::process::waitpid(Some(self.pid), rustix::process::WaitOptions::NOHANG) {
                Ok(Some((_, status))) => Some(ChildEvent::Exited(Some(ExitStatusExt::from_raw(status.as_raw())))),
                // ECHILD after an earlier reap ⇒ nothing new to report.
                Ok(None) | Err(_) => None,
            }
        }
    }

    impl OnResize for AdoptedPty {
        fn on_resize(&mut self, window_size: WindowSize) {
            // Best-effort, unlike upstream's die-on-error: an EIO here
            // just means the child is going away and the Exited event is
            // in flight.
            if let Err(e) = rustix::termios::tcsetwinsize(&self.file, winsize(window_size)) {
                warn!("hotswap: TIOCSWINSZ on adopted PTY failed: {e}");
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use std::os::fd::IntoRawFd;

        use super::*;

        fn tmpdir() -> tempfile::TempDir {
            tempfile::tempdir().unwrap()
        }

        fn write_manifest(dir: &Path, m: &Manifest) -> PathBuf {
            let p = dir.join("handoff.json");
            std::fs::write(&p, serde_json::to_string(m).unwrap()).unwrap();
            p
        }

        /// A real fd this process owns, "leaked" the way exec-inherited
        /// fds arrive: as a bare number nothing else owns.
        fn leaked_fd(dir: &Path, name: &str) -> RawFd {
            File::create(dir.join(name)).unwrap().into_raw_fd()
        }

        #[test]
        fn manifest_roundtrips() {
            let m = Manifest {
                handoff_version: HANDOFF_VERSION,
                pid: 42,
                from_version: "0.5.0".into(),
                tabs: vec![ManifestTab {
                    id: "abc".into(),
                    fd: 7,
                    pid: 1234,
                    ring: Some(PathBuf::from("/tmp/x.ring")),
                }],
            };
            let back: Manifest = serde_json::from_str(&serde_json::to_string(&m).unwrap()).unwrap();
            assert_eq!(back.handoff_version, HANDOFF_VERSION);
            assert_eq!(back.tabs[0].id, "abc");
            assert_eq!(back.tabs[0].fd, 7);
            assert_eq!(back.tabs[0].pid, 1234);
        }

        #[test]
        fn adopt_rejects_foreign_pid_and_cleans_up() {
            let dir = tmpdir();
            let ring = dir.path().join("t.ring");
            std::fs::write(&ring, b"bytes").unwrap();
            let p = write_manifest(
                dir.path(),
                &Manifest {
                    handoff_version: HANDOFF_VERSION,
                    pid: std::process::id().wrapping_add(1),
                    from_version: "x".into(),
                    tabs: vec![ManifestTab {
                        id: "t".into(),
                        fd: 9999,
                        pid: 1,
                        ring: Some(ring.clone()),
                    }],
                },
            );
            assert_eq!(adopt_manifest(&p), 0);
            assert!(!p.exists(), "manifest removed");
            assert!(!ring.exists(), "ring sidecar removed");
            assert!(!adoptable("t"));
        }

        #[test]
        fn adopt_rejects_unknown_version() {
            let dir = tmpdir();
            let p = write_manifest(
                dir.path(),
                &Manifest {
                    handoff_version: HANDOFF_VERSION + 1,
                    pid: std::process::id(),
                    from_version: "x".into(),
                    tabs: vec![],
                },
            );
            assert_eq!(adopt_manifest(&p), 0);
            assert!(!p.exists());
        }

        #[test]
        fn adopt_claims_fd_and_ring_then_take_consumes() {
            let dir = tmpdir();
            let fd = leaked_fd(dir.path(), "master");
            let ring = dir.path().join("t.ring");
            std::fs::write(&ring, b"scrollback").unwrap();
            let p = write_manifest(
                dir.path(),
                &Manifest {
                    handoff_version: HANDOFF_VERSION,
                    pid: std::process::id(),
                    from_version: "x".into(),
                    tabs: vec![ManifestTab {
                        id: "tab-1".into(),
                        fd,
                        pid: std::process::id(), // any live pid; not waited on here
                        ring: Some(ring.clone()),
                    }],
                },
            );
            assert_eq!(adopt_manifest(&p), 1);
            assert!(!p.exists() && !ring.exists(), "handoff files consumed");
            assert!(adoptable("tab-1"));
            let a = take_adopted("tab-1").unwrap();
            assert_eq!(a.ring, b"scrollback");
            assert_eq!(a.pid, std::process::id());
            // CLOEXEC restored on the adopted fd.
            let flags = rustix::io::fcntl_getfd(&a.master).unwrap();
            assert!(flags.contains(rustix::io::FdFlags::CLOEXEC));
            // Consumed: a second take misses.
            assert!(take_adopted("tab-1").is_none());
        }

        #[test]
        fn adopt_skips_invalid_fd_entries() {
            let dir = tmpdir();
            let fd = leaked_fd(dir.path(), "good");
            let p = write_manifest(
                dir.path(),
                &Manifest {
                    handoff_version: HANDOFF_VERSION,
                    pid: std::process::id(),
                    from_version: "x".into(),
                    tabs: vec![
                        ManifestTab {
                            id: "bad-stdio".into(),
                            fd: 0,
                            pid: 1,
                            ring: None,
                        },
                        ManifestTab {
                            id: "good".into(),
                            fd,
                            pid: std::process::id(),
                            ring: None,
                        },
                    ],
                },
            );
            assert_eq!(adopt_manifest(&p), 1);
            assert!(!adoptable("bad-stdio"));
            assert!(take_adopted("good").is_some());
        }

        #[test]
        fn adopted_pty_reads_child_output_and_reports_exit() {
            // A pipe stands in for the PTY master: AdoptedPty only needs
            // an fd to read + a child pid to watch (the winsize ioctl is
            // best-effort and simply fails on a pipe).
            let (read_end, write_end) = std::io::pipe().unwrap();
            // The `sleep` keeps the child alive through `adopt`'s
            // liveness probe; its exit afterwards exercises the
            // SIGCHLD-pipe path.
            let mut child = std::process::Command::new("/bin/sh")
                .arg("-c")
                .arg("echo swap-alive; sleep 0.5")
                .stdout(write_end)
                .spawn()
                .unwrap();
            let ws = WindowSize {
                num_lines: 24,
                num_cols: 80,
                cell_width: 8,
                cell_height: 16,
            };
            let mut pty = AdoptedPty::adopt(OwnedFd::from(read_end), child.id(), ws).unwrap();
            assert!(pty.master_copy().is_some());
            // Poll for the output (the fd is non-blocking).
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            let mut buf = [0u8; 64];
            let n = loop {
                match pty.reader().read(&mut buf) {
                    Ok(n) if n > 0 => break n,
                    Ok(_) => panic!("EOF before output"),
                    Err(e) if e.kind() == ErrorKind::WouldBlock => {
                        assert!(std::time::Instant::now() < deadline, "no output within 5s");
                        std::thread::sleep(std::time::Duration::from_millis(10));
                    }
                    Err(e) => panic!("read failed: {e}"),
                }
            };
            assert_eq!(&buf[..n], b"swap-alive\n");
            // The child has exited (or will momentarily): next_child_event
            // reaps it once the SIGCHLD byte arrives.
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            loop {
                if let Some(ChildEvent::Exited(status)) = pty.next_child_event() {
                    assert_eq!(status.and_then(|s| s.code()), Some(0));
                    break;
                }
                assert!(std::time::Instant::now() < deadline, "no child-exit event within 5s");
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            // Already reaped — drop must not block on waitpid.
            drop(pty);
            // The AdoptedPty reaped the child, so this waitpid comes
            // back ECHILD; it's here for the Child-handle bookkeeping.
            let _ = child.wait();
        }

        #[test]
        fn adopt_refuses_dead_child() {
            let (read_end, _write_end) = std::io::pipe().unwrap();
            let mut child = std::process::Command::new("/bin/true").spawn().unwrap();
            let pid = child.id();
            child.wait().unwrap(); // reaped ⇒ pid is gone for waitpid
            let ws = WindowSize {
                num_lines: 24,
                num_cols: 80,
                cell_width: 8,
                cell_height: 16,
            };
            assert!(AdoptedPty::adopt(OwnedFd::from(read_end), pid, ws).is_none());
        }

        #[test]
        fn passthrough_args_strips_handoff_pair() {
            // Can't fake argv, but the filter is pure enough to check on
            // the real one: our test argv has no --handoff, so the round
            // trip is identity.
            let ours: Vec<_> = std::env::args_os().skip(1).collect();
            assert_eq!(passthrough_args(), ours);
        }
    }
}
