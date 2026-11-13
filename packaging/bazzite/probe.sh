#!/bin/bash
# Report the host facts the Bazzite/Fedora Atomic package depends on.
# Read-only: it changes nothing. Run it as your normal user on the Bazzite host
# (not inside a toolbox or distrobox); a few checks print more with sudo.
#   packaging/bazzite/probe.sh > artifacts/bazzite-probe.txt 2>&1
set -u

section() { printf '\n== %s\n' "$*"; }
run() { printf '$ %s\n' "$*"; "$@" 2>&1; }

if [ -f /run/.containerenv ] || [ -f /.dockerenv ]; then
    echo "warning: this looks like a container; run probe.sh on the host itself" >&2
fi

section "System"
run cat /etc/os-release
run rpm-ostree status --booted
run uname -r

section "Packages"
rpm -q kwin plasma-workspace plasma-desktop plasma-login-manager sddm \
    xdg-desktop-portal xdg-desktop-portal-kde xdg-desktop-portal-gtk \
    pipewire pipewire-pulseaudio wireplumber pipewire-utils pulseaudio-utils \
    python3 python3-gobject openssl dbus-tools dbus-daemon dbus-broker \
    xorg-x11-server-Xwayland dejavu-sans-fonts ffmpeg ffmpeg-libs ffmpeg-free \
    libavcodec-free libavcodec libxkbcommon mesa-libEGL libglvnd-egl \
    nvidia-modprobe nvidia-driver distrobox podman toolbox selinux-policy-targeted \
    firewalld policycoreutils-python-utils 2>&1
run kwin_wayland --version

section "FFmpeg"
for library in /usr/lib64/libavcodec.so.* /usr/lib64/libavutil.so.* \
               /usr/lib64/libswscale.so.* /usr/lib64/libswresample.so.*; do
    [ -e "$library" ] || continue
    printf '%-40s %s\n' "$library" "$(rpm -qf "$(readlink -f "$library")" 2>&1)"
done
if command -v ffmpeg >/dev/null; then
    ffmpeg -hide_banner -encoders 2>/dev/null | grep -Ei 'nvenc|libopenh264|libx264|vaapi' || echo "no H.264 hardware/x264 encoders listed"
else
    echo "ffmpeg CLI not installed"
fi

section "Repositories"
run ls /etc/yum.repos.d/
grep -H '^enabled=1' /etc/yum.repos.d/*.repo 2>/dev/null

section "Commands"
for command in pactl parec pw-cat pipewire pipewire-pulse wireplumber busctl \
               dbus-run-session dbus-update-activation-environment kwin_wayland \
               plasma_session plasmashell kded6 openssl nologin distrobox-enter \
               nvidia-modprobe firewall-cmd semodule semanage restorecon \
               systemd-sysusers useradd usermod passwd; do
    printf '%-40s %s\n' "$command" "$(command -v "$command" || echo MISSING)"
done

section "Paths"
ls -l /usr/libexec/xdg-desktop-portal* /usr/lib/xdg-desktop-portal* 2>&1
ls -ld /var/empty /usr/bin/nologin /usr/sbin/nologin 2>&1
ls -l /usr/share/pipewire/pipewire.conf /usr/share/pipewire/pipewire-pulse.conf \
    /usr/share/wireplumber/wireplumber.conf \
    /usr/lib/systemd/user-environment-generators/30-systemd-environment-d-generator \
    /etc/xdg/autostart/org.kde.plasmashell.desktop \
    /usr/share/xdg-desktop-portal/kde-portals.conf \
    /usr/libexec/ibus-ui-gtk3 /usr/share/fonts/dejavu-sans-fonts 2>&1
run ls -l /dev/dri/ /dev/udmabuf
ls -l /dev/nvidia* 2>&1
ls /proc/driver/nvidia/gpus 2>&1

section "Users and groups"
grep -H 'kde-rdp' /etc/passwd /etc/group /usr/lib/passwd /usr/lib/group 2>/dev/null || echo "no kde-rdp entries"
grep -E '^(passwd|group|shadow):' /etc/nsswitch.conf
run id

section "SELinux"
run getenforce
matchpathcon /usr/libexec/kde-rdp/kde-rdp-broker /usr/lib/kde-rdp/kde-rdp-broker \
    /etc/kde-rdp/tls /run/kde-rdp 2>&1
run cat /etc/selinux/targeted/contexts/users/unconfined_u
run cat /etc/selinux/targeted/contexts/failsafe_context
# shellcheck disable=SC2009 # pgrep cannot print SELinux contexts.
ps -eZ 2>/dev/null | grep -E 'kde-rdp|plasmashell|kwin_wayland' | head -n 20

section "Firewall"
run systemctl is-active firewalld
run firewall-cmd --get-default-zone
run firewall-cmd --get-active-zones
if [ "$(id -u)" -eq 0 ]; then
    run firewall-cmd --list-all
    run firewall-cmd --info-service=kde-rdp
else
    echo "(firewall-cmd --list-all needs sudo)"
fi
for zone in /etc/firewalld/zones/*.xml /usr/lib/firewalld/zones/FedoraWorkstation.xml; do
    [ -f "$zone" ] && { printf -- '-- %s\n' "$zone"; cat "$zone"; }
done

section "PAM"
for file in /etc/pam.d/kde-rdp /etc/pam.d/sshd /etc/pam.d/password-auth \
            /etc/pam.d/system-auth /etc/pam.d/postlogin /etc/pam.d/plasmalogin \
            /etc/pam.d/sddm; do
    [ -f "$file" ] && { printf -- '-- %s\n' "$file"; grep -v '^[[:space:]]*$' "$file"; }
done
command -v authselect >/dev/null && run authselect current

section "Login manager and autologin"
run systemctl status display-manager --no-pager --lines=0
for file in /etc/plasmalogin.conf /etc/plasmalogin.conf.d/*.conf /usr/lib/plasmalogin.conf.d/*.conf \
            /etc/sddm.conf /etc/sddm.conf.d/*.conf /usr/lib/sddm/sddm.conf.d/*.conf; do
    [ -f "$file" ] && { printf -- '-- %s\n' "$file"; cat "$file"; }
done
run loginctl list-sessions --no-legend
for session in $(loginctl list-sessions --no-legend | awk '{print $1}'); do
    loginctl show-session "$session" -p Name -p Type -p Class -p Service -p Remote -p Desktop -p State
done

section "systemd"
run systemctl --version
for unit in kde-rdp-broker kde-rdp-frontend kde-rdp-nvidia-devices; do
    printf '%-28s %s %s\n' "$unit" "$(systemctl is-enabled "$unit" 2>&1)" "$(systemctl is-active "$unit" 2>&1)"
done

section "Build tools"
rustc --version 2>&1
run podman --version
run toolbox --version
