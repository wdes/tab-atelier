# Arch Linux packaging

Pacman packages for Arch users, mirroring the two cargo-deb variants in the
root `Cargo.toml` (`[package.metadata.deb]`). One source tree builds a split
package:

| package                   | binary                     | equivalent .deb           |
| ------------------------- | -------------------------- | ------------------------- |
| `tab-atelier`             | `tab-atelier` (GUI, X11)   | `tab-atelier_*.deb`       |
| `tab-atelier-headless`    | `tab-atelier-headless`     | `tab-atelier-headless_*.deb` |

Both ship `/usr/bin/catbus-agent`, so they **conflict** — install one or the
other, exactly like the two `.deb`s.

## Files

- `PKGBUILD` — versioned release package. Source is the GitHub tag tarball
  (`v<pkgver>`). This is what gets submitted to the AUR.
- `PKGBUILD.git` — VCS package (`tab-atelier-git` / `tab-atelier-headless-git`)
  built from `main` HEAD. Use this to build **today**, before a release is cut.
- `tab-atelier-headless.sysusers` → `/usr/lib/sysusers.d/tab-atelier.conf` —
  creates the `tab-atelier` system user (pacman's `systemd-sysusers` hook).
- `tab-atelier-headless.tmpfiles` → `/usr/lib/tmpfiles.d/tab-atelier.conf` —
  creates `/var/lib/tab-atelier` `0750` (pacman's `systemd-tmpfiles` hook).
- `tab-atelier-headless.install` — scriptlet printing the first-run cheat-sheet.
- `.SRCINFO` — generated metadata for the versioned `PKGBUILD` (regenerate with
  `makepkg --printsrcinfo > .SRCINFO` after any edit).

The Debian maintainer scripts (`assets/headless-debian/*`) `adduser` and
`install -d` imperatively and auto-enable the unit. On Arch that is declarative:
the sysusers/tmpfiles drop-ins are applied by pacman hooks, and **the service is
not auto-started** (Arch policy) — the admin runs
`systemctl enable --now tab-atelier-headless.service`.

## Install from the hosted repo (no build)

CI publishes a rolling pacman repository alongside the apt repo. Add to
`/etc/pacman.conf`:

```ini
[tab-atelier]
SigLevel = Optional TrustAll
Server = https://deb.tab-atelier.wdes.eu/arch
```

then `sudo pacman -Sy tab-atelier` (or `tab-atelier-headless` — they conflict).
The packages are built by `.github/workflows/arch-pkg.yml` in an `archlinux`
container (makepkg can't run on the Ubuntu .deb runner), and
`.github/workflows/apt-publish.yml` pulls the latest build into `site/arch/`,
regenerates the repo db with `repo-add`, and serves it from GitHub Pages — the
same single-publisher flow used for the Windows MSI and Android APK. It's a
rolling channel (newest build wins, 10 kept); the AUR `PKGBUILD` below is the
versioned path.

## Build & test today (from a working tree, no release tag needed)

```sh
cd packaging/arch
makepkg -p PKGBUILD.git -si   # builds tab-atelier-git + tab-atelier-headless-git
```

`makepkg` clones the repo fresh into `src/`; commit your changes first (or point
the `PKGBUILD.git` `source=` at `git+file:///mnt/Dev/@wdes/tab-atelier#branch=main`
to build uncommitted work).

## Cut a release (versioned AUR package)

1. Tag and push `v<version>` (e.g. `v0.5.0`) so the GitHub archive tarball
   exists.
2. Bump `pkgver` in `PKGBUILD` to match, reset `pkgrel=1`.
3. Fill the tarball digest:
   ```sh
   updpkgsums            # rewrites sha256sums=() from the real tarball
   makepkg --printsrcinfo > .SRCINFO
   ```
4. Verify a clean build in a chroot (catches missing deps the host already has):
   ```sh
   makepkg -sc           # or: extra-x86_64-build  (devtools, clean chroot)
   namcap PKGBUILD *.pkg.tar.zst
   ```
5. Publish to the AUR: push `PKGBUILD`, `.SRCINFO`, and the three
   `tab-atelier-headless.*` helper files to the `tab-atelier` AUR git repo.

## Dependency mapping (deb → Arch)

| Debian                         | Arch                               |
| ------------------------------ | ---------------------------------- |
| `libc6`, `libgcc-s1`           | `glibc`, `gcc-libs`                |
| `libfreetype6`                 | `freetype2`                        |
| `libxcb1`                      | `libxcb`                           |
| `libxkbcommon0` / `-x11-0`     | `libxkbcommon` / `libxkbcommon-x11`|
| `fonts-dejavu-core`            | `ttf-dejavu`                       |
| `fonts-noto-color-emoji` (Rec) | `noto-fonts-emoji` (optdepends)    |
| `nftables`, `bubblewrap`       | `nftables`, `bubblewrap`           |
| `adduser`, `init-system-helpers` | handled by sysusers/tmpfiles + systemd |
