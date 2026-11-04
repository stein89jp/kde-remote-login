#!/usr/bin/env bash
# KwinInput wheel mapping as a Qt 6 client sees it: qt-wheel (lab/kwin/qt-wheel.cpp, a fullscreen
# QWidget printing QWheelEvent deltas; built by build.sh) under the kwin-input-probe sequence
# (copy target/release/kwin-input-probe to $OUT/bin).
exec >"$LOGS/input.out" 2>&1
sleep 1
QT_QPA_PLATFORM=wayland qt-wheel >"$LOGS/target.out" 2>&1 &
sleep 3
fd-handoff -- kwin-input-probe >"$LOGS/probe.out" 2>&1
echo "probe exit $?"
wait
