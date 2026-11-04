#!/usr/bin/env bash
# The bridge in broker mode on KWin, like an installed frontend: a mock NRB3
# broker (no PAM) hands over KWin's wayland-0 and the session's pipewire-0, and
# a FreeRDP 3 client sends direct ClientInfo credentials (no login form), as
# mstsc does. Settings come from /work/rdp-env (sourced).
exec >"$LOGS/rdp.out" 2>&1
[[ -f /work/rdp-env ]] && source /work/rdp-env
target-window --seconds 60 --animate 0 40 >"$LOGS/target.out" 2>&1 &
mkdir -p /tmp/tls /run/kde-rdp
openssl req -x509 -newkey rsa:2048 -nodes -days 1 -subj /CN=localhost \
    -keyout /tmp/tls/key.pem -out /tmp/tls/cert.pem >/dev/null 2>&1
cat >/tmp/mock-broker.py <<'PY'
import array, socket, struct
listener = socket.socket(socket.AF_UNIX)
listener.bind("/run/kde-rdp/broker.sock")
listener.listen(4)
leases = []
while True:
    stream, _ = listener.accept()
    magic, ul, pl, w, h, family = struct.unpack("!4sHHHHB", stream.recv(13, socket.MSG_WAITALL))
    stream.recv({0: 0, 4: 4, 6: 16}[family] + ul + pl, socket.MSG_WAITALL)
    print("broker request", magic, w, h, flush=True)
    fds = []
    for name in ("wayland-0", "pipewire-0"):
        s = socket.socket(socket.AF_UNIX)
        s.connect("/tmp/rt/" + name)
        fds.append(s)
    stream.sendmsg([b"NRB3\x00"], [(socket.SOL_SOCKET, socket.SCM_RIGHTS,
                                   array.array("i", [s.fileno() for s in fds]))])
    for s in fds:
        s.close()
    leases.append(stream)
PY
python3 /tmp/mock-broker.py >"$LOGS/broker.log" 2>&1 &
for _ in $(seq 50); do [[ -S /run/kde-rdp/broker.sock ]] && break; sleep 0.1; done
export KDE_RDP_MODE=broker KDE_RDP_LISTEN=127.0.0.1:33991 \
    KDE_RDP_CERT=/tmp/tls/cert.pem KDE_RDP_KEY=/tmp/tls/key.pem \
    KDE_RDP_DIAGNOSTICS=1
kde-rdp-bridge >"$LOGS/bridge.log" 2>&1 &
bridge=$!
for _ in $(seq 50); do (exec 3<>/dev/tcp/127.0.0.1/33991) 2>/dev/null && break; sleep 0.1; done
timeout -k 2 "${RDP_SECONDS:-12}" sdl-freerdp3 /v:127.0.0.1:33991 /u:test /p:test /sec:tls /cert:ignore \
    /size:${RDP_SIZE:-1280x800} ${RDP_ARGS:-/gfx:AVC420} /log-level:INFO >"$LOGS/client.log" 2>&1
echo "client exit $?"
kill $bridge 2>/dev/null; wait $bridge 2>/dev/null
echo "bridge exit $?"
