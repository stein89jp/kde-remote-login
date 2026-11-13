#!/usr/bin/env bash
# Debug helper: does Xwayland start (DISPLAY usable) in the sandbox?
exec >"$LOGS/xwayland.out" 2>&1
sleep 4
echo "DISPLAY=$DISPLAY"; ls -la /tmp/.X11-unix /var/lib/xkb 2>&1
for c in xrandr xprop xdpyinfo; do command -v $c >/dev/null && { echo "== $c"; timeout 5 $c $([[ $c == xprop ]] && echo -root) 2>&1 | head -3; break; }; done
sleep 1
