#!/usr/bin/python3 -I
"""Prototype of session-plasma:
1. push the full environment (WAYLAND_DISPLAY, DISPLAY, XAUTHORITY,
   XDG_CURRENT_DESKTOP, ...) into the bus activation environment, as
   startplasma does; the private dbus-daemon started before KWin and
   D-Bus-activated Plasma services (kactivitymanagerd!) otherwise abort;
2. own org.kde.KWinWrapper so plasma_session's legacy (non-systemd) startup
   does not start its own nested kwin_wayland_wrapper;
3. run plasma_session and exit with its status."""
import os
import subprocess
import sys

from gi.repository import Gio, GLib

bus = Gio.bus_get_sync(Gio.BusType.SESSION, None)
env = {k: v for k, v in os.environ.items() if k.isidentifier()}
bus.call_sync("org.freedesktop.DBus", "/org/freedesktop/DBus", "org.freedesktop.DBus",
              "UpdateActivationEnvironment", GLib.Variant("(a{ss})", (env,)),
              None, Gio.DBusCallFlags.NONE, -1, None)
print("UpdateActivationEnvironment: WAYLAND_DISPLAY=%s DISPLAY=%s" % (env.get("WAYLAND_DISPLAY"), env.get("DISPLAY")), flush=True)
reply = bus.call_sync("org.freedesktop.DBus", "/org/freedesktop/DBus", "org.freedesktop.DBus",
                      "RequestName", GLib.Variant("(su)", ("org.kde.KWinWrapper", 4)),
                      GLib.VariantType("(u)"), Gio.DBusCallFlags.NONE, -1, None)
print("RequestName org.kde.KWinWrapper ->", reply.unpack()[0], "(1 = primary owner)", flush=True)
sys.exit(subprocess.run(["/usr/bin/plasma_session", *sys.argv[1:]]).returncode)
