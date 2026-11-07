"""Container tools in a remote desktop reach the account's genuine runtime."""
import os
from pathlib import Path
import socket
import subprocess
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[1]
SHIM = ROOT / "broker/config/session-containers"
TOOLS = ("podman", "buildah", "toolbox", "docker")
REPORTED = ("XDG_RUNTIME_DIR", "DBUS_SESSION_BUS_ADDRESS", "DOCKER_HOST", "PATH", "WAYLAND_DISPLAY")


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
        # Stand-in real tools report their invocation and environment, and
        # write their exact argv NUL-separated to argv.
        self.bin = self.directory / "bin"
        self.bin.mkdir()
        self.argv = self.directory / "argv"
        for tool in TOOLS + ("distrobox",):
            fake = self.bin / tool
            fake.write_text(f'#!/bin/sh\nprintf "%s\\0" "$@" > "{self.argv}"\n'
                            'printf "ZERO=%s\\nARGS=%s\\n" "$0" "$*"\n' + "".join(
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
                           DBUS_SESSION_BUS_ADDRESS=f"unix:path={self.private}/bus",
                           WAYLAND_DISPLAY="wayland-0")
        if remote:
            environment["KDE_RDP_RUNTIME_DIR"] = str(self.private)
        environment.update(overrides)
        result = subprocess.run([str(self.shim_dir / tool), *args], env=environment,
                                capture_output=True, text=True, timeout=10)
        return result, dict(line.split("=", 1) for line in result.stdout.splitlines() if "=" in line)

    def received_argv(self):
        return self.argv.read_bytes().decode().split("\0")[:-1]

    def test_remote_desktop_tool_uses_account_runtime_and_user_bus(self):
        self.make_host()
        for tool in ("podman", "buildah", "docker"):
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
                self.assertEqual(reported["WAYLAND_DISPLAY"], "wayland-0")

    def test_toolbox_keeps_desktop_bus_and_runs_podman_through_the_shim(self):
        self.make_host()
        result, reported = self.run_tool("toolbox", "enter", "php dev")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(reported["ZERO"], str(self.bin / "toolbox"))
        self.assertEqual(self.received_argv(), ["enter", "php dev"])
        self.assertEqual(reported["XDG_RUNTIME_DIR"], str(self.host))
        self.assertEqual(reported["DBUS_SESSION_BUS_ADDRESS"], f"unix:path={self.private}/bus")
        self.assertEqual(reported["PATH"], f"{self.shim_dir}:{self.bin}:/usr/bin:/bin")
        # Absolute, so the podman rewrite turns it into /run/host/...
        self.assertEqual(reported["WAYLAND_DISPLAY"], f"{self.private}/wayland-0")
        result, reported = self.run_tool("toolbox", WAYLAND_DISPLAY="/elsewhere/wayland-1")
        self.assertEqual(reported["WAYLAND_DISPLAY"], "/elsewhere/wayland-1")
        result, reported = self.run_tool("toolbox", remote=False)
        self.assertEqual(reported["WAYLAND_DISPLAY"], "wayland-0")
        self.assertEqual(reported["XDG_RUNTIME_DIR"], str(self.private))
        # podman as toolbox runs it: account runtime already set, desktop
        # values forwarded as --env.
        result, reported = self.run_tool(
            "podman", "--log-level", "error", "exec", "--interactive", "--user", "me",
            "--workdir", "/w", f"--env=WAYLAND_DISPLAY={self.private}/wayland-0",
            f"--env=XDG_RUNTIME_DIR={self.host}", "box", "sh",
            XDG_RUNTIME_DIR=str(self.host))
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(reported["DBUS_SESSION_BUS_ADDRESS"], f"unix:path={self.host}/bus")
        self.assertEqual(self.received_argv(), [
            "--log-level", "error", "exec", "--interactive", "--user", "me", "--workdir", "/w",
            f"--env=WAYLAND_DISPLAY=/run/host{self.private}/wayland-0",
            f"--env=XDG_RUNTIME_DIR={self.host}", "box", "sh"])

    def test_podman_exec_environment_reaches_desktop_through_run_host(self):
        self.make_host()
        p = str(self.private)
        odd = "two words\nnext line * ? [x] $HOME 'q' \"d\" \\"
        args = [
            "--log-level", "debug", "exec", "--interactive", "--detach-keys=", "--user=me", "--tty",
            "--workdir=/run/host/home", f"--env=XDG_RUNTIME_DIR={p}",
            f"--env=PULSE_SERVER=unix:{p}/pulse/native",
            f"--env=DBUS_SESSION_BUS_ADDRESS=unix:path={p}/bus,guid=1",
            f"--env=DOCKER_HOST=unix://{p}/docker.sock",
            f"--env=PIPEWIRE_RUNTIME_DIR={p}", f"--env=NESTED=/x{p}", f"--env=OTHER={p}-other",
            f"--env=LIST={p}:{p}/a", "--env=DISPLAY=:0", f"--env=ODD={odd}", f"--env={p}",
            "-e", f"KDE_RDP_RUNTIME_DIR={p}/", "--env", f"SPLIT={p}/x", "-e", "BARE",
            "php dev", "sh", "-c", odd, f"--env=XDG_RUNTIME_DIR={p}", "-e", f"A={p}"]
        expected = list(args)
        expected[8:13] = [f"--env=XDG_RUNTIME_DIR=/run/host{p}",
                          f"--env=PULSE_SERVER=unix:/run/host{p}/pulse/native",
                          f"--env=DBUS_SESSION_BUS_ADDRESS=unix:path=/run/host{p}/bus,guid=1",
                          f"--env=DOCKER_HOST=unix:///run/host{p}/docker.sock",
                          f"--env=PIPEWIRE_RUNTIME_DIR=/run/host{p}"]
        expected[20] = f"KDE_RDP_RUNTIME_DIR=/run/host{p}/"
        expected[22] = f"SPLIT=/run/host{p}/x"
        result, _ = self.run_tool("podman", *args)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.received_argv(), expected)
        for subcommand in (["run"], ["create"], ["container", "exec"]):
            with self.subTest(subcommand=subcommand):
                self.run_tool("podman", *subcommand, "-v", "/a:/b", f"--env=X={p}/x", "img",
                              f"--env=X={p}/x")
                self.assertEqual(self.received_argv(), [*subcommand, "-v", "/a:/b",
                                                        f"--env=X=/run/host{p}/x", "img",
                                                        f"--env=X={p}/x"])

    def test_podman_arguments_unchanged_when_not_applicable(self):
        p = str(self.private)
        args = ["exec", f"--env=XDG_RUNTIME_DIR={p}", "-e", f"A={p}/a", "ctr", "x y"]
        self.run_tool("podman", *args)  # no account runtime
        self.assertEqual(self.received_argv(), args)
        self.make_host()
        self.run_tool("podman", *args, remote=False)
        self.assertEqual(self.received_argv(), args)
        self.run_tool("podman", *args, XDG_RUNTIME_DIR=str(self.directory))
        self.assertEqual(self.received_argv(), args)
        for tool in ("buildah", "docker", "toolbox"):
            with self.subTest(tool=tool):
                self.run_tool(tool, *args)
                self.assertEqual(self.received_argv(), args)
        for other in (["inspect", "exec", f"--env=A={p}"], ["ps", f"--env=A={p}"],
                      ["exec", "--", f"--env=A={p}"], ["--remote", "-e", f"A={p}"]):
            with self.subTest(other=other):
                self.run_tool("podman", *other)
                self.assertEqual(self.received_argv(), other)

    def test_distrobox_is_not_shimmed_and_its_podman_calls_are(self):
        self.make_host()
        self.assertFalse((self.shim_dir / "distrobox").exists())
        self.assertFalse((self.shim_dir / "distrobox-enter").exists())
        # A distrobox-enter stand-in: the desktop environment, podman via PATH.
        enter = self.directory / "distrobox-enter"
        enter.write_text('#!/bin/sh\nexec podman exec "--env=XDG_RUNTIME_DIR=$XDG_RUNTIME_DIR" '
                         '"--env=WAYLAND_DISPLAY=$WAYLAND_DISPLAY" "$@"\n')
        enter.chmod(0o755)
        environment = {key: value for key, value in os.environ.items() if key not in REPORTED}
        environment.update(PATH=f"{self.shim_dir}:{self.bin}:/usr/bin:/bin",
                           KDE_RDP_RUNTIME_DIR=str(self.private), XDG_RUNTIME_DIR=str(self.private),
                           WAYLAND_DISPLAY="wayland-0")
        result = subprocess.run([str(enter), "box", "id"], env=environment,
                                capture_output=True, text=True, timeout=10)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn(f"XDG_RUNTIME_DIR={self.host}\n", result.stdout)
        self.assertEqual(self.received_argv(), [
            "exec", f"--env=XDG_RUNTIME_DIR=/run/host{self.private}",
            "--env=WAYLAND_DISPLAY=wayland-0", "box", "id"])

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
        self.assertIn("LIBEXECDIR ?= /usr/lib/kde-rdp\n", makefile)
        self.assertIn("session-containers", makefile.split("SESSION_HELPERS = ", 1)[1].split("\n\n", 1)[0])
        self.assertIn('ln -sfn ../session-containers "$(DESTDIR)$(LIBEXECDIR)/containers/$$tool"', makefile)
        # tests/test_install_layout.py checks the installed links and the
        # rewritten shim directory for both the Arch and Fedora layouts.
        worker = (ROOT / "broker/config/session-worker").read_text()
        self.assertIn('export PATH="/usr/lib/kde-rdp/containers:${PATH:-/usr/bin:/bin}"\n', worker)
        self.assertLess(worker.index('export XDG_RUNTIME_DIR="$KDE_RDP_RUNTIME_DIR"'),
                        worker.index("/usr/lib/kde-rdp/containers:"))


if __name__ == "__main__":
    unittest.main()
