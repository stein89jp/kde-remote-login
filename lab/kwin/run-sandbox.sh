#!/usr/bin/env bash
# Run a KWin --virtual lab scenario in a disposable bwrap sandbox.
#   lab/kwin/run-sandbox.sh [--gpu] [--wrapper|--xwayland] [--size WxH] SCENARIO
# --wrapper starts KWin as `kwin_wayland_wrapper --xwayland` (owns
# org.kde.KWinWrapper; its socket is wayland-1, never wayland-0).
# --xwayland passes --xwayland to a direct kwin_wayland.
# SCENARIO is lab/kwin/scenarios/SCENARIO.sh; it runs as KWin's
# --exit-with-session program inside a private dbus session with a private
# PipeWire + WirePlumber. Probes are expected in $OUT/bin (build.sh).
# Logs: $OUT/logs/SCENARIO/. --gpu exposes only the NVIDIA render node and
# /dev/nvidia* (never /dev/dri/card*) and drops software rendering.
set -euo pipefail
here=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
out=${OUT:-$here/out}
gpu=0 size=1280x720 wrapper=0 xwayland=0
while [[ ${1:-} == --* ]]; do
    case $1 in
        --gpu) gpu=1; shift ;;
        --size) size=$2; shift 2 ;;
        --wrapper) wrapper=1; shift ;;
        --xwayland) xwayland=1; shift ;;
        *) echo "unknown option $1" >&2; exit 2 ;;
    esac
done
scenario=${1:?scenario}
tag=$scenario; [[ $gpu == 1 ]] && tag=$scenario-gpu
mkdir -p "$out/logs/$tag"
args=(--unshare-all --die-with-parent --new-session --ro-bind /usr /usr
      --symlink usr/lib /lib --symlink usr/lib /lib64 --symlink usr/bin /bin --ro-bind /etc /etc
      --proc /proc --dev /dev --tmpfs /tmp --tmpfs /run --tmpfs /var --dir /home/test
      --bind "$out" /work --ro-bind "$here" /src --chdir /work
      --clearenv --setenv PATH /work/bin:/usr/bin --setenv HOME /home/test
      --setenv XDG_RUNTIME_DIR /tmp/rt --setenv PIPEWIRE_RUNTIME_DIR /tmp/rt
      --setenv LOGS "/work/logs/$tag" --setenv SCENARIO "$scenario"
      --setenv KWIN_SIZE "$size" --setenv KWIN_WRAPPER "$wrapper" --setenv KWIN_XWAYLAND "$xwayland")
if [[ $gpu == 1 ]]; then
    shopt -s nullglob
    for d in /dev/nvidia* /dev/dri/renderD*; do args+=(--dev-bind "$d" "$d"); done
    args+=(--ro-bind /sys /sys)
else
    args+=(--setenv LIBGL_ALWAYS_SOFTWARE 1)
fi
exec timeout -k 5 ${TIMEOUT:-120} bwrap "${args[@]}" bash /src/inner.sh
