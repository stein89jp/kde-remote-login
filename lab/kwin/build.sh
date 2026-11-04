#!/usr/bin/env bash
# Build the KWin probe clients. Output: $OUT/bin (default: lab/kwin/out).
# Protocol XMLs come from bridge/protocols (see its README for provenance).
set -euo pipefail
here=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
proto="$here/../../bridge/protocols"
out=${OUT:-$here/out}
mkdir -p "$out/gen" "$out/bin"
gen=()
for xml in xdg-shell fake-input zkde-screencast-unstable-v1 kde-output-device-v2 kde-output-management-v2; do
    wayland-scanner client-header "$proto/$xml.xml" "$out/gen/$xml-client.h"
    wayland-scanner private-code "$proto/$xml.xml" "$out/gen/$xml.c"
    gen+=("$out/gen/$xml.c")
done
cflags=(-O1 -g -Wall -Wno-unused-parameter -I"$out/gen" -I"$here")
wl=$(pkg-config --cflags --libs wayland-client)
pw=$(pkg-config --cflags --libs libpipewire-0.3)
for p in probe-globals target-window fake-input output-probe fd-handoff; do
    gcc "${cflags[@]}" -o "$out/bin/$p" "$here/$p.c" "${gen[@]}" $wl
done
gcc "${cflags[@]}" -o "$out/bin/input-target" "$here/input-target.c" "${gen[@]}" $wl $(pkg-config --cflags --libs xkbcommon)
gcc "${cflags[@]}" -o "$out/bin/screencast" "$here/screencast.c" "${gen[@]}" $wl $pw -lz -lm
# Optional toolkit clients for scenarios/bridge-{wheel-qt,wheel-gtk,clipboard}.sh.
if pkg-config --exists Qt6Widgets; then
    g++ -O1 -fPIC -o "$out/bin/qt-wheel" "$here/qt-wheel.cpp" $(pkg-config --cflags --libs Qt6Widgets)
    g++ -O1 -fPIC -o "$out/bin/qt-clip" "$here/qt-clip.cpp" $(pkg-config --cflags --libs Qt6Widgets)
fi
if pkg-config --exists gtk+-3.0; then
    gcc "${cflags[@]}" -o "$out/bin/gtk-wheel" "$here/gtk-wheel.c" $(pkg-config --cflags --libs gtk+-3.0)
fi
echo "built into $out/bin"
