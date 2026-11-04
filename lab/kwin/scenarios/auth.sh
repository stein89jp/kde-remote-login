#!/usr/bin/env bash
# Q1: are restricted globals granted to (a) a plain client, (b) the
# broker-style fd-handoff receiver, (c) with/without a .desktop grant?
exec >"$LOGS/auth.out" 2>&1
sleep 1
echo "=== (a) plain unauthorized client, direct socket path"
probe-globals | grep -v '^global'
echo "=== (b) fd-handoff: connector child connects + exits, receiver binds"
fd-handoff -- probe-globals | grep -v '^global'
echo "=== (c) XDG_DATA_DIRS with an X-KDE-Wayland-Interfaces .desktop for probe-globals"
mkdir -p /tmp/xdg/applications
cat >/tmp/xdg/applications/kde-rdp-probe.desktop <<D
[Desktop Entry]
Type=Application
Name=probe
Exec=/work/bin/probe-globals
NoDisplay=true
X-KDE-Wayland-Interfaces=org_kde_kwin_fake_input,zkde_screencast_unstable_v1
D
XDG_DATA_DIRS=/tmp/xdg:/usr/share probe-globals | grep -v '^global'
echo "=== full global list (plain client)"
probe-globals | grep '^global'
