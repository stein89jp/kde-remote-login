#!/bin/bash
# Install KDE-RDP on Bazzite (or another Fedora Atomic KDE desktop) with one
# command, from a checkout, as your normal user (it uses sudo when needed):
#
#   ./packaging/bazzite/install.sh
#
# It checks the host, builds the RPM in a podman container that matches the
# host's Fedora release and FFmpeg (nothing is added to the host for the
# build), layers it with rpm-ostree and applies it live, authorizes your
# account, turns off an autologin that would block it, enables NVIDIA encoding
# when an NVIDIA GPU is present, opens the firewall if the zone blocks the
# port, and starts the service. Run it again to update from a newer checkout.
# Details: docs/install-bazzite.md.
set -euo pipefail

PACKAGE=kde-remote-login
MIN_KWIN=6.8
PORT=33991
SERVICE_STATE_DIR=/etc/kde-rdp
NVIDIA_DROP_IN=/etc/systemd/system/kde-rdp-frontend.service.d/nvidia.conf
# Lets tests point the display-manager lookups at a scratch tree.
CONFIG_ROOT=${KDE_RDP_CONFIG_ROOT:-}
AUTOLOGIN_DROP_IN=zz-kde-rdp-no-autologin.conf
BUILD_CACHE_VOLUME=kde-rdp-build-cache

here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd -P)
repo=$(cd "$here/../.." && pwd -P)
out=$here/out

say() { printf '\n==> %s\n' "$*"; }
info() { printf '    %s\n' "$*"; }
warn() { printf 'warning: %s\n' "$*" >&2; }
die() { printf 'error: %s\n' "$*" >&2; exit 1; }

usage() {
    cat <<EOF
Usage: packaging/bazzite/install.sh [options]

Builds and installs KDE-RDP on Bazzite / Fedora Atomic KDE, then starts it.
Run it again from an updated checkout to update.

  --user NAME          authorize this existing account (default: $USER)
  --create-user NAME   create a separate account for remote logins instead
  --keep-autologin     do not turn off autologin for the authorized account
  --no-firewall        leave firewalld alone
  --no-nvidia          do not enable NVIDIA (NVENC) encoding
  --skip-tests         skip the package's test suites during the build
  --build-only         only build the RPM (into packaging/bazzite/out/rpms)
  --finish             skip the build; only authorize, configure and start
                       (after a reboot that finished the rpm-ostree layering)
  -y, --yes            answer yes to questions
  -h, --help           show this help
EOF
}

# ---------------------------------------------------------------------------
# Pure helpers (tests/test_install_layout.py sources this file to test them).

# version_at_least VERSION MINIMUM: true when VERSION >= MINIMUM.
version_at_least() {
    [ "$(printf '%s\n%s\n' "$2" "$1" | sort -V | head -n 1)" = "$2" ]
}

# port_listed PORT PROTOCOL ENTRY...: true when one firewalld port entry
# (N/proto or N-M/proto, as --list-ports prints them) covers PORT/PROTOCOL.
port_listed() {
    local port=$1 protocol=$2 entry range low high
    shift 2
    for entry in "$@"; do
        [ "${entry#*/}" = "$protocol" ] || continue
        range=${entry%/*}
        low=${range%-*}
        high=${range#*-}
        case $low$high in *[!0-9]*|'') continue ;; esac
        if [ "$port" -ge "$low" ] && [ "$port" -le "$high" ]; then
            return 0
        fi
    done
    return 1
}

# display_manager_configs NAME: the config files plasmalogin or SDDM reads, in
# order; later files override earlier ones (vendor conf.d, /etc conf.d, then
# the main /etc file).
display_manager_configs() {
    local name=$1 vendor file
    case $name in
        plasmalogin) vendor=$CONFIG_ROOT/usr/lib/plasmalogin/plasmalogin.conf.d ;;
        sddm) vendor=$CONFIG_ROOT/usr/lib/sddm/sddm.conf.d ;;
        *) return 1 ;;
    esac
    for file in "$vendor"/*.conf "$CONFIG_ROOT/etc/$name.conf.d"/*.conf "$CONFIG_ROOT/etc/$name.conf"; do
        [ -f "$file" ] && printf '%s\n' "$file"
    done
    return 0
}

# autologin_setting FILE...: the effective [Autologin] User= value and the
# file that sets it last, as "USER<TAB>FILE" (nothing when never set).
autologin_setting() {
    awk '
        /^[[:space:]]*\[/ { section = $0; gsub(/[[:space:]\[\]]/, "", section); next }
        section == "Autologin" && /^[[:space:]]*User[[:space:]]*=/ {
            value = $0; sub(/^[^=]*=[[:space:]]*/, "", value); sub(/[[:space:]]+$/, "", value)
            user = value; file = FILENAME; found = 1
        }
        END { if (found) printf "%s\t%s\n", user, file }
    ' "$@" </dev/null
}

# ---------------------------------------------------------------------------

confirm() {
    local answer
    [ "$assume_yes" = 1 ] && return 0
    [ -t 0 ] || return 1
    read -r -p "$1 [Y/n] " answer
    case $answer in ''|[Yy]|[Yy][Ee][Ss]) return 0 ;; *) return 1 ;; esac
}

# session_values PROPERTY: PROPERTY of every logind session, one per line.
# Collected in full: never pipe loginctl into grep -q under pipefail, where an
# early grep exit reads as "not found".
session_values() {
    local session sessions
    sessions=$(loginctl list-sessions --no-legend 2>/dev/null | awk '{print $1}') || :
    for session in $sessions; do
        loginctl show-session "$session" -p "$1" --value 2>/dev/null || :
    done
}

# has_local_desktop USER: true while USER has a local (not KDE-RDP) graphical
# session, which makes the broker refuse that account's remote login.
has_local_desktop() {
    local session sessions
    sessions=$(loginctl list-sessions --no-legend 2>/dev/null | awk -v user="$1" '$3 == user {print $1}') || :
    for session in $sessions; do
        case "$(loginctl show-session "$session" -p Type --value 2>/dev/null)" in
            wayland|x11) ;;
            *) continue ;;
        esac
        [ "$(loginctl show-session "$session" -p Service --value 2>/dev/null)" != kde-rdp ] && return 0
    done
    return 1
}

in_remote_session() {
    [ -n "${KDE_RDP_RUNTIME_DIR:-}" ] && return 0
    [ -n "${XDG_SESSION_ID:-}" ] &&
        [ "$(loginctl show-session "$XDG_SESSION_ID" -p Service --value 2>/dev/null)" = kde-rdp ]
}

# Prints "booted_base pending pending_base local_nevra repo_requested" from
# rpm-ostree's JSON status, describing the newest deployment.
ostree_state() {
    rpm-ostree status --json | python3 -c '
import json, sys
deployments = json.load(sys.stdin)["deployments"]
booted = next(d for d in deployments if d.get("booted"))
newest = deployments[0]
def base(d):
    return d.get("base-checksum") or d.get("checksum") or "-"
local = [p for p in newest.get("requested-local-packages", []) if p.startswith("kde-remote-login-")]
repo = "kde-remote-login" in newest.get("requested-packages", [])
print(base(booted), "no" if newest.get("booted") else "yes", base(newest),
      local[0] if local else "-", "yes" if repo else "no")
'
}

preflight() {
    [ "$(id -u)" -ne 0 ] || die "run this as your normal user; it uses sudo itself"
    if [ -e /run/.containerenv ] || [ -e /.dockerenv ]; then
        die "run this on the Bazzite host itself, not inside toolbox or distrobox"
    fi
    [ -e /run/ostree-booted ] ||
        die "this installer is for Bazzite / Fedora Atomic (rpm-ostree); on Arch use packaging/arch/kde-rdp"
    # shellcheck disable=SC1091
    . /etc/os-release
    case " ${ID:-} ${ID_LIKE:-} " in *" fedora "*) ;; *) die "unsupported system: ${PRETTY_NAME:-unknown}" ;; esac
    fedora_version=${VERSION_ID:?}
    command -v kwin_wayland >/dev/null ||
        die "KDE Plasma (kwin_wayland) is not installed: use a KDE image (bazzite, bazzite-nvidia, Kinoite)"
    kwin_version=$(kwin_wayland --version 2>/dev/null | awk '{print $2}')
    version_at_least "${kwin_version:-0}" "$MIN_KWIN" ||
        die "KWin ${kwin_version:-?} is too old (need $MIN_KWIN or newer): update the system (ujust update), reboot, and run this again"
    for command in podman rpm-ostree sudo python3; do
        command -v "$command" >/dev/null || die "$command is missing"
    done
    if [ "$create_user" = 0 ]; then
        getent passwd "$account" >/dev/null || die "account $account does not exist (use --create-user $account)"
    elif getent passwd "$account" >/dev/null; then
        die "account $account already exists; use --user $account"
    fi
    local node found=
    for node in /dev/dri/renderD*; do [ -c "$node" ] && found=1; done
    [ -n "$found" ] || warn "no GPU render node (/dev/dri/renderD*): remote desktops need GPU compositing and will not start"
    version=$(sed -n 's/^pkgver=//p' "$repo/packaging/arch/kde-rdp/PKGBUILD")
    [ -n "$version" ] && [ "$version" = "$(sed -n 's/^Version:[[:space:]]*//p' "$here/kde-remote-login.spec")" ] ||
        die "packaging/bazzite/kde-remote-login.spec and the Arch PKGBUILD disagree on the version"
}

check_staged_update() {
    local state booted_base pending pending_base local_nevra repo_requested
    state=$(ostree_state) || die "cannot read the rpm-ostree status (is an update running? try again shortly)"
    read -r booted_base pending pending_base local_nevra repo_requested <<<"$state"
    if [ "$pending" = yes ] && [ "$pending_base" != "$booted_base" ]; then
        die "a system update is staged and waits for a reboot. Reboot, then run this command again."
    fi
    layered_local=$local_nevra
    [ "$repo_requested" = no ] || die "$PACKAGE is already requested from a repository; remove it with: sudo rpm-ostree uninstall $PACKAGE"
}

# package_field PACKAGE TAG: one rpm tag of the native-architecture package
# (Bazzite also installs i686 multilib copies of the FFmpeg libraries).
package_field() {
    local value
    # rpm reports a missing package on stdout; print nothing for it.
    value=$(rpm -q --qf "%{$2}\n" "$1.$(uname -m)" 2>/dev/null) || return 0
    printf '%s\n' "${value%%$'\n'*}"
}

ffmpeg_source() {
    if [ "$(package_field libavcodec VENDOR)" = negativo17.org ]; then
        ffmpeg_from=negativo17
        ffmpeg_version=$(package_field libavcodec VERSION)
    elif [[ $(package_field ffmpeg-libs VENDOR) == *"RPM Fusion"* ]]; then
        ffmpeg_from=rpmfusion
        ffmpeg_version=$(package_field ffmpeg-libs VERSION)
    elif [ -n "$(package_field libavcodec-free VERSION)" ]; then
        ffmpeg_from=fedora
        ffmpeg_version=$(package_field libavcodec-free VERSION)
    else
        die "cannot tell which FFmpeg this system uses (negativo17, RPM Fusion or Fedora's ffmpeg-free)"
    fi
}

build_rpm() {
    ffmpeg_source
    say "Building $PACKAGE $version for Fedora $fedora_version (FFmpeg $ffmpeg_version from $ffmpeg_from) in a container"
    info "The first build downloads a Fedora image and Rust crates and takes a while."
    rm -rf "$out/context" "$out/in"
    mkdir -p "$out/context/repos" "$out/in" "$out/rpms"
    cp "$here/build-container/Containerfile" "$here/build-container/build-rpm.sh" "$out/context/"
    if [ "$ffmpeg_from" = negativo17 ]; then
        local repo_file=/etc/yum.repos.d/negativo17-fedora-multimedia.repo
        if [ -f "$repo_file" ]; then
            # Bazzite ships the file disabled; enable only its main repository.
            awk '/^\[/ { main = ($0 == "[fedora-multimedia]") } main && /^enabled=/ { $0 = "enabled=1" } { print }' \
                "$repo_file" > "$out/context/repos/negativo17-fedora-multimedia.repo"
        else
            # shellcheck disable=SC2016 # dnf expands $releasever and $basearch.
            printf '%s\n' '[fedora-multimedia]' 'name=negativo17 - Multimedia' \
                'baseurl=https://negativo17.org/repos/multimedia/fedora-$releasever/$basearch/' \
                'enabled=1' 'gpgcheck=1' 'gpgkey=https://negativo17.org/repos/RPM-GPG-KEY-slaanesh' \
                > "$out/context/repos/negativo17-fedora-multimedia.repo"
        fi
    fi
    local image=localhost/kde-rdp-build:f$fedora_version-$ffmpeg_from-$ffmpeg_version
    podman build --pull=missing -t "$image" \
        --build-arg FEDORA_VERSION="$fedora_version" --build-arg FFMPEG_SOURCE="$ffmpeg_from" \
        "$out/context" || die "building the container image failed (network or repository problem?); see above"
    tar -C "$repo" -czf "$out/in/$PACKAGE-$version.tar.gz" \
        --exclude=bridge/target --exclude=bridge/.cargo-cache \
        --exclude='packaging/arch/*/src' --exclude='packaging/arch/*/pkg' \
        --exclude='packaging/arch/*/repo-*' --exclude='packaging/arch/*/*.tar.*' \
        --exclude=packaging/bazzite/out --exclude=__pycache__ --exclude='*.pyc' \
        --exclude=broker/kde-rdp-broker --exclude=broker/broker-tests --exclude=broker/broker-test-root \
        --transform "s,^,$PACKAGE-$version/," \
        LICENSE README.md bridge broker docs packaging scripts tests
    # Each local build gets a newer release, so rpm-ostree sees an update.
    local release
    release=1.$(date -u +%Y%m%d%H%M%S)
    rm -f "$out"/rpms/*.rpm
    # keep-id: build and run the test suites as you, not as root (makepkg
    # refuses root too), and leave the RPM owned by you.
    podman run --rm --userns=keep-id --security-opt label=disable \
        -v "$out/in:/in:ro" -v "$out/rpms:/out" -v "$BUILD_CACHE_VOLUME:/cache" \
        -e KDE_RDP_RELEASE="$release" -e KDE_RDP_CHECK="$run_checks" "$image" ||
        die "the RPM build failed; see the messages above"
    rpm_file=$(compgen -G "$out/rpms/$PACKAGE-$version-$release.*.rpm" | head -n 1 || :)
    [ -n "$rpm_file" ] && [ -f "$rpm_file" ] || die "the build produced no RPM"
    # Every shared library the binaries link must exist on this host, or
    # rpm-ostree would refuse the layer (or a later update would stall).
    local requirement missing=
    while read -r requirement; do
        rpm -q --whatprovides "$requirement" >/dev/null 2>&1 || missing="$missing $requirement"
    done < <(rpm -qp --requires "$rpm_file" 2>/dev/null | awk '$1 ~ /^lib.*\.so/ {print $1}')
    [ -z "$missing" ] ||
        die "the package needs libraries this system lacks:$missing. Update the system (ujust update), reboot, and run this again."
    info "Built $rpm_file"
}

layer_rpm() {
    say "Installing $PACKAGE with rpm-ostree"
    if [ "$layered_local" != - ]; then
        info "Replacing $layered_local"
        sudo rpm-ostree install --uninstall="$layered_local" "$rpm_file"
        replacing=1
    else
        sudo rpm-ostree install "$rpm_file"
        replacing=0
    fi
    # --allow-replacement: an update replaces files, and so does a reinstall
    # after an uninstall that is still waiting for its reboot.
    if ! sudo rpm-ostree apply-live --allow-replacement; then
        say "Reboot to finish"
        info "rpm-ostree could not apply the package to the running system."
        info "Reboot, then finish with: $0 --finish"
        exit 0
    fi
    sudo systemctl daemon-reload
}

# The package's %post put the kde_rdp SELinux module (it lets the broker start
# desktops in the user's login context) into the new deployment's policy
# store. Rebuild and load the running policy so it applies without a reboot.
activate_selinux() {
    command -v selinuxenabled >/dev/null && selinuxenabled || return 0
    say "Activating the KDE-RDP SELinux module"
    if ! grep -qx kde_rdp <<<"$(sudo semodule -l)"; then
        sudo semodule -X 200 -i "/usr/share/selinux/packages/targeted/kde_rdp.cil"
    fi
    sudo semodule -B
    grep -qx kde_rdp <<<"$(sudo semodule -l)" || die "the kde_rdp SELinux module did not load; see: sudo semodule -l"
}

authorize_account() {
    say "Authorizing $account for remote login"
    command -v kde-rdp-setup >/dev/null || die "$PACKAGE is not active yet; reboot first, then run: $0 --finish"
    if [ "$create_user" = 1 ]; then
        # Interactive: passwd asks for the new account's password.
        sudo kde-rdp-setup --create-user "$account"
    else
        sudo kde-rdp-setup --user "$account" >/dev/null
    fi
    # The broker checks the merged NSS view (/etc/group and the image's group).
    grep -qx kde-rdp-users <<<"$(id -nG "$account" | tr ' ' '\n')" ||
        die "$account was not added to kde-rdp-users; check /etc/group and: getent group kde-rdp-users"
    info "$account is a member of kde-rdp-users."
}

disable_autologin() {
    local manager configs setting user file
    for manager in plasmalogin sddm; do
        mapfile -t configs < <(display_manager_configs "$manager")
        [ "${#configs[@]}" -gt 0 ] || continue
        setting=$(autologin_setting "${configs[@]}" 2>/dev/null) || setting=
        user=${setting%%$'\t'*}
        file=${setting#*$'\t'}
        [ -n "$setting" ] && [ "$user" = "$account" ] || continue
        say "Autologin"
        info "$manager logs $account in automatically ($file)."
        info "The broker refuses remote logins while that account has a local desktop,"
        info "so after every boot you would first have to log out at the machine."
        if [ "$keep_autologin" = 1 ] || ! confirm "Turn off autologin for $account?"; then
            info "Autologin kept. Log out locally before connecting remotely."
            continue
        fi
        if [ "$file" = "$CONFIG_ROOT/etc/$manager.conf" ]; then
            # The main file overrides every drop-in: comment its setting out.
            sudo cp -p "$file" "$file.kde-rdp-backup"
            sudo sed -i '/^[[:space:]]*\[Autologin\]/,/^[[:space:]]*\[/ s/^\([[:space:]]*User[[:space:]]*=.*\)$/# Disabled by KDE-RDP (backup: '"${file##*/}"'.kde-rdp-backup): \1/' "$file"
        fi
        printf '%s\n' '# Written by the KDE-RDP installer: autologin would keep the authorized' \
            '# account busy, and the broker refuses remote logins while it has a local' \
            '# desktop. packaging/bazzite/uninstall.sh removes this file.' \
            '[Autologin]' 'User=' 'Relogin=false' |
            sudo install -D -m 644 /dev/stdin "$CONFIG_ROOT/etc/$manager.conf.d/$AUTOLOGIN_DROP_IN"
        info "Autologin is off (applies from the next boot)."
    done
}

enable_nvidia() {
    [ "$nvidia" = 1 ] || return 0
    compgen -G '/proc/driver/nvidia/gpus/*/information' >/dev/null || return 0
    say "NVIDIA GPU found: enabling NVENC hardware encoding"
    sudo install -D -m 644 "/usr/share/doc/$PACKAGE/nvidia-frontend.conf" "$NVIDIA_DROP_IN"
    sudo systemctl daemon-reload
}

open_firewall() {
    [ "$firewall" = 1 ] || return 0
    systemctl is-active --quiet firewalld || return 0
    local zone zones services target reloaded=0
    zones=$(sudo firewall-cmd --get-active-zones | awk '/^[^[:space:]]/ {print $1}')
    [ -n "$zones" ] || zones=$(sudo firewall-cmd --get-default-zone)
    for zone in $zones; do
        target=$(sudo firewall-cmd --permanent --zone="$zone" --get-target 2>/dev/null || echo default)
        [ "$target" = ACCEPT ] && continue
        services=$(sudo firewall-cmd --zone="$zone" --list-services)
        case " $services " in *" kde-rdp "*) continue ;; esac
        # shellcheck disable=SC2046
        port_listed "$PORT" tcp $(sudo firewall-cmd --zone="$zone" --list-ports) && continue
        if [ "$reloaded" = 0 ]; then
            # firewalld learns new service files (ours came with the package) on reload.
            sudo firewall-cmd --reload >/dev/null
            reloaded=1
        fi
        say "Firewall"
        sudo firewall-cmd --permanent --zone="$zone" --add-service=kde-rdp >/dev/null
        sudo firewall-cmd --zone="$zone" --add-service=kde-rdp >/dev/null
        info "Opened TCP $PORT (service kde-rdp) in firewalld zone $zone."
    done
}

start_service() {
    say "Starting the KDE-RDP service"
    local was_active=0
    systemctl is-active --quiet kde-rdp-frontend.service && was_active=1
    sudo systemctl enable --now kde-rdp-frontend.service
    if [ "$was_active" = 1 ] && [ "$replacing" = 1 ]; then
        if in_remote_session; then
            info "The new version starts after a reboot, or when you restart the services"
            info "from a local or SSH terminal: sudo systemctl restart kde-rdp-broker kde-rdp-frontend"
            info "(that ends connected remote desktops)."
        elif grep -qx kde-rdp <<<"$(session_values Service)" &&
                ! confirm "Restart the services now? This ends connected remote desktops."; then
            info "Restart later to use the new version: sudo systemctl restart kde-rdp-broker kde-rdp-frontend"
        else
            sudo systemctl restart kde-rdp-broker.service kde-rdp-frontend.service
        fi
    fi
    local _
    for _ in $(seq 1 30); do
        [ -n "$(ss -Hltn "sport = :$PORT" 2>/dev/null)" ] && break
        sleep 1
    done
    if [ -z "$(ss -Hltn "sport = :$PORT" 2>/dev/null)" ]; then
        sudo systemctl --no-pager status kde-rdp-broker.service kde-rdp-frontend.service || :
        die "the frontend is not listening on TCP $PORT; see: journalctl -b -u kde-rdp-broker -u kde-rdp-frontend"
    fi
}

summary() {
    local fingerprint addresses
    fingerprint=$(sudo openssl x509 -in "$SERVICE_STATE_DIR/tls/cert.pem" -noout -fingerprint -sha256 2>/dev/null |
                  sed 's/^[^=]*=//')
    addresses=$(ip -4 -o addr show scope global 2>/dev/null | awk '{sub(/\/.*/, "", $4); print $4}' | paste -sd ' ' -)
    say "KDE-RDP is running"
    info "Connect with Windows Remote Desktop (mstsc.exe) to:"
    for address in ${addresses:-HOST}; do info "    $address:$PORT"; done
    info "Sign in as: $account"
    [ -n "$fingerprint" ] && info "Certificate SHA-256 fingerprint: $fingerprint"
    if has_local_desktop "$account"; then
        info ""
        info "Log out of $account's local desktop before connecting: the remote login is"
        info "refused while the same account has a local graphical session."
    fi
    info ""
    info "Update: run this installer again from an updated checkout."
    info "Uninstall: $repo/packaging/bazzite/uninstall.sh"
}

main() {
    account=$USER
    create_user=0
    keep_autologin=0
    firewall=1
    nvidia=1
    run_checks=1
    build_only=0
    finish=0
    assume_yes=0
    replacing=0
    while [ $# -gt 0 ]; do
        case $1 in
            --user) [ $# -ge 2 ] || die "--user needs a name"; account=$2; shift ;;
            --user=*) account=${1#*=} ;;
            --create-user) [ $# -ge 2 ] || die "--create-user needs a name"; account=$2; create_user=1; shift ;;
            --create-user=*) account=${1#*=}; create_user=1 ;;
            --keep-autologin) keep_autologin=1 ;;
            --no-firewall) firewall=0 ;;
            --no-nvidia) nvidia=0 ;;
            --skip-tests) run_checks=0 ;;
            --build-only) build_only=1 ;;
            --finish) finish=1 ;;
            -y|--yes) assume_yes=1 ;;
            -h|--help) usage; exit 0 ;;
            *) die "unknown option: $1 (see --help)" ;;
        esac
        shift
    done
    preflight
    if [ "$finish" = 0 ]; then
        [ "$build_only" = 1 ] || check_staged_update
        build_rpm
        [ "$build_only" = 0 ] || exit 0
        # Ask for the sudo password once, after the build. (Not sudo -v: that
        # prompts even with a NOPASSWD rule when another sudoers entry lacks it.)
        sudo true
        layer_rpm
    else
        sudo true
    fi
    activate_selinux
    authorize_account
    disable_autologin
    enable_nvidia
    open_firewall
    start_service
    summary
}

if [ "${BASH_SOURCE[0]}" = "$0" ]; then
    main "$@"
fi
