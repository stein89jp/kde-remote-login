#!/usr/bin/env bash
# Bridge screencast capture (bridge/src/wayland/run.rs + screencast.c) on
# KWin: kwin-capture-probe drives Desktop directly (no RDP): first frame,
# frame flow while target-window animates, stream-size resizes through the
# display-control path, the DMA-BUF ticket path and back, and a quiet desktop.
# Probe settings come from /work/capture-env (sourced; see bridge-integration.sh).
exec >"$LOGS/capture.out" 2>&1
[[ -f /work/capture-env ]] && source /work/capture-env
target-window --seconds ${WINDOW_SECONDS:-15} --animate 0 ${ANIMATE_UNTIL:-9} >"$LOGS/target.out" 2>&1 &
sleep 1
KDE_RDP_PROBE_SAVE="$LOGS/frame" kwin-capture-probe /tmp/rt/wayland-0 >"$LOGS/probe.out" 2>"$LOGS/probe.err"
echo "probe exit $?"
pw-dump >"$LOGS/pw-dump.json" 2>&1
