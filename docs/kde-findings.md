# KWin 6.8.2 lab findings

Lab probes for the port contract in [kde-port.md](kde-port.md). Everything
below was measured on 2026-11-03 on the dev host (Arch, kwin 6.8.2,
plasma-workspace 6.8.2, pipewire 1.6.10, wireplumber 0.5.18, NVIDIA RTX 4070 Ti
with nvidia 615.78.08) inside disposable bwrap sandboxes, then checked against
the KWin source at tag v6.8.2 (f130ab33c8e6f3b9d728aade87836086ae9a8ed4) and
plasma-workspace v6.8.2 `startkde/`.

## Reproduce

```sh
lab/kwin/build.sh                                  # probes -> lab/kwin/out/bin (OUT=... to override)
lab/kwin/run-sandbox.sh [--gpu] [--xwayland|--wrapper] [--size WxH] SCENARIO
# logs: lab/kwin/out/logs/SCENARIO[-gpu]/
```

`run-sandbox.sh` is the bwrap pattern from the brief plus `--symlink usr/bin /bin`
(Xwayland runs xkbcomp through `/bin/sh`) and `--tmpfs /var`. Inside, `inner.sh`
→ `dbus-run-session` → `session.sh` starts a private `pipewire` + `wireplumber`
(`PIPEWIRE_RUNTIME_DIR=XDG_RUNTIME_DIR=/tmp/rt`) and then
`kwin_wayland --virtual --socket wayland-0 --no-lockscreen --exit-with-session=scenarios/SCENARIO.sh`.
`--gpu` adds `--dev-bind` for `/dev/nvidia*` and `/dev/dri/renderD*` (never
`card*`) and `--ro-bind /sys /sys`, and drops `LIBGL_ALWAYS_SOFTWARE`.

| Scenario | Question | Command |
| --- | --- | --- |
| `auth` | 1 | `run-sandbox.sh --gpu auth` (and without `--gpu`) |
| `input` | 2 | `run-sandbox.sh --gpu input` |
| `screencast` | 3, 6 | `run-sandbox.sh --gpu screencast`; consumer args from `out/sc-extra` (e.g. `--dmabuf --mods 0x300000000e08014,...`), launcher from `out/sc-handoff` (e.g. `fd-handoff --pipewire --linger 3 --`), `touch out/pw-unrestricted` for the access.socket config |
| `resize`, `resize-stream` | 4 | `run-sandbox.sh --gpu resize`, `run-sandbox.sh --gpu resize-stream` |
| `plasma` | 5 | `TIMEOUT=200 run-sandbox.sh --gpu --xwayland plasma` |

Probes (`lab/kwin/*.c`): `probe-globals` (list + bind + use restricted globals),
`fd-handoff` (forked connector connects `wayland-0` [+ `pipewire-0`], sends
the fds over SCM_RIGHTS, exits and is reaped; then the receiver is exec'd with
`WAYLAND_SOCKET` / `KDE_RDP_PW_FD`), `target-window` (fullscreen xdg_toplevel
that logs wl_pointer/wl_keyboard), `fake-input`, `screencast` (zkde stream →
libpipewire consumer over `pw_context_connect_fd`, PNG writer),
`output-probe` (kde_output_device_v2 dump, custom-mode resize),
`plasma-session-holder.py` (prototype `session-plasma`). Evidence images:
`lab/kwin/evidence/`. Protocol XML provenance: `bridge/protocols/README.md`.

## 1. Authorization: no .desktop grant exists in 6.8.2

**Result:** a plain client, the fd-handoff receiver (its connector already
exited and reaped), and the same client with an `XDG_DATA_DIRS` `.desktop`
grant all behave the same way. All three see `org_kde_kwin_fake_input` v6 and
`zkde_screencast_unstable_v1` v6, and all three can use both. The fake-input
events reach a window (section 2), and the screencast creates a PipeWire node
on the GPU path.

**Source (`src/wayland_server.cpp`, `KWinDisplay::allowInterface`):** the
restricted set (`org_kde_plasma_window_management`, `org_kde_kwin_fake_input`,
`zkde_screencast_unstable_v1`, `org_kde_plasma_activation_feedback`,
`kde_lockscreen_overlay_v1`, `wp_security_context_manager_v1`,
`ext_data_control_manager_v1`) is hidden **only** from clients where
`ClientConnection::isSandboxed()` is true. That flag is set in two cases:

* the client connected through a `wp_security_context_v1` listener
  (`setSecurityContextAppId`), or
* at client creation, the pid from `wl_client_get_credentials` (SO_PEERCRED of
  the connecting socket, so the **connector's** pid) is in the systemd user
  `app.slice` with a unit named `app-flatpak-*` or `snap.*`
  (`sd_pid_get_user_slice/unit`, compiled in with HAVE_SYSTEMD).

There is no `X-KDE-Wayland-Interfaces` / `.desktop` / `/proc/pid/exe` check
anywhere in KWin 6.8.2. Only `tests/` mentions the key. `executablePath` is
read from `/proc/<connector pid>/exe` once, at connect time, and is used only by
the screencast plugin. That plugin hides the windows of the stream client's pid
from the stream, except for `xdg-desktop-portal-kde`/`plasmashell`.
`fake_input.authenticate` is `// TODO: make secure` and always sets
authenticated. KWin does not check the uid of a client, and neither does
libwayland. The lab cannot change uid without root, so the uid case was not run.

**Production recommendation:** keep the broker pattern as is: a root-forked
child running as the session user connects to `$priv/wayland-0`, passes the fd,
and exits. No `.desktop` file needs installing. Do not run that connector in an
`app-flatpak-*`/`snap.*` unit, and do not route the bridge through a
security-context listener. Security note: any process of the session user can
inject input and capture the screen, which is the same as on a normal Plasma
session. One harmless quirk: KWin keeps the dead connector's pid for the
window-hiding rule, so if that pid is reused by a desktop app, that app's
windows would be missing from the stream. The risk is very small.

### PipeWire side of the handoff (blocking until fixed)

On a default PipeWire config, a connection whose connector has already exited
(`fd-handoff --pipewire`) is classified by `module-access` as
`pipewire.access = "flatpak"`, which is restricted. The consumer then cannot see
KWin's node, and the stream fails with `no target node available`. Three
variants were checked in `pw-dump`:

| Variant | access | stream |
| --- | --- | --- |
| connector exits before the receiver connects (default config) | `flatpak` | fails |
| connector stays alive 3 s (`--linger 3`) | `unrestricted` | works |
| connector exits, private PipeWire with `access.socket = { pipewire-0 = "unrestricted", pipewire-0-manager = "unrestricted" }` | `unrestricted` | works |

Recommendation: `session-audio` already writes its own `PIPEWIRE_CONFIG_DIR`.
Set `access.socket` there, in the `libpipewire-module-access` args (verified).
The socket sits in the 0700 private runtime dir, so only the session user and
root can reach it. As an alternative, keep the broker's connector alive until
the bridge acknowledges that its `pw_core` sync completed.

Also: KWin's node `object.serial` is not its id. Use the zkde v6 `serial`
event as `PW_KEY_TARGET_OBJECT`. Setting `target.object` to the node id breaks
linking (`no target node available`).

## 2. Fake input works on the virtual backend

Protocol version **6**. Evidence is in `out/logs/input-gpu/target.out`: a
fullscreen `target-window` and an fd-handoff `fake-input`.

* Without `authenticate`, every request is silently dropped and the target
  receives nothing. With it, every request is delivered.
* `pointer_motion_absolute(x, y)` uses **global logical compositor
  coordinates**, in fixed-point with fractions kept. (100,200) gave surface
  (100.00,200.00), and (640.5,360.25) gave (640.50,360.25), on a 1280x720
  scale-1 output. At other scales or offsets, map RDP pixels → logical
  (`x/scale + output.x`).
* `pointer_motion` (relative), `button` (evdev `BTN_LEFT` 0x110 press/release)
  and `axis` work. Axis values pass through as `wl_pointer.axis` units
  (15 → 15.00), with source Unknown and no `axis_discrete`/value120.
* `keyboard_key` takes **evdev keycodes** (KEY_A=30). Modifiers are tracked by
  KWin (Shift+A gave `modifiers dep=0x1`).
* `keyboard_keysym` (v6) exists. If the keysym is in the current keymap ('b'),
  KWin sends that keycode with temporary modifiers. If it is not (eacute on
  `us`, or U+263A sent as `0x100263a`), KWin swaps in a one-key keymap, so
  clients get a new `wl_keyboard.keymap` and scratch keycode 194, then restores
  the keymap. For those, the client's releases are ignored and KWin sends the
  release itself. This is usable for RDP Unicode events; it costs a keymap
  round trip per character.
* Touch requests exist (v2+). Not tested.
* **Caveat:** the virtual backend has no pointer of its own. The seat
  advertises the pointer and touch capabilities only while a fake_input
  resource is bound (caps 0x2 → 0x7 on bind, 0x2 again on destroy). Destroying
  the resource also releases held keys and buttons. **Bind one fake_input for
  the whole session** and never re-bind it per RDP reconnect.

## 3. Screencast

**Requirement:** `stream_output` only works with OpenGL compositing
(`isSupportedCompositingType`). In the software sandbox, KWin logs
`Found no render device!`, and the stream fails with
`Unsupported compositing type`. KWin's software device (Mesa
`EGL_MESA_device_software` plus the udmabuf allocator) needs `/dev/udmabuf`. On
this host that node is `root:kvm 0660` with a uaccess ACL for the active local
seat user only (currently `plasmalogin`). So user stein, and any remote
session, gets no software path. On this host, the NVIDIA render node (0666)
must be used; see section 6.

GPU path, `zkde stream_output(Virtual-0, pointer=metadata)` → node →
consumer connected with `pw_context_connect_fd`:

* KWin's `EnumFormat` (from `pw-dump`): `BGRA`, `BGRx`. Size range
  200x200..10000x10000, default = output size. `framerate 0/1`, `maxFramerate`
  0..60/1 (output refresh). There are two pods: one with the DmaBuf modifier
  list (section 6), and one without modifiers (MemFd).
* A consumer offering no modifier negotiated `BGRx 1280x720`, `SPA_DATA_MemFd`,
  stride 5120, maxsize 3686400, flags 0xb (mappable). Metas: Header,
  VideoDamage, Cursor (+ SyncTimeline for DmaBuf).
* Frame rate: **0 fps idle**. One frame arrives right after streaming starts,
  then nothing until something changes. **60 fps** while a client animates
  every frame (55–60 per second measured).
* `SPA_META_VideoDamage`: one rect per changed region. It was 0,0 1280x720
  when the client damaged everything, and a cursor-sized rect when the cursor
  was painted into the frame.
* Cursor (`pointer=metadata`): a cursor move produces **cursor-only buffers at
  60/s**. These have `SPA_CHUNK_FLAG_CORRUPTED` (and Header CORRUPTED), **while
  chunk size stays the full frame size**, and no damage. Their
  `spa_meta_cursor` has id=1, position, and hotspot (3,1). The bitmap (24x24
  RGBA) is sent only when the cursor changes; otherwise `bitmap_offset` is set
  with size 0. `id=0` means the cursor is hidden or not on this output: a
  client surface that sets no cursor, or a KWin effect drawing it.
  **The bridge must test the CORRUPTED flag, not chunk size.**
* Gotcha: KWin's **shake-cursor effect** (`shakecursor`, enabled by default)
  fires on back-and-forth motion. It then renders an enlarged cursor into
  video frames with cursor meta `id=0`. Disable it in remote sessions
  (`kwinrc [Plugins] shakecursorEnabled=false`).
* Proof frames: `lab/kwin/evidence/screencast-memfd-frame.png` (target window
  gradient, BGRx MemFd) and `screencast-1920x1080-after-stream-resize.png`.

## 4. Resize: renegotiate the stream; do not use output management

`kde_output_device_v2` (via `kde_output_device_registry_v2` v26) reports name
`Virtual-0` (not `HEADLESS-1`). It has one mode, 1280x720@60000 mHz,
preferred, flags 0. Capabilities are 0x22000 (`custom_modes | disable`). Make,
model, serial and EDID are empty, phys size is -1x-1 mm, and the uuid is random.

* **kde_output_management_v2 works:** `create_mode_list` + `set_resolution` +
  `set_refresh_rate(60000)` + `add_mode`, then `set_custom_modes` + `apply`
  gives `applied`. The new mode appears (CVT-generated, e.g.
  1600x900@59946, 1024x768@59920). Then `configuration.mode` + `apply` gives
  `applied`, and the fullscreen client gets `configure`. That makes two
  round trips.
* **The stream does not renegotiate after that.** For virtual outputs,
  `OutputScreenCastSource::followsStreamSize()` returns
  `VirtualOutput::canResize()`, which is true. So the **consumer's negotiated
  size drives the output**, and frames keep that size.
* **Verified alternative:** the consumer calls `pw_stream_update_params` with an
  `EnumFormat` whose size is fixed to WxH. The stream renegotiates
  (`[RENEGOTIATED] 1600x900`), KWin calls `VirtualOutput::resize`, the output
  gets a single mode WxH@60000, and fullscreen clients get `configure`. This
  worked with both MemFd and DmaBuf (1280x720 → 1920x1080 → 1600x900).
* The consumer's **initial** EnumFormat default size also resizes the output.
  The probe defaulted to 1280x720, and both `stream_output` and
  `stream_virtual_output(1920x1080)` came out at 1280x720. Always offer the
  desired desktop size as the default.
* **Do not mix the two methods.** After any kde_output_management mode apply,
  KWin's output configuration pins that mode, and later stream-driven resizes
  no longer change the output (verified both orders).
* Recommendation: resize **only** by renegotiating the screencast stream size.
  `stream_virtual_output` is not needed, and it adds a second output.

## 5. Plasma launch without systemd

What works (`lab/kwin/evidence/plasma-shot.png` shows panel, wallpaper and
Welcome Center; GL is NVIDIA GLES 3.2):

```
kwin_wayland --virtual --xwayland --socket wayland-0 --no-lockscreen --width W --height H \
             --exit-with-session=session-plasma
session-plasma (prototype: lab/kwin/plasma-session-holder.py):
  export XDG_CURRENT_DESKTOP=KDE XDG_SESSION_DESKTOP=KDE XDG_SESSION_TYPE=wayland \
         KDE_FULL_SESSION=true KDE_SESSION_VERSION=6
  1. UpdateActivationEnvironment with the full env (WAYLAND_DISPLAY, DISPLAY,
     XAUTHORITY, XDG_*), e.g. `dbus-update-activation-environment --all`
  2. own org.kde.KWinWrapper on the private bus
  3. run /usr/bin/plasma_session, exit with its status
```

Why each step is needed (all observed):

* **Plain `plasma_session` nests a second KWin.** Its non-systemd startup
  (`startkde/plasma-session/startup.cpp`) always runs
  `kwin_wayland_wrapper --xwayland` unless `org.kde.KWinWrapper` is already on
  the bus. Our `WAYLAND_DISPLAY` made that a nested Wayland-backend KWin window.
  Plasma ran inside it (screenshot showed a "KDE Wayland Compositor WL-0"
  window), and logout then closed the outer KWin.
* Running the session KWin as `kwin_wayland_wrapper` (which owns the name) also
  stops the nesting. But the wrapper always picks the first free `wayland-N`
  **starting at 1**, and an extra `--socket` renames the socket without moving
  the fd (clients failed with `Failed to create wl_display`). That conflicts
  with the broker's `wayland-0`, so use direct `kwin_wayland` plus the holder.
* **Activation environment:** the private `dbus-daemon` starts before KWin. So
  D-Bus-activated Qt services (kactivitymanagerd, activated by powerdevil)
  started without `WAYLAND_DISPLAY` and aborted. plasmashell then logged
  `Aborting shell load: The activity manager daemon (kactivitymanagerd) is not
  running`, and only the splash showed. startplasma normally syncs this
  environment.
* Startup: the splash covers the screen for about 30 s on a fresh HOME. The
  desktop and plasma-welcome were up at about 50 s. Consider
  `ksplashrc [KSplash] Engine=None` for remote sessions.
* Xwayland works with `--xwayland` (KWin starts it on the first X client).
  In the sandbox it needed `/bin/sh` and `/tmp/.X11-unix`, which a real host
  has.
* Benign errors with no system bus or systemd: polkit, UPower, UDisks, rtkit,
  NetworkManager and powerdevil helpers; `org.freedesktop.systemd1`
  activation fails (Exec=/bin/false); document portal (no `/dev/fuse`); KIO
  "legacy forking mode"; `/tmp/.ICE-unix` not created; portal app-id
  registration warnings. The real host has a system bus and `/dev/fuse`, so
  some of these disappear there.
* **Logout** (`qdbus6 org.kde.Shutdown /Shutdown logout`): plasma-shutdown →
  ksmserver saves the session, and KWin closes its windows and **exits with
  status 0 about 0.2 s later**. KWin kills the `--exit-with-session` program
  with it ("QProcess: Destroyed while process ... is still running"). The
  session end is therefore always KWin's exit, which `session-worker` can wait
  on.

## 6. GPU path (NVIDIA render node only)

* With only `/dev/dri/renderD128`, `/dev/nvidia*` and `/sys` exposed (no
  `card*`, no `LIBGL_ALWAYS_SOFTWARE`), KWin logs
  `Added render device /dev/dri/renderD128: NVIDIA GeForce RTX 4070 Ti` →
  `Chose /dev/dri/renderD128 as the primary GPU` →
  `OpenGL compositing has been successfully initialized`. `supportInformation`
  reports Compositing Type OpenGL ES, vendor NVIDIA Corporation, renderer
  `NVIDIA GeForce RTX 4070 Ti/PCIe/SSE2`, `OpenGL ES 3.2 NVIDIA 615.78.08`,
  EGL platform (GBM on the render node, `EGL_PLATFORM_GBM_KHR`).
* Device selection: by default KWin scans render nodes through udev (so it
  needs `/sys`) and prefers non-software devices. `KWIN_DRM_DEVICES=/dev/dri/renderD128`
  forces the primary device; with this env set but unusable, KWin falls back
  to the software device. `KWIN_RENDER_NODES` limits which nodes are scanned.
  Here the only GPU was chosen automatically.
* **DmaBuf offer:** `drm format AR24, with DMA-BUF: true`. The DmaBuf
  EnumFormat lists 12 NVIDIA block-linear modifiers (0x300000000606010–015,
  0x300000000e08010–015) plus `DRM_FORMAT_MOD_INVALID`. There is **no LINEAR**.
  * A consumer offering LINEAR+INVALID: KWin logs `offering our best suited
    modifier false`, because implicit modifiers fail on NVIDIA GBM, and falls
    back to MemFd.
  * A consumer offering the NVIDIA modifiers: negotiated
    `BGRx modifier 0x300000000e08014`, `SPA_DATA_DmaBuf`, one plane,
    stride 5120, maxsize 3686400 (8294400 at 1080p). Cursor and damage
    metadata are identical to MemFd. Stream-driven resize also works.
* **CPU mmap of the DmaBuf** works (`DMA_BUF_IOCTL_SYNC` + mmap OK), but the
  data is block-linear tiled
  (`lab/kwin/evidence/screencast-dmabuf-tiled-mmap.png`), and one 1280x720
  readback took about 3.4 s. For NVENC, import the fd with its modifier (EGL
  `EGL_LINUX_DMA_BUF_EXT` + modifier, or Vulkan/CUDA external memory). For a
  CPU or software encoder, ask for MemFd and let KWin do the GPU→SHM copy. The
  KWin-side cost of that copy was not measured.
* Production on this host: the session needs read/write on
  `/dev/dri/renderD128` (0666 here) and `/dev/nvidia*` (0666). No special
  environment is needed.

## 7. Bridge input and clipboard on KWin (bridge `KwinInput`, ext_data_control)

Measured with the bridge's own code (`bridge/src/wayland/kwin_input.rs`,
`bridge/src/clipboard.rs`) through an fd handoff. Scenarios:
`bridge-input` (probe `kwin-input-probe` → `input-target`, an xkbcommon-decoding
copy of `target-window`), `bridge-wheel-qt`, `bridge-wheel-gtk`,
`bridge-clipboard` (ignored test `clipboard::tests::live_ext_data_control_roundtrip`
↔ `qt-clip`). Logs: `lab/kwin/evidence/bridge-*.log`.

* **Wheel: 10 axis units per RDP notch, not 15.** fake_input's `axis` reaches
  clients as `wl_pointer.axis` only (source Unknown, no value120), so toolkits
  use their continuous fallback: Qt 6 `angleDelta = 12 × value`, GTK 3
  `delta = value / 10`. With 15 per notch both scrolled 1.5 notches; with 10,
  Qt got `angleDelta 120` and GTK `1.000` per notch, and half notches gave
  60 / 0.5. KWin's 15 (libinput/EIS) applies only together with value120.
  The KWin EIS plugin does forward v120; fake_input cannot.
* **Caps Lock and `keyboard_keysym`:** KWin adds the level's modifiers but keeps
  the locked mask, so a mapped 'a' arrives as "A" while Caps Lock is on, and
  clients capitalize scratch-keymap keysyms too. The bridge taps Caps Lock off
  and on around cased characters. Verified: with Caps+Num locked, Unicode
  "a", "A", "é" decoded as "a", "A", "é".
* Keysyms missing from the layout (é, ☺, U+1F600 on `us`) go through KWin's
  scratch keymap: `pc+us` plus key 194, keycodes section named `"custom"`;
  the seat `wl_keyboard` sees it and then the restored map. That path also
  **releases every key the device holds**. `KwinInput` ignores `"custom"`
  maps and mirrors the release.
* Fake input has no modifier request, so RDP Synchronize taps KEY_CAPSLOCK /
  KEY_NUMLOCK when the mirrored lock differs (`lock=0x12` on, `0x0` off).
  Scroll Lock is skipped: stock XKB maps give it no lock action. KWin starts
  with no locks on the virtual backend.
* Pointer: absolute RDP (100,200) → (100.00,200.00); relative +10,+5;
  BTN_LEFT/RIGHT/MIDDLE; `release_all` released held Ctrl and middle button.
  Seat caps 0x3/0x7 on bind, 0x6/0x2 after `destroy`.
* **Clipboard:** `ext_data_control_manager_v1` v1 works as zwlr did. RDP
  text/HTML/PNG → Qt saw `from-rdp: héllo ☺`, the HTML and a 2×1 PNG. Qt
  text+HTML → bridge Local text + CF_HTML (no image alongside text, as
  before); Qt image only → PNG (97 B) + DIB. Qt offers `image/png` (and many
  `image/*`) on the wire. A Wayland client can set the selection only with a
  fresh input serial (the lab clicks via fake input first). Primary selection
  exists in ext_data_control v1 too; the bridge ignores it, as it did before.

## Blocking problems / follow-ups

1. **No software rendering for remote sessions.** Without `/dev/udmabuf` access
   (uaccess goes only to the active local seat user), KWin has no render device
   and the screencast refuses. This host must use the NVIDIA render node, and
   GPU-less hosts and non-GPU CI cannot screencast. A udev rule granting
   `kde-rdp-users` access to `/dev/udmabuf` would be a packaging decision; it
   was not tested here.
2. **PipeWire access classification** of the handed-off fd (section 1): set
   `access.socket` in the private PipeWire config, or keep the connector alive
   until the bridge acknowledges.
3. `session-plasma` must sync the activation environment and own
   `org.kde.KWinWrapper` before `plasma_session` (section 5).
4. Resize must go through stream renegotiation, and the bridge must never
   apply output-management modes (section 4).
5. Keep one fake_input bound for the session life (section 2). Disable
   `shakecursor` (section 3).
6. `lab/kwin/out/` (build output and logs) should be added to `.gitignore`.
