# Protocol XMLs (vendored)

Downloaded 2026-11-03 (the host had no `plasma-wayland-protocols` /
`wayland-protocols` packages at the time). The versions are the newest the
running KWin 6.8.2 advertises or newer: KWin 6.8.2 advertises fake_input v6,
zkde_screencast v6, kde_output_device_registry_v2 v26,
kde_output_management_v2 v22 and ext_data_control_manager_v1 v1.

| File | Source | Tag / commit |
| --- | --- | --- |
| `fake-input.xml` | https://invent.kde.org/libraries/plasma-wayland-protocols/-/raw/v1.23.0/src/protocols/fake-input.xml | v1.23.0 / c5ac4db818f4a575a6ccfe0065b73ecfaba6e93e |
| `zkde-screencast-unstable-v1.xml` | …/plasma-wayland-protocols/-/raw/v1.23.0/src/protocols/zkde-screencast-unstable-v1.xml | v1.23.0 / c5ac4db8 |
| `kde-output-device-v2.xml` | …/plasma-wayland-protocols/-/raw/v1.23.0/src/protocols/kde-output-device-v2.xml | v1.23.0 / c5ac4db8 |
| `kde-output-management-v2.xml` | …/plasma-wayland-protocols/-/raw/v1.23.0/src/protocols/kde-output-management-v2.xml | v1.23.0 / c5ac4db8 |
| `ext-data-control-v1.xml` | https://gitlab.freedesktop.org/wayland/wayland-protocols/-/raw/1.50/staging/ext-data-control/ext-data-control-v1.xml | 1.50 / 3eab2dccf4bfdd85c16d312a6c8c8fbc2a6e73a8 |
| `xdg-shell.xml` | https://gitlab.freedesktop.org/wayland/wayland-protocols/-/raw/1.50/stable/xdg-shell/xdg-shell.xml | 1.50 / 3eab2dcc (lab test window only) |

SHA-256:

```
293deba5dfdb974fbcf8dcf891a30a51a36a8bba0047dc6bdf6a03194089b7fb  ext-data-control-v1.xml
89f4ada4a1f09168a4528f47c0495e3f89dc2a25e8b23f4539ed3f5110eff708  fake-input.xml
710d2474e846dfcb20dfa8b84ddcac28be24386d2c8910c5c462a88aa2807c06  kde-output-device-v2.xml
93a796b716324e19224bd0fb2cb296aaff8b28df07506b648fad46ebb482bb1b  kde-output-management-v2.xml
8cd3980d0783e1f55d12901ac51533204af71487cc4dc69ee00bb7096647d9de  xdg-shell.xml
0d9202011a47bb79e356774229a3ad64dca7c5d527e25a8cfc70a5fa6ba4dc20  zkde-screencast-unstable-v1.xml
```

Notes for binding generation: `kde_output_device_v2` is no longer a per-output
global; it is announced by the `kde_output_device_registry_v2` global (KWin
rejects registry versions < 21). Bind `zkde_screencast_unstable_v1` at v6 to get
the `serial` event (PipeWire `object.serial`, needed for `target.object`).
The lab probes in `lab/kwin/` are generated from these files by
`lab/kwin/build.sh`.
