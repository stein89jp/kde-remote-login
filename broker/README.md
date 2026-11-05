# PAM/logind broker

This is an experimental privileged broker implementation, separate from the
unprivileged RDP frontend. Native tests use fake PAM and never create a real
login session. A real seatless session, boot without local login, and fatal
cleanup still require the installed disposable Arch test machine. Nothing in
this directory has been installed or configured on the working host.

Build with `make -C broker`; run `make -C broker check`. Dependencies are GCC or
Clang, Linux-PAM headers/library, and systemd headers/libsystemd. Stage with an
**absolute** DESTDIR, for example:

```sh
make -C broker DESTDIR="$PWD/artifacts/broker-stage" install
```

The coordinator's Arch package installs these artifacts; do not run the install
target with an empty DESTDIR on the working host. Package service enablement,
TLS credentials, account setup, upgrade and uninstall instructions are in the
main installation documentation.

## Ownership and policy

The root system service listens only on `/run/kde-rdp/broker.sock`, not TCP.
It requires the peer's kernel-reported UID to equal the dedicated `kde-rdp`
frontend account. The socket is root:kde-rdp mode 0660. Its parent is root-owned
mode 0755 so desktop UIDs and Bubblewrap can read and traverse the runtime
ancestors. Bubblewrap opens parent directories for reading when mounting
Flatpak's D-Bus proxies; mode 0711 prevents Flatpak applications from starting.
The desktops parent is also 0755; session records are 0700 and every desktop
runtime is 0700 owned by its account.

Requests contain only a Linux username and password. The frontend cannot choose
an executable, arguments, session environment, account UID, socket path or
output size. Accounts must have UID >= 1000, a login shell and membership in
`kde-rdp-users`. The frontend account and root are rejected. The final identity
returned by PAM is resolved and authorized; the originally requested identity
is not trusted after PAM. A nonblocking per-UID lock prevents concurrent broker
desktops. A retained broker desktop can be resumed after fresh PAM authentication,
account checks and group authorization; its own logind session is exempted from
the existing-session check. Other X11/Wayland sessions, including closing
sessions, reject login. The initial desktop should use a dedicated test account. A simultaneous
local login can race this check; the broker cannot lock GDM or an unrelated
login manager, and same-UID desktops are not an isolation boundary.

Each desktop has a separate privileged holder process. Its per-UID resume socket
is root-only and authenticates the root broker peer; the frontend cannot access
it directly. The holder retains the original PAM session while detached.
A resume request authenticates anew without opening another desktop session. The dedicated
`kde-rdp` PAM policy includes Arch's remote-login auth/account policy and
requires `pam_systemd`. Password-only PAM is supported; a second username or
secret prompt across the handle's lifetime is rejected, including OTP/MFA and
password-change conversations. Credentials are not logged or placed in argv or
persistent state; the broker erases its request buffer and disables core dumps.
PAM owns its internal credential allocations. Credential reading has a 10s
socket timeout; the holder catches a 30s alarm, and a parent 35s watchdog kills a
stalled startup even if a PAM module ignores interrupted I/O.

Before launch the broker checks logind's recorded UID, leader, service,
remote flag, Wayland type, absent seat and absent VT. It requires PAM's genuine
`/run/user/UID` to exist with correct ownership and permissions. It launches
only `/usr/bin/dbus-run-session -- /usr/lib/kde-rdp/session-worker` after
setting account groups and UID/GID. The environment is constructed afresh;
only PAM's validated runtime path and session ID are carried over, plus
`XDG_CURRENT_DESKTOP=KDE`, `XDG_SESSION_DESKTOP=KDE` and, when the frontend sent
one, the client's desktop size as `KDE_RDP_WIDTH`/`KDE_RDP_HEIGHT`. The worker
uses a private runtime directory for KWin and its applications and a private bus,
and starts `kwin_wayland --virtual --xwayland --no-lockscreen --socket wayland-0
--width W --height H --exit-with-session=/usr/lib/kde-rdp/session-plasma`
(W×H revalidated: 200–4096, even width, default 1920×1080). `session-plasma`
exports the KDE session environment (`KDE_FULL_SESSION=true`,
`KDE_SESSION_VERSION=6`) to the private bus, starts the portal helper and
launches Plasma; its single `start_plasma` function chooses `plasma_session`
(Plasma's non-systemd session) or falls back to kded6, the autostart helper and
`plasmashell`. `session-plasma` exports the whole environment to D-Bus
activation (otherwise kactivitymanagerd starts without the display and Plasma's
shell refuses to load) and owns `org.kde.KWinWrapper` on the private bus for the
session (otherwise `plasma_session` starts a nested `kwin_wayland_wrapper`).
The worker also puts `/usr/lib/kde-rdp/containers` first on `PATH`. Its
`podman`, `buildah`, `toolbox` and `docker` links run `session-containers`,
which hands only those tools the account's genuine `/run/user/UID` and systemd
user bus (and a matching rootless `DOCKER_HOST`), so rootless containers are
shared with the account's other logins instead of living in a second, empty
user namespace. distrobox keeps the desktop's environment and reaches podman
through the link, which rewrites `--env` values of `exec`, `run` and `create`
under the private runtime to `/run/host/...`, so container GUI and audio
applications use the remote desktop.
The worker prepends a session-only `XDG_CONFIG_DIRS` overlay with
`[Plugins] shakecursorEnabled[$i]=false` in `kwinrc`: KWin's shake-cursor effect
paints the cursor into screencast frames, and the immutable entry wins over the
user's kwinrc without writing to `~/.config`. The root-owned
`/etc/kde-rdp/renderer.conf` selects `auto` (default) or `hardware`; `software`
is refused because KWin's screencast needs GPU compositing, and the worker
fails clearly when no render node is accessible. GPU access uses the account's
existing device permissions; the broker does not
acquire DRM master or grant extra groups. It does not use `startplasma-wayland`,
import environment into the user manager, or start `graphical-session.target` or
the host's `plasma-*.service` units. The per-UID runtime and user manager still exist as
required by logind; this is not a separate Unix security boundary.

Before the compositor starts, a UID-owned helper reads the account's configured
input-method, locale, keyboard, and XDG configuration settings. Saved localed XKB
defaults reach KWin as `XKB_DEFAULT_*` when Plasma has no layout configured. After
the private display exists, `session-plasma` exports the application environment
to the private bus before any startup applications launch; `plasma_session` then
runs the usual configured XDG autostart entries. An explicitly configured Fcitx or IBus
starts before user applications, with private writable profile/transport state.
The remote display is never imported into the shared user manager.

The worker also starts the session's private audio helpers as the account.
`session-audio` owns the private PipeWire graph and serves playback on
`audio-0`. `session-microphone` is a client of that graph: it serves `audio-in-0`
and runs the **Remote microphone (RDP)** source (`kde-rdp-microphone`, the
default source). It has its own finite restart budget (two worker restarts, five
`pw-cat` attempts, reset after ten healthy minutes), separate from playback's;
nothing waits for it, and its failure never delays or ends the desktop.

## Wire contract v3

Use an AF_UNIX SOCK_STREAM socket. Send exactly:

| Field | Size | Value |
|---|---:|---|
| Magic | 4 bytes | `NRB3` |
| Username length | 2 bytes | Unsigned big endian, 1..64 |
| Password length | 2 bytes | Unsigned big endian, 1..512 |
| Desktop width | 2 bytes | Unsigned big endian; 0 with height 0 when unknown |
| Desktop height | 2 bytes | Unsigned big endian; 0 with width 0 when unknown |
| Address family | 1 byte | 0 unknown, 4 IPv4, 6 IPv6 |
| Client address | 0, 4 or 16 bytes | RDP client IP in network order; IPv4-mapped IPv6 is sent as IPv4 |
| Username | Length bytes | Linux ASCII account name (`[A-Za-z0-9_.-]`), no NUL |
| Password | Length bytes | UTF-8 password bytes, no NUL |

The broker sets the address as `PAM_RHOST` (left unset when unknown) and
records it in its journal audit entries. A known desktop size (both sides
non-zero) is clamped to 200–4096 with an even width and becomes the initial size
of KWin's virtual output; otherwise the worker uses 1920×1080. The login screen
sends its negotiated size; ClientInfo credentials are validated before the size
is negotiated and send 0×0. Version 2 (`NRB2`) and older requests are rejected:
the frontend and broker are upgraded together.

The response is 5 bytes: `NRB3` and one status byte. Status 0 carries **one to
four positional** SCM_RIGHTS descriptors, in this order:

1. a Wayland connection to KWin's `wayland-0` (always present);
2. a connection to the session's private PipeWire (`pipewire-0`, started by
   `session-audio`), for `pw_context_connect_fd` and KWin's screencast stream;
3. a raw stereo 48 kHz S16LE PCM capture of the private session's sink monitor
   (`audio-0`; the frontend resamples it to 44.1 kHz for the RDP client);
4. the remote microphone socket (`audio-in-0`, NRM1 framing), served by
   `session-microphone`.

Position alone identifies each descriptor: each is sent only together with all
earlier ones, so the reply is `[desktop, pipewire, audio, microphone]`,
`[desktop, pipewire, audio]`, `[desktop, pipewire]` or `[desktop]`. Playback and
the microphone are PipeWire clients, so neither is offered without PipeWire. All
but the desktop are ancillary: an unavailable helper never rejects an
authenticated login or resume, and a missing microphone never removes playback.
Each reconnect receives fresh descriptors. The frontend rejects five or more
descriptors. Each descriptor is connected by a short-lived fork of the broker
running as the account, so KWin and PipeWire see the account's credentials.
KWin 6.8 has no per-executable grant: its restricted interfaces (fake input,
screencast, data control) are hidden only from sandboxed clients (security
context or `app-flatpak-*`/`snap.*` units), so the connector must not run in one.
The connector exits before the frontend uses the PipeWire connection, which
PipeWire's default access module would then classify as restricted; the private
PipeWire written by `session-audio` therefore sets `access.socket` to
`unrestricted` for `pipewire-0` and `pipewire-0-manager` (both live in the 0700
private runtime). A single-descriptor reply leaves audio unavailable (also used
by transport fixtures). The PCM socket waits for one ASCII `A` byte before
starting capture; the frontend sends it after sound-format negotiation and
authenticated desktop binding. The microphone socket starts with an NRM1 `HELLO`
from the frontend; see the [bridge microphone contract](../bridge/README.md#microphone).

An older frontend rejects the three-descriptor reply, so the frontend and broker
must be upgraded together. The combined package does this; with split packages,
install both at once and restart both services.

| Status | Meaning |
|---:|---|
| 0 | Desktop opened (descriptors attached) |
| 1 | Generic authentication/account/policy rejection |
| 2 | Session startup failed, or the account's desktop is starting or ending: try again |
| 3 | Malformed/unauthorized request or connection capacity exceeded |
| 4 | The account's desktop is attached to another connection |
| 5 | The account has a local graphical session, including one still closing |

Statuses 2, 4 and 5 are sent only after PAM and the `kde-rdp-users` check
accepted the account, so they reveal nothing to a password guesser; every
earlier failure, including unknown users and non-members, is status 1.
No human-readable credentials/errors are returned. The frontend must use
`recvmsg`, preserve received FDs while finishing a fragmented 5-byte header,
validate ancillary metadata, and reject missing/excess descriptors. The broker
connects to the compositor, PCM and microphone sockets as its target UID before passing the FDs; the
frontend never connects to the user's local Wayland socket or filesystem.

Keep the control stream open while attached to the desktop. EOF detaches the
frontend and retains the desktop for a grace window (`grace_seconds` in
`/etc/kde-rdp/broker.conf`, default 45 seconds) measured from
broker-observed detachment. If no authorized reconnect completes before expiry,
the holder fully logs out and cleans up the desktop. A subsequent login starts
a fresh desktop after cleanup. An ordinary login within the grace window with
the same account performs fresh authentication and authorization, then receives
a new Wayland connection to that desktop. No client resume token or reconnect
cookie bypasses authentication. Unexpected additional bytes detach the frontend
and start the same grace window.
Compositor exit, explicit compositor logout, or broker shutdown tears down the
desktop. Close all frontend-held descriptors when detaching.

## Cleanup and failure behavior

Normal teardown kills the compositor process group (also after its leader has
exited), closes the PAM session, deletes credentials and ends PAM. The private
directory is traversed with no symlink following by an unprivileged cleanup
process, then its empty root is removed from the root-owned parent.

The parent maintains root-only records of each logind ID and terminates only
sessions whose PAM service is `kde-rdp`. A holder crash is also matched by
logind's recorded leader PID, covering death between PAM registration and record
creation. ExecStopPost scans records and service-owned logind sessions after a
broker crash. It never terminates GNOME/GDM/GRD or unrelated sessions.
After `TerminateSession` the broker also calls `KillSession(id, "all", signal)`:
processes that left the compositor's process group (for example podman's
rootless pause process) survive a Plasma logout, logind then keeps the session
`closing` with the default `KillUserProcesses=no`, and `TerminateSession` does
not end a session that is already closing. While logind still lists the
session, its record is retained and retried every 5s: the reap after a logout
sends SIGTERM, and retries send SIGKILL once that SIGTERM is 2s old (the
record's mtime). A service stop and ExecStopPost send SIGTERM, wait at most 2s
for the sessions to disappear, then send SIGKILL, inside `TimeoutStopSec`. A
session logind reports gone counts as done. Transient logind failures also
retain records for 5s retries; an unrecorded crashed holder's record is
reconstructed from logind before termination. Directories remain until the
session's termination is observed on a later retry. When counting the account's
graphical sessions, a session that the user's logind list still names but logind
no longer knows (`ENXIO`/`ENOENT`) is skipped; closing sessions still count.
Real logind TerminateSession and descendant cgroup cleanup need target-machine
validation; native process-group tests are not proof of cgroup cleanup. Programs
that deliberately start persistent user-manager units need a separate lifetime
audit. The test default launches only a terminal in the dedicated session.

## Manual broker checks on the disposable Arch target

After installing the project package and configuring the dedicated test
account/service, invoke this repository's client as the frontend service UID:

```sh
sudo -u kde-rdp python broker/tools/client.py kde-rdp-test --expect reject
sudo -u kde-rdp python broker/tools/client.py kde-rdp-test --expect success --seconds 30
```

The first invocation should use an incorrect password and return status 1;
repeat with a denied account and expired/locked test account. Use disposable
credentials: this Python smoke client cannot reliably erase immutable password
copies and is not the production frontend.

While the success client holds its control socket, use `loginctl` on the target
to inspect the new session's User, Leader, Service, Type, Remote, Seat and VTNr.
Expect the account UID, `kde-rdp`, `wayland`, Remote=yes, no seat and VTNr=0.
Inspect its process cgroup and verify the compositor/app share that session
scope; no compositor opens a physical DRM card or VT. After the control socket
closes, verify the same session, compositor/app/bus processes and desktop remain.
Reconnect and check the logind session ID, compositor PID and open application
state are unchanged. Reject a wrong password and revoked group membership on
resume without creating another session. Then log out of the compositor and
verify the session and compositor/app/bus processes disappear and
`/run/kde-rdp/desktops` and `/run/kde-rdp/sessions` contain no session resources.
An unrelated SSH session or existing user manager can legitimately keep
`/run/user/UID` alive; it must not be deleted manually by the broker.

Repeat with missing/failing compositor executable in the disposable VM, a
compositor SIGKILL, holder SIGKILL, broker SIGKILL and a boot with no local login.
Watch for no stale session scope, child process or private runtime. Then use the
packaged frontend and ordinary Windows mstsc directly at `HOST:33991`. The broker
prepares the TLS identity on first start before the frontend loads credentials.
Clients omitting a password see the frontend's restricted login screen; PAM still
authorizes the account before any desktop is started. These
checks and physical-monitor-off testing are pending, tracked in CHECKPOINT.md.
