# Uninstall the Arch remote desktop host

Save your remote desktop work, then run these commands from a local terminal or
an independent SSH session. Stopping the services ends the remote desktop.

```sh
sudo systemctl disable --now kde-rdp-frontend.service
sudo systemctl stop kde-rdp-broker.service
loginctl list-sessions
sudo pacman -R kde-remote-login
sudo systemctl daemon-reload
```

Verify no `kde-rdp` logind session, remote `kwin_wayland --virtual` process,
frontend or port 33991 listener remains. If broker cleanup failed, inspect the session's service, UID and leader
before terminating **that specific kde-rdp session** with
`sudo loginctl terminate-session SESSION_ID`. Do not terminate local Plasma,
GNOME or other sessions.

Older or manual installations can leave portal files behind, causing a
`conflicting files` error when reinstalling. Pacman normally removes these files
when uninstalling the package. If either remains, check its ownership:

```sh
pacman -Qo /usr/lib/kde-rdp/session-portals \
  /usr/share/xdg-desktop-portal/kde-rdp-portals.conf
```

If no package owns the remaining files, remove them before reinstalling:

```sh
sudo rm -f -- /usr/lib/kde-rdp/session-portals \
  /usr/share/xdg-desktop-portal/kde-rdp-portals.conf
```

If another package owns either file, resolve that package's ownership before
removing the file.

Pacman preserves changed configuration as `.pacsave`; inspect it before removal.
The package configuration files are `/etc/kde-rdp/frontend.conf`,
`renderer.conf`, `broker.conf` (left as
`/etc/kde-rdp/broker.conf.pacsave` if you changed the grace period) and
`/etc/pam.d/kde-rdp`. A `frontend.conf` in which you uncommented settings,
such as the `KDE_RDP_AUDIO_CHANNELS`, `KDE_RDP_AUDIO_RATE` or
`KDE_RDP_AUDIO_DRIFT_PPM` playback settings, is left as `frontend.conf.pacsave`;
a fresh install starts from the shipped defaults (44.1 kHz stereo PCM), so copy
over only the settings you still want. Remove the leftovers when discarding the
experiment:

```sh
sudo rm -f -- /etc/kde-rdp/*.pacsave /etc/kde-rdp/*.pacnew \
  /etc/pam.d/kde-rdp.pacsave /etc/pam.d/kde-rdp.pacnew
```

If you copied the fail2ban example, `/etc/fail2ban/filter.d/kde-rdp.conf` and
`/etc/fail2ban/jail.d/kde-rdp.local` are not package-owned. Remove both and run
`sudo fail2ban-client reload` (or disable fail2ban if nothing else uses it).

TLS identity files in `/etc/kde-rdp/tls`, including the `generated` marker that
records a self-signed certificate for renewal, are generated on first start (or
provided manually) and are not package-owned. Remove those files and the
remaining project configuration only after deciding to discard the experiment
(`sudo rm -rf -- /etc/kde-rdp` removes the TLS identity and every remaining
file there; a later install then creates a new certificate with a new
fingerprint). Keep backups if rollback is intended.
If you enabled NVIDIA encoding, its copied drop-in
`/etc/systemd/system/kde-rdp-frontend.service.d/nvidia.conf` is also not
package-owned. Remove that file when discarding the experiment, then run
`sudo systemctl daemon-reload` again. If you keep it for a reinstall, copy the
new package's drop-in over it afterwards as described in
[install](install.md#optional-nvidia-encoding); older copies lack the device
allow-list that NVENC now needs.
The NVIDIA device-preparation helper is package-owned and is removed by pacman.
If you installed the earlier manual workaround, also remove its
local `/etc/systemd/system/kde-rdp-nvidia-devices.service` and
`/etc/systemd/system/kde-rdp-frontend.service.d/nvidia-devices.conf`; pacman
does not own those local files.
The private PAM failure tally can optionally be removed with
`sudo rm -rf /run/faillock-kde-rdp`. It is a tmpfs directory and is also
cleared at reboot.
The service user and `kde-rdp-users` group also remain; remove them only if they
are unused. The dedicated human test account/home remains your choice; package
removal never deletes user data or accounts.

KWin, Plasma, SDDM, KRDP, Niri-RDP, GNOME Remote Login, GDM, their PAM files and
system settings are untouched by this package. Removing it does not remove
Plasma; the Plasma packages it pulled in as dependencies stay installed unless
you remove them yourself (`pacman -Qdt` lists orphans).
Your Plasma configuration, input-method profiles, `environment.d`, and XDG
autostart entries are preserved and reused when you reinstall.

To switch back to Niri-RDP, uninstall this package as above first: both
frontends use TCP 33991, and `kde-rdp-frontend.service` conflicts with
`niri-rdp-frontend.service`.
To reinstall your current checkout with new fixes, follow [install](install.md)
using `makepkg -fsi` from its `packaging/arch/kde-rdp` directory. Removing the
installed package does not remove old package archives from that build directory;
the `-f` ensures those archives are rebuilt from the current source.
