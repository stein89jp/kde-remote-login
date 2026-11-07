#!/usr/bin/python3
"""Authorize a remote-login account and prepare the host's TLS certificate."""
import argparse
import ctypes
import os
from pathlib import Path
import pwd
import re
import subprocess

GROUP = "kde-rdp-users"
ETC_GROUP = Path("/etc/group")
# Fedora Atomic (Bazzite): groups created while rpm-ostree layers a package
# land in the image's /usr/lib/group (nss-altfiles), which usermod never edits.
IMAGE_GROUP = Path("/usr/lib/group")


def group_entry(path, name):
    try:
        lines = path.read_text().splitlines()
    except FileNotFoundError:
        return None
    return next((line for line in lines if line.split(":", 1)[0] == name), None)


def ensure_local_group(name=GROUP):
    """Copy an image-only group into /etc/group so usermod can record members.

    NSS merges both files, so the copy (without members) changes nothing until
    usermod adds the account. A no-op on Arch, which has no /usr/lib/group."""
    if group_entry(ETC_GROUP, name) is not None:
        return False
    line = group_entry(IMAGE_GROUP, name)
    if line is None:
        return False
    fields = line.split(":")
    if len(fields) != 4:
        raise ValueError(f"unexpected {IMAGE_GROUP} entry for {name}")
    fields[3] = ""
    libc = ctypes.CDLL(None, use_errno=True)
    if libc.lckpwdf() != 0:
        raise OSError(ctypes.get_errno(), "cannot lock the account files")
    try:
        # Append in place: the file keeps its mode, owner and SELinux label.
        with ETC_GROUP.open("r+") as stream:
            content = stream.read()
            if content and not content.endswith("\n"):
                stream.write("\n")
            stream.write(":".join(fields) + "\n")
    finally:
        libc.ulckpwdf()
    return True


def account_name(value):
    if not re.fullmatch(r"[a-z_][a-z0-9_-]{0,31}", value):
        raise argparse.ArgumentTypeError("use a local Linux account name (no domain prefix)")
    return value


def authorize(name, create=False):
    try:
        account = pwd.getpwnam(name)
    except KeyError:
        account = None
    if create:
        if account is not None:
            raise ValueError(f"account {name} already exists; use --user {name}")
        subprocess.run(["/usr/bin/useradd", "-m", "-s", "/bin/bash", "--", name], check=True)
        subprocess.run(["/usr/bin/passwd", "--", name], check=True)
        account = pwd.getpwnam(name)
    if account is None:
        raise ValueError(f"account {name} does not exist; use --create-user {name}")
    if account.pw_uid < 1000 or name == "kde-rdp" or not account.pw_shell or \
            "nologin" in account.pw_shell or account.pw_shell in ("/bin/false", "/usr/bin/false"):
        raise ValueError("choose a normal login account with UID >= 1000")
    subprocess.run(["/usr/bin/systemd-sysusers", "/usr/lib/sysusers.d/kde-rdp.conf"], check=True)
    ensure_local_group()
    subprocess.run(["/usr/bin/usermod", "-aG", "kde-rdp-users", "--", name], check=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    accounts = parser.add_mutually_exclusive_group()
    accounts.add_argument("--user", type=account_name, help="authorize an existing account")
    accounts.add_argument("--create-user", type=account_name, help="create an account and prompt for its password")
    parser.add_argument("--hostname", help="DNS name or IP for a new certificate")
    arguments = parser.parse_args()
    if os.geteuid() != 0:
        parser.error("run with sudo")
    name = arguments.user or arguments.create_user or os.environ.get("SUDO_USER")
    if not name:
        parser.error("specify --user or --create-user")
    try:
        account_name(name)
        authorize(name, create=arguments.create_user is not None)
        command = ["/usr/lib/kde-rdp/prepare-tls"]
        if arguments.hostname:
            command.extend(["--hostname", arguments.hostname])
        subprocess.run(command, check=True)
        fingerprint = subprocess.check_output([
            "/usr/bin/openssl", "x509", "-in", "/etc/kde-rdp/tls/cert.pem",
            "-noout", "-fingerprint", "-sha256",
        ], text=True).strip()
    except (ValueError, argparse.ArgumentTypeError, OSError, subprocess.SubprocessError) as error:
        parser.exit(1, f"Setup failed: {error}\n")
    print(f"Account {name} is authorized for KDE remote login.")
    print(fingerprint)
    print("Start at boot: sudo systemctl enable --now kde-rdp-frontend.service")
    print("Connect with Windows Remote Desktop to HOST:33991 using this Linux account.")
    print("Log out of this account's local graphical desktop before connecting remotely.")


if __name__ == "__main__":
    main()
