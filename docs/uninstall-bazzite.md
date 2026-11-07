# Uninstall from Bazzite (Fedora Atomic)

Save your remote desktop work, then run this from a **local terminal or an SSH
session**, not from inside a remote desktop: stopping the services ends every
connected remote desktop.

```sh
cd kde-remote-login
./packaging/bazzite/uninstall.sh
systemctl reboot
```

The script reverses what the installer changed:

- stops and disables `kde-rdp-frontend` and `kde-rdp-broker`;
- removes the NVIDIA encoder drop-in
  (`/etc/systemd/system/kde-rdp-frontend.service.d/nvidia.conf`) if present;
- removes the `kde-rdp` firewalld service from every zone it was added to;
- removes the `kde_rdp` SELinux module from the running policy;
- restores the autologin setting if the installer turned it off (it removes
  its `zz-kde-rdp-no-autologin.conf` drop-in and, if it had to edit
  `/etc/plasmalogin.conf`, restores the backup);
- runs `rpm-ostree uninstall` for the layered package. Like every
  `rpm-ostree` change, the removal is complete after the next reboot.

It keeps `/etc/kde-rdp` (your settings and the TLS certificate, so a
reinstall keeps the same fingerprint), the `kde-rdp` service account and the
`kde-rdp-users` group. To delete those as well:

```sh
./packaging/bazzite/uninstall.sh --purge
```

`--purge` also removes the private PAM failure tally (`/run/faillock-kde-rdp`)
and any `/etc/pam.d/kde-rdp.rpmsave` left from a changed PAM file. Your user
accounts, home directories and Plasma settings are never touched. The build
container image and its cache remain in your podman storage; remove them with:

```sh
podman rmi --ignore $(podman images --filter reference=localhost/kde-rdp-build --quiet)
podman volume rm --force kde-rdp-build-cache
rm -rf packaging/bazzite/out
```

Verify afterwards that no `kde-rdp` logind session, remote `kwin_wayland
--virtual` process or port 33991 listener remains (`loginctl list-sessions`,
`ss -ltn sport = :33991`). Plasma, plasmalogin, firewalld and your PAM
configuration are otherwise unchanged.
