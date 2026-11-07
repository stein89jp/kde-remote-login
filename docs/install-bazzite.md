# Install on Bazzite (Fedora Atomic KDE)

KDE-RDP runs on **Bazzite KDE** images (`bazzite`, `bazzite-nvidia`,
`bazzite-nvidia-open`) and should work on other Fedora Atomic KDE desktops
(Kinoite). It is the same remote desktop as on Arch: a stock Plasma session on
KWin's virtual backend, a root PAM/logind broker and the unprivileged RDP
frontend on TCP 33991. Only the packaging differs: an RPM layered with
`rpm-ostree`, built locally by one script. For Arch, see [install](install.md).

**Prerequisites:**

- A **KDE** image with **KWin 6.8 or newer** (`kwin_wayland --version`). KWin
  6.7 still restricts the screencast and fake-input interfaces with a
  `.desktop` grant that this design does not use, so the installer refuses
  older versions; update first (`ujust update`, reboot).
- **A GPU render node** (`/dev/dri/renderD*`). Software rendering is not
  supported. In a virtual machine, use a virtio GPU with 3D acceleration
  (virt-manager: Video *Virtio* with *3D acceleration*, Display *Spice* with
  *OpenGL*).
- **No staged system update.** `rpm-ostree` can apply the package to the
  running system only when no update is waiting for a reboot. If one is, the
  installer stops before building and asks you to reboot first.
- Internet access for the first build, and about 6 GB of free disk space for
  the build container and its caches (in your podman storage, not in the OS).

## Install

From a normal terminal on the Bazzite desktop (not inside toolbox or
distrobox), as your normal user:

```sh
git clone https://github.com/stein89jp/kde-remote-login.git
cd kde-remote-login
./packaging/bazzite/install.sh
```

The installer asks for your sudo password once, after the build, and prints
what it does. It:

1. checks the system (Bazzite/Fedora Atomic, KDE, KWin 6.8+, no staged update);
2. builds the `kde-remote-login` RPM inside a podman container that matches
   your Fedora release and the FFmpeg your system uses (negativo17 on
   Bazzite), then runs the package's test suites. Nothing is installed on the
   host for the build. The first build downloads a Fedora image and Rust
   crates and compiles for a while; later builds reuse a cache;
3. checks that every library the binaries need exists on your system, then
   layers the RPM with `rpm-ostree` and applies it to the running system,
   without a reboot, and activates the package's small SELinux module (see
   [SELinux](#selinux));
4. authorizes your account (adds it to `kde-rdp-users`) and creates the TLS
   certificate;
5. turns off autologin for that account if plasmalogin (or SDDM) has it on,
   after asking. The broker refuses remote logins while the account has a
   local desktop, so autologin would block remote access after every boot;
6. enables NVENC hardware encoding when an NVIDIA GPU is present (installs the
   `nvidia.conf` drop-in described in [install](install.md#optional-nvidia-encoding));
7. opens TCP 33991 in firewalld, only if the active zone does not already
   allow it. Bazzite's default `FedoraWorkstation` zone allows ports
   1025-65535, so usually nothing changes;
8. enables and starts `kde-rdp-frontend.service` (which starts the broker),
   then prints the addresses to connect to and the certificate fingerprint.

Options:

| Option | Effect |
| --- | --- |
| `--user NAME` | Authorize an existing account instead of yours |
| `--create-user NAME` | Create a separate account for remote logins (prompts for its password) |
| `--keep-autologin` | Leave autologin as it is |
| `--no-firewall` | Leave firewalld alone |
| `--no-nvidia` | Do not enable NVENC encoding |
| `--skip-tests` | Skip the package's test suites during the build (faster) |
| `--build-only` | Only build the RPM into `packaging/bazzite/out/rpms/` |
| `--finish` | Skip the build; authorize, configure and start (see below) |
| `-y`, `--yes` | Answer yes to questions |

If `rpm-ostree` cannot apply the package live, the installer says so: reboot,
then run `./packaging/bazzite/install.sh --finish` to complete steps 4-8.

## Connect

Connect exactly as described in [install](install.md#connect-with-windows-remote-desktop):
Windows Remote Desktop (`mstsc.exe`) to `HOST:33991`, your Linux account name
and password. Audio, the microphone, the clipboard and resizing work the same.

**Log out of the account's local desktop before connecting.** As on Arch, the
broker refuses a remote login while the same account has a local graphical
session (the login screen shows "A local desktop session is still open."). This is the
intended single-account workflow: log out at the machine, then connect. A
logout can take a few seconds to finish; if the first attempt is refused,
retry. SSH and TTY logins do not count. Bazzite's desktop images do not log
in automatically unless you turned that on; the installer checks.

To use a separate account instead, so your own can stay logged in locally:

```sh
./packaging/bazzite/install.sh --create-user remote
```

## Update

Pull the new version and run the installer again. It builds the new RPM and
replaces the layered one in the same way:

```sh
cd kde-remote-login
git pull
./packaging/bazzite/install.sh
```

The running services restart onto the new version, which ends connected remote
desktops. If you run the installer from inside a remote desktop, it does not
restart them: the new version starts at the next reboot, or after
`sudo systemctl restart kde-rdp-broker kde-rdp-frontend` from a local or SSH
terminal.

## Bazzite system updates

The layered package stays installed across normal Bazzite updates
(`ujust update`, automatic updates) and keeps working as long as the update
keeps the libraries it was built against. The main risk is a new FFmpeg major
version (a changed `libavcodec.so.N`): `rpm-ostree` then refuses the update
instead of breaking KDE-RDP, and Bazzite's updater reports the failure. To
recover, from a local terminal:

```sh
sudo rpm-ostree uninstall kde-remote-login   # find the exact name with: rpm-ostree status
ujust update                                 # or: sudo rpm-ostree upgrade
systemctl reboot
./packaging/bazzite/install.sh               # rebuild against the new libraries
```

Your settings in `/etc/kde-rdp`, the TLS certificate and the account
authorization survive this.

## What is installed where

| Item | Bazzite / Fedora | Arch |
| --- | --- | --- |
| Frontend, broker, session helpers | `/usr/libexec/kde-rdp/` | `/usr/lib/kde-rdp/` |
| PAM policy | `/etc/pam.d/kde-rdp` ([Fedora stack](../packaging/fedora/kde-rdp.pam), with `pam_selinux`) | `/etc/pam.d/kde-rdp` |
| Settings and TLS identity | `/etc/kde-rdp/` | same |
| systemd units | `kde-rdp-broker`, `kde-rdp-frontend`, `kde-rdp-nvidia-devices` | same |
| firewalld service | `/usr/lib/firewalld/services/kde-rdp.xml` | none |
| SELinux module | `kde_rdp` ([one rule](../packaging/fedora/kde_rdp.cil)) | none |
| Documentation | `/usr/share/doc/kde-remote-login/` | same |

Everything else in [install](install.md) applies unchanged: the
[broker settings](install.md#broker-settings), the
[remote microphone](install.md#remote-microphone),
[input methods](install.md#input-methods-and-keyboard-layouts), the
[login audit log and fail2ban](install.md#login-audit-log-and-fail2ban),
[Flatpak applications](install.md#flatpak-applications-and-updates) and
[performance settings](performance.md). Where those pages show
`/usr/lib/kde-rdp/...`, use `/usr/libexec/kde-rdp/...`; where they use
`pacman`, the package is managed with `rpm-ostree` instead.

## SELinux

SELinux stays enforcing. The broker and frontend run as ordinary system
services (`unconfined_service_t`), and the PAM policy (`pam_selinux`) moves
each remote desktop into your normal login context (`unconfined_t`, with
Xwayland in `xserver_t`), the same as a local or SSH login. Files the remote
desktop creates in your home therefore get their usual labels, and Flatpak,
podman, toolbox and distrobox behave as they do locally.

Fedora's policy lets only sshd and display managers enter a user's login
context, so the package adds one rule as the SELinux module `kde_rdp`: the
broker's service domain may start the desktop in `unconfined_t`. Both domains
are already unconfined, so the rule grants no new access. The package installs
the module into each deployment's policy (it survives Bazzite updates and is
gone with the package); the installer also loads it into the running system.
Check it with `sudo semodule -l | grep kde_rdp`.

## Known differences from a local session

- **Administrator prompts.** Polkit grants actions such as network control
  without a password only to the *active local* session. In a remote desktop,
  Plasma's network applet and similar tools ask for an administrator password
  instead (your own, if your account is an administrator). These are the
  upstream polkit defaults for remote sessions, not specific to Bazzite.
- **No accessibility bus.** The remote desktop uses a private D-Bus bus without
  systemd, so its bus launches `at-spi-bus-launcher` directly, and Fedora's
  policy has no valid context for that (`SELINUX_ERR ... gnome_atspi_t` in the
  audit log). Screen readers do not work in remote desktops, and Flatpak
  prints `Can't find a11y bus`; applications otherwise start normally.
- **Virtual machines** usually lack the NVIDIA block-linear buffers the
  zero-copy capture path uses; the frontend logs `DMA-BUF screencast
  unavailable; using SHM` and works normally.

## Troubleshooting

```sh
systemctl status kde-rdp-broker kde-rdp-frontend
journalctl -b -u kde-rdp-broker -u kde-rdp-frontend
sudo ausearch -m AVC -ts boot          # SELinux denials, if any
rpm-ostree status                      # is kde-remote-login layered?
```

- **"A local desktop session is still open.":** the account still has a local
  graphical session (`loginctl list-sessions`). Log out locally, wait a few
  seconds, and retry.
- **Five failed logins lock the account for 5 minutes** for RDP only. Reset
  with `sudo faillock --dir /run/faillock-kde-rdp --user NAME --reset`.
- **The installer stops because a system update is staged:** reboot, then run
  it again.
- **No desktop appears after login:** check that a render node exists and is
  usable (`ls -l /dev/dri/`); in a VM, enable 3D acceleration.

To remove KDE-RDP, see [uninstall-bazzite](uninstall-bazzite.md).
