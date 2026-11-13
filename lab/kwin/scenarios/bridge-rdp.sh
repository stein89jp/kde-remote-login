#!/usr/bin/env bash
# The bridge in disposable mode on KWin, with a real FreeRDP 3 client: the
# bridge connects /tmp/rt/wayland-0 and /tmp/rt/pipewire-0 itself
# (KDE_RDP_WAYLAND_SOCKET / KDE_RDP_PIPEWIRE_SOCKET). sdl-freerdp3 runs as
# a KWin client in the same session (its window is part of the captured
# desktop); RDP_CLIENT=rdp-probe uses the displayless lab/kwin/rdp-probe.c
# (bitmap updates, no GFX) instead. Settings come from /work/rdp-env (sourced).
exec >"$LOGS/rdp.out" 2>&1
[[ -f /work/rdp-env ]] && source /work/rdp-env
target-window --seconds 60 --animate 0 40 >"$LOGS/target.out" 2>&1 &
mkdir -p /tmp/tls
openssl req -x509 -newkey rsa:2048 -nodes -days 1 -subj /CN=localhost \
    -keyout /tmp/tls/key.pem -out /tmp/tls/cert.pem >/dev/null 2>&1
export KDE_RDP_MODE=disposable KDE_RDP_LISTEN=127.0.0.1:33991 \
    KDE_RDP_CERT=/tmp/tls/cert.pem KDE_RDP_KEY=/tmp/tls/key.pem \
    KDE_RDP_USERNAME=test KDE_RDP_PASSWORD=test \
    KDE_RDP_WAYLAND_SOCKET=/tmp/rt/wayland-0 KDE_RDP_PIPEWIRE_SOCKET=/tmp/rt/pipewire-0 \
    KDE_RDP_DIAGNOSTICS=1
kde-rdp-bridge >"$LOGS/bridge.log" 2>&1 &
bridge=$!
for _ in $(seq 50); do (exec 3<>/dev/tcp/127.0.0.1/33991) 2>/dev/null && break; sleep 0.1; done
if [[ ${RDP_CLIENT:-sdl-freerdp3} == rdp-probe ]]; then
    RDP_USER=test RDP_PASS=test timeout -k 2 $(( ${RDP_SECONDS:-12} + 10 )) rdp-probe 33991 \
        "${RDP_SIZE:-1024x768}" "${RDP_SECONDS:-12}" "$LOGS/rdp-frame.ppm" >"$LOGS/client.log" 2>&1
else
    timeout -k 2 "${RDP_SECONDS:-12}" sdl-freerdp3 /v:127.0.0.1:33991 /u:test /p:test /cert:ignore \
        /size:${RDP_SIZE:-1024x768} ${RDP_ARGS:-/gfx} /log-level:INFO >"$LOGS/client.log" 2>&1
fi
echo "client exit $?"
kill $bridge 2>/dev/null; wait $bridge 2>/dev/null
echo "bridge exit $?"
