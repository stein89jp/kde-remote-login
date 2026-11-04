#!/usr/bin/env bash
# Q5: full Plasma under KWin --virtual, private dbus, no systemd.
# plasma_session (via the holder) is started in the background so the scenario can take a
# screenshot and then request logout; in production plasma_session itself is
# the --exit-with-session program (see session-plasma in docs/kde-findings.md).
exec >"$LOGS/plasma.out" 2>&1
export XDG_CURRENT_DESKTOP=KDE XDG_SESSION_DESKTOP=KDE XDG_SESSION_TYPE=wayland \
       KDE_FULL_SESSION=true KDE_SESSION_VERSION=6 KDE_APPLICATIONS_AS_SCOPE=0
mkdir -p "$HOME/.config"
t0=$(date +%s.%N)
# Under kwin_wayland_wrapper plain plasma_session is fine; under a direct
# kwin_wayland it must find org.kde.KWinWrapper already owned (holder).
launcher=/src/plasma-session-holder.py
[[ $KWIN_WRAPPER == 1 ]] && launcher=plasma_session
echo "launcher: $launcher"
$launcher >"$LOGS/plasma_session.log" 2>&1 &
ps_pid=$!
for i in $(seq 40); do
    sleep 1
    qdbus6 org.kde.plasmashell >/dev/null 2>&1 && { echo "org.kde.plasmashell on bus after ${i}s"; break; }
done
sleep 25
echo "=== bus names"; qdbus6 | grep -E 'org\.kde|freedesktop' | sort
echo "=== processes"; ps -eo pid,ppid,etimes,comm --sort=pid
echo "=== kwin support info (GL)"; qdbus6 org.kde.KWin /KWin supportInformation | grep -iE "OpenGL|GL_RENDERER|renderer|vendor|Compositing|Platform|backend|Virtual|Output|geometry" | head -30
echo "=== screenshot at ~27s"; timeout 30 spectacle -b -n -f -o "$LOGS/shot.png"; echo "spectacle rc=$?"
sleep 20
echo "=== screenshot at ~50s"; timeout 30 spectacle -b -n -f -o "$LOGS/shot-late.png"; echo "spectacle rc=$?"
echo "ksplashqml running: $(pgrep -c ksplashqml)"
echo "=== logout via org.kde.Shutdown at $(date +%s.%N)"
qdbus6 org.kde.Shutdown /Shutdown logout; echo "logout call rc=$?"
for i in $(seq 30); do kill -0 $ps_pid 2>/dev/null || break; sleep 1; done
if kill -0 $ps_pid 2>/dev/null; then echo "plasma_session still running ${i}s after logout"; else wait $ps_pid; echo "plasma_session exited rc=$? ${i}s after logout"; fi
echo "=== processes after logout"; ps -eo pid,ppid,comm --sort=pid
