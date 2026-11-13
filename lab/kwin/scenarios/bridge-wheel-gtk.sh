#!/usr/bin/env bash
# KwinInput wheel mapping as a GTK 3 client sees it: gtk-wheel (lab/kwin/gtk-wheel.c, a fullscreen
# GtkWindow printing smooth-scroll deltas; built by build.sh) under the kwin-input-probe sequence
# (copy target/release/kwin-input-probe to $OUT/bin).
exec >"$LOGS/input.out" 2>&1
sleep 1
GDK_BACKEND=wayland gtk-wheel >"$LOGS/target.out" 2>&1 &
sleep 3
fd-handoff -- kwin-input-probe >"$LOGS/probe.out" 2>&1
echo "probe exit $?"
wait
