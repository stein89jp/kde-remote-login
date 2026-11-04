#!/usr/bin/env bash
# KDE port bridge integration in disposable KWin sandboxes (run-sandbox.sh
# --gpu: private dbus, PipeWire + WirePlumber, kwin_wayland --virtual on the
# NVIDIA render node only). Results: $OUT/logs/bridge-*-gpu/.
#   lab/kwin/bridge-integration.sh [capture] [rdp-shm] [rdp-nvenc] [rdp-bitmap]
# capture:    kwin-capture-probe: frames, resizes, DMA-BUF tickets, idle.
# rdp-*:      kde-rdp-bridge (disposable mode) + a FreeRDP 3 client:
#             rdp-bitmap = lab/kwin/rdp-probe.c, no GFX (bitmap updates
#                          from SHM frames, decoded desktop saved),
#             rdp-shm    = GFX/AVC420 with NVENC from SHM frames,
#             rdp-nvenc  = GFX/AVC420 with NVENC from DMA-BUF frames.
set -euo pipefail
here=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
root=$(cd -- "$here/../.." && pwd)
out=${OUT:-$here/out}
export OUT=$out
[[ -x $out/bin/target-window ]] || "$here/build.sh"
cargo build --manifest-path "$root/bridge/Cargo.toml" --release --locked \
    --bin kde-rdp-bridge --bin kwin-capture-probe
gcc -O1 -Wall -o "$out/bin/rdp-probe" "$here/rdp-probe.c" -Wno-deprecated-declarations $(pkg-config --cflags --libs freerdp3 winpr3)
for b in kde-rdp-bridge kwin-capture-probe; do
    install -m755 "$root/bridge/target/release/$b" "$out/bin/$b"
done
scenarios=("$@")
[[ ${#scenarios[@]} -gt 0 ]] || scenarios=(capture rdp-bitmap rdp-shm rdp-nvenc)
for s in "${scenarios[@]}"; do
    rm -f "$out/capture-env" "$out/rdp-env"
    case $s in
        capture)
            cat >"$out/capture-env" <<ENV
export KDE_RDP_PROBE_SIZE=1280x720
export KDE_RDP_PROBE_GPU=${GPU:-1} KDE_RDP_PROBE_IDLE_SECONDS=${IDLE:-6} ANIMATE_UNTIL=${ANIMATE_UNTIL:-9}
export KDE_RDP_PROBE_EMBEDDED=${EMBEDDED:-1} KDE_RDP_PROBE_CURSOR=${CURSOR:-1} WINDOW_SECONDS=${WINDOW_SECONDS:-15} KDE_RDP_PROBE_STATIC=${STATIC:-0}
export KDE_RDP_PROBE_RESIZE=${RESIZE:-1920x1080,1600x900,1024x768}
export KDE_RDP_PROBE_EXIT_DELAY_MS=${EXIT_DELAY_MS:-1000}
ENV
            TIMEOUT=${TIMEOUT:-90} "$here/run-sandbox.sh" --gpu bridge-capture >"$out/sandbox-$s.log" 2>&1 || true
            cat "$out/logs/bridge-capture-gpu/probe.out" ;;
        rdp-bitmap|rdp-shm|rdp-nvenc)
            case $s in
                rdp-bitmap) env='export KDE_RDP_ENCODER=bitmap RDP_CLIENT=rdp-probe' ;;
                rdp-shm) env='export KDE_RDP_ENCODER=nvenc KDE_RDP_GPU_CAPTURE=shm RDP_ARGS="/gfx:AVC420"' ;;
                rdp-nvenc) env='export KDE_RDP_ENCODER=nvenc KDE_RDP_GPU_CAPTURE=auto RDP_ARGS="/gfx:AVC420"' ;;
            esac
            printf '%s\nexport RDP_SECONDS=%s\n' "$env" "${RDP_SECONDS:-12}" >"$out/rdp-env"
            mkdir -p "$out/logs/bridge-rdp-gpu"
            TIMEOUT=${TIMEOUT:-90} "$here/run-sandbox.sh" --gpu bridge-rdp >"$out/sandbox-$s.log" 2>&1 || true
            rm -rf "$out/logs/$s"; cp -r "$out/logs/bridge-rdp-gpu" "$out/logs/$s"
            echo "== $s"; cat "$out/logs/$s/rdp.out"
            grep -h '"probe"' "$out/logs/$s/client.log" || true
            grep -E '^(RDP graphics|RDP GPU|RDP connection summary)' "$out/logs/$s/bridge.log" | grep -v 'duration_s=0.0' || true
            grep -E '"component":"video"' "$out/logs/$s/bridge.log" | tail -n 1 || true ;;
        *) echo "unknown scenario $s" >&2; exit 2 ;;
    esac
done
