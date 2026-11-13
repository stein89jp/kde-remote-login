#!/bin/bash
# Runs inside the build container (see Containerfile): build the RPM from the
# source tarball in /in and copy it to /out. /cache persists the crate
# downloads and Cargo's target directory between builds.
#   KDE_RDP_RELEASE  RPM release, e.g. 1.20261107120000 (required)
#   KDE_RDP_CHECK    0 skips %check (the test suites)
set -euo pipefail
: "${KDE_RDP_RELEASE:?}"
# The test suites assume an ordinary account (as makepkg's check() does);
# install.sh runs the container with --userns=keep-id.
[ "$(id -u)" -ne 0 ] || { echo "build-rpm.sh must not run as root" >&2; exit 1; }
export HOME=/tmp/home
mkdir -p "$HOME"
shopt -s nullglob
tarballs=(/in/kde-remote-login-*.tar.gz)
[ "${#tarballs[@]}" -eq 1 ] || { echo "expected one source tarball in /in" >&2; exit 1; }
topdir=/tmp/rpmbuild
mkdir -p "$topdir/SOURCES" "$topdir/SPECS" /cache/cargo /cache/target /out
cp "${tarballs[0]}" "$topdir/SOURCES/"
tar -xzOf "${tarballs[0]}" --wildcards '*/packaging/bazzite/kde-remote-login.spec' \
    > "$topdir/SPECS/kde-remote-login.spec"
check=()
[ "${KDE_RDP_CHECK:-1}" = 0 ] && check=(--nocheck)
rpmbuild -bb "${check[@]}" \
    --define "_topdir $topdir" \
    --define "kde_rdp_release $KDE_RDP_RELEASE" \
    --define "kde_rdp_cargo_home /cache/cargo" \
    --define "kde_rdp_target_dir /cache/target" \
    "$topdir/SPECS/kde-remote-login.spec"
rm -f /out/kde-remote-login-*.rpm
cp "$topdir"/RPMS/x86_64/kde-remote-login-*.rpm /out/
ls -l /out
