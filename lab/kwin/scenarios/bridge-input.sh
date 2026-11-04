#!/usr/bin/env bash
# Bridge KwinInput (bridge/src/wayland/kwin_input.rs) end to end: the
# kwin-input-probe binary (cargo build --release --bin kwin-input-probe, copied
# to $OUT/bin) types and points into a fullscreen input-target window, which
# decodes text with xkbcommon. The probe connects through a broker-style fd
# handoff like the production bridge.
exec >"$LOGS/input.out" 2>&1
sleep 1
input-target --seconds 14 >"$LOGS/target.out" 2>&1 &
sleep 2
fd-handoff -- kwin-input-probe >"$LOGS/probe.out" 2>&1
echo "probe exit $?"
wait
