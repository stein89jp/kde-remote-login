#!/usr/bin/env bash
# Q3 (and Q6 with --gpu): stream_output -> PipeWire consumer over a
# handed-off PipeWire fd. Timeline (seconds after the consumer starts):
#   0-5 idle, 5-10 window animating every frame, 10-12 idle,
#   ~11 window closes, 12-15 cursor-only motion over the bare desktop via
#   fake_input (KWin default cursor), 15-20 idle.
# Consumer args (e.g. --dmabuf) come from /work/sc-extra, the launcher
# (default "fd-handoff --pipewire --") from /work/sc-handoff.
exec >"$LOGS/screencast.out" 2>&1
extra=$(cat /work/sc-extra 2>/dev/null || true)
sleep 1
target-window --seconds 12 --animate 6 11 >"$LOGS/target.out" 2>&1 &
sleep 1
handoff=$(cat /work/sc-handoff 2>/dev/null || echo "fd-handoff --pipewire --")
echo "consumer launcher: $handoff"
$handoff screencast --seconds 20 --save "$LOGS/frame" $extra &
sc=$!
sleep 3
pw-dump >"$LOGS/pw-dump.json" 2>&1
sleep 9
fake-input --cursor-wiggle 3
wait $sc
