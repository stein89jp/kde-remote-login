#!/usr/bin/env bash
# Q4b: the consumer's negotiated size drives the virtual output size
# (OutputScreenCastSource::followsStreamSize() == VirtualOutput::canResize()).
exec >"$LOGS/resize-stream.out" 2>&1
NV=0x300000000606010,0x300000000606011,0x300000000606012,0x300000000606013,0x300000000606014,0x300000000606015,0x300000000e08010,0x300000000e08011,0x300000000e08012,0x300000000e08013,0x300000000e08014,0x300000000e08015
sleep 1
target-window --seconds 30 >"$LOGS/target.out" 2>&1 &
sleep 1
for pass in $(cat /work/rs-passes 2>/dev/null || echo memfd dmabuf); do
    extra=""; [[ $pass == dmabuf ]] && extra="--dmabuf --mods $NV"
    echo "=== pass $pass: consumer asks 1920x1080, renegotiates to 1600x900 at t=4"
    output-probe | grep -E 'current_mode'
    fd-handoff --pipewire --linger 2 -- screencast --quiet --seconds 8 --size 1920x1080 \
        --renegotiate 4 1600x900 --save "$LOGS/rs-$pass" --save-seq 2 $extra >"$LOGS/sc-$pass.out" 2>&1 &
    sc=$!
    sleep 2.5; echo "t~2:"; output-probe | grep -E 'current_mode'
    sleep 4; echo "t~6:"; output-probe | grep -E 'current_mode|mode\['
    wait $sc
    echo "after consumer exit:"; output-probe | grep -E 'current_mode'
    echo "=== management resize to 1366x768 while no stream"
    output-probe --resize 1366 768 | grep -E 'APPLIED|FAILED|current_mode'
done
wait
