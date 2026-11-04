#!/usr/bin/env bash
# Inside the sandbox: private runtime dir, dbus session, PipeWire, KWin.
mkdir -m700 -p /tmp/rt
rm -f "$LOGS"/*
exec dbus-run-session -- bash /src/session.sh
