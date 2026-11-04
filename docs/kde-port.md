# KDE port: design contract

KDE-RDP is a port of Niri-RDP (`../Niri-RDP`, read-only reference) that
replaces the patched headless Niri with stock KWin 6.8's virtual backend and
a Plasma shell. Everything else (IronRDP bridge, PAM/logind broker, audio,
microphone, clipboard, login form, NVENC, packaging) is reused.

## Names (already mechanically renamed in this tree)

| Niri-RDP | KDE-RDP |
| --- | --- |
| package `niri-remote-login` | `kde-remote-login` |
| user `niri-rdp`, group `niri-rdp-users` | `kde-rdp`, `kde-rdp-users` |
| `/run/niri-rdp`, `/etc/niri-rdp`, `/usr/lib/niri-rdp` | `/run/kde-rdp`, `/etc/kde-rdp`, `/usr/lib/kde-rdp` |
| PAM/logind service `niri-rdp` | `kde-rdp` |
| units `niri-rdp-*.service` | `kde-rdp-*.service` |
| env `NIRI_RDP_*` | `KDE_RDP_*` |

The listener stays on TCP **33991** (same as Niri-RDP). The two frontends
cannot run at the same time; the frontend unit declares
`Conflicts=niri-rdp-frontend.service`.

## Session chain

```
broker (PAM/logind, unchanged design)
 └─ dbus-run-session -- /usr/lib/kde-rdp/session-worker      (private bus)
     ├─ session-audio, session-microphone (private PipeWire, unchanged)
     └─ exec kwin_wayland --virtual --xwayland --no-lockscreen
              --socket wayland-0 --width W --height H
              --exit-with-session=/usr/lib/kde-rdp/session-plasma
                 └─ Plasma shell + services, portals (kde), autostart
```

* `XDG_RUNTIME_DIR` is the private desktop dir, so KWin's socket is
  `$priv/wayland-0`, exactly where the broker already connects.
* `XDG_CURRENT_DESKTOP=KDE`, `XDG_SESSION_DESKTOP=KDE`, `KDE_FULL_SESSION=true`,
  `KDE_SESSION_VERSION=6`. The host's systemd user units for Plasma
  (`plasma-*.service`) are **not** used or modified: the remote desktop must not
  touch the user manager shared with local logins.
* W×H: the client's requested desktop size when the broker knows it, else
  1920×1080. The virtual output name is whatever KWin reports (probe it; do not
  hard-code `HEADLESS-1`).

## Compositor protocols (KWin 6.8, verified present in a headless sandbox)

| Need | Niri-RDP | KDE-RDP |
| --- | --- | --- |
| frames + damage | zwlr_screencopy | `zkde_screencast_unstable_v1.stream_output` → PipeWire node, consumed in C (`bridge/src/screencast.c`) over a PipeWire fd passed by the broker |
| cursor | ext_image_copy_capture cursor session | PipeWire `SPA_META_Cursor` (cursor mode metadata) |
| keyboard | zwp_virtual_keyboard_v1 | `org_kde_kwin_fake_input.keyboard_key` |
| pointer | zwlr_virtual_pointer_v1 | `org_kde_kwin_fake_input` pointer requests |
| resize | zwlr_output_manager_v1 | `kde_output_management_v2` (see docs/kde-findings.md) |
| clipboard | zwlr_data_control_v1 | `ext_data_control_v1` |

KWin only grants `fake_input` and `zkde_screencast` to clients whose
executable is listed in a `.desktop` file with
`X-KDE-Wayland-Interfaces=` (as `/usr/share/applications/org.kde.krdpserver.desktop`
does). How this applies to the broker-connected, fd-passed socket is recorded
in docs/kde-findings.md.

## Broker → bridge fds

The broker already passes `wayland-0`, `audio-0`, `audio-in-0` over
SCM_RIGHTS. It additionally passes a connection to the session's PipeWire
socket (`$priv/pipewire-0`) so the bridge (running as `kde-rdp`) can
`pw_context_connect_fd` to the user's private PipeWire for screencast.

## Ownership while porting

* broker/, broker/config/, bridge/src/broker.rs (wire protocol), packaging/,
  tests for those: **session agent**.
* bridge/src/wayland/, bridge/src/screencast.c, bridge/src/clipboard.rs,
  bridge/build.rs, bridge/Cargo.toml: **bridge agent**.
* docs/kde-findings.md, lab/: **lab agent**, then shared.

## Audio baseline

The accepted audio baseline ([session audio](../bridge/README.md#session-audio)) applies unchanged (44.1 kHz stereo PCM first, the
earned drift skip, etc.). Do not touch audio code as part of the port.
