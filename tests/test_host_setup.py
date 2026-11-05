"""Exercise first-start TLS provisioning without changing the host."""
import importlib.util
import configparser
from importlib.machinery import SourceFileLoader
import os
import json
from pathlib import Path
import subprocess
import shutil
import tempfile
import time
import unittest
from unittest.mock import patch


ROOT = Path(__file__).resolve().parents[1]


def load(name, filename):
    spec = importlib.util.spec_from_file_location(name, ROOT / "packaging" / filename)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


tls = load("prepare_tls", "prepare-tls.py")
setup = load("host_setup", "kde-rdp-setup.py")
portals_loader = SourceFileLoader("session_portals", str(ROOT / "broker/config/session-portals"))
portals = importlib.util.module_from_spec(importlib.util.spec_from_loader(portals_loader.name, portals_loader))
portals_loader.exec_module(portals)
autostart_loader = SourceFileLoader("session_autostart", str(ROOT / "broker/config/session-autostart"))
autostart = importlib.util.module_from_spec(importlib.util.spec_from_loader(autostart_loader.name, autostart_loader))
autostart_loader.exec_module(autostart)


class HostSetupTests(unittest.TestCase):
    def test_frontend_orders_nvidia_preparation_without_blocking_bitmap_hosts(self):
        frontend = configparser.ConfigParser(interpolation=None, strict=False)
        frontend.read(ROOT / "packaging/systemd/kde-rdp-frontend.service")
        helper = "kde-rdp-nvidia-devices.service"
        self.assertIn(helper, frontend["Unit"]["Wants"].split())
        self.assertIn(helper, frontend["Unit"]["After"].split())
        self.assertNotIn(helper, frontend["Unit"]["Requires"].split())
        self.assertEqual(frontend["Service"]["User"], "kde-rdp")
        self.assertEqual(frontend["Service"]["NoNewPrivileges"], "yes")
        self.assertEqual(frontend["Service"]["PrivateDevices"], "yes")

    def test_frontend_sandbox_stays_hardened_and_nvidia_drop_in_allows_only_gpu_groups(self):
        frontend = configparser.ConfigParser(interpolation=None, strict=False)
        frontend.read(ROOT / "packaging/systemd/kde-rdp-frontend.service")
        service = frontend["Service"]
        for key, value in {"CapabilityBoundingSet": "", "DevicePolicy": "closed",
                           "PrivateIPC": "yes", "RemoveIPC": "yes", "ProtectProc": "invisible",
                           "ProtectKernelLogs": "yes", "ProtectClock": "yes",
                           "ProtectHostname": "yes", "RestrictNamespaces": "yes",
                           "RestrictRealtime": "yes", "LockPersonality": "yes",
                           "SystemCallArchitectures": "native",
                           "SystemCallFilter": "@system-service",
                           "SystemCallErrorNumber": "EPERM"}.items():
            self.assertEqual(service.get(key), value, key)
        # The NVIDIA driver reads /proc/driver/nvidia and may map executable memory.
        self.assertNotIn("MemoryDenyWriteExecute", service)
        self.assertNotIn("ProcSubset", service)
        lines = (ROOT / "packaging/nvidia-frontend.conf").read_text().splitlines()
        self.assertEqual([line.split("=", 1)[1] for line in lines if line.startswith("DeviceAllow=")],
                         ["char-nvidia rw", "char-nvidiactl rw", "char-nvidia-modeset rw",
                          "char-nvidia-uvm rw", "char-drm rw"])
        self.assertIn("PrivateDevices=no", lines)

    def test_nvidia_preparation_requests_gpu_control_and_uvm_nodes_on_every_start(self):
        path = ROOT / "packaging/systemd/kde-rdp-nvidia-devices.service"
        helper = configparser.ConfigParser(interpolation=None, strict=False)
        helper.read(path)
        self.assertEqual(helper["Unit"]["ConditionPathExists"], "/usr/bin/nvidia-modprobe")
        self.assertEqual(helper["Unit"]["ConditionPathExistsGlob"],
                         "/proc/driver/nvidia/gpus/*/information")
        self.assertIn("kde-rdp-frontend.service", helper["Unit"]["Before"].split())
        # Remote KWin sessions render on the GPU too: devices precede the broker.
        self.assertIn("kde-rdp-broker.service", helper["Unit"]["Before"].split())
        broker = configparser.ConfigParser(interpolation=None, strict=False)
        broker.read(ROOT / "broker/config/kde-rdp-broker.service")
        self.assertIn("kde-rdp-nvidia-devices.service", broker["Unit"]["Wants"].split())
        self.assertIn("kde-rdp-nvidia-devices.service", broker["Unit"]["After"].split())
        self.assertEqual(helper["Service"].get("User", "root"), "root")
        self.assertEqual(helper["Service"]["Type"], "oneshot")
        self.assertNotEqual(helper["Service"].get("RemainAfterExit", "no"), "yes")
        self.assertEqual([line.split("=", 1)[1] for line in path.read_text().splitlines()
                          if line.startswith("ExecStart=")],
                         ["/usr/bin/nvidia-modprobe -c 0 -c 255",
                          "/usr/bin/nvidia-modprobe -u -c 0",
                          "/usr/bin/nvidia-modprobe -m"])
        self.assertNotIn("Install", helper, "frontend starts the helper without manual enablement")

    def test_input_framework_requires_explicit_configuration_and_handles_toolkit_fallbacks(self):
        helper = autostart.input_method
        self.assertIsNone(helper.configured_framework({}))
        for values, expected in (({"XMODIFIERS": "@im=fcitx"}, "fcitx"),
                                 ({"INPUT_METHOD": "ibus", "QT_IM_MODULE": "fcitx"}, "ibus"),
                                 ({"QT_IM_MODULES": "wayland;ibus"}, "ibus"),
                                 ({"GTK_IM_MODULE": "wayland"}, None)):
            self.assertEqual(helper.configured_framework(values), expected)

    def test_ibus_transport_is_private_and_exported_for_dbus_activation(self):
        helper = autostart.input_method
        environment = {"KDE_RDP_RUNTIME_DIR": "/private/remote", "QT_IM_MODULE": "ibus",
                       "IBUS_ADDRESS": "unix:path=/physical/ibus",
                       "IBUS_ADDRESS_FILE": "/physical/address"}
        actual = helper.private_settings(environment)
        self.assertEqual(actual["IBUS_ADDRESS"], "unix:path=/private/remote/ibus/bus")
        self.assertEqual(actual["IBUS_ADDRESS_FILE"], "/private/remote/ibus/address")
        with patch.object(autostart.subprocess, "run") as update:
            autostart.activation_environment(actual)
        arguments = update.call_args.args[0]
        self.assertIn("IBUS_ADDRESS=" + actual["IBUS_ADDRESS"], arguments)
        self.assertIn("IBUS_ADDRESS_FILE=" + actual["IBUS_ADDRESS_FILE"], arguments)
        self.assertNotIn("--systemd", arguments)
        other = helper.private_settings(dict(environment, QT_IM_MODULE="fcitx"))
        self.assertNotIn("IBUS_ADDRESS", other)
        self.assertEqual(environment["IBUS_ADDRESS"], "unix:path=/physical/ibus")

    def test_fcitx_profile_snapshot_prevents_remote_layout_changes_overwriting_host(self):
        helper = autostart.input_method
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            host = directory / "host/fcitx5"
            host.mkdir(parents=True)
            profile = host / "profile"
            profile.write_text("[Groups/0]\nDefault Layout=jp\nDefaultIM=mozc\n")
            # A user symlink must become a copy, not a write path to the host.
            (host / "config").symlink_to(profile)
            runtime = directory / "remote"
            runtime.mkdir()
            environment = {"HOME": str(directory), "XDG_CONFIG_HOME": str(host.parent),
                           "KDE_RDP_RUNTIME_DIR": str(runtime), "XDG_RUNTIME_DIR": str(runtime),
                           "QT_IM_MODULE": "fcitx", "WAYLAND_DISPLAY": "wayland-0",
                           "DBUS_SESSION_BUS_ADDRESS": "unix:path=/private/bus"}
            with patch.object(helper.shutil, "which", return_value="/usr/bin/fcitx5"), \
                    patch.object(helper, "owns_name", side_effect=[False, True]), \
                    patch.object(helper, "ready", return_value=True), \
                    patch.object(helper.subprocess, "Popen") as start:
                helper.start(environment)
            self.assertEqual(start.call_args.args[0], ["/usr/bin/fcitx5", "-D"])
            child = start.call_args.kwargs["env"]
            copied = Path(child["XDG_CONFIG_HOME"]) / "fcitx5"
            self.assertEqual((copied / "profile").read_text(), profile.read_text())
            self.assertFalse((copied / "config").is_symlink())
            (copied / "profile").write_text("Default Layout=us\n")
            (copied / "config").write_text("Default Layout=us\n")
            self.assertIn("Default Layout=jp", profile.read_text())
            self.assertEqual(child["DBUS_SESSION_BUS_ADDRESS"], environment["DBUS_SESSION_BUS_ADDRESS"])
            self.assertEqual(environment["XDG_CONFIG_HOME"], str(host.parent))

    def test_ibus_starts_daemon_directly_with_private_address_without_replacement(self):
        helper = autostart.input_method
        with tempfile.TemporaryDirectory() as runtime:
            environment = helper.private_settings({"KDE_RDP_RUNTIME_DIR": runtime,
                           "XDG_RUNTIME_DIR": runtime, "WAYLAND_DISPLAY": "wayland-0",
                           "DBUS_SESSION_BUS_ADDRESS": "unix:path=/private/bus", "INPUT_METHOD": "ibus"})
            with patch.object(helper.shutil, "which", return_value="/usr/bin/ibus-daemon"), \
                    patch.object(helper, "owns_name", side_effect=[False, True]), \
                    patch.object(helper.subprocess, "Popen") as start:
                helper.start(environment)
            self.assertEqual(start.call_args.args[0], [helper.sys.executable,
                             str(ROOT / "broker/config/session-input-method"), "--supervise-ibus"])
            with patch.object(helper.shutil, "which", side_effect=["/usr/bin/ibus-daemon", None]), \
                    patch.object(helper.os, "access", return_value=False):
                self.assertEqual(helper.ibus_arguments(environment), ["/usr/bin/ibus-daemon", "--xim",
                                 "--desktop=plasma", "--address=" + environment["IBUS_ADDRESS"]])
            self.assertEqual(start.call_args.kwargs["env"], environment)
            self.assertTrue((Path(runtime) / "ibus").is_dir())

    def test_ibus_native_wayland_panel_is_requested_when_available(self):
        helper = autostart.input_method
        with patch.object(helper.shutil, "which", side_effect=["/usr/bin/ibus-daemon", None]), \
                patch.object(helper.os, "access", side_effect=lambda path, mode: path == "/usr/lib/ibus/ibus-ui-gtk3"):
            arguments = helper.ibus_arguments({"IBUS_ADDRESS": "unix:path=/private/ibus/bus"})
        self.assertIn("--panel=/usr/lib/ibus/ibus-ui-gtk3 --enable-wayland-im", arguments)

    def test_ibus_supervisor_stays_alive_until_foreground_daemon_exits(self):
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            executable = directory / "ibus-daemon"
            executable.write_text("#!/usr/bin/python3\nimport os, time\n"
                                  "print(os.getppid(), flush=True)\n"
                                  "time.sleep(0.15)\nraise SystemExit(7)\n")
            executable.chmod(0o755)
            environment = dict(os.environ, PATH=temporary, IBUS_ADDRESS="unix:path=/private/ibus/bus")
            with subprocess.Popen(["/usr/bin/python3", str(ROOT / "broker/config/session-input-method"),
                                   "--supervise-ibus"], env=environment, stdout=subprocess.PIPE,
                                  stderr=subprocess.PIPE, text=True) as supervisor:
                self.assertEqual(int(supervisor.stdout.readline()), supervisor.pid)
                self.assertIsNone(supervisor.poll())
                _, errors = supervisor.communicate(timeout=3)
                self.assertEqual(supervisor.returncode, 7, errors)

    def test_eligible_fcitx_autostart_without_toolkit_settings_uses_private_profile_startup(self):
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary) / "autostart"
            directory.mkdir()
            # GIO rejects entries whose Exec program is missing, so provide a
            # stand-in fcitx5 instead of depending on a host installation.
            fcitx = Path(temporary) / "bin" / "fcitx5"
            fcitx.parent.mkdir()
            fcitx.write_text("#!/bin/sh\nexit 0\n")
            fcitx.chmod(0o755)
            (directory / "org.fcitx.Fcitx5.desktop").write_text(
                f"[Desktop Entry]\nType=Application\nName=Fcitx\nExec={fcitx}\nOnlyShowIn=KDE;\n")
            environment = {"HOME": temporary, "XDG_CONFIG_HOME": temporary,
                           "XDG_CONFIG_DIRS": "/missing", "XDG_CURRENT_DESKTOP": "KDE"}
            with patch.dict(os.environ, environment, clear=True), \
                    patch.object(autostart, "start_input_method") as start:
                autostart.launch_entries(environment)
            start.assert_called_once_with(dict(environment, INPUT_METHOD="fcitx"))

    def test_input_startup_respects_user_disabled_entry_and_suppresses_shared_service_launcher(self):
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary) / "autostart"
            directory.mkdir()
            entry = directory / "org.freedesktop.IBus.desktop"
            entry.write_text("[Desktop Entry]\nType=Application\nName=IBus\nExec=ibus start\n"
                             "OnlyShowIn=GNOME;\nHidden=true\n")
            environment = {"HOME": temporary, "XDG_CONFIG_HOME": temporary,
                           "XDG_CONFIG_DIRS": "/missing", "INPUT_METHOD": "ibus"}
            with patch.object(autostart.input_method, "start") as start:
                autostart.start_input_method(environment)
            start.assert_called_once_with(environment, disabled=True)
            entry.write_text(entry.read_text().replace("Hidden=true", "Hidden=false"))
            self.assertFalse(autostart.input_method_disabled(environment))
            import gi
            gi.require_version("GioUnix", "2.0")
            from gi.repository import GioUnix
            entry.write_text(entry.read_text().replace("OnlyShowIn=GNOME;", "OnlyShowIn=KDE;"))
            with patch.dict(os.environ, dict(environment, XDG_CURRENT_DESKTOP="KDE"), clear=True), \
                    patch.object(GioUnix.DesktopAppInfo, "launch") as launch:
                autostart.launch_entries(environment)
            launch.assert_not_called()

    def test_input_startup_never_replaces_existing_owner_and_reports_failure_without_breaking_desktop(self):
        helper = autostart.input_method
        environment = {"KDE_RDP_RUNTIME_DIR": "/private/remote", "XDG_RUNTIME_DIR": "/private/remote",
                       "WAYLAND_DISPLAY": "wayland-0", "DBUS_SESSION_BUS_ADDRESS": "unix:path=/private/bus",
                       "QT_IM_MODULE": "fcitx"}
        with patch.object(helper.shutil, "which", return_value="/usr/bin/fcitx5"), \
                patch.object(helper, "owns_name", return_value=True), \
                patch.object(helper.subprocess, "Popen") as start:
            helper.start(environment)
        start.assert_not_called()
        with patch.object(autostart, "input_method_disabled", return_value=False), \
                patch.object(helper, "start", side_effect=RuntimeError("startup timed out")), \
                patch("builtins.print") as report:
            autostart.start_input_method(environment)
        self.assertIn("startup timed out", report.call_args.args[0])

    def test_generic_activation_preserves_configured_input_method_and_private_transport(self):
        for module in ("fcitx", "ibus"):
            environment = {"XDG_RUNTIME_DIR": "/private/remote", "WAYLAND_DISPLAY": "wayland-1",
                           "DBUS_SESSION_BUS_ADDRESS": "unix:path=/private/bus", "QT_IM_MODULE": module,
                           "GTK_IM_MODULE": module, "XMODIFIERS": "@im=" + module}
            with patch.object(autostart.subprocess, "run") as update:
                autostart.activation_environment(environment)
                arguments = update.call_args.args[0]
                self.assertNotIn("--systemd", arguments)
                self.assertIn("QT_IM_MODULE=" + module, arguments)
                self.assertIn("GTK_IM_MODULE=" + module, arguments)
                self.assertIn("WAYLAND_DISPLAY=wayland-1", arguments)
                self.assertEqual(update.call_args.kwargs["env"], environment)

    def test_generic_autostart_honors_overrides_and_launches_on_private_environment(self):
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            user = directory / "user/autostart"
            system = directory / "system/autostart"
            user.mkdir(parents=True)
            system.mkdir(parents=True)
            probe = directory / "probe with spaces.py"
            probe.write_text("import json, os, sys\nfrom pathlib import Path\n"
                             "Path(sys.argv[1]).write_text(json.dumps({key: os.environ.get(key) for key in "
                             "('QT_IM_MODULE', 'XDG_RUNTIME_DIR', 'WAYLAND_DISPLAY', 'DBUS_SESSION_BUS_ADDRESS')}))\n")
            def entry(name, extra="", location=system):
                output = directory / (name + ".json")
                (location / (name + ".desktop")).write_text(
                    '[Desktop Entry]\nType=Application\nName=Probe\nExec=/usr/bin/python3 "' +
                    str(probe) + '" "' + str(output) + '"\n' + extra)
                return output
            allowed = entry("configured-framework", "NoDisplay=true\nOnlyShowIn=KDE;\n")
            suppressed = [entry("overridden"), entry("other-desktop", "OnlyShowIn=GNOME;\n"),
                          entry("disabled", "X-GNOME-Autostart-enabled=false\n"),
                          entry("systemd-skip", "X-systemd-skip=true\n"),
                          entry("missing-program", "TryExec=/missing/input-method\n")]
            entry("overridden", "Hidden=true\n", location=user)
            environment = dict(os.environ, HOME=str(directory), XDG_CONFIG_HOME=str(user.parent),
                               XDG_CONFIG_DIRS=str(system.parent), XDG_CURRENT_DESKTOP="KDE",
                               QT_IM_MODULE="ibus", XDG_RUNTIME_DIR="/private/remote",
                               WAYLAND_DISPLAY="wayland-1", DBUS_SESSION_BUS_ADDRESS="unix:path=/private/bus")
            with patch.dict(os.environ, environment, clear=True):
                autostart.launch_entries(environment)
            deadline = time.monotonic() + 3
            while not allowed.exists() and time.monotonic() < deadline:
                time.sleep(0.02)
            self.assertTrue(allowed.exists())
            self.assertEqual(json.loads(allowed.read_text()), {key: environment[key] for key in
                             ("QT_IM_MODULE", "XDG_RUNTIME_DIR", "WAYLAND_DISPLAY", "DBUS_SESSION_BUS_ADDRESS")})
            self.assertFalse(any(output.exists() for output in suppressed))

    def test_generic_autostart_preparation_and_single_launch_require_private_session(self):
        with tempfile.TemporaryDirectory() as temporary:
            environment = {"KDE_RDP_RUNTIME_DIR": temporary, "XDG_RUNTIME_DIR": temporary,
                           "WAYLAND_DISPLAY": "wayland-1", "DBUS_SESSION_BUS_ADDRESS": "unix:path=/private/bus"}
            with patch.dict(os.environ, environment, clear=True), \
                    patch.object(autostart, "activation_environment") as update, \
                    patch.object(autostart, "launch_entries") as launch, \
                    patch.object(autostart, "start_input_method") as start_input, \
                    patch.object(autostart.sys, "argv", ["session-autostart", "--prepare"]):
                autostart.main()
                update.assert_called_once_with(environment)
                launch.assert_not_called()
                start_input.assert_called_once_with(environment)
            with patch.dict(os.environ, environment, clear=True), \
                    patch.object(autostart, "activation_environment"), \
                    patch.object(autostart, "launch_entries") as launch, \
                    patch.object(autostart, "start_input_method"), \
                    patch.object(autostart.sys, "argv", ["session-autostart"]):
                autostart.main()
                autostart.main()
                launch.assert_called_once_with(environment)
            for changes in ({"XDG_RUNTIME_DIR": "/run/user/1000"}, {"WAYLAND_DISPLAY": ""},
                            {"DBUS_SESSION_BUS_ADDRESS": ""}):
                with patch.dict(os.environ, dict(environment, **changes), clear=True), \
                        patch.object(autostart, "activation_environment") as update:
                    with self.assertRaises(RuntimeError):
                        autostart.main()
                    update.assert_not_called()

    def test_autostart_empty_or_relative_config_home_uses_account_default(self):
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary) / ".config/autostart"
            directory.mkdir(parents=True)
            entry = directory / "configured.desktop"
            entry.touch()
            for value in ("", "relative/path"):
                entries = list(autostart.autostart_entries({"HOME": temporary, "XDG_CONFIG_HOME": value,
                                                          "XDG_CONFIG_DIRS": "relative/system"}))
                self.assertEqual(entries, [entry])

    def test_portal_startup_exports_only_private_activation_environment(self):
        environment = {"KDE_RDP_RUNTIME_DIR": "/private/remote",
                       "XDG_RUNTIME_DIR": "/private/remote", "WAYLAND_DISPLAY": "wayland-1",
                       "DBUS_SESSION_BUS_ADDRESS": "unix:path=/private/bus",
                       "XDG_CURRENT_DESKTOP": "KDE", "XDG_CONFIG_HOME": "/user/settings",
                       "PULSE_SERVER": "unix:/private/remote/pulse/native"}
        with patch.dict(os.environ, environment, clear=True), \
                patch.object(portals.subprocess, "run") as update, \
                patch.object(portals, "replace_stale_backends") as replace, \
                patch.object(portals.os, "execve") as execute:
            portals.main()
            update.assert_called_once()
            arguments = update.call_args.args[0]
            self.assertNotIn("--systemd", arguments)
            self.assertIn("XDG_RUNTIME_DIR=/private/remote", arguments)
            self.assertIn("WAYLAND_DISPLAY=wayland-1", arguments)
            self.assertIn("DISPLAY=", arguments)
            # Activated services see plain KDE; only the portal frontend also
            # lists kde-rdp, so kde-portals.conf wins and ours is the fallback.
            self.assertIn("XDG_CURRENT_DESKTOP=KDE", arguments)
            self.assertNotIn("XDG_CURRENT_DESKTOP=KDE:kde-rdp", arguments)
            self.assertEqual(update.call_args.kwargs["env"]["DBUS_SESSION_BUS_ADDRESS"],
                             environment["DBUS_SESSION_BUS_ADDRESS"])
            execute.assert_called_once()
            replace.assert_called_once_with(update.call_args.kwargs["env"])
            self.assertEqual(execute.call_args.args[2]["XDG_CONFIG_HOME"], "/user/settings")
            self.assertEqual(execute.call_args.args[2]["XDG_CURRENT_DESKTOP"], "KDE:kde-rdp")
            self.assertEqual(os.environ["XDG_CURRENT_DESKTOP"], "KDE")

    def test_portal_backend_owner_query_addresses_only_private_bus(self):
        environment = {"DBUS_SESSION_BUS_ADDRESS": "unix:path=/private/bus"}
        with patch.object(portals.subprocess, "run",
                          return_value=subprocess.CompletedProcess([], 0, 's ":1.7"\n')) as query:
            self.assertEqual(portals.bus_owner("org.example.Backend", environment), 's ":1.7"')
            self.assertIn("--address=unix:path=/private/bus", query.call_args.args[0])
            self.assertIn("GetNameOwner", query.call_args.args[0])
            self.assertNotIn("StartServiceByName", query.call_args.args[0])

    def test_portal_backend_replacement_only_restarts_existing_owners(self):
        environment = {"DBUS_SESSION_BUS_ADDRESS": "unix:path=/private/bus"}
        with patch.object(portals, "bus_owner", side_effect=['s ":1.1"', 's ":1.1"', 's ":1.2"', None]), \
                patch.object(portals.Path, "is_file", return_value=True), \
                patch.object(portals.subprocess, "Popen") as start, \
                patch.object(portals.time, "sleep"):
            start.return_value.poll.return_value = None
            portals.replace_stale_backends(environment)
            start.assert_called_once_with(["/usr/lib/xdg-desktop-portal-gnome", "--replace"],
                                          env=environment)
        with patch.object(portals, "bus_owner", return_value=None), \
                patch.object(portals.subprocess, "Popen") as start:
            portals.replace_stale_backends(environment)
            start.assert_not_called()

    def test_portal_backend_replacement_is_bounded(self):
        with patch.object(portals, "bus_owner", return_value='s ":1.1"'), \
                patch.object(portals.Path, "is_file", return_value=True), \
                patch.object(portals.subprocess, "Popen") as start, \
                patch.object(portals.time, "monotonic", side_effect=[0, 0, 6]), \
                patch.object(portals.time, "sleep"):
            start.return_value.poll.return_value = None
            with self.assertRaisesRegex(RuntimeError, "timed out"):
                portals.replace_stale_backends({"DBUS_SESSION_BUS_ADDRESS": "unix:path=/private/bus"})
            start.return_value.terminate.assert_not_called()

    def test_portal_startup_rejects_non_remote_context_and_failed_export(self):
        contexts = [{}, {"KDE_RDP_RUNTIME_DIR": "/private/remote",
                         "XDG_RUNTIME_DIR": "/run/user/1000", "WAYLAND_DISPLAY": "wayland-0"},
                    {"KDE_RDP_RUNTIME_DIR": "/private/remote", "XDG_RUNTIME_DIR": "/private/remote",
                     "DBUS_SESSION_BUS_ADDRESS": "unix:path=/private/bus"}]
        for environment in contexts:
            with self.subTest(environment=environment), patch.dict(os.environ, environment, clear=True), \
                    patch.object(portals.subprocess, "run") as update, \
                    patch.object(portals.os, "execve") as execute:
                with self.assertRaises((KeyError, RuntimeError)):
                    portals.main()
                update.assert_not_called()
                execute.assert_not_called()
        with patch.dict(os.environ, {"KDE_RDP_RUNTIME_DIR": "/private/remote",
                                    "XDG_RUNTIME_DIR": "/private/remote", "WAYLAND_DISPLAY": "wayland-1",
                                    "DBUS_SESSION_BUS_ADDRESS": "unix:path=/private/bus"}, clear=True), \
                patch.object(portals.subprocess, "run",
                             side_effect=subprocess.CalledProcessError(1, "export")), \
                patch.object(portals.os, "execve") as execute:
            with self.assertRaises(subprocess.CalledProcessError):
                portals.main()
            execute.assert_not_called()

    @unittest.skipUnless(all(shutil.which(command) for command in
                         ("dbus-run-session", "dbus-update-activation-environment", "busctl")),
                         "requires D-Bus command-line tools")
    def test_private_bus_activation_receives_remote_runtime_and_display(self):
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            for runtime in (directory / "physical", directory / "private"):
                runtime.mkdir(mode=0o700)
            services = directory / "data/dbus-1/services"
            services.mkdir(parents=True)
            output = directory / "activated.json"
            probe = directory / "probe.py"
            probe.write_text("import json, os\nfrom pathlib import Path\n" +
                             "Path(" + repr(str(output)) + ").write_text(json.dumps(dict(os.environ)))\n")
            (services / "org.example.KdeRdpActivationProbe.service").write_text(
                "[D-BUS Service]\nName=org.example.KdeRdpActivationProbe\nExec=/usr/bin/python3 " +
                str(probe) + "\n")
            runner = directory / "runner.py"
            runner.write_text(
                "import os, runpy, subprocess\nfrom unittest.mock import patch\n" +
                "os.environ.update(XDG_RUNTIME_DIR=" + repr(str(directory / "private")) +
                ", KDE_RDP_RUNTIME_DIR=" + repr(str(directory / "private")) +
                ", WAYLAND_DISPLAY='wayland-remote', PULSE_SERVER='unix:/private/pulse/native')\n" +
                "with patch('os.execve'):\n    runpy.run_path(" +
                repr(str(ROOT / "broker/config/session-portals")) + ", run_name='__main__')\n" +
                # The probe records its activation environment and exits without owning the name.
                "subprocess.run(['busctl', '--address=' + os.environ['DBUS_SESSION_BUS_ADDRESS'], " +
                "'--timeout=2', 'call', 'org.freedesktop.DBus', '/org/freedesktop/DBus', " +
                "'org.freedesktop.DBus', 'StartServiceByName', 'su', " +
                "'org.example.KdeRdpActivationProbe', '0'], capture_output=True)\n")
            environment = dict(os.environ, XDG_RUNTIME_DIR=str(directory / "physical"),
                               XDG_DATA_HOME=str(directory / "data"))
            for key in ("DBUS_SESSION_BUS_ADDRESS", "WAYLAND_DISPLAY", "DISPLAY"):
                environment.pop(key, None)
            result = subprocess.run(["dbus-run-session", "--", "/usr/bin/python3", str(runner)],
                                    env=environment, capture_output=True, text=True, timeout=10)
            self.assertEqual(result.returncode, 0, result.stderr)
            activated = json.loads(output.read_text())
            self.assertEqual(activated["XDG_RUNTIME_DIR"], str(directory / "private"))
            self.assertEqual(activated["WAYLAND_DISPLAY"], "wayland-remote")
            self.assertEqual(activated["XDG_CURRENT_DESKTOP"], "KDE")
            self.assertEqual(activated["PULSE_SERVER"], "unix:/private/pulse/native")
            self.assertEqual(activated["DISPLAY"], "")

    def run_worker(self, policy, input_environment=None, size=None, render_nodes="host"):
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            config = directory / "renderer.conf"
            if policy is not None:
                config.write_text(policy + "\n")
            # Stand-in KWin: report its arguments, then its environment.
            executable = directory / "kwin_wayland"
            executable.write_text('#!/bin/sh\nprintf "KWIN_ARGS=%s\\n" "$*"\n'
                                  '[ -e "$HOME/.config/kwinrc" ] && echo HOME_CONFIG=present || echo HOME_CONFIG=absent\n'
                                  'printf "KWINRC=%s\\n" "$(tr \'\\n\' \'|\' < "${XDG_CONFIG_DIRS%%:*}/kwinrc")"\n'
                                  'exec /usr/bin/env\n')
            executable.chmod(0o755)
            worker = (ROOT / "broker/config/session-worker").read_text()
            worker = "set -- --environment-ready\n" + worker
            worker = worker.replace("/etc/kde-rdp/renderer.conf", str(config))
            worker = worker.replace("/usr/lib/kde-rdp/session-audio &", "true &")
            worker = worker.replace('while [ ! -S "$KDE_RDP_RUNTIME_DIR/audio-0" ]; do', 'while false; do')
            self.assertIn("/usr/bin/kwin_wayland ", worker)
            # Device checks use stand-in nodes so the suite also runs without a GPU.
            self.assertIn("for node in /dev/dri/renderD*; do", worker)
            if render_nodes != "host":
                nodes = directory / "dri"
                nodes.mkdir()
                if render_nodes == "unreadable":
                    (nodes / "renderD128").touch(mode=0o000)
                worker = worker.replace("/dev/dri/renderD*", str(nodes / "renderD*"))
            else:
                worker = worker.replace("[ -c \"$node\" ] && ", "")
                worker = worker.replace("/dev/dri/renderD*", str(directory / "renderD128"))
                (directory / "renderD128").touch()
            worker = worker.replace("/usr/bin/kwin_wayland ", str(executable) + " ")
            environment = dict(os.environ, KDE_RDP_RUNTIME_DIR=str(directory),
                               HOME=str(directory),
                               DISPLAY=":0", WAYLAND_DISPLAY="local-wayland",
                               LIBGL_ALWAYS_SOFTWARE="1", GALLIUM_DRIVER="llvmpipe",
                               __EGL_VENDOR_LIBRARY_FILENAMES="/usr/share/glvnd/egl_vendor.d/10_nvidia.json")
            for key in ("XDG_CONFIG_HOME", "XDG_CONFIG_DIRS", "XMODIFIERS", "QT_IM_MODULE",
                        "GTK_IM_MODULE", "KDE_RDP_WIDTH", "KDE_RDP_HEIGHT"):
                environment.pop(key, None)
            environment.update(input_environment or {})
            if size is not None:
                environment.update(KDE_RDP_WIDTH=size[0], KDE_RDP_HEIGHT=size[1])
            result = subprocess.run(["/bin/sh", "-c", worker], env=environment,
                                    text=True, capture_output=True)
            # No compositor configuration is generated any more.
            self.assertFalse((directory / "config.kdl").exists())
            return result

    @staticmethod
    def worker_environment(result):
        return dict(line.split("=", 1) for line in result.stdout.splitlines() if "=" in line)

    def test_worker_starts_kwin_virtual_backend_with_plasma_session(self):
        result = self.run_worker("auto")
        self.assertEqual(result.returncode, 0, result.stderr)
        environment = self.worker_environment(result)
        self.assertEqual(environment["KWIN_ARGS"],
                         "--virtual --xwayland --no-lockscreen --socket wayland-0 "
                         "--width 1920 --height 1080 "
                         "--exit-with-session=/usr/lib/kde-rdp/session-plasma")
        for key in ("NIRI_BACKEND", "NIRI_SOCKET", "KDE_RDP_WIDTH", "KDE_RDP_HEIGHT"):
            self.assertNotIn(key, environment)
        self.assertEqual(environment["PIPEWIRE_REMOTE"], "pipewire-0")
        self.assertEqual(environment["PIPEWIRE_RUNTIME_DIR"], environment["KDE_RDP_RUNTIME_DIR"])
        # Container tools resolve through the session-containers links first.
        self.assertTrue(environment["PATH"].startswith("/usr/lib/kde-rdp/containers:"))

    def test_worker_uses_client_size_and_rejects_invalid_sizes(self):
        result = self.run_worker("auto", size=("1280", "720"))
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("--width 1280 --height 720 ", self.worker_environment(result)["KWIN_ARGS"])
        for width, height in (("1281", "720"), ("100", "720"), ("1280", "4097"), ("abc", "720"),
                              ("0800", "600"), ("", "720"), ("1280", "7200; echo unsafe")):
            with self.subTest(width=width, height=height):
                result = self.run_worker("auto", size=(width, height))
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertIn("--width 1920 --height 1080 ", self.worker_environment(result)["KWIN_ARGS"])
                self.assertIn("using 1920x1080", result.stderr)
                self.assertNotIn("unsafe\n", result.stdout)

    def test_worker_passes_configured_input_framework_without_defaults(self):
        result = self.run_worker("auto")
        self.assertEqual(result.returncode, 0, result.stderr)
        environment = self.worker_environment(result)
        self.assertNotIn("QT_IM_MODULE", environment)
        self.assertNotIn("XMODIFIERS", environment)
        for module in ("fcitx", "ibus"):
            settings = {"QT_IM_MODULE": module, "XMODIFIERS": "@im=" + module}
            environment = self.worker_environment(self.run_worker("auto", input_environment=settings))
            for key, value in settings.items():
                self.assertEqual(environment[key], value)

    @staticmethod
    def load_session_plasma():
        loader = SourceFileLoader("session_plasma", str(ROOT / "broker/config/session-plasma"))
        module = importlib.util.module_from_spec(importlib.util.spec_from_loader(loader.name, loader))
        loader.exec_module(module)
        return module

    @unittest.skipUnless(all(shutil.which(command) for command in ("dbus-run-session", "busctl")),
                         "requires D-Bus command-line tools")
    def test_session_plasma_exports_full_environment_and_holds_kwin_wrapper_name(self):
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            runtime = directory / "private"
            runtime.mkdir(mode=0o700)
            libexec = directory / "libexec"
            libexec.mkdir()
            log = directory / "log"
            for name in ("session-autostart", "session-portals"):
                helper = libexec / name
                helper.write_text('#!/bin/sh\necho "${0##*/} $*" >> ' + str(log) + '\n')
                helper.chmod(0o755)
            services = directory / "data/dbus-1/services"
            services.mkdir(parents=True)
            activated = directory / "activated.json"
            probe = directory / "probe.py"
            probe.write_text("import json, os\nfrom pathlib import Path\n"
                             "Path(" + repr(str(activated)) + ").write_text(json.dumps(dict(os.environ)))\n")
            (services / "org.example.KdeRdpPlasmaProbe.service").write_text(
                "[D-BUS Service]\nName=org.example.KdeRdpPlasmaProbe\nExec=/usr/bin/python3 " +
                str(probe) + "\n")
            # Stand-in plasma_session: record whether the wrapper name is owned
            # (else the real one would start a nested KWin), activate a probe
            # service, and exit with a distinctive status.
            plasma = directory / "plasma_session"
            plasma.write_text(
                "#!/bin/sh\n"
                "busctl --address=\"$DBUS_SESSION_BUS_ADDRESS\" call org.freedesktop.DBus "
                "/org/freedesktop/DBus org.freedesktop.DBus NameHasOwner s org.kde.KWinWrapper "
                "> " + str(directory / "owner") + "\n"
                "busctl --address=\"$DBUS_SESSION_BUS_ADDRESS\" --timeout=3 call org.freedesktop.DBus "
                "/org/freedesktop/DBus org.freedesktop.DBus StartServiceByName su "
                "org.example.KdeRdpPlasmaProbe 0 > /dev/null 2>&1\n"
                "echo \"plasma_session $XDG_CURRENT_DESKTOP $KDE_FULL_SESSION $KDE_SESSION_VERSION\" >> "
                + str(log) + "\nexit 3\n")
            plasma.chmod(0o755)
            runner = directory / "runner.py"
            runner.write_text(
                "import runpy, sys\nfrom pathlib import Path\n"
                "module = runpy.run_path(" + repr(str(ROOT / "broker/config/session-plasma")) + ")\n"
                "main = module['main']\n"
                "main.__globals__.update(LIBEXEC=Path(" + repr(str(libexec)) + "), "
                "PLASMA_SESSION=" + repr(str(plasma)) + ")\n"
                "sys.exit(main())\n")
            environment = {"PATH": "/usr/bin:/bin", "HOME": str(directory),
                           "XDG_DATA_HOME": str(directory / "data"),
                           "KDE_RDP_RUNTIME_DIR": str(runtime), "XDG_RUNTIME_DIR": str(runtime),
                           "WAYLAND_DISPLAY": "wayland-0", "DISPLAY": ":1",
                           "XAUTHORITY": str(runtime / "xauth_kwin"),
                           "XDG_CURRENT_DESKTOP": "niri", "QT_QPA_PLATFORM": "wayland",
                           "PULSE_SERVER": "unix:" + str(runtime / "pulse/native")}
            result = subprocess.run(["dbus-run-session", "--", "/usr/bin/python3", str(runner)],
                                    env=environment, capture_output=True, text=True, timeout=20)
            self.assertEqual(result.returncode, 3, result.stderr)
            self.assertEqual((directory / "owner").read_text().strip(), "b true")
            exported = json.loads(activated.read_text())
            for key, value in (("WAYLAND_DISPLAY", "wayland-0"), ("DISPLAY", ":1"),
                               ("XAUTHORITY", str(runtime / "xauth_kwin")),
                               ("XDG_RUNTIME_DIR", str(runtime)), ("XDG_CURRENT_DESKTOP", "KDE"),
                               ("XDG_SESSION_DESKTOP", "KDE"), ("KDE_FULL_SESSION", "true"),
                               ("KDE_SESSION_VERSION", "6"), ("QT_QPA_PLATFORM", "wayland")):
                self.assertEqual(exported.get(key), value, key)
            time.sleep(0.2)
            calls = log.read_text().splitlines()
            self.assertIn("session-autostart --prepare", calls)
            self.assertIn("session-portals ", calls)
            self.assertIn("plasma_session KDE true 6", calls)
            # plasma_session runs XDG autostart itself: no second launch.
            self.assertNotIn("session-autostart ", calls)
            self.assertLess(calls.index("session-autostart --prepare"),
                            calls.index("plasma_session KDE true 6"))

    def test_session_plasma_falls_back_to_plasmashell_with_generic_autostart(self):
        module = self.load_session_plasma()
        with patch.object(module.os, "access", return_value=False), \
                patch.object(module.subprocess, "run",
                             return_value=subprocess.CompletedProcess([], 0)) as run, \
                patch.object(module.subprocess, "Popen") as start:
            self.assertEqual(module.start_plasma({"A": "1"}), 0)
        self.assertEqual([call.args[0] for call in run.call_args_list],
                         [["/usr/lib/kde-rdp/session-autostart", "--prepare"],
                          ["/usr/bin/plasmashell", "--no-respawn"]])
        self.assertEqual([call.args[0] for call in start.call_args_list],
                         [["/usr/bin/kded6"], ["/usr/lib/kde-rdp/session-autostart"]])

    def test_session_plasma_requires_private_runtime_display_and_free_wrapper_name(self):
        module = self.load_session_plasma()
        base = {"KDE_RDP_RUNTIME_DIR": "/private", "XDG_RUNTIME_DIR": "/private",
                "WAYLAND_DISPLAY": "wayland-0", "DBUS_SESSION_BUS_ADDRESS": "unix:path=/private/bus"}
        self.assertEqual(module.session_environment(base)["XDG_CURRENT_DESKTOP"], "KDE")
        # Like startplasma: Plasma's menu is plasma-applications.menu.
        self.assertEqual(module.session_environment(base)["XDG_MENU_PREFIX"], "plasma-")
        self.assertEqual(module.session_environment(dict(base, XDG_MENU_PREFIX="custom-"))["XDG_MENU_PREFIX"],
                         "custom-")
        for change in ({"XDG_RUNTIME_DIR": "/run/user/1000"}, {"WAYLAND_DISPLAY": ""},
                       {"DBUS_SESSION_BUS_ADDRESS": ""}):
            with self.subTest(change=change), self.assertRaises(RuntimeError):
                module.session_environment(dict(base, **change))
        from gi.repository import GLib
        bus = unittest.mock.Mock()
        bus.call_sync.side_effect = [None, GLib.Variant("(u)", (3,))]
        with self.assertRaisesRegex(RuntimeError, "KWinWrapper"):
            module.prepare_bus(bus, module.session_environment(base))
        self.assertNotIn("BAD\0", module.activation_environment({"OK": "1", "BAD": "a\0b", "1x": "y"}))
        self.assertEqual(module.activation_environment({"OK": "1", "BAD": "a\0b", "1x": "y"}), {"OK": "1"})

    def test_packaging_targets_plasma_without_compositor_or_desktop_grant(self):
        makefile = (ROOT / "broker/Makefile").read_text()
        self.assertIn("/usr/lib/kde-rdp/session-plasma", makefile)
        # KWin 6.8.2 has no .desktop interface grant (docs/kde-findings.md).
        self.assertNotIn("/usr/share/applications", makefile)
        for removed in ("session-desktop", "headless.kdl", "waybar.json",
                        "org.kde.kde-rdp-broker.desktop"):
            self.assertNotIn(removed, makefile)
            self.assertFalse((ROOT / "broker/config" / removed).exists())
        portals_conf = configparser.ConfigParser(interpolation=None)
        portals_conf.read(ROOT / "broker/config/kde-rdp-portals.conf")
        self.assertEqual(portals_conf["preferred"]["default"], "kde;")
        frontend = configparser.ConfigParser(interpolation=None, strict=False)
        frontend.read(ROOT / "packaging/systemd/kde-rdp-frontend.service")
        self.assertIn("niri-rdp-frontend.service", frontend["Unit"]["Conflicts"].split())
        self.assertIn("33991", (ROOT / "packaging/frontend.conf").read_text())
        frontend_conf = (ROOT / "packaging/frontend.conf").read_text()
        self.assertNotRegex(frontend_conf, r"(?m)^KDE_RDP_OUTPUT=", "KWin output must default to the first output")
        self.assertNotIn("HEADLESS-1", frontend_conf)
        for package in ("kde-rdp", "kde-remote-login"):
            with self.subTest(package=package):
                pkgbuild = ROOT / "packaging/arch" / package / "PKGBUILD"
                if not pkgbuild.exists():
                    self.skipTest("PKGBUILDs are not part of the package source tree")
                text = pkgbuild.read_text()
                for dependency in ("'kwin'", "'plasma-workspace'", "'plasma-desktop'",
                                   "'xdg-desktop-portal-kde'", "'xorg-xwayland'", "'pipewire'"):
                    self.assertIn(dependency, text)
                for dropped in ("'foot'", "'fuzzel'", "'waybar'", "'seatd'",
                                "'xdg-desktop-portal-gtk'", "niri-headless", "headless.kdl",
                                "kde-rdp-headless", "compositor/"):
                    self.assertNotIn(dropped, text)

    def test_worker_auto_and_hardware_allow_gpu_without_local_desktop(self):
        for policy in (None, "auto", "hardware"):
            with self.subTest(policy=policy):
                result = self.run_worker(policy)
                self.assertEqual(result.returncode, 0, result.stderr)
                environment = dict(line.split("=", 1) for line in result.stdout.splitlines() if "=" in line)
                self.assertEqual(environment["KDE_RDP_RENDERER"], policy or "auto")
                for key in ("LIBGL_ALWAYS_SOFTWARE", "GALLIUM_DRIVER", "__EGL_VENDOR_LIBRARY_FILENAMES",
                            "DISPLAY", "WAYLAND_DISPLAY"):
                    self.assertNotIn(key, environment)
                self.assertEqual(environment["XDG_RUNTIME_DIR"], environment["KDE_RDP_RUNTIME_DIR"])

    def test_worker_refuses_software_rendering_with_explanation(self):
        result = self.run_worker("software")
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(result.stdout, "")
        self.assertIn("software rendering is not supported", result.stderr)
        self.assertIn("GPU render node", result.stderr)

    def test_worker_requires_accessible_render_node(self):
        for nodes in ("missing", "unreadable"):
            with self.subTest(nodes=nodes):
                result = self.run_worker("auto", render_nodes=nodes)
                self.assertNotEqual(result.returncode, 0)
                self.assertEqual(result.stdout, "")
                self.assertIn("no DRM render node", result.stderr)

    def test_worker_disables_shake_cursor_in_session_overlay_only(self):
        result = self.run_worker("auto", input_environment={"XDG_CONFIG_DIRS": "/etc/custom:/etc/xdg"})
        self.assertEqual(result.returncode, 0, result.stderr)
        environment = self.worker_environment(result)
        overlay = environment["KDE_RDP_RUNTIME_DIR"] + "/xdg-config"
        self.assertEqual(environment["XDG_CONFIG_DIRS"], overlay + ":/etc/custom:/etc/xdg")
        self.assertEqual(environment["KWINRC"].rstrip("|").split("|")[1:],
                         ["[Plugins]", "shakecursorEnabled[$i]=false"])
        # The account's own configuration is never written.
        self.assertEqual(environment["HOME_CONFIG"], "absent")
        result = self.run_worker("auto")
        self.assertTrue(self.worker_environment(result)["XDG_CONFIG_DIRS"].endswith("/xdg-config:/etc/xdg"))

    def test_worker_rejects_invalid_policy_without_executing_shell_text(self):
        result = self.run_worker("auto; echo unsafe")
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(result.stdout, "")
        self.assertIn("must contain auto or hardware", result.stderr)

    def test_certificate_identity_rejects_openssl_syntax_injection(self):
        for value in ("host,IP:1.2.3.4", "host/CN=other", "-host", "", "a..b"):
            with self.subTest(value=value), self.assertRaises(ValueError):
                tls.identity_names(value)
        self.assertEqual(tls.identity_names("192.168.1.20")[1],
                         "IP:192.168.1.20,DNS:localhost,IP:127.0.0.1")
        self.assertIn("DNS:arch.local,DNS:arch", tls.identity_names("arch.local")[1])

    def test_partial_identity_is_preserved_and_rejected(self):
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            cert = directory / "cert.pem"
            cert.write_text("existing certificate")
            with self.assertRaisesRegex(ValueError, "incomplete TLS identity"):
                tls.ensure_tls(directory, "test-host", os.getuid())
            self.assertEqual(cert.read_text(), "existing certificate")
            self.assertFalse((directory / "key.pem").exists())

    def test_key_symlink_is_rejected(self):
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            (directory / "cert.pem").write_text("certificate")
            (directory / "target").write_text("preserve")
            (directory / "key.pem").symlink_to(directory / "target")
            with self.assertRaisesRegex(ValueError, "regular file"):
                tls.ensure_tls(directory, "test-host", os.getuid())
            self.assertEqual((directory / "target").read_text(), "preserve")

    @unittest.skipUnless(Path("/usr/bin/openssl").exists(), "OpenSSL not installed")
    def test_real_tls_creation_and_restart_preserve_identity(self):
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            self.assertTrue(tls.ensure_tls(directory, "arch.local", os.getuid()))
            cert = (directory / "cert.pem").read_bytes()
            key = (directory / "key.pem").read_bytes()
            self.assertEqual((directory / "key.pem").stat().st_mode & 0o777, 0o600)
            self.assertFalse(tls.ensure_tls(directory, "different-host", os.getuid()))
            self.assertEqual((directory / "cert.pem").read_bytes(), cert)
            self.assertEqual((directory / "key.pem").read_bytes(), key)
            result = subprocess.run(["/usr/bin/openssl", "x509", "-in", str(directory / "cert.pem"),
                                     "-noout", "-checkhost", "arch.local"],
                                    check=True, capture_output=True, text=True)
            self.assertIn("does match", result.stdout)
            (directory / "key.pem").chmod(0o644)
            with self.assertRaisesRegex(ValueError, "unsafe permissions"):
                tls.ensure_tls(directory, "arch.local", os.getuid())

    def test_system_accounts_cannot_be_authorized(self):
        account = type("Account", (), {"pw_uid": 0, "pw_shell": "/bin/bash"})()
        with patch.object(setup.pwd, "getpwnam", return_value=account), \
                patch.object(setup.subprocess, "run") as run:
            with self.assertRaisesRegex(ValueError, "normal login account"):
                setup.authorize("root")
            run.assert_not_called()

    def test_existing_account_gets_supplementary_membership_without_password_reset(self):
        account = type("Account", (), {"pw_uid": 1000, "pw_shell": "/bin/bash"})()
        with patch.object(setup.pwd, "getpwnam", return_value=account), \
                patch.object(setup.subprocess, "run") as run:
            setup.authorize("alice")
            self.assertEqual(run.call_count, 2)
            self.assertEqual(run.call_args.args[0],
                             ["/usr/bin/usermod", "-aG", "kde-rdp-users", "--", "alice"])


if __name__ == "__main__":
    unittest.main()
