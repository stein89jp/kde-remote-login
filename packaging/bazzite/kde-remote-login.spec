# KDE-RDP for Bazzite and other Fedora Atomic KDE desktops.
#
# packaging/bazzite/install.sh builds this inside a container that matches the
# host's Fedora release and FFmpeg repository, then layers the result with
# rpm-ostree (docs/install-bazzite.md). Version follows the Arch PKGBUILD
# (tests/test_install_layout.py checks they match). Optional defines:
#   kde_rdp_release      Release prefix; the installer stamps each local build
#   kde_rdp_cargo_home   persistent CARGO_HOME (crate download cache)
#   kde_rdp_target_dir   persistent CARGO_TARGET_DIR (incremental rebuilds)

%global debug_package %{nil}
# No %changelog: release dates come from the build, not a changelog entry.
%global source_date_epoch_from_changelog 0
%global _lto_cflags %{nil}
%global kde_rdp_libexecdir %{_libexecdir}/kde-rdp
# Cargo builds the bridge as on Arch. aws-lc's jitterentropy requires -O0, so
# cc-rs controls its own C flags (the PKGBUILD unsets CFLAGS too), and Fedora's
# RUSTFLAGS (codegen-units=1, opt-level=3 everywhere) give way to Cargo's
# profiles. Link with lld, as Rust does on Arch (rust-lld is its default
# linker there; Fedora's rustc uses ld.bfd): build.rs lists the FFmpeg,
# PipeWire and GL libraries before the static C archives that use them, which
# lld resolves in any order but ld.bfd's --as-needed drops for the test binary.
# The broker keeps Fedora's hardened flags.
%global kde_rdp_cargo_env env -u CFLAGS -u CXXFLAGS -u LDFLAGS RUSTFLAGS="-C link-arg=-fuse-ld=lld"
%global kde_rdp_cargo %{?kde_rdp_cargo_home}%{!?kde_rdp_cargo_home:%{_builddir}/cargo-home}
%global kde_rdp_target %{?kde_rdp_target_dir}%{!?kde_rdp_target_dir:%{_builddir}/%{name}-%{version}/bridge/target}

Name:           kde-remote-login
Version:        1.7.0
Release:        %{?kde_rdp_release}%{!?kde_rdp_release:1}%{?dist}
Summary:        KDE Plasma remote desktop host (KWin virtual backend) with a PAM/logind broker
# Bridge source is MIT; the combined binary links GPL-licensed FFmpeg.
License:        MIT AND GPL-3.0-only
URL:            https://github.com/stein89jp/kde-remote-login
Source0:        %{name}-%{version}.tar.gz
ExclusiveArch:  x86_64

BuildRequires:  cargo
BuildRequires:  rust >= 1.94
BuildRequires:  gcc
BuildRequires:  lld
BuildRequires:  make
BuildRequires:  pkgconf-pkg-config
BuildRequires:  python3
BuildRequires:  pam-devel
BuildRequires:  systemd-rpm-macros
BuildRequires:  pkgconfig(libsystemd)
BuildRequires:  pkgconfig(xkbcommon)
BuildRequires:  pkgconfig(wayland-client)
BuildRequires:  pkgconfig(egl)
BuildRequires:  pkgconfig(gl)
BuildRequires:  pkgconfig(libpipewire-0.3)
# The same FFmpeg the host runs (Bazzite: negativo17); see build-container/.
BuildRequires:  pkgconfig(libavcodec)
BuildRequires:  pkgconfig(libavutil)
BuildRequires:  pkgconfig(libswscale)
BuildRequires:  pkgconfig(libswresample)
# %%check: the PKGBUILD's suites, including real TLS creation.
BuildRequires:  openssl

# Stock Plasma 6 desktop: KWin's virtual backend with Xwayland, the Plasma
# shell and session (plasma_session, ksmserver, kded6) and portals. Bazzite
# replaces some Fedora packages (Terra's WirePlumber and Xwayland), so name
# the commands the session helpers run rather than package names.
Requires:       /usr/bin/kwin_wayland
Requires:       /usr/bin/plasma_session
Requires:       /usr/bin/plasmashell
Requires:       /usr/bin/kded6
Requires:       /usr/bin/Xwayland
Requires:       xdg-desktop-portal
Requires:       xdg-desktop-portal-kde
Requires:       /usr/bin/pipewire
Requires:       /usr/bin/pipewire-pulse
Requires:       /usr/bin/wireplumber
Requires:       /usr/bin/pw-cat
Requires:       /usr/bin/pactl
Requires:       /usr/bin/parec
Requires:       /usr/bin/dbus-run-session
Requires:       /usr/bin/dbus-update-activation-environment
Requires:       /usr/bin/busctl
Requires:       /usr/bin/openssl
Requires:       /usr/bin/python3
Requires:       python3-gobject
Requires:       xkeyboard-config
%{?systemd_requires}
# The kde_rdp SELinux module (packaging/fedora/kde_rdp.cil).
Requires:       selinux-policy-targeted
Requires(post): policycoreutils
Requires(postun): policycoreutils

%description
A login-independent RDP host for KDE Plasma 6. A root PAM/logind broker
starts a private KWin virtual-backend desktop with a Plasma session for each
authenticated account; the unprivileged frontend serves it to Windows Remote
Desktop (mstsc.exe) on TCP 33991.

%prep
%autosetup -n %{name}-%{version}
export CARGO_HOME=%{kde_rdp_cargo}
cargo fetch --locked --manifest-path bridge/Cargo.toml

%build
export CARGO_HOME=%{kde_rdp_cargo}
export CARGO_TARGET_DIR=%{kde_rdp_target}
%{kde_rdp_cargo_env} cargo build --release --locked --offline --manifest-path bridge/Cargo.toml
%make_build -C broker LIBEXECDIR=%{kde_rdp_libexecdir}
python3 scripts/collect-licenses.py bridge/Cargo.toml THIRD-PARTY-NOTICES.txt

%check
export CARGO_HOME=%{kde_rdp_cargo}
export CARGO_TARGET_DIR=%{kde_rdp_target}
%{kde_rdp_cargo_env} cargo test --locked --offline --manifest-path bridge/Cargo.toml
make -C broker check
python3 -m unittest tests.test_host_setup tests.test_session_environment \
    tests.test_session_containers tests.test_native_video tests.test_audio \
    tests.test_microphone tests.test_performance_harness tests.test_pam_config \
    tests.test_install_layout -v

%install
install -Dm755 %{kde_rdp_target}/release/kde-rdp-bridge %{buildroot}%{kde_rdp_libexecdir}/kde-rdp-bridge
install -Dm755 %{kde_rdp_target}/release/encoder-probe %{buildroot}%{kde_rdp_libexecdir}/encoder-probe
make -C broker DESTDIR=%{buildroot} LIBEXECDIR=%{kde_rdp_libexecdir} UNITDIR=%{_unitdir} \
    PAM_FILE="$PWD/packaging/fedora/kde-rdp.pam" install
for unit in packaging/systemd/*.service; do
    sed -e 's|/usr/lib/kde-rdp|%{kde_rdp_libexecdir}|g' "$unit" > %{buildroot}%{_unitdir}/"${unit##*/}"
    chmod 644 %{buildroot}%{_unitdir}/"${unit##*/}"
done
install -dm755 %{buildroot}%{_bindir}
sed -e 's|/usr/lib/kde-rdp|%{kde_rdp_libexecdir}|g' packaging/kde-rdp-setup.py > %{buildroot}%{_bindir}/kde-rdp-setup
chmod 755 %{buildroot}%{_bindir}/kde-rdp-setup
install -Dm755 packaging/prepare-tls.py %{buildroot}%{kde_rdp_libexecdir}/prepare-tls
install -Dm644 packaging/kde-rdp.sysusers %{buildroot}%{_sysusersdir}/kde-rdp.conf
install -Dm644 packaging/frontend.conf %{buildroot}%{_sysconfdir}/kde-rdp/frontend.conf
install -Dm644 packaging/bazzite/kde-rdp.firewalld.xml %{buildroot}%{_prefix}/lib/firewalld/services/kde-rdp.xml
install -Dm644 packaging/fedora/kde_rdp.cil %{buildroot}%{_datadir}/selinux/packages/targeted/kde_rdp.cil
install -Dm644 packaging/nvidia-frontend.conf %{buildroot}%{_docdir}/%{name}/nvidia-frontend.conf
for document in install-bazzite uninstall-bazzite acceptance performance; do
    install -Dm644 docs/$document.md %{buildroot}%{_docdir}/%{name}/$document.md
done
install -Dm644 README.md %{buildroot}%{_docdir}/%{name}/README.md
# fail2ban examples; the package never enables them.
install -Dm644 packaging/fail2ban/kde-rdp.conf %{buildroot}%{_docdir}/%{name}/fail2ban/kde-rdp.conf
install -Dm644 packaging/fail2ban/kde-rdp.jail.example %{buildroot}%{_docdir}/%{name}/fail2ban/kde-rdp.jail.example
# Nothing outside the documentation may still name the Arch helper directory.
if grep -rl --exclude-dir=doc /usr/lib/kde-rdp %{buildroot}; then
    echo 'Arch helper path left in the Fedora layout' >&2
    exit 1
fi

%post
%systemd_post kde-rdp-broker.service kde-rdp-frontend.service kde-rdp-nvidia-devices.service
# Install the SELinux module into the policy store (on rpm-ostree: the new
# deployment's). -n: no reload here; packaging/bazzite/install.sh activates it
# on the running system, a reboot does otherwise.
. /etc/selinux/config 2>/dev/null || :
if [ "${SELINUXTYPE:-targeted}" = targeted ] && command -v semodule >/dev/null; then
    semodule -n -s targeted -X 200 -i %{_datadir}/selinux/packages/targeted/kde_rdp.cil || :
    selinuxenabled 2>/dev/null && load_policy 2>/dev/null || :
fi

%preun
%systemd_preun kde-rdp-broker.service kde-rdp-frontend.service kde-rdp-nvidia-devices.service

%postun
# Never restart on upgrade: that would end every connected remote desktop.
%systemd_postun kde-rdp-broker.service kde-rdp-frontend.service kde-rdp-nvidia-devices.service
if [ $1 -eq 0 ] && command -v semodule >/dev/null; then
    semodule -n -s targeted -X 200 -r kde_rdp >/dev/null 2>&1 || :
    selinuxenabled 2>/dev/null && load_policy 2>/dev/null || :
fi

%files
%license LICENSE THIRD-PARTY-NOTICES.txt
%{_docdir}/%{name}/
%{_bindir}/kde-rdp-setup
%{kde_rdp_libexecdir}/
%{_unitdir}/kde-rdp-broker.service
%{_unitdir}/kde-rdp-frontend.service
%{_unitdir}/kde-rdp-nvidia-devices.service
%{_sysusersdir}/kde-rdp.conf
%{_datadir}/xdg-desktop-portal/kde-rdp-portals.conf
%{_prefix}/lib/firewalld/services/kde-rdp.xml
%{_datadir}/selinux/packages/targeted/kde_rdp.cil
%dir %{_sysconfdir}/kde-rdp
%config(noreplace) %{_sysconfdir}/kde-rdp/frontend.conf
%config(noreplace) %{_sysconfdir}/kde-rdp/renderer.conf
%config(noreplace) %{_sysconfdir}/kde-rdp/broker.conf
%config(noreplace) %{_sysconfdir}/pam.d/kde-rdp
