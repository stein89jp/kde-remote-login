"""Stage the broker install for the Arch and Fedora layouts and check the
Bazzite packaging, without building or touching the host."""
import configparser
from pathlib import Path
import re
import shutil
import subprocess
import tempfile
import unittest
import xml.etree.ElementTree as ElementTree


ROOT = Path(__file__).resolve().parents[1]
CONFIG = ROOT / "broker/config"
HELPERS = sorted(path.name for path in CONFIG.glob("session-*") if path.is_file())
TOOLS = ("podman", "buildah", "toolbox", "docker")
FEDORA_LIBEXEC = "/usr/libexec/kde-rdp"
FEDORA_PAM = ROOT / "packaging/fedora/kde-rdp.pam"
SPEC = ROOT / "packaging/bazzite/kde-remote-login.spec"
INSTALLER = ROOT / "packaging/bazzite/install.sh"
# Installed by the package itself rather than the broker Makefile.
PACKAGE_FILES = {"kde-rdp-bridge", "encoder-probe", "prepare-tls"}


@unittest.skipUnless(shutil.which("make") and shutil.which("sed"), "requires make and sed")
class BrokerInstallLayoutTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory(prefix="kde-rdp-layout-")
        self.addCleanup(self.tmp.cleanup)
        # A copy, so the repository never gains a build product; a stand-in
        # binary marked old (-o) keeps make from compiling the broker.
        self.broker = Path(self.tmp.name, "broker")
        shutil.copytree(CONFIG, self.broker / "config")
        shutil.copy(ROOT / "broker/Makefile", self.broker / "Makefile")
        shutil.copy(ROOT / "broker/broker.c", self.broker / "broker.c")
        (self.broker / "kde-rdp-broker").write_bytes(b"\x7fELF stand-in")

    def stage(self, name, *variables):
        root = Path(self.tmp.name, name)
        subprocess.run(["make", "-s", "-C", str(self.broker), "-o", "kde-rdp-broker", "install",
                        f"DESTDIR={root}", *variables], check=True, capture_output=True, text=True)
        return root

    def compile_command(self, *variables):
        return subprocess.run(["make", "-n", "-B", "-C", str(self.broker), "kde-rdp-broker", *variables],
                              check=True, capture_output=True, text=True).stdout

    def test_default_layout_is_the_arch_package_byte_for_byte(self):
        root = self.stage("arch")
        libexec = root / "usr/lib/kde-rdp"
        self.assertEqual(sorted(path.name for path in libexec.iterdir()),
                         sorted([*HELPERS, "containers", "kde-rdp-broker"]))
        for helper in HELPERS:
            with self.subTest(helper=helper):
                self.assertEqual((libexec / helper).read_bytes(), (CONFIG / helper).read_bytes())
                self.assertEqual((libexec / helper).stat().st_mode & 0o777, 0o755)
        for tool in TOOLS:
            self.assertEqual((libexec / "containers" / tool).readlink(), Path("../session-containers"))
        unit = root / "usr/lib/systemd/system/kde-rdp-broker.service"
        self.assertEqual(unit.read_bytes(), (CONFIG / "kde-rdp-broker.service").read_bytes())
        self.assertEqual(unit.stat().st_mode & 0o777, 0o644)
        self.assertEqual((root / "etc/pam.d/kde-rdp").read_bytes(), (CONFIG / "kde-rdp.pam").read_bytes())
        for name in ("renderer.conf", "broker.conf"):
            self.assertEqual((root / "etc/kde-rdp" / name).read_bytes(), (CONFIG / name).read_bytes())
        self.assertFalse((root / "usr/libexec").exists())
        self.assertIn("""-DWORKER='"/usr/lib/kde-rdp/session-worker"'""", self.compile_command())

    def test_fedora_layout_moves_every_helper_reference_to_libexec(self):
        root = self.stage("fedora", f"LIBEXECDIR={FEDORA_LIBEXEC}", f"PAM_FILE={FEDORA_PAM}")
        libexec = root / FEDORA_LIBEXEC.lstrip("/")
        self.assertFalse((root / "usr/lib/kde-rdp").exists())
        installed = [path for path in root.rglob("*") if path.is_file() and not path.is_symlink()]
        references = set()
        for path in installed:
            text = path.read_text(errors="replace")
            with self.subTest(path=str(path.relative_to(root))):
                self.assertNotIn("/usr/lib/kde-rdp", text)
            references.update(re.findall(re.escape(FEDORA_LIBEXEC) + r"/([A-Za-z0-9_-]+(?:\.[A-Za-z0-9_-]+)*)", text))
        self.assertTrue(references)
        for name in references - PACKAGE_FILES:
            with self.subTest(reference=name):
                self.assertTrue((libexec / name).exists(), name)
        for helper in HELPERS:
            with self.subTest(helper=helper):
                arch = (CONFIG / helper).read_text()
                self.assertEqual((libexec / helper).read_text(),
                                 arch.replace("/usr/lib/kde-rdp", FEDORA_LIBEXEC))
                self.assertEqual((libexec / helper).stat().st_mode & 0o777, 0o755)
        unit = configparser.ConfigParser(interpolation=None, strict=False)
        unit.read(root / "usr/lib/systemd/system/kde-rdp-broker.service")
        self.assertEqual(unit["Service"]["ExecStart"], f"{FEDORA_LIBEXEC}/kde-rdp-broker")
        self.assertEqual(unit["Service"]["ExecStartPre"], f"{FEDORA_LIBEXEC}/prepare-tls")
        worker = (libexec / "session-worker").read_text()
        self.assertIn(f'export PATH="{FEDORA_LIBEXEC}/containers:${{PATH:-/usr/bin:/bin}}"\n', worker)
        self.assertIn(f"shim_dir={FEDORA_LIBEXEC}/containers\n", (libexec / "session-containers").read_text())
        for tool in TOOLS:
            self.assertEqual((libexec / "containers" / tool).readlink(), Path("../session-containers"))
        self.assertEqual((root / "etc/pam.d/kde-rdp").read_bytes(), FEDORA_PAM.read_bytes())
        self.assertIn(f"""-DWORKER='"{FEDORA_LIBEXEC}/session-worker"'""",
                      self.compile_command(f"LIBEXECDIR={FEDORA_LIBEXEC}"))

    def test_install_refuses_relative_destdir(self):
        result = subprocess.run(["make", "-s", "-C", str(self.broker), "-o", "kde-rdp-broker", "install",
                                 "DESTDIR=relative"], capture_output=True, text=True)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("absolute staging DESTDIR", result.stderr)


class BazzitePackagingTests(unittest.TestCase):
    def setUp(self):
        self.spec = SPEC.read_text()

    def test_spec_version_follows_the_arch_pkgbuilds(self):
        version = re.search(r"(?m)^Version:\s*(\S+)$", self.spec).group(1)
        for package in ("kde-rdp", "kde-remote-login"):
            pkgbuild = ROOT / "packaging/arch" / package / "PKGBUILD"
            if not pkgbuild.exists():
                self.skipTest("PKGBUILDs are not part of the package source tree")
            with self.subTest(package=package):
                self.assertEqual(re.search(r"(?m)^pkgver=(\S+)$", pkgbuild.read_text()).group(1), version)

    def test_spec_installs_the_fedora_layout_and_keeps_configuration(self):
        self.assertIn("%global kde_rdp_libexecdir %{_libexecdir}/kde-rdp", self.spec)
        self.assertIn('PAM_FILE="$PWD/packaging/fedora/kde-rdp.pam" install', self.spec)
        self.assertIn("make_build -C broker LIBEXECDIR=%{kde_rdp_libexecdir}", self.spec)
        for path in ("frontend.conf", "renderer.conf", "broker.conf"):
            self.assertIn(f"%config(noreplace) %{{_sysconfdir}}/kde-rdp/{path}", self.spec)
        self.assertIn("%config(noreplace) %{_sysconfdir}/pam.d/kde-rdp", self.spec)
        # Same build discipline as the PKGBUILD: locked, offline, cc-rs flags.
        self.assertIn('%global kde_rdp_cargo_env env -u CFLAGS -u CXXFLAGS -u LDFLAGS '
                      'RUSTFLAGS="-C link-arg=-fuse-ld=lld"', self.spec)
        self.assertIn("%{kde_rdp_cargo_env} cargo build --release --locked --offline", self.spec)
        self.assertIn("%{kde_rdp_cargo_env} cargo test --locked --offline", self.spec)
        # Restarting on upgrade would end every connected remote desktop.
        self.assertNotIn("systemd_postun_with_restart", self.spec)
        for unit in (ROOT / "packaging/systemd").glob("*.service"):
            self.assertIn("%{_unitdir}/" + unit.name, self.spec)

    def test_spec_tests_cover_the_pkgbuild_suites(self):
        pkgbuild = ROOT / "packaging/arch/kde-rdp/PKGBUILD"
        if not pkgbuild.exists():
            self.skipTest("PKGBUILDs are not part of the package source tree")
        suites = set(re.findall(r"tests\.(test_\w+)", pkgbuild.read_text()))
        self.assertTrue(suites)
        self.assertLessEqual(suites, set(re.findall(r"tests\.(test_\w+)", self.spec)))

    def test_selinux_module_only_lets_the_broker_enter_the_login_context(self):
        rules = [line for line in (ROOT / "packaging/fedora/kde_rdp.cil").read_text().splitlines()
                 if line.strip() and not line.lstrip().startswith(";")]
        self.assertEqual(rules, ["(allow unconfined_service_t unconfined_t "
                                 "(process (transition noatsecure rlimitinh siginh)))"])
        self.assertIn("%{_datadir}/selinux/packages/targeted/kde_rdp.cil", self.spec)
        self.assertIn("semodule -n -s targeted -X 200 -i %{_datadir}/selinux/packages/targeted/kde_rdp.cil",
                      self.spec)
        self.assertIn("semodule -n -s targeted -X 200 -r kde_rdp", self.spec)
        # The PAM stack is what asks for the transition.
        self.assertIn("pam_selinux.so       open", FEDORA_PAM.read_text())

    def test_firewalld_service_opens_the_frontend_port(self):
        service = ElementTree.parse(ROOT / "packaging/bazzite/kde-rdp.firewalld.xml").getroot()
        ports = [(port.get("protocol"), port.get("port")) for port in service.iter("port")]
        listen = re.search(r"(?m)^KDE_RDP_LISTEN=\S+:(\d+)$", (ROOT / "packaging/frontend.conf").read_text())
        self.assertEqual(ports, [("tcp", listen.group(1))])


@unittest.skipUnless(shutil.which("bash"), "requires bash")
class InstallerHelperTests(unittest.TestCase):
    def call(self, function, *arguments, config_root=""):
        # Sourcing runs no installer step: main only runs when executed.
        script = 'source "$1"; shift; "$@"'
        return subprocess.run(["bash", "-c", script, "installer", str(INSTALLER), function, *arguments],
                              capture_output=True, text=True,
                              env={"PATH": "/usr/bin:/bin", "KDE_RDP_CONFIG_ROOT": config_root})

    def test_scripts_parse(self):
        for script in sorted((ROOT / "packaging/bazzite").rglob("*.sh")):
            with self.subTest(script=script.name):
                subprocess.run(["bash", "-n", str(script)], check=True)

    def test_kwin_version_gate(self):
        for version, allowed in (("6.8.2", True), ("6.8", True), ("6.10.1", True), ("7.0", True),
                                 ("6.7.5", False), ("6.7.90", False), ("5.27.11", False)):
            with self.subTest(version=version):
                self.assertEqual(self.call("version_at_least", version, "6.8").returncode == 0, allowed)

    def test_firewall_port_ranges(self):
        cases = (
            (("1025-65535/udp", "1025-65535/tcp"), True),   # FedoraWorkstation
            (("33991/tcp",), True),
            (("33991/udp",), False),
            (("22/tcp", "1024-33990/tcp", "33992-40000/tcp"), False),
            ((), False),
        )
        for entries, allowed in cases:
            with self.subTest(entries=entries):
                result = self.call("port_listed", "33991", "tcp", *entries)
                self.assertEqual(result.returncode == 0, allowed)

    def test_autologin_lookup_follows_display_manager_precedence(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            vendor = root / "usr/lib/plasmalogin/plasmalogin.conf.d"
            local = root / "etc/plasmalogin.conf.d"
            vendor.mkdir(parents=True)
            local.mkdir(parents=True)
            main = root / "etc/plasmalogin.conf"
            main.write_text("[Autologin]\n#User=\n#Relogin=false\n[General]\nUser=not-autologin\n")

            def effective():
                configs = self.call("display_manager_configs", "plasmalogin",
                                    config_root=str(root)).stdout.split()
                return self.call("autologin_setting", *configs).stdout.rstrip("\n")

            self.assertEqual(effective(), "")
            (vendor / "00-vendor.conf").write_text("[Autologin]\nUser=gamer\nSession=gamescope\n")
            self.assertEqual(effective(), f"gamer\t{vendor / '00-vendor.conf'}")
            (local / "kde_settings.conf").write_text("[Autologin]\nRelogin=false\nUser = alice \n")
            self.assertEqual(effective(), f"alice\t{local / 'kde_settings.conf'}")
            # The installer's own drop-in sorts last among the drop-ins.
            (local / "zz-kde-rdp-no-autologin.conf").write_text("[Autologin]\nUser=\nRelogin=false\n")
            self.assertEqual(effective(), f"\t{local / 'zz-kde-rdp-no-autologin.conf'}")
            # The main file overrides every drop-in.
            main.write_text("[Autologin]\nUser=bob\n")
            self.assertEqual(effective(), f"bob\t{main}")
            self.assertEqual(self.call("display_manager_configs", "sddm", config_root=str(root)).stdout, "")


if __name__ == "__main__":
    unittest.main()
