# Firecracker-backed tabs — investigation

Status: **investigation only**, not implemented. Edit/reject freely.

## Problem

A tab-atelier tab runs a shell (or a catbus agent's `bash`/`edit`/`write`
tools) directly on the host, as the user. Anything the tab can reach, the
user can reach: `$HOME`, SSH keys, the X11 socket, `~/.config/tab-atelier`
secrets, the rest of the process table. The headless daemon
(`tab-atelier-headless.service`) already pushes the host-side blast radius
down hard with systemd hardening (`ProtectSystem=strict`, a `@system-service`
seccomp allowlist, an empty `CapabilityBoundingSet`, `ProtectHome=true`),
but that protects the *daemon process*, not the *shell it spawns* — the PTY
child inherits the daemon's namespace and can touch `/var/lib/tab-atelier`
and anything else the service user owns.

The sharp case is the catbus agent: a remotely-driven model executing
`bash -lc "<arbitrary>"` (`crates/catbus-agent/src/tools/bash.rs`) on the
host. We want a hard boundary — a separate kernel — between guest workloads
and the host.

[Firecracker](https://github.com/firecracker-microvm/firecracker) is AWS's
Rust KVM-based VMM (the engine behind Lambda / Fargate). It boots a minimal
microVM — one guest kernel + one block device, a handful of emulated
devices — in well under a second, with a ~5 MiB per-VM VMM memory overhead.
That is a real kernel/hypervisor boundary, not a namespace trick, and it is
the strongest isolation we can offer short of separate hardware.

## How it would attach to a tab

The lucky part: a Firecracker tab fits the existing PTY model almost
exactly, with no change to the rendering/grid/event-loop machinery.

Today every tab is born in `TerminalView::new_with_colors_and_env`
(`src/terminal.rs`) via `alacritty_terminal`'s `tty::new(&opts, ws, 0)`.
`opts` is `tty::Options { working_directory, env, .. }` and we **leave
`shell` unset**, so alacritty spawns the user's login shell as the PTY
child. The injection point is that one field:

```rust
let opts = tty::Options {
    working_directory: cwd.map(Path::to_path_buf),
    env,
    shell: Some(tty::Shell::new(
        "firecracker".into(),
        vec!["--config-file".into(), vm_config_path],
    )),
    ..Default::default()
};
```

Firecracker wires the guest's serial console (`ttyS0`) straight to its own
stdin/stdout. When `firecracker` is the PTY child, *its* stdio is the PTY
slave — so the guest's serial console is wired directly to the tab's grid.
Boot the guest with `console=ttyS0` and a `getty`/shell on `ttyS0`, and the
user is typing into the VM with byte-for-byte the same plumbing we already
have for a host shell. Resize, scrollback, exit-detection
(`process_alive(pid)` watches the `firecracker` PID instead of a shell PID)
all keep working unchanged.

So the terminal layer barely moves. The weight of this feature is entirely
in **provisioning and lifecycle**, not rendering.

The byte path — identical to a host shell, just one more hop at the end:

```
 ┌────────────────────────── tab-atelier ──────────────────────────┐
 │                                                                  │
 │   keypress ──▶ TerminalView ──▶ EventLoop ──▶ notifier ──▶ ╗     │
 │   grid     ◀── alacritty Term ◀── EventProxy ◀──────────── ║     │
 │                                                            ║     │
 └────────────────────────────────────────────────────────── ║ ────┘
                                                      PTY master ║
                                                      (host fd)  ║
            ┌─────────────────────────────────────── PTY slave ─╨──┐
            │  firecracker  (the PTY child — its stdio = the slave) │
            │      stdin  ─────▶  guest ttyS0  (serial console)     │
            │      stdout ◀─────  guest ttyS0                        │
            │  ┌──────────────── KVM boundary ─────────────────┐    │
            │  │  guest kernel  ▶  getty/login on ttyS0  ▶  sh  │    │
            │  └────────────────────────────────────────────────┘  │
            └───────────────────────────────────────────────────────┘

  Today: the PTY child is the user's login shell.
  Proposed: the PTY child is `firecracker`, and the shell lives one
            KVM boundary deeper — inside the guest.
```

## What a VM needs (the actual work)

1. **`/dev/kvm` access.** Read/write to `/dev/kvm`, x86_64 or aarch64, on a
   host where nested virt / KVM is available. This rules out the feature on
   non-KVM hosts and inside most unprivileged containers; it must be a
   detected, optional capability, never assumed. (Aligns fine with the
   project's Linux/Debian-13 target.) Note this collides with the headless
   service's current `PrivateDevices`-adjacent stance: the unit deliberately
   keeps `/dev` open for `/dev/ptmx`; we'd additionally need `/dev/kvm` and
   `/dev/net/tun` allowed through, plus a relaxed `SystemCallFilter`
   (`@privileged`/`ioctl` for KVM) — i.e. the hardening profile changes.

2. **A guest kernel image.** One uncompressed `vmlinux` we ship or build.
   Pin a version, reuse it read-only across all VMs.

3. **A root filesystem.** An `ext4` image with a shell + the baseline tools a
   user expects in a tab (coreutils, git, an editor, maybe a language
   toolchain). This is the policy-heavy decision: minimal busybox image
   (fast, secure, surprising to users) vs. a full Debian rootfs (familiar,
   large). Per-VM writes go to an **overlay / ephemeral copy** (or a
   `copy-on-write` block device) so VMs don't corrupt the golden image and
   so "fresh tab = fresh machine" is the default.

4. **Sharing the working directory.** The host shell starts in `cwd`; a VM
   has no host filesystem. Options, roughly in order of preference:
   - **virtio-fs / 9p share** of just the project directory into the guest.
     This is what users actually want — edit host files inside the sandbox —
     but it punctures the isolation by exactly the directory shared, and
     virtio-fs support adds setup weight.
   - **No share** — the VM is a clean room; move files in/out over `vsock`
     or a shared block device. Strongest isolation, least convenient.
   - This choice is the real product decision and should be per-tab, not
     global.

5. **Networking (optional).** A `tap` device + NAT if the guest needs the
   network. Off by default is itself a security win (a tab with no network
   can't exfiltrate). When on, it's another host-side privileged setup step
   (`tap` creation needs `CAP_NET_ADMIN`).

6. **Lifecycle & cleanup.** One `firecracker` process per VM. On tab close /
   shell-exit we already `Msg::Shutdown` the PTY; we'd additionally need to
   reap the VM, delete the per-VM overlay, tear down the tap, and remove the
   API socket. Crash/leak handling matters — a leaked microVM holds memory
   and a tap device.

Lifecycle of one Firecracker tab, end to end:

```
  open tab
     │
     ▼
  ┌─────────────┐  reflink/snapshot base → per-VM rootfs; write vm-config.json;
  │ PROVISION   │  create tap (if net on); allocate vsock; mkdir jail chroot
  └─────┬───────┘
        ▼
  ┌─────────────┐  tty::Options.shell = jailer → firecracker --config-file …
  │ SPAWN PTY   │  PTY child = jailer; its stdio = guest ttyS0
  └─────┬───────┘
        ▼
  ┌─────────────┐  guest kernel boots (<1s) → getty on ttyS0 → user shell
  │ RUN         │  process_alive(firecracker_pid) drives exit detection
  └─────┬───────┘
        │  user types  exit  │  closes tab  │  daemon shuts down
        ▼
  ┌─────────────┐  Msg::Shutdown → reap firecracker → drop ephemeral upper,
  │ TEARDOWN    │  delete tap, rm API socket + jail chroot
  └─────────────┘  (persistent upper kept, keyed by tab UUID)
```

## Use the jailer

Don't run bare `firecracker`. Firecracker ships
[`jailer`](https://github.com/firecracker-microvm/firecracker/blob/main/docs/jailer.md),
which `chroot`s the VMM, drops it into its own cgroup + PID/mount/net
namespaces, drops privileges to a target uid/gid, and applies a seccomp
filter — before the VMM ever touches guest config. So the model is
defence-in-depth:

```
 ╔═══════════════════════════════════════════════════════════════╗  ← host
 ║  systemd unit: ProtectSystem=strict · seccomp · no caps         ║
 ║  ┌───────────────────────────────────────────────────────────┐ ║
 ║  │  tab-atelier-headless  (service user)                       │ ║
 ║  │  ┌─────────────────────────────────────────────────────┐  │ ║
 ║  │  │  jailer    chroot + cgroup + PID/mount/net ns + seccomp│ │ ║
 ║  │  │  ┌───────────────────────────────────────────────┐  │  │ ║
 ║  │  │  │  firecracker (VMM)        small attack surface  │  │  │ ║
 ║  │  │  │  ══════════════ KVM / hypervisor boundary ════  │  │  │ ║
 ║  │  │  │  ┌─────────────────────────────────────────┐    │  │  │ ║
 ║  │  │  │  │  GUEST kernel ▶ getty(ttyS0) ▶ user shell│    │  │  │ ║
 ║  │  │  │  │  untrusted bash / model tools run HERE   │    │  │  │ ║
 ║  │  │  │  └─────────────────────────────────────────┘    │  │  │ ║
 ║  │  │  └───────────────────────────────────────────────┘  │  │ ║
 ║  │  └─────────────────────────────────────────────────────┘  │ ║
 ║  └───────────────────────────────────────────────────────────┘ ║
 ╚═══════════════════════════════════════════════════════════════╝

 An escape must cross, in order: KVM → jailer ns/seccomp → service-user
 limits → systemd unit hardening, before it ever reaches the host.
```

Even a Firecracker VMM escape (rare; small attack surface, but non-zero)
lands in the jailer's stripped namespace, not on the host.

## Headless-only (recommended scope)

The hook is identical in both spawn paths — the GUI's
`TerminalView::new_with_colors_and_env` and the daemon's
`spawn_pty_tab` (`src/headless.rs`) both build a `tty::Options` with
`shell` unset. So supporting either is mechanically the same. But the
feature should ship **headless-only first**, for reasons that are about fit,
not difficulty:

- **The threat model lives there.** Headless is the remote-driven,
  catbus-agent, server scenario — untrusted `bash` running while nobody's
  watching. The GUI is the user's own interactive desktop; they already
  trust themselves with their own `$HOME`.
- **There's a clean home for the privilege grant.** The headless daemon runs
  as a dedicated service user under a systemd unit
  (`tab-atelier-headless.service`). Granting *that one unit* `/dev/kvm` +
  `/dev/net/tun` via `DeviceAllow=` and relaxing its `SystemCallFilter` for
  KVM ioctls is a contained, auditable change. A GUI launched from the
  user's login session has no equivalent control surface — you'd be handing
  KVM to the whole desktop session.
- **The convenience tax barely bites.** Headless tabs are already
  agent-driven and non-interactive-ish; losing the host `$HOME`/SSH-agent/X
  socket is the *point*. A GUI user, by contrast, expects their full desktop
  environment inside a tab, and a microVM breaks that hard — and forwarding
  the X11 socket into the VM to "fix" it would defeat the isolation entirely.
- **Smaller surface to build and test.** No X-sharing rabbit hole, one
  spawn path, one privilege profile.

Keep the `shell`-injection helper generic so the GUI *can* opt in later
behind a preference — but don't ship GUI support in v1.

## Base images

"Base image" really means three separable pieces: a **guest kernel**, a
**read-only golden rootfs**, and a **per-VM writable layer**. The isolation
and the "fresh tab = fresh machine" property both come from keeping the first
two immutable and shared, and giving every VM its own throwaway third layer.

```
   shipped once in the package              created per tab, thrown away
   (read-only, shared by ALL VMs)           on close (or kept if persistent)
   ┌────────────────────────────┐           ┌────────────────────────────┐
   │  vmlinux   (guest kernel)  │           │  writable upper layer       │
   ├────────────────────────────┤           │  • ephemeral: tmpfs (RAM)   │
   │  golden-rootfs.ext4         │           │  • persistent: dm-snapshot  │
   │  /bin /usr /etc … (Debian)  │           │    or reflink delta, keyed  │
   │  version = v1 · sha256=…    │           │    by tab UUID              │
   └────────────────────────────┘           └────────────────────────────┘
                │                                          │
                │  attached read-only as /dev/vda          │ overlayfs upper
                └──────────────┬───────────────────────────┘
                               ▼
                ┌───────────────────────────────┐
                │   guest sees one writable /    │   tab A ─┐
                │   = ro base  +  its own upper  │   tab B ─┼─ same base,
                └───────────────────────────────┘   tab C ─┘  separate uppers
                                                     (base never mutates)
```

### Kernel

One uncompressed `vmlinux` per arch (x86_64, aarch64), built from a minimal
config (Firecracker publishes recommended kernel configs). Pinned, read-only,
shared by every VM, updated rarely (security patches). Ships in the optional
package. Building it ourselves keeps it small and reproducible and avoids
trusting an external artifact.

### Golden rootfs — how it's produced and shipped

Prefer a **prebuilt golden image, built in our CI and shipped in the
package** (e.g. `tab-atelier-firecracker.deb`), signed through the same apt
repo + GPG path the project already uses — so image integrity rides on
infrastructure that already exists. It's deterministic, offline, and
auditable. Build it with `mkosi`/`debootstrap` (Debian rootfs — familiar,
larger) or busybox (tiny, fast, but surprising). The trade is size vs.
familiarity; a small Debian base is probably the right default for a Debian
project.

Rejected as the *default*: building on first run (needs network + build tools
+ minutes, non-deterministic) and pulling an OCI image from a registry (adds
a registry client + network + trust surface). Both are fine as opt-in
*advanced* paths, not the out-of-box experience.

### Per-VM writable layer — how layering actually works

The golden rootfs is mounted **read-only** and never mutated. Each VM's
writes go somewhere else:

- **Ephemeral (default): in-guest `overlayfs`.** Attach the golden image as a
  read-only `/dev/vda`; the guest init mounts an `overlayfs` with a `tmpfs`
  (or a small per-VM ext4 delta) upper. No host-side copy at all — works
  regardless of the host filesystem, and "close tab" = throw away the upper.
  This is the cleanest fit and the recommended default.
- **Persistent (opt-in): per-tab delta keyed by tab UUID.** A writable layer
  that survives respawns so a "project tab" keeps its installed packages.
  Implement via `dm-snapshot` (block-level CoW over the read-only base) or, on
  CoW filesystems (btrfs/xfs), a `cp --reflink` of the base — near-instant
  there, full copy as the ext4 fallback. Maps naturally onto tab-atelier's
  existing per-tab UUID + state model. On tab delete, drop the layer.

### Versioning & customization

- The golden image carries a version/hash; a persistent tab records which
  base it booted, so a package upgrade to base v2 doesn't silently change a
  pinned tab underneath the user.
- Power users get *their* tools two ways: a documented `mkosi`/Containerfile
  recipe + a preference pointing at a custom image path (swap the whole
  base), or — far simpler — `apt install` inside a **persistent** tab, since
  the writable overlay + optional network already make that work without
  rebuilding any image.

## Cost / weight (be honest)

- **New runtime dependencies on the host:** `firecracker` + `jailer` binaries
  (not crates — external binaries we'd detect/ship), a kernel image, a rootfs
  image (tens to hundreds of MiB). That's a meaningful packaging change for a
  `cargo deb` that today is one binary. Likely a separate optional package
  (e.g. `tab-atelier-firecracker`) carrying the images, gated behind a
  preference.
- **No Windows.** Firecracker is Linux/KVM only — fine, the GUI already
  `#[cfg(windows)]`-stubs the PID path, and this is a Linux-target project,
  but the feature must compile away cleanly off-Linux.
- **First-boot latency:** sub-second once images are warm, but image build /
  download is a one-time multi-second-to-minutes cost we must surface.
- **Per-tab memory:** guest RAM (configurable, e.g. 128–512 MiB) + ~5 MiB
  VMM. Many heavy tabs = real RAM. Needs a guest-RAM preference and probably
  a cap on concurrent VMs.
- **The convenience tax:** a sandboxed tab is *less useful by design* — no
  host `$HOME`, no inherited SSH agent, no host PATH/toolchains unless we
  plumb them in. Users will feel this immediately. The directory-sharing and
  network toggles above are what make it tolerable, and each one we add
  trades isolation back away. The honest framing: this is opt-in per tab, for
  workloads you don't trust (a model's `bash`, an untrusted script), not the
  everyday default.

## Alternatives considered

- **Namespaces/`bubblewrap`/`nsjail`** — far lighter (no kernel, no images,
  no KVM, works in more environments) and would cover most of the catbus
  threat model. But it's the *same kernel*: a kernel-LPE in the guest is a
  host compromise. Firecracker's whole value proposition is that it is not
  that. Worth noting as the "80% of the benefit, 10% of the cost" option if
  the hard kernel boundary isn't required.
- **gVisor** — userspace kernel, middle ground; another large external
  dependency, and the Go runtime is a poor fit for this project.
- **Plain `cloud-hypervisor` / QEMU microvm** — comparable isolation,
  heavier and slower to boot; Firecracker is the right pick if we go VM.

## Recommendation

Feasible, and the PTY-`shell`-field hook makes the *terminal* integration
small. The cost is everywhere else: image build/ship, jailer wiring, the
KVM/seccomp relaxation of the headless unit, directory-sharing UX, and
lifecycle cleanup. Sequencing if pursued:

1. Spike: hand-built kernel + busybox rootfs, bare `firecracker` as the PTY
   `shell`, no network, no share. Prove a tab boots into a guest shell and
   exits cleanly. (Pure feasibility — throwaway.)
2. Add `jailer` + per-VM ephemeral overlay + reliable teardown.
3. Decide rootfs policy (busybox vs Debian) and ship it as an optional
   package gated behind a preference.
4. Add opt-in, per-tab directory share (virtio-fs) and network toggle.
5. Point the catbus agent at a Firecracker tab as its execution target — the
   highest-value consumer, and the one that justifies the whole effort.

Sources:
- [Firecracker getting-started](https://github.com/firecracker-microvm/firecracker/blob/main/docs/getting-started.md)
- [Firecracker jailer](https://github.com/firecracker-microvm/firecracker/blob/main/docs/jailer.md)
- [Firecracker project site](https://firecracker-microvm.github.io/)
