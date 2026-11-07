#!/bin/bash
# Remove KDE-RDP from Bazzite / Fedora Atomic: the reverse of install.sh.
# Run it from a local terminal or SSH, not from a remote desktop: stopping the
# services ends every connected remote desktop. Details: docs/uninstall-bazzite.md.
#
#   ./packaging/bazzite/uninstall.sh [--purge] [-y]
set -euo pipefail

PACKAGE=kde-remote-login
NVIDIA_DROP_IN=/etc/systemd/system/kde-rdp-frontend.service.d/nvidia.conf
AUTOLOGIN_DROP_IN=zz-kde-rdp-no-autologin.conf

say() { printf '\n==> %s\n' "$*"; }
info() { printf '    %s\n' "$*"; }
die() { printf 'error: %s\n' "$*" >&2; exit 1; }

usage() {
    cat <<'EOF'
Usage: packaging/bazzite/uninstall.sh [options]

Stops and removes KDE-RDP. The removal finishes with the next reboot.

  --purge     also delete the TLS identity and settings (/etc/kde-rdp), the
              kde-rdp service account and the kde-rdp-users group
  -y, --yes   answer yes to questions
  -h, --help  show this help
EOF
}

confirm() {
    local answer
    [ "$assume_yes" = 1 ] && return 0
    [ -t 0 ] || return 1
    read -r -p "$1 [y/N] " answer
    case $answer in [Yy]|[Yy][Ee][Ss]) return 0 ;; *) return 1 ;; esac
}

purge=0
assume_yes=0
while [ $# -gt 0 ]; do
    case $1 in
        --purge) purge=1 ;;
        -y|--yes) assume_yes=1 ;;
        -h|--help) usage; exit 0 ;;
        *) die "unknown option: $1 (see --help)" ;;
    esac
    shift
done

[ "$(id -u)" -ne 0 ] || die "run this as your normal user; it uses sudo itself"
[ -e /run/ostree-booted ] || die "this is for Bazzite / Fedora Atomic; on Arch see docs/uninstall.md"
if [ -n "${KDE_RDP_RUNTIME_DIR:-}" ] || { [ -n "${XDG_SESSION_ID:-}" ] &&
        [ "$(loginctl show-session "$XDG_SESSION_ID" -p Service --value 2>/dev/null)" = kde-rdp ]; }; then
    die "run this from a local terminal or SSH: stopping KDE-RDP ends this remote desktop"
fi
confirm "Remove KDE-RDP? Connected remote desktops end now." || { info "Nothing changed."; exit 0; }
# Not sudo -v: it prompts even with a NOPASSWD rule when another entry lacks it.
sudo true

say "Stopping the services"
sudo systemctl disable --now kde-rdp-frontend.service 2>/dev/null || :
sudo systemctl stop kde-rdp-broker.service 2>/dev/null || :
sudo systemctl disable kde-rdp-broker.service 2>/dev/null || :

if [ -f "$NVIDIA_DROP_IN" ]; then
    say "Removing the NVIDIA encoder drop-in"
    sudo rm -f -- "$NVIDIA_DROP_IN"
    sudo rmdir --ignore-fail-on-non-empty -- "${NVIDIA_DROP_IN%/*}"
fi
sudo systemctl daemon-reload

if systemctl is-active --quiet firewalld; then
    for zone in $(sudo firewall-cmd --get-zones); do
        removed=0
        if sudo firewall-cmd --permanent --zone="$zone" --query-service=kde-rdp >/dev/null 2>&1; then
            sudo firewall-cmd --permanent --zone="$zone" --remove-service=kde-rdp >/dev/null
            removed=1
        fi
        if sudo firewall-cmd --zone="$zone" --query-service=kde-rdp >/dev/null 2>&1; then
            sudo firewall-cmd --zone="$zone" --remove-service=kde-rdp >/dev/null
            removed=1
        fi
        [ "$removed" = 0 ] || info "Removed service kde-rdp from firewalld zone $zone."
    done
fi

for manager in plasmalogin sddm; do
    if [ -f "/etc/$manager.conf.d/$AUTOLOGIN_DROP_IN" ]; then
        say "Restoring the $manager autologin setting"
        sudo rm -f -- "/etc/$manager.conf.d/$AUTOLOGIN_DROP_IN"
        if [ -f "/etc/$manager.conf.kde-rdp-backup" ]; then
            sudo mv -f -- "/etc/$manager.conf.kde-rdp-backup" "/etc/$manager.conf"
        fi
        info "Autologin is configured as before the installation (from the next boot)."
    fi
done

if command -v selinuxenabled >/dev/null && selinuxenabled &&
        grep -qx kde_rdp <<<"$(sudo semodule -l)"; then
    say "Removing the kde_rdp SELinux module"
    sudo semodule -X 200 -r kde_rdp >/dev/null || :
fi

say "Removing the $PACKAGE package"
state=$(rpm-ostree status --json | python3 -c '
import json, sys
newest = json.load(sys.stdin)["deployments"][0]
names = [p for p in newest.get("requested-local-packages", []) if p.startswith("kde-remote-login-")]
names += [p for p in newest.get("requested-packages", []) if p == "kde-remote-login"]
print(" ".join(names))
') || die "cannot read the rpm-ostree status (is an update running? try again shortly)"
if [ -n "$state" ]; then
    # shellcheck disable=SC2086 # one or more package specs
    sudo rpm-ostree uninstall $state
    info "The package is gone after the next reboot."
else
    info "$PACKAGE is not layered."
fi

if [ "$purge" = 1 ]; then
    say "Deleting settings, the TLS identity and the service account"
    sudo rm -rf -- /etc/kde-rdp /run/faillock-kde-rdp
    sudo rm -f -- /etc/pam.d/kde-rdp.rpmsave /etc/pam.d/kde-rdp.rpmnew
    if getent passwd kde-rdp >/dev/null; then sudo userdel kde-rdp || :; fi
    if getent group kde-rdp-users >/dev/null; then sudo groupdel kde-rdp-users || :; fi
    if getent group kde-rdp >/dev/null; then sudo groupdel kde-rdp || :; fi
else
    info ""
    info "Kept: /etc/kde-rdp (TLS identity and settings), the kde-rdp account and the"
    info "kde-rdp-users group, so a reinstall keeps the same certificate. Remove them"
    info "with: $0 --purge"
fi
say "Done. Reboot to finish removing $PACKAGE."
