"""Container tools in a remote desktop reach the account's genuine runtime."""
import os
from pathlib import Path
import socket
import subprocess
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[1]
SHIM = ROOT / "broker/config/session-containers"
TOOLS = ("podman", "buildah", "distrobox", "distrobox-enter", "toolbox", "docker")
REPORTED = ("XDG_RUNTIME_DIR", "DBUS_SESSION_BUS_ADDRESS", "DOCKER_HOST", "PATH")


class ContainerShimTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.directory = Path(temporary.name)
        self.shim_dir = self.directory / "containers"
        self.shim_dir.mkdir()
        self.user_root = self.directory / "run-user"
        self.host = self.user_root / str(os.getuid())
        self.private = self.directory / "private"
        self.private.mkdir()
        shim = self.directory / "session-containers"
        text = SHIM.read_text()
        self.assertIn("shim_dir=/usr/lib/kde-rdp/containers\n", text)
        self.assertIn("user_runtime_root=/run/user\n", text)
        shim.write_text(text.replace("shim_dir=/usr/lib/kde-rdp/containers", f"shim_dir={self.shim_dir}")
                        .replace("user_runtime_root=/run/user", f"user_runtime_root={self.user_root}"))
        shim.chmod(0o755)
        for tool in TOOLS + ("kde-rdp-missing-tool",):
            (self.shim_dir / tool).symlink_to("../session-containers")
        # Stand-in real tools report their invocation and environment.
        self.bin = self.directory / "bin"
        self.bin.mkdir()
        for tool in TOOLS:
            fake = self.bin / tool
            fake.write_text('#!/bin/sh\nprintf "ZERO=%s\\nARGS=%s\\n" "$0" "$*"\n' + "".join(
                f'printf "{key}=%s\\n" "${{{key}-unset}}"\n' for key in REPORTED))
            fake.chmod(0o755)

    def make_host(self, bus=True):
        self.host.mkdir(parents=True)
        if bus:
            server = socket.socket(socket.AF_UNIX)
            self.addCleanup(server.close)
            server.bind(str(self.host / "bus"))

    def run_tool(self, tool, *args, remote=True, **overrides):
        environment = {key: value for key, value in os.environ.items()
                       if key not in REPORTED + ("KDE_RDP_RUNTIME_DIR",)}
        environment.update(PATH=f"{self.shim_dir}:{self.bin}:/usr/bin:/bin",
                           XDG_RUNTIME_DIR=str(self.private),
                           DBUS_SESSION_BUS_ADDRESS=f"unix:path={self.private}/bus")
        if remote:
            environment["KDE_RDP_RUNTIME_DIR"] = str(self.private)
        environment.update(overrides)
        result = subprocess.run([str(self.shim_dir / tool), *args], env=environment,
                                capture_output=True, text=True, timeout=10)
        return result, dict(line.split("=", 1) for line in result.stdout.splitlines() if "=" in line)

    def test_remote_desktop_tool_uses_account_runtime_and_user_bus(self):
        self.make_host()
        for tool in TOOLS:
            with self.subTest(tool=tool):
                result, reported = self.run_tool(tool, "enter", "php dev")
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(reported["ZERO"], str(self.bin / tool))
                self.assertEqual(reported["ARGS"], "enter php dev")
                self.assertEqual(reported["XDG_RUNTIME_DIR"], str(self.host))
                self.assertEqual(reported["DBUS_SESSION_BUS_ADDRESS"], f"unix:path={self.host}/bus")
                # The shim never re-enters itself through the real tool's children.
                self.assertEqual(reported["PATH"], f"{self.bin}:/usr/bin:/bin")
                self.assertEqual(reported["DOCKER_HOST"], "unset")

    def test_missing_user_bus_keeps_private_bus(self):
        self.make_host(bus=False)
        result, reported = self.run_tool("podman")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(reported["XDG_RUNTIME_DIR"], str(self.host))
        self.assertEqual(reported["DBUS_SESSION_BUS_ADDRESS"], f"unix:path={self.private}/bus")

    def test_environment_unchanged_outside_remote_desktop_or_without_user_runtime(self):
        cases = [("local login", dict(remote=False), True),
                 ("other runtime", dict(XDG_RUNTIME_DIR=str(self.directory)), True),
                 ("no logind runtime", {}, False)]
        for label, options, host in cases:
            with self.subTest(label):
                if host and not self.host.exists():
                    self.make_host()
                if not host and self.host.exists():
                    (self.host / "bus").unlink()
                    self.host.rmdir()
                result, reported = self.run_tool("podman", **options)
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(reported["XDG_RUNTIME_DIR"],
                                 options.get("XDG_RUNTIME_DIR", str(self.private)))
                self.assertEqual(reported["DBUS_SESSION_BUS_ADDRESS"], f"unix:path={self.private}/bus")
                self.assertEqual(reported["PATH"], f"{self.bin}:/usr/bin:/bin")

    def test_rootless_docker_host_follows_account_runtime_only_when_private(self):
        self.make_host()
        cases = {f"unix://{self.private}/docker.sock": f"unix://{self.host}/docker.sock",
                 "unix:///var/run/docker.sock": "unix:///var/run/docker.sock",
                 "tcp://127.0.0.1:2375": "tcp://127.0.0.1:2375"}
        for value, expected in cases.items():
            with self.subTest(value=value):
                result, reported = self.run_tool("docker", "ps", DOCKER_HOST=value)
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(reported["DOCKER_HOST"], expected)

    def test_missing_tool_and_self_reference_fail_without_looping(self):
        self.make_host()
        result, _ = self.run_tool("kde-rdp-missing-tool")
        self.assertEqual(result.returncode, 127)
        self.assertIn("command not found", result.stderr)
        # A second spelling of the shim directory still resolves to the shim.
        alias = self.directory / "alias"
        alias.symlink_to(self.shim_dir)
        result, _ = self.run_tool("podman", PATH=f"{self.shim_dir}:{alias}:/usr/bin:/bin")
        self.assertEqual(result.returncode, 127)
        self.assertIn("only the KDE-RDP container shim", result.stderr)

    def test_broker_install_links_every_tool_and_worker_prefers_them(self):
        makefile = (ROOT / "broker/Makefile").read_text()
        self.assertIn("CONTAINER_TOOLS = " + " ".join(TOOLS) + "\n", makefile)
        self.assertIn('install -Dm755 config/session-containers "$(DESTDIR)/usr/lib/kde-rdp/session-containers"',
                      makefile)
        self.assertIn('ln -sfn ../session-containers "$(DESTDIR)/usr/lib/kde-rdp/containers/$$tool"', makefile)
        worker = (ROOT / "broker/config/session-worker").read_text()
        self.assertIn('export PATH="/usr/lib/kde-rdp/containers:${PATH:-/usr/bin:/bin}"\n', worker)
        self.assertLess(worker.index('export XDG_RUNTIME_DIR="$KDE_RDP_RUNTIME_DIR"'),
                        worker.index("/usr/lib/kde-rdp/containers:"))


if __name__ == "__main__":
    unittest.main()
