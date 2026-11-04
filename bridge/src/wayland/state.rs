//! Worker-owned Wayland state and protocol event dispatch: outputs (with
//! their xdg-output logical geometry), the seat keymap, KWin's screencast
//! stream and the fake-input global (bound by `kwin_input`).
// Shapes published by the worker carry cache index 0. The per-activation
// display stream (`Updates` in wayland/mod.rs) assigns real indices through
// this cache, because only it knows which shapes reached the client.
pub(super) mod cursor_cache;
use super::kwin_input::read_keymap;
use super::protocols::fake_input::org_kde_kwin_fake_input::OrgKdeKwinFakeInput;
use super::protocols::screencast::{
    zkde_screencast_stream_unstable_v1::{self, ZkdeScreencastStreamUnstableV1},
    zkde_screencast_unstable_v1::ZkdeScreencastUnstableV1,
};
use std::fs::File;
use wayland_client::{
    Connection, Dispatch, QueueHandle, WEnum, delegate_noop,
    protocol::{wl_callback, wl_keyboard, wl_output, wl_registry, wl_seat},
};
use wayland_protocols::xdg::xdg_output::zv1::client::{
    zxdg_output_manager_v1::ZxdgOutputManagerV1,
    zxdg_output_v1::{self, ZxdgOutputV1},
};

/// zkde_screencast v6 adds the PipeWire object serial.
pub(super) const SCREENCAST_VERSION: u32 = 6;

/// One advertised `wl_output` and what KWin reported about it.
pub(super) struct OutputRecord {
    pub(super) global: u32,
    pub(super) output: wl_output::WlOutput,
    pub(super) xdg: Option<ZxdgOutputV1>,
    pub(super) name: String,
    pub(super) scale: i32,
    pub(super) normal_transform: bool,
    /// Current mode in physical pixels.
    pub(super) mode: (u32, u32),
    /// xdg-output logical position and size, when known.
    pub(super) logical_position: (i32, i32),
    pub(super) logical_size: Option<(i32, i32)>,
}
impl OutputRecord {
    /// Logical = physical / scale. KWin's fractional scale is only visible
    /// through the logical size; wl_output.scale is its ceiling.
    pub(super) fn effective_scale(&self) -> f64 {
        match self.logical_size {
            Some((width, _)) if width > 0 && self.mode.0 > 0 => {
                f64::from(self.mode.0) / f64::from(width)
            }
            _ => f64::from(self.scale.max(1)),
        }
    }
    pub(super) fn origin(&self) -> (f64, f64) {
        (
            f64::from(self.logical_position.0),
            f64::from(self.logical_position.1),
        )
    }
}

/// What KWin reported for the screencast stream.
#[derive(Default, Debug, PartialEq, Eq)]
pub(super) struct StreamEvents {
    pub(super) serial: Option<u64>,
    pub(super) node: Option<u32>,
    pub(super) failed: Option<String>,
    pub(super) closed: bool,
}

#[derive(Default)]
pub(super) struct State {
    pub(super) registry: Option<wl_registry::WlRegistry>,
    pub(super) seat: Option<wl_seat::WlSeat>,
    pub(super) seat_keyboard: Option<wl_keyboard::WlKeyboard>,
    /// The newest seat keymap not yet consumed by the input device.
    pub(super) seat_keymap: Option<String>,
    pub(super) keymap_error: Option<String>,
    pub(super) synced: bool,
    pub(super) outputs: Vec<OutputRecord>,
    pub(super) xdg_output_manager: Option<ZxdgOutputManagerV1>,
    pub(super) selected_output: Option<u32>,
    pub(super) output_removed: bool,
    pub(super) screencast: Option<ZkdeScreencastUnstableV1>,
    /// The `org_kde_kwin_fake_input` global name and version.
    pub(super) fake_input: Option<(u32, u32)>,
    pub(super) stream: StreamEvents,
}

impl State {
    fn remove_output(&mut self, name: u32) {
        self.output_removed |= self.selected_output == Some(name);
        self.outputs.retain(|o| o.global != name);
    }
    pub(super) fn validate_output(&self) -> anyhow::Result<()> {
        anyhow::ensure!(!self.output_removed, "Wayland output removed");
        if let Some(output) = self.output() {
            anyhow::ensure!(
                output.scale > 0 && output.normal_transform,
                "bridge requires positive scale and normal transform"
            );
        } else if self.selected_output.is_some() {
            anyhow::bail!("Wayland output removed");
        }
        Ok(())
    }
    /// The selected output.
    pub(super) fn output(&self) -> Option<&OutputRecord> {
        let selected = self.selected_output?;
        self.outputs.iter().find(|o| o.global == selected)
    }
    /// The output named `name`, or the first named output when `name` is
    /// empty (KWin's virtual backend has exactly one, e.g. `Virtual-0`).
    pub(super) fn find_output(&self, name: &str) -> Option<&OutputRecord> {
        self.outputs
            .iter()
            .filter(|o| !o.name.is_empty())
            .find(|o| name.is_empty() || o.name == name)
    }
    pub(super) fn create_xdg_outputs(&mut self, h: &QueueHandle<State>) {
        if let Some(manager) = &self.xdg_output_manager {
            for output in self.outputs.iter_mut().filter(|o| o.xdg.is_none()) {
                output.xdg = Some(manager.get_xdg_output(&output.output, h, output.global));
            }
        }
    }
}

impl Dispatch<wl_registry::WlRegistry, ()> for State {
    fn event(
        s: &mut Self,
        registry: &wl_registry::WlRegistry,
        e: wl_registry::Event,
        _: &(),
        _: &Connection,
        q: &QueueHandle<Self>,
    ) {
        match e {
            wl_registry::Event::Global {
                name,
                interface,
                version,
            } => match interface.as_str() {
                "wl_seat" if s.seat.is_none() => {
                    s.seat = Some(registry.bind(name, version.min(7), q, ()))
                }
                "wl_output" => {
                    let output = registry.bind(name, version.min(4), q, ());
                    s.outputs.push(OutputRecord {
                        global: name,
                        output,
                        xdg: None,
                        name: String::new(),
                        scale: 1,
                        normal_transform: true,
                        mode: (0, 0),
                        logical_position: (0, 0),
                        logical_size: None,
                    });
                    s.create_xdg_outputs(q);
                }
                "zxdg_output_manager_v1" => {
                    s.xdg_output_manager = Some(registry.bind(name, version.min(3), q, ()));
                    s.create_xdg_outputs(q);
                }
                "zkde_screencast_unstable_v1" if version >= SCREENCAST_VERSION => {
                    s.screencast = Some(registry.bind(name, SCREENCAST_VERSION, q, ()))
                }
                "org_kde_kwin_fake_input" => s.fake_input = Some((name, version)),
                _ => {}
            },
            wl_registry::Event::GlobalRemove { name } => s.remove_output(name),
            _ => {}
        }
    }
}

impl Dispatch<wl_output::WlOutput, ()> for State {
    fn event(
        s: &mut Self,
        p: &wl_output::WlOutput,
        e: wl_output::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let Some(o) = s.outputs.iter_mut().find(|o| &o.output == p) {
            match e {
                wl_output::Event::Name { name } => o.name = name,
                wl_output::Event::Scale { factor } => o.scale = factor,
                wl_output::Event::Mode {
                    flags: WEnum::Value(flags),
                    width,
                    height,
                    ..
                } if flags.contains(wl_output::Mode::Current) => {
                    o.mode = (width.max(0) as u32, height.max(0) as u32);
                }
                wl_output::Event::Geometry { transform, .. } => {
                    o.normal_transform = transform == WEnum::Value(wl_output::Transform::Normal)
                }
                _ => {}
            }
        }
    }
}

impl Dispatch<ZxdgOutputV1, u32> for State {
    fn event(
        s: &mut Self,
        _: &ZxdgOutputV1,
        e: zxdg_output_v1::Event,
        global: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let Some(o) = s.outputs.iter_mut().find(|o| o.global == *global) {
            match e {
                zxdg_output_v1::Event::LogicalPosition { x, y } => o.logical_position = (x, y),
                zxdg_output_v1::Event::LogicalSize { width, height } => {
                    o.logical_size = Some((width, height))
                }
                _ => {}
            }
        }
    }
}

impl Dispatch<ZkdeScreencastStreamUnstableV1, ()> for State {
    fn event(
        s: &mut Self,
        _: &ZkdeScreencastStreamUnstableV1,
        e: zkde_screencast_stream_unstable_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match e {
            zkde_screencast_stream_unstable_v1::Event::Serial {
                object_serial_hi,
                object_serial_low,
            } => {
                s.stream.serial =
                    Some((u64::from(object_serial_hi) << 32) | u64::from(object_serial_low))
            }
            zkde_screencast_stream_unstable_v1::Event::Created { node } => {
                s.stream.node = Some(node)
            }
            zkde_screencast_stream_unstable_v1::Event::Failed { error } => {
                s.stream.failed = Some(error)
            }
            zkde_screencast_stream_unstable_v1::Event::Closed => s.stream.closed = true,
        }
    }
}

impl Dispatch<wl_keyboard::WlKeyboard, ()> for State {
    fn event(
        s: &mut Self,
        _: &wl_keyboard::WlKeyboard,
        event: wl_keyboard::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_keyboard::Event::Keymap { format, fd, size } = event {
            let result = (|| -> anyhow::Result<String> {
                anyhow::ensure!(
                    format == WEnum::Value(wl_keyboard::KeymapFormat::XkbV1),
                    "unsupported compositor keyboard map format"
                );
                read_keymap(File::from(fd), size)
            })();
            match result {
                // KwinInput ignores KWin's temporary one-key Unicode map.
                Ok(text) => s.seat_keymap = Some(text),
                Err(error) => s.keymap_error = Some(error.to_string()),
            }
        }
    }
}

impl Dispatch<wl_callback::WlCallback, ()> for State {
    fn event(
        s: &mut Self,
        _: &wl_callback::WlCallback,
        _: wl_callback::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        s.synced = true;
    }
}

delegate_noop!(State: ignore wl_seat::WlSeat);
delegate_noop!(State: ignore ZxdgOutputManagerV1);
delegate_noop!(State: ignore ZkdeScreencastUnstableV1);
delegate_noop!(State: ignore OrgKdeKwinFakeInput);

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixStream;

    fn record(global: u32, output: wl_output::WlOutput, name: &str) -> OutputRecord {
        OutputRecord {
            global,
            output,
            xdg: None,
            name: name.into(),
            scale: 1,
            normal_transform: true,
            mode: (1920, 1080),
            logical_position: (0, 0),
            logical_size: None,
        }
    }

    #[test]
    fn unrelated_output_removal_does_not_end_capture() {
        let mut state = State {
            selected_output: Some(10),
            ..State::default()
        };
        state.remove_output(11);
        assert!(!state.output_removed);
        state.remove_output(10);
        assert!(state.output_removed);
        assert!(state.validate_output().is_err());
    }

    #[test]
    fn empty_output_name_selects_kwins_first_output_and_scale_follows_logical_size() {
        let (socket, _compositor) = UnixStream::pair().unwrap();
        let connection = Connection::from_socket(socket).unwrap();
        let events = connection.new_event_queue::<State>();
        let handle = events.handle();
        let registry = connection.display().get_registry(&handle, ());
        let unnamed: wl_output::WlOutput = registry.bind(1, 4, &handle, ());
        let virtual0: wl_output::WlOutput = registry.bind(2, 4, &handle, ());
        let mut state = State::default();
        state.outputs.push(record(1, unnamed, ""));
        state.outputs.push(record(2, virtual0, "Virtual-0"));
        assert_eq!(state.find_output("").unwrap().name, "Virtual-0");
        assert_eq!(state.find_output("Virtual-0").unwrap().global, 2);
        assert!(state.find_output("HEADLESS-1").is_none());
        let output = &mut state.outputs[1];
        assert_eq!(output.effective_scale(), 1.0);
        // KWin at 150%: 1920 physical pixels are 1280 logical units.
        output.scale = 2;
        output.logical_size = Some((1280, 720));
        output.logical_position = (100, 0);
        assert_eq!(output.effective_scale(), 1.5);
        assert_eq!(output.origin(), (100.0, 0.0));
        output.logical_size = None;
        assert_eq!(output.effective_scale(), 2.0);
    }
}
