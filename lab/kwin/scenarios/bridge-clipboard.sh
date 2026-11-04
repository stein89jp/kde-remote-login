#!/usr/bin/env bash
# Bridge clipboard (bridge/src/clipboard.rs) on KWin's ext_data_control_v1:
# the ignored live test clipboard::tests::live_ext_data_control_roundtrip
# (test binary copied to $OUT/bin/clipboard-live-test) publishes RDP text,
# HTML and PNG; qt-clip (build.sh) logs them, then sets its own selection,
# on a click from kwin-input-probe, which the test reads back in every
# wire form.
exec >"$LOGS/input.out" 2>&1
sleep 1
QT_QPA_PLATFORM=wayland qt-clip >"$LOGS/target.out" 2>&1 &
sleep 3
(sleep 3; fd-handoff -- kwin-input-probe >"$LOGS/input-probe.out" 2>&1) &
fd-handoff -- env KDE_RDP_LIVE_CLIPBOARD=1 clipboard-live-test --ignored --exact \
    clipboard::tests::live_ext_data_control_roundtrip --nocapture >"$LOGS/probe.out" 2>&1
echo "test exit $?"
wait
