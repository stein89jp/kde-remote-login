//! Disposable lab probe: drive `KwinInput` against a real KWin and log each
//! step, for `lab/kwin/scenarios/bridge-input.sh` (its `input-target` window
//! logs what arrives). Honours WAYLAND_SOCKET / WAYLAND_DISPLAY.
use ironrdp_pdu::input::fast_path::SynchronizeFlags;
use ironrdp_server::{DesktopSize, KeyboardEvent, MouseButton, MouseEvent};
use kde_rdp_bridge::wayland::kwin_input::{FAKE_INPUT_VERSION, KwinInput, read_keymap};
use kde_rdp_bridge::wayland::protocols::fake_input::org_kde_kwin_fake_input::OrgKdeKwinFakeInput;
use std::{
    fs::File,
    time::{Duration, Instant},
};
use wayland_client::{
    Connection, Dispatch, QueueHandle, WEnum, delegate_noop,
    protocol::{wl_keyboard, wl_registry, wl_seat},
};

#[derive(Default)]
struct Probe {
    fake: Option<(u32, u32)>,
    seat: Option<wl_seat::WlSeat>,
    keymap: Option<String>,
}

impl Dispatch<wl_registry::WlRegistry, ()> for Probe {
    fn event(
        s: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global {
            name,
            interface,
            version,
        } = event
        {
            match interface.as_str() {
                "org_kde_kwin_fake_input" => s.fake = Some((name, version)),
                "wl_seat" if s.seat.is_none() => {
                    s.seat = Some(registry.bind(name, version.min(7), qh, ()))
                }
                _ => {}
            }
        }
    }
}
impl Dispatch<wl_seat::WlSeat, ()> for Probe {
    fn event(
        _: &mut Self,
        _: &wl_seat::WlSeat,
        event: wl_seat::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_seat::Event::Capabilities { capabilities } = event {
            println!("probe seat capabilities {capabilities:?}");
        }
    }
}
impl Dispatch<wl_keyboard::WlKeyboard, ()> for Probe {
    fn event(
        s: &mut Self,
        _: &wl_keyboard::WlKeyboard,
        event: wl_keyboard::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_keyboard::Event::Keymap { format, fd, size } = event
            && format == WEnum::Value(wl_keyboard::KeymapFormat::XkbV1)
        {
            match read_keymap(File::from(fd), size) {
                Ok(text) => s.keymap = Some(text),
                Err(error) => eprintln!("probe keymap error: {error}"),
            }
        }
    }
}
delegate_noop!(Probe: ignore OrgKdeKwinFakeInput);

fn main() -> anyhow::Result<()> {
    let connection = Connection::connect_to_env()?;
    let mut queue = connection.new_event_queue::<Probe>();
    let qh = queue.handle();
    let registry = connection.display().get_registry(&qh, ());
    let mut s = Probe::default();
    queue.roundtrip(&mut s)?;
    let (name, version) = s
        .fake
        .ok_or_else(|| anyhow::anyhow!("org_kde_kwin_fake_input not advertised"))?;
    let seat = s
        .seat
        .clone()
        .ok_or_else(|| anyhow::anyhow!("no wl_seat"))?;
    seat.get_keyboard(&qh, ());
    let deadline = Instant::now() + Duration::from_secs(5);
    while s.keymap.is_none() {
        anyhow::ensure!(Instant::now() < deadline, "no seat keymap");
        queue.roundtrip(&mut s)?;
    }
    let target = DesktopSize {
        width: 1280,
        height: 720,
    };
    let mut input = KwinInput::bind(
        &registry,
        name,
        version,
        &qh,
        s.keymap.take().unwrap(),
        target,
        1.0,
    )?;
    println!("probe bound fake_input v{version} (using v{FAKE_INPUT_VERSION})");
    let mut step = |input: &mut KwinInput, what: &str, s: &mut Probe| -> anyhow::Result<()> {
        queue.roundtrip(s)?;
        if let Some(text) = s.keymap.take() {
            input.update_keymap(text)?;
        }
        println!("probe step {what}");
        std::thread::sleep(Duration::from_millis(150));
        Ok(())
    };
    let key = |code: u8, pressed: bool| {
        if pressed {
            KeyboardEvent::Pressed {
                code,
                extended: false,
            }
        } else {
            KeyboardEvent::Released {
                code,
                extended: false,
            }
        }
    };
    step(&mut input, "start", &mut s)?;
    input.mouse_event(MouseEvent::Move { x: 100, y: 200 });
    step(&mut input, "move 100,200", &mut s)?;
    input.mouse_event(MouseEvent::RelMove { x: 10, y: 5 });
    step(&mut input, "relmove +10,+5", &mut s)?;
    for pressed in [true, false] {
        input.mouse_event(MouseEvent::Button {
            x: 640,
            y: 360,
            button: MouseButton::Left,
            pressed,
        });
    }
    step(&mut input, "left click at 640,360", &mut s)?;
    for pressed in [true, false] {
        input.mouse_event(MouseEvent::Button {
            x: 640,
            y: 360,
            button: MouseButton::Right,
            pressed,
        });
    }
    step(&mut input, "right click", &mut s)?;
    input.mouse_event(MouseEvent::VerticalScroll { value: 120 });
    step(&mut input, "wheel one notch up (RDP +120)", &mut s)?;
    input.mouse_event(MouseEvent::VerticalScroll { value: -120 });
    step(&mut input, "wheel one notch down (RDP -120)", &mut s)?;
    input.mouse_event(MouseEvent::VerticalScroll { value: -60 });
    step(&mut input, "wheel half notch down (RDP -60)", &mut s)?;
    input.mouse_event(MouseEvent::HorizontalScroll { value: 120 });
    step(&mut input, "hwheel one notch right (RDP +120)", &mut s)?;
    // Scancodes: a, Shift+a.
    input.key_event(key(0x1e, true));
    input.key_event(key(0x1e, false));
    step(&mut input, "scancode a", &mut s)?;
    input.key_event(key(0x2a, true));
    input.key_event(key(0x1e, true));
    input.key_event(key(0x1e, false));
    input.key_event(key(0x2a, false));
    step(&mut input, "scancode Shift+a", &mut s)?;
    for ch in "aA1é€☺".chars() {
        input.unicode(ch);
        step(&mut input, &format!("unicode {ch}"), &mut s)?;
    }
    for unit in "😀".encode_utf16() {
        input.key_event(KeyboardEvent::UnicodePressed(unit));
        input.key_event(KeyboardEvent::UnicodeReleased(unit));
    }
    step(&mut input, "unicode surrogate pair U+1F600", &mut s)?;
    input.key_event(KeyboardEvent::Synchronize(
        SynchronizeFlags::from_bits_truncate(4 | 2),
    ));
    step(&mut input, "synchronize caps+num on", &mut s)?;
    input.key_event(key(0x1e, true));
    input.key_event(key(0x1e, false));
    step(&mut input, "scancode a with caps", &mut s)?;
    for ch in "aAé1".chars() {
        input.unicode(ch);
        step(&mut input, &format!("unicode {ch} with caps"), &mut s)?;
    }
    input.key_event(KeyboardEvent::Synchronize(
        SynchronizeFlags::from_bits_truncate(0),
    ));
    step(&mut input, "synchronize all off", &mut s)?;
    input.unicode('\r');
    input.unicode('\u{8}');
    step(&mut input, "unicode Return, BackSpace", &mut s)?;
    // Disconnect with a key and a button held.
    input.key_event(key(0x1d, true));
    input.mouse_event(MouseEvent::Button {
        x: 10,
        y: 10,
        button: MouseButton::Middle,
        pressed: true,
    });
    step(&mut input, "hold ctrl + middle", &mut s)?;
    input.release_all();
    step(&mut input, "release_all", &mut s)?;
    // The resource stays bound across RDP reconnects; the seat keeps its
    // pointer capability until session end.
    std::thread::sleep(Duration::from_millis(500));
    input.destroy();
    queue.roundtrip(&mut s)?;
    println!("probe done");
    Ok(())
}
