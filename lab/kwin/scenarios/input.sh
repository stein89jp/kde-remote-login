#!/usr/bin/env bash
# Q2: fake_input events delivered to a fullscreen xdg toplevel.
exec >"$LOGS/input.out" 2>&1
sleep 1
target-window --seconds 9 >"$LOGS/target.out" 2>&1 &
sleep 2
echo "=== without authenticate (expect nothing delivered)"
fake-input --no-auth
sleep 1
echo "MARK no-auth-done" >>"$LOGS/target.out"
echo "=== with authenticate, via broker-style fd handoff"
fd-handoff -- fake-input
wait
