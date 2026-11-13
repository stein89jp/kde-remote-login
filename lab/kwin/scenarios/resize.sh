#!/usr/bin/env bash
# Q4: runtime resize of the virtual output via kde_output_management_v2
# custom modes, while a screencast consumer watches for renegotiation;
# then the stream_virtual_output alternative.
exec >"$LOGS/resize.out" 2>&1
sleep 1
echo "=== initial output state"
output-probe
fd-handoff --pipewire --linger 2 -- screencast --quiet --seconds 14 >"$LOGS/sc.out" 2>&1 &
sc=$!
sleep 3
echo "=== resize to 1600x900 (custom mode)"
output-probe --resize 1600 900
sleep 3
echo "=== resize to 1024x768 (custom mode)"
output-probe --resize 1024 768
sleep 3
echo "=== back to the original 1280x720 mode (listed mode, no custom)"
output-probe --mode-only 1280 720
wait $sc
echo "=== final output state"
output-probe
echo "=== stream_virtual_output 1920x1080 alternative"
fd-handoff --pipewire --linger 2 -- screencast --virtual 1920 1080 --seconds 3 --quiet >"$LOGS/sc-virtual.out" 2>&1
echo "=== outputs while no virtual stream"
output-probe | grep -E 'name|current_mode'
