#!/usr/bin/env bash
# Inside the private dbus session.
cd /work
if [[ -f /work/pw-unrestricted ]]; then
    # Private PipeWire whose pipewire-0 socket skips module-access checks.
    mkdir -p /tmp/pwconf
    sed 's|#access.socket = { pipewire-0 = "default", pipewire-0-manager = "unrestricted" }|access.socket = { pipewire-0 = "unrestricted", pipewire-0-manager = "unrestricted" }|' \
        /usr/share/pipewire/pipewire.conf >/tmp/pwconf/pipewire.conf
    grep -n 'access.socket' /tmp/pwconf/pipewire.conf >"$LOGS/pw-access.txt"
    PIPEWIRE_CONFIG_DIR=/tmp/pwconf pipewire >"$LOGS/pipewire.log" 2>&1 &
else
    pipewire >"$LOGS/pipewire.log" 2>&1 &
fi
for _ in $(seq 50); do [[ -S /tmp/rt/pipewire-0 ]] && break; sleep 0.1; done
wireplumber >"$LOGS/wireplumber.log" 2>&1 &
sleep 0.5
w=${KWIN_SIZE%x*} h=${KWIN_SIZE#*x}
export QT_FORCE_STDERR_LOGGING=1
export QT_LOGGING_RULES=${QT_LOGGING_RULES:-"kwin_*.debug=true;kwin_libinput.debug=false;org.kde.plasma.session.debug=true;org.kde.plasma.ksmserver.debug=true"}
mkdir -m1777 -p /tmp/.X11-unix   # exists on a real host
kwin=(kwin_wayland --socket wayland-0)
[[ $KWIN_XWAYLAND == 1 ]] && kwin+=(--xwayland)
# The wrapper picks the first free wayland-N starting at 1 and passes its own
# --socket; an extra --socket would override the name but not the fd.
[[ $KWIN_WRAPPER == 1 ]] && kwin=(kwin_wayland_wrapper --xwayland)
"${kwin[@]}" --virtual --width "$w" --height "$h" --no-lockscreen \
    --exit-with-session="/src/scenarios/$SCENARIO.sh" >"$LOGS/kwin.log" 2>&1
echo "kwin exit status $?" >>"$LOGS/kwin.log"
