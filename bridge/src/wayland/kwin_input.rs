//! RDP keyboard and pointer input through KWin's `org_kde_kwin_fake_input` v6.
//!
//! One fake-input resource is bound for the whole session (docs/kde-findings.md
//! section 2): on KWin's virtual backend the seat advertises a pointer only
//! while a fake-input resource exists, and destroying it releases every key
//! and button it holds. Unlike the virtual keyboard this replaces, fake input
//! has no keymap or modifier requests; KWin runs `keyboard_key` evdev codes
//! through its own XKB state with the seat's keymap. The bridge therefore
//! mirrors that state from the seat's `wl_keyboard.keymap` and the keys it has
//! sent, so RDP Synchronize can tap lock keys and committed Unicode text can
//! avoid Caps Lock.
use super::protocols::fake_input::org_kde_kwin_fake_input::OrgKdeKwinFakeInput;
use crate::evdev_key;
use crate::keyboard::UnicodeInput;
use ironrdp_pdu::input::fast_path::SynchronizeFlags;
use ironrdp_server::{DesktopSize, KeyboardEvent, MouseButton, MouseEvent};
use std::{
    collections::BTreeSet,
    fs::File,
    io::{Read, Seek, SeekFrom},
};
use wayland_client::{Dispatch, QueueHandle, protocol::wl_registry::WlRegistry};
use xkb::Keysym;
use xkbcommon::xkb;

/// The fake-input version this module needs: `keyboard_keysym` is v6,
/// `keyboard_key` v4 and `pointer_motion_absolute` v3.
pub const FAKE_INPUT_VERSION: u32 = 6;

/// `wl_keyboard.key_state` / `wl_pointer.button_state` wire values, which is
/// what KWin's fake-input requests take.
const RELEASED: u32 = 0;
const PRESSED: u32 = 1;
/// `wl_pointer.axis` wire values.
const AXIS_VERTICAL: u32 = 0;
const AXIS_HORIZONTAL: u32 = 1;

/// evdev codes of the lock keys RDP Synchronize reports.
const KEY_CAPSLOCK: u32 = 58;
const KEY_NUMLOCK: u32 = 69;
/// RDP synchronize bits: Scroll=1, Num=2, Caps=4, Kana=8. Only Num and Caps
/// Lock are mirrored: stock XKB maps give Scroll_Lock no lock action, so its
/// state could never match and every Synchronize would tap it again, and Kana
/// has no evdev key.
const LOCKS: [(u8, &str, u32); 2] = [(2, "NumLock", KEY_NUMLOCK), (4, "Lock", KEY_CAPSLOCK)];

/// RDP (and Windows) report wheel rotation in WHEEL_DELTA units per notch.
const WHEEL_DELTA: i32 = 120;
/// Axis units per RDP notch. fake_input forwards the value unchanged as
/// `wl_pointer.axis` with source Unknown and no axis_discrete/value120, so
/// clients fall back to their own continuous-scroll scaling, and both
/// toolkits treat 10 units as one wheel step there: Qt 6 reports
/// `angleDelta = 12 * value` (10 -> 120, one notch) and GTK 3 smooth-scroll
/// `delta = value / 10` (10 -> 1.0). Lab-verified (bridge-wheel-qt/-gtk).
/// libinput's 15 per notch is only right when value120 accompanies it; with
/// 15, Qt and GTK scrolled 1.5 notches per RDP notch.
const WHEEL_NOTCH_AXIS: f64 = 10.0;
/// Bound a single scroll event like the core RDP PDU (i16 rotation units).
const WHEEL_UNITS_MAX: i32 = i16::MAX as i32;

/// Read a `wl_keyboard.keymap` (xkb_v1) fd: the map text without its NUL.
pub fn read_keymap(mut file: File, size: u32) -> anyhow::Result<String> {
    anyhow::ensure!(
        (1..=1024 * 1024).contains(&size),
        "invalid compositor keyboard map size"
    );
    file.seek(SeekFrom::Start(0))?;
    let mut bytes = Vec::new();
    file.take(u64::from(size)).read_to_end(&mut bytes)?;
    anyhow::ensure!(
        bytes.len() == size as usize && bytes.pop() == Some(0),
        "invalid compositor keyboard map data"
    );
    Ok(String::from_utf8(bytes)?)
}

/// KWin's `keyboard_keysym` path for a keysym missing from the keymap swaps in
/// a temporary `pc+us` map whose keycodes section is named "custom", then
/// reconfigures back (KWin `Xkb::createKeymapForKeysym`). The seat keyboard
/// sees both maps; the temporary one must never replace the mirrored map.
fn is_kwin_scratch_keymap(text: &str) -> bool {
    text.contains("xkb_keycodes \"custom\"")
}

/// The seat keymap and the XKB state KWin holds for the keys we sent.
pub(super) struct PhysicalKeymap {
    text: String,
    state: xkb::State,
}

impl PhysicalKeymap {
    pub(super) fn new(text: String) -> anyhow::Result<Self> {
        let context = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
        let map = xkb::Keymap::new_from_string(
            &context,
            text.clone(),
            xkb::KEYMAP_FORMAT_TEXT_V1,
            xkb::KEYMAP_COMPILE_NO_FLAGS,
        )
        .ok_or_else(|| anyhow::anyhow!("invalid compositor XKB keymap"))?;
        Ok(Self {
            text,
            state: xkb::State::new(&map),
        })
    }

    fn update_key(&mut self, evdev: u32, pressed: bool) {
        self.state.update_key(
            (evdev + 8).into(),
            if pressed {
                xkb::KeyDirection::Down
            } else {
                xkb::KeyDirection::Up
            },
        );
    }

    /// Whether a (real or virtual) modifier is locked in the mirrored state.
    fn locked(&self, name: &str) -> bool {
        self.state.mod_name_is_active(name, xkb::STATE_MODS_LOCKED)
    }

    /// Mirror of KWin's `Xkb::keycodeFromKeysym`: the first keycode (in
    /// ascending order, excluding the maximum) and level of the effective
    /// layout that carries `keysym`. KWin sends that key for `keyboard_keysym`;
    /// with no match it uses a temporary keymap instead.
    fn kwin_keycode_for(&self, keysym: Keysym) -> Option<u32> {
        let map = self.state.get_keymap();
        let layout = self.state.serialize_layout(xkb::STATE_LAYOUT_EFFECTIVE);
        let (min, max) = (map.min_keycode().raw(), map.max_keycode().raw());
        (min..max).find_map(|code| {
            let levels = map.num_levels_for_key(code.into(), layout);
            (0..levels)
                .any(|level| {
                    map.key_get_syms_by_level(code.into(), layout, level)
                        .contains(&keysym)
                })
                .then(|| code.saturating_sub(8))
        })
    }
}

/// The keysym KWin should type for committed Unicode text. Control characters
/// use their function keysyms (`xkb_utf32_to_keysym` would make '\n' a
/// Linefeed); printable characters use the proper keysym, which KWin finds in
/// the seat keymap when it can; anything else is the Unicode keysym
/// `0x1000000 + code point`, which KWin types through a temporary keymap.
fn unicode_keysym(character: char) -> Keysym {
    let raw = match character {
        '\u{8}' => 0xff08, // BackSpace
        '\t' => 0xff09,    // Tab
        '\r' | '\n' => 0xff0d,
        '\u{1b}' => 0xff1b, // Escape
        '\u{7f}' => 0xffff, // Delete
        _ => match xkb::utf32_to_keysym(character as u32).raw() {
            0 => 0x0100_0000 | character as u32,
            raw => raw,
        },
    };
    Keysym::new(raw)
}

/// Caps Lock changes the case KWin's `keyboard_keysym` produces: KWin adds
/// the level's modifiers (Shift for 'A') but keeps the locked mask, and
/// clients also capitalize one-level keysyms. Cased text is typed with Caps
/// Lock briefly off.
fn caps_sensitive(character: char) -> bool {
    character.is_lowercase() || character.is_uppercase()
}

/// RDP wheel units (WHEEL_DELTA per notch) to the `fake_input.axis` value.
/// fake_input has no discrete steps, so fractions of a notch from
/// high-resolution wheels are forwarded immediately and proportionally, as
/// the previous virtual-pointer path forwarded its continuous distance.
fn wheel_axis_value(units: i32) -> f64 {
    f64::from(units.clamp(-WHEEL_UNITS_MAX, WHEEL_UNITS_MAX)) * WHEEL_NOTCH_AXIS
        / f64::from(WHEEL_DELTA)
}

fn button_code(button: MouseButton) -> Option<u32> {
    Some(match button {
        MouseButton::Left => 0x110,
        MouseButton::Right => 0x111,
        MouseButton::Middle => 0x112,
        MouseButton::X1 => 0x113,
        MouseButton::X2 => 0x114,
        _ => return None,
    })
}

/// Session-long KWin fake-input device plus the mirrored keyboard state.
pub struct KwinInput {
    fake: OrgKdeKwinFakeInput,
    physical_keymap: PhysicalKeymap,
    unicode: UnicodeInput,
    keys: BTreeSet<u32>,
    buttons: BTreeSet<u32>,
    /// Pointer estimate in RDP desktop pixels (for cursor tracking).
    point: (u32, u32),
    /// RDP desktop size in physical pixels.
    target: DesktopSize,
    /// Output scale: logical = physical / scale.
    scale: f64,
    /// The output's position in the global logical compositor space.
    origin: (f64, f64),
}

impl KwinInput {
    /// Bind the advertised `org_kde_kwin_fake_input` global (`global`,
    /// `version` from `wl_registry.global`) and authenticate it. Without
    /// `authenticate` KWin silently drops every request. `seat_keymap` is the
    /// first `wl_keyboard.keymap` of the seat (see [`read_keymap`]).
    ///
    /// Call this once per session, never per RDP reconnect: the seat's
    /// pointer capability exists only while this resource is bound.
    pub fn bind<D>(
        registry: &WlRegistry,
        global: u32,
        version: u32,
        qh: &QueueHandle<D>,
        seat_keymap: String,
        target: DesktopSize,
        scale: f64,
    ) -> anyhow::Result<Self>
    where
        D: Dispatch<OrgKdeKwinFakeInput, ()> + 'static,
    {
        anyhow::ensure!(
            version >= FAKE_INPUT_VERSION,
            "org_kde_kwin_fake_input v{version} lacks keyboard_keysym (v{FAKE_INPUT_VERSION})"
        );
        let physical_keymap = PhysicalKeymap::new(seat_keymap)?;
        let fake: OrgKdeKwinFakeInput = registry.bind(global, FAKE_INPUT_VERSION, qh, ());
        fake.authenticate("kde-rdp".into(), "remote desktop input".into());
        let mut input = Self {
            fake,
            physical_keymap,
            unicode: UnicodeInput::default(),
            keys: BTreeSet::new(),
            buttons: BTreeSet::new(),
            point: (0, 0),
            target,
            scale: 1.0,
            origin: (0.0, 0.0),
        };
        input.set_geometry(target, scale, (0.0, 0.0));
        Ok(input)
    }

    /// The RDP desktop size (physical pixels), the output scale and the
    /// output's logical position, after a resize or scale change.
    pub fn set_geometry(&mut self, target: DesktopSize, scale: f64, origin: (f64, f64)) {
        self.target = DesktopSize {
            width: target.width.max(1),
            height: target.height.max(1),
        };
        self.scale = if scale.is_finite() && scale > 0.0 {
            scale
        } else {
            1.0
        };
        self.origin = origin;
        self.point = (
            self.point.0.min(u32::from(self.target.width) - 1),
            self.point.1.min(u32::from(self.target.height) - 1),
        );
    }

    /// The last injected pointer position in RDP desktop pixels.
    pub fn point(&self) -> (u32, u32) {
        self.point
    }

    /// Follow a new seat keymap (a layout change or reload). Held keys and
    /// locked modifiers carry over by name. KWin's temporary Unicode keymap
    /// and an unchanged map are ignored.
    pub fn update_keymap(&mut self, text: String) -> anyhow::Result<()> {
        if text == self.physical_keymap.text || is_kwin_scratch_keymap(&text) {
            return Ok(());
        }
        let mut next = PhysicalKeymap::new(text)?;
        for key in &self.keys {
            next.update_key(*key, true);
        }
        let old = &self.physical_keymap.state;
        let old_map = old.get_keymap();
        let next_map = next.state.get_keymap();
        let old_locked = old.serialize_mods(xkb::STATE_MODS_LOCKED);
        let mut locked = 0;
        for index in 0..old_map.num_mods().min(32) {
            if old_locked & (1 << index) != 0 {
                let next_index = next_map.mod_get_index(old_map.mod_get_name(index));
                if next_index < 32 {
                    locked |= 1 << next_index;
                }
            }
        }
        next.state.update_mask(
            next.state.serialize_mods(xkb::STATE_MODS_DEPRESSED),
            0,
            locked,
            0,
            0,
            0,
        );
        self.physical_keymap = next;
        Ok(())
    }

    /// Press or release one evdev key. Repeated presses of a held key and
    /// releases of a key that is not held are dropped, as KWin does.
    pub fn key(&mut self, evdev: u32, pressed: bool) {
        let changed = if pressed {
            self.keys.insert(evdev)
        } else {
            self.keys.remove(&evdev)
        };
        if changed {
            self.fake
                .keyboard_key(evdev, if pressed { PRESSED } else { RELEASED });
            self.physical_keymap.update_key(evdev, pressed);
        }
    }

    fn tap(&mut self, evdev: u32) {
        self.key(evdev, true);
        self.key(evdev, false);
    }

    /// Set Caps and Num Lock to the RDP Synchronize state by tapping
    /// the lock keys whose mirrored state differs (fake input cannot set a
    /// modifier mask).
    pub fn synchronize(&mut self, flags: SynchronizeFlags) {
        for (bit, name, key) in LOCKS {
            let wanted = flags.bits() & bit != 0;
            if self.physical_keymap.locked(name) != wanted && !self.keys.contains(&key) {
                self.tap(key);
            }
        }
    }

    /// Type one committed character with `keyboard_keysym`.
    pub fn unicode(&mut self, ch: char) {
        let keysym = unicode_keysym(ch);
        let caps = caps_sensitive(ch)
            && self.physical_keymap.locked("Lock")
            && !self.keys.contains(&KEY_CAPSLOCK);
        if caps {
            self.tap(KEY_CAPSLOCK);
        }
        let mapped = self.physical_keymap.kwin_keycode_for(keysym).is_some();
        self.fake.keyboard_keysym(keysym.raw(), PRESSED);
        self.fake.keyboard_keysym(keysym.raw(), RELEASED);
        if !mapped {
            // KWin's temporary-keymap path releases every key this device
            // holds (and ignores our release of the keysym). Mirror it.
            for key in std::mem::take(&mut self.keys) {
                self.physical_keymap.update_key(key, false);
            }
        }
        if caps {
            self.tap(KEY_CAPSLOCK);
        }
    }

    /// Move to global logical compositor coordinates.
    pub fn pointer_absolute(&mut self, x: f64, y: f64) {
        self.fake.pointer_motion_absolute(x, y);
    }

    /// Move by logical compositor units.
    pub fn pointer_relative(&mut self, dx: f64, dy: f64) {
        self.fake.pointer_motion(dx, dy);
    }

    /// Press or release an evdev button (BTN_LEFT = 0x110 ...).
    pub fn button(&mut self, btn: u32, pressed: bool) {
        let changed = if pressed {
            self.buttons.insert(btn)
        } else {
            self.buttons.remove(&btn)
        };
        if changed {
            self.fake
                .button(btn, if pressed { PRESSED } else { RELEASED });
        }
    }

    /// Scroll by RDP wheel units (WHEEL_DELTA = 120 per notch) in Wayland
    /// directions (positive = down / right). One notch is axis 10.0.
    pub fn axis(&mut self, horizontal: i32, vertical: i32) {
        for (axis, units) in [(AXIS_VERTICAL, vertical), (AXIS_HORIZONTAL, horizontal)] {
            if units != 0 {
                self.fake.axis(axis, wheel_axis_value(units));
            }
        }
    }

    /// Release every key and button this session pressed (RDP disconnect or
    /// focus loss). The fake-input resource itself stays bound.
    pub fn release_all(&mut self) {
        self.unicode = UnicodeInput::default();
        for key in self.keys.clone() {
            self.key(key, false);
        }
        for btn in self.buttons.clone() {
            self.button(btn, false);
        }
    }

    /// Destroy the resource at session end (KWin releases what it holds).
    pub fn destroy(self) {
        self.fake.destroy();
    }

    /// Dispatch one RDP keyboard event.
    pub fn key_event(&mut self, event: KeyboardEvent) {
        match event {
            KeyboardEvent::Pressed { code, extended } => {
                self.unicode = UnicodeInput::default();
                if let Some(key) = evdev_key(code, extended) {
                    self.key(key, true)
                }
            }
            KeyboardEvent::Released { code, extended } => {
                if let Some(key) = evdev_key(code, extended) {
                    self.key(key, false)
                }
            }
            KeyboardEvent::Synchronize(flags) => self.synchronize(flags),
            KeyboardEvent::UnicodePressed(unit) => {
                if let Some(character) = self.unicode.press(unit) {
                    self.unicode(character)
                }
            }
            // Unicode input is committed text; each scalar is a balanced tap.
            KeyboardEvent::UnicodeReleased(_) => {}
        }
    }

    /// RDP desktop pixels to global logical coordinates.
    fn logical(&self, (x, y): (u32, u32)) -> (f64, f64) {
        (
            f64::from(x) / self.scale + self.origin.0,
            f64::from(y) / self.scale + self.origin.1,
        )
    }

    fn move_to(&mut self, x: u16, y: u16) {
        self.point = (
            u32::from(x).min(u32::from(self.target.width) - 1),
            u32::from(y).min(u32::from(self.target.height) - 1),
        );
        let (x, y) = self.logical(self.point);
        self.pointer_absolute(x, y);
    }

    fn move_by(&mut self, dx: i32, dy: i32) {
        let (width, height) = (i32::from(self.target.width), i32::from(self.target.height));
        let (dx, dy) = (dx.clamp(-width, width), dy.clamp(-height, height));
        self.point = (
            (i64::from(self.point.0) + i64::from(dx)).clamp(0, i64::from(width) - 1) as u32,
            (i64::from(self.point.1) + i64::from(dy)).clamp(0, i64::from(height) - 1) as u32,
        );
        // RDP deltas are physical pixels; KWin moves in logical units.
        if dx != 0 || dy != 0 {
            self.pointer_relative(f64::from(dx) / self.scale, f64::from(dy) / self.scale);
        }
    }

    fn rdp_button(&mut self, button: MouseButton, pressed: bool) {
        if let Some(code) = button_code(button) {
            self.button(code, pressed);
        }
    }

    /// Dispatch one RDP mouse event. KWin ends every fake-input pointer
    /// request with its own pointer frame.
    pub fn mouse_event(&mut self, event: MouseEvent) {
        match event {
            MouseEvent::Move { x, y } => self.move_to(x, y),
            MouseEvent::RelMove { x, y } => self.move_by(x, y),
            MouseEvent::Button {
                x,
                y,
                button,
                pressed,
            } => {
                self.move_to(x, y);
                self.rdp_button(button, pressed);
            }
            MouseEvent::ButtonRel {
                x,
                y,
                button,
                pressed,
            } => {
                self.move_by(x, y);
                self.rdp_button(button, pressed);
            }
            // RDP positive vertical rotation is away from the user (up).
            MouseEvent::VerticalScroll { value } => self.axis(0, -i32::from(value)),
            MouseEvent::HorizontalScroll { value } => self.axis(i32::from(value), 0),
            // FreeRDP's ainput carries the same signed rotation units in x/y.
            MouseEvent::Scroll { x, y } => self.axis(x, y.saturating_neg()),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{io::Write, os::unix::net::UnixStream, time::Duration};
    use wayland_client::{Connection, Proxy, delegate_noop};

    struct Probe;
    delegate_noop!(Probe: ignore WlRegistry);
    delegate_noop!(Probe: ignore OrgKdeKwinFakeInput);

    fn keymap_text(layout: &str, options: Option<String>) -> String {
        let context = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
        xkb::Keymap::new_from_names(&context, "evdev", "pc105", layout, "", options, 0)
            .unwrap()
            .get_as_string(xkb::KEYMAP_FORMAT_TEXT_V1)
    }

    /// A bound `KwinInput` whose requests reach `compositor` as raw wire
    /// words; the registry/bind/authenticate setup is already drained.
    fn wired(layout: &str) -> (UnixStream, Connection, KwinInput) {
        let (socket, mut compositor) = UnixStream::pair().unwrap();
        compositor
            .set_read_timeout(Some(Duration::from_millis(200)))
            .unwrap();
        let connection = Connection::from_socket(socket).unwrap();
        let queue = connection.new_event_queue::<Probe>();
        let handle = queue.handle();
        let registry = connection.display().get_registry(&handle, ());
        let target = DesktopSize {
            width: 1280,
            height: 720,
        };
        assert!(
            KwinInput::bind(
                &registry,
                9,
                5,
                &handle,
                keymap_text(layout, None),
                target,
                1.0
            )
            .is_err(),
            "v5 lacks keyboard_keysym"
        );
        let input = KwinInput::bind(
            &registry,
            9,
            6,
            &handle,
            keymap_text(layout, None),
            target,
            1.0,
        )
        .unwrap();
        let setup = requests(&connection, &mut compositor);
        let fake = input.fake.id().protocol_id();
        // wl_registry.bind(name 9, "org_kde_kwin_fake_input", version 6, id)
        // then authenticate(application, reason).
        assert!(setup.iter().any(|(object, opcode, args)| {
            *object == registry.id().protocol_id() && *opcode == 0 && args[0] == 9
        }));
        assert_eq!(setup.last().map(|r| (r.0, r.1)), Some((fake, AUTHENTICATE)));
        (compositor, connection, input)
    }

    /// Flush and split the requests into (object, opcode, argument words).
    fn requests(connection: &Connection, compositor: &mut UnixStream) -> Vec<(u32, u16, Vec<u32>)> {
        connection.flush().unwrap();
        let mut wire = vec![0u8; 1 << 16];
        let size = compositor.read(&mut wire).unwrap_or(0);
        let word = |at: usize| u32::from_ne_bytes(wire[at..at + 4].try_into().unwrap());
        let mut requests = Vec::new();
        let mut offset = 0;
        while offset + 8 <= size {
            let header = word(offset + 4);
            let length = (header >> 16) as usize;
            let arguments = (offset + 8..offset + length).step_by(4).map(word).collect();
            requests.push((word(offset), (header & 0xffff) as u16, arguments));
            offset += length;
        }
        requests
    }

    /// Requests on the fake-input object as (opcode, argument words).
    fn sent(
        connection: &Connection,
        compositor: &mut UnixStream,
        input: &KwinInput,
    ) -> Vec<(u16, Vec<u32>)> {
        let fake = input.fake.id().protocol_id();
        requests(connection, compositor)
            .into_iter()
            .filter(|(object, _, _)| *object == fake)
            .map(|(_, opcode, args)| (opcode, args))
            .collect()
    }

    // org_kde_kwin_fake_input request opcodes (protocols/fake-input.xml).
    const AUTHENTICATE: u16 = 0;
    const POINTER_MOTION: u16 = 1;
    const BUTTON: u16 = 2;
    const AXIS: u16 = 3;
    const POINTER_MOTION_ABSOLUTE: u16 = 9;
    const KEYBOARD_KEY: u16 = 10;
    const KEYBOARD_KEYSYM: u16 = 12;

    fn fixed(value: f64) -> u32 {
        // wayland-client truncates toward zero.
        (value * 256.0) as i32 as u32
    }

    #[test]
    fn rdp_keys_reach_fake_input_as_evdev_codes() {
        let (mut compositor, connection, mut input) = wired("us");
        for event in [
            // Shift, a, a again (held: dropped), release a, release Shift.
            KeyboardEvent::Pressed {
                code: 0x2a,
                extended: false,
            },
            KeyboardEvent::Pressed {
                code: 0x1e,
                extended: false,
            },
            KeyboardEvent::Pressed {
                code: 0x1e,
                extended: false,
            },
            KeyboardEvent::Released {
                code: 0x1e,
                extended: false,
            },
            KeyboardEvent::Released {
                code: 0x2a,
                extended: false,
            },
            // Extended Right Ctrl, still held at disconnect.
            KeyboardEvent::Pressed {
                code: 0x1d,
                extended: true,
            },
        ] {
            input.key_event(event);
        }
        input.release_all();
        input.release_all();
        assert_eq!(
            sent(&connection, &mut compositor, &input),
            [(42, 1), (30, 1), (30, 0), (42, 0), (97, 1), (97, 0)]
                .map(|(key, state)| (KEYBOARD_KEY, vec![key, state]))
        );
        assert_eq!(
            input
                .physical_keymap
                .state
                .serialize_mods(xkb::STATE_MODS_DEPRESSED),
            0
        );
    }

    #[test]
    fn synchronize_taps_only_mismatched_lock_keys() {
        let (mut compositor, connection, mut input) = wired("us");
        // Caps Lock and Num Lock on: both taps.
        input.key_event(KeyboardEvent::Synchronize(
            SynchronizeFlags::from_bits_truncate(2 | 4),
        ));
        assert!(input.physical_keymap.locked("Lock"));
        assert!(input.physical_keymap.locked("NumLock"));
        // Same state again: nothing. Then Caps off only.
        input.synchronize(SynchronizeFlags::from_bits_truncate(2 | 4));
        input.synchronize(SynchronizeFlags::from_bits_truncate(2));
        assert!(!input.physical_keymap.locked("Lock"));
        assert!(input.physical_keymap.locked("NumLock"));
        let keys: Vec<_> = sent(&connection, &mut compositor, &input)
            .into_iter()
            .map(|(opcode, args)| {
                assert_eq!(opcode, KEYBOARD_KEY);
                (args[0], args[1])
            })
            .collect();
        assert_eq!(
            keys,
            vec![(69, 1), (69, 0), (58, 1), (58, 0), (58, 1), (58, 0)]
        );
    }

    #[test]
    fn unicode_uses_keysyms_and_avoids_caps_lock() {
        let (mut compositor, connection, mut input) = wired("us");
        // 'b' is in the map: a plain keysym tap.
        input.unicode('b');
        // U+263A is not: the Unicode keysym, and KWin's temporary keymap
        // releases held keys, which the mirror forgets as well.
        input.key(29, true);
        input.unicode('☺');
        assert!(input.keys.is_empty());
        // Surrogate pairs decode before typing.
        input.key_event(KeyboardEvent::UnicodePressed(0xd83d));
        input.key_event(KeyboardEvent::UnicodeReleased(0xd83d));
        input.key_event(KeyboardEvent::UnicodePressed(0xde00));
        assert_eq!(
            sent(&connection, &mut compositor, &input),
            vec![
                (KEYBOARD_KEYSYM, vec![0x62, 1]),
                (KEYBOARD_KEYSYM, vec![0x62, 0]),
                (KEYBOARD_KEY, vec![29, 1]),
                (KEYBOARD_KEYSYM, vec![0x100263a, 1]),
                (KEYBOARD_KEYSYM, vec![0x100263a, 0]),
                (KEYBOARD_KEYSYM, vec![0x101f600, 1]),
                (KEYBOARD_KEYSYM, vec![0x101f600, 0]),
            ]
        );
        // With Caps Lock on, cased text is typed with Caps briefly off;
        // digits and symbols are not.
        input.synchronize(SynchronizeFlags::from_bits_truncate(4));
        let _ = requests(&connection, &mut compositor);
        input.unicode('a');
        input.unicode('1');
        assert!(input.physical_keymap.locked("Lock"));
        assert_eq!(
            sent(&connection, &mut compositor, &input),
            vec![
                (KEYBOARD_KEY, vec![58, 1]),
                (KEYBOARD_KEY, vec![58, 0]),
                (KEYBOARD_KEYSYM, vec![0x61, 1]),
                (KEYBOARD_KEYSYM, vec![0x61, 0]),
                (KEYBOARD_KEY, vec![58, 1]),
                (KEYBOARD_KEY, vec![58, 0]),
                (KEYBOARD_KEYSYM, vec![0x31, 1]),
                (KEYBOARD_KEYSYM, vec![0x31, 0]),
            ]
        );
    }

    #[test]
    fn unicode_keysyms_cover_controls_latin_and_other_scripts() {
        for (character, keysym) in [
            ('a', 0x61),
            ('A', 0x41),
            ('@', 0x40),
            ('é', 0xe9),
            ('€', 0x20ac),
            ('あ', 0x1003042),
            ('漢', 0x1006f22),
            ('😀', 0x101f600),
            ('\u{8}', 0xff08),
            ('\t', 0xff09),
            ('\r', 0xff0d),
            ('\n', 0xff0d),
            ('\u{1b}', 0xff1b),
            ('\u{7f}', 0xffff),
        ] {
            assert_eq!(unicode_keysym(character).raw(), keysym, "{character:?}");
        }
        assert!(caps_sensitive('a') && caps_sensitive('Ä') && caps_sensitive('ж'));
        assert!(!caps_sensitive('1') && !caps_sensitive('あ') && !caps_sensitive('\t'));
    }

    #[test]
    fn keysym_lookup_matches_kwins_first_keycode_search() {
        let us = PhysicalKeymap::new(keymap_text("us", None)).unwrap();
        // 'a' and 'A' on KEY_A, '1' on KEY_1 (before KP_1), Return on KEY_ENTER.
        assert_eq!(us.kwin_keycode_for(Keysym::new(0x61)), Some(30));
        assert_eq!(us.kwin_keycode_for(Keysym::new(0x41)), Some(30));
        assert_eq!(us.kwin_keycode_for(Keysym::new(0x31)), Some(2));
        assert_eq!(us.kwin_keycode_for(Keysym::new(0xff0d)), Some(28));
        assert_eq!(us.kwin_keycode_for(Keysym::new(0xe9)), None);
        let se = PhysicalKeymap::new(keymap_text("se", None)).unwrap();
        assert_eq!(se.kwin_keycode_for(Keysym::new(0xe5)), Some(26)); // å
        // Multiple groups: only the effective layout counts.
        let mut both =
            PhysicalKeymap::new(keymap_text("us,se", Some("grp:alt_shift_toggle".into()))).unwrap();
        assert_eq!(both.kwin_keycode_for(Keysym::new(0xe5)), None);
        both.state.update_mask(0, 0, 0, 0, 0, 1);
        assert_eq!(both.kwin_keycode_for(Keysym::new(0xe5)), Some(26));
    }

    #[test]
    fn keymap_updates_keep_locks_and_ignore_kwins_scratch_map() {
        let (_compositor, _connection, mut input) = wired("us");
        input.synchronize(SynchronizeFlags::from_bits_truncate(4));
        input.key(42, true);
        input.update_keymap(keymap_text("se", None)).unwrap();
        let state = &input.physical_keymap.state;
        assert!(input.physical_keymap.locked("Lock"));
        assert_ne!(state.serialize_mods(xkb::STATE_MODS_DEPRESSED), 0);
        // Swedish map, held Shift and locked Caps cancel out on a letter.
        assert_eq!(state.key_get_utf8(34u32.into()), "å");
        input.key(42, false);
        assert_eq!(input.physical_keymap.state.key_get_utf8(34u32.into()), "Å");
        // KWin's temporary Unicode map (keycodes "custom") is not adopted.
        let scratch = r#"xkb_keymap {
  xkb_keycodes "custom" { <CSTM> = 202; };
  xkb_types "(custom)" { include "complete" };
  xkb_compatibility "custom" { include "complete" };
  xkb_symbols "custom" { include "pc+us" key <CSTM> { [ U263A ] }; };
};"#;
        assert!(PhysicalKeymap::new(scratch.into()).is_ok());
        input.update_keymap(scratch.into()).unwrap();
        assert!(input.physical_keymap.text.contains("se"));
        assert!(input.update_keymap("not a keymap".into()).is_err());
    }

    #[test]
    fn inherited_compositor_keymaps_preserve_swedish_jp106_and_groups() {
        for (layout, expected) in [
            ("se", [(26u32, "å"), (39, "ö"), (40, "ä")]),
            ("us", [(26u32, "["), (39, ";"), (40, "'")]),
        ] {
            let keymap = PhysicalKeymap::new(keymap_text(layout, None)).unwrap();
            for (key, character) in expected {
                assert_eq!(keymap.state.key_get_utf8((key + 8).into()), character);
            }
        }
        let context = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
        let jp = xkb::Keymap::new_from_names(&context, "evdev", "jp106", "jp", "", None, 0)
            .unwrap()
            .get_as_string(xkb::KEYMAP_FORMAT_TEXT_V1);
        let mut jp = PhysicalKeymap::new(jp).unwrap();
        for (evdev, character) in [(26u32, "@"), (40, ":"), (89, "\\"), (124, "\\")] {
            assert_eq!(jp.state.key_get_utf8((evdev + 8).into()), character);
        }
        jp.update_key(42, true);
        for (evdev, character) in [(3u32, "\""), (89, "_"), (124, "|")] {
            assert_eq!(jp.state.key_get_utf8((evdev + 8).into()), character);
        }
    }

    #[test]
    fn compositor_keymap_reader_round_trips_and_rejects_bad_data() {
        let text = keymap_text("us", None);
        let mut file = tempfile::tempfile().unwrap();
        file.write_all(text.as_bytes()).unwrap();
        file.write_all(&[0]).unwrap();
        assert_eq!(read_keymap(file, text.len() as u32 + 1).unwrap(), text);
        for (bytes, size) in [
            (b"abc".as_slice(), 4),
            (b"abc".as_slice(), 3),
            (b"abc\0".as_slice(), 1024 * 1024 + 1),
            (b"".as_slice(), 0),
        ] {
            let mut file = tempfile::tempfile().unwrap();
            file.write_all(bytes).unwrap();
            assert!(read_keymap(file, size).is_err());
        }
    }

    #[test]
    fn pointer_maps_rdp_pixels_to_logical_coordinates() {
        let (mut compositor, connection, mut input) = wired("us");
        input.mouse_event(MouseEvent::Move { x: 100, y: 200 });
        // Out of range clamps to the desktop's last pixel.
        input.mouse_event(MouseEvent::Move { x: 5000, y: 5000 });
        assert_eq!(input.point(), (1279, 719));
        // Scale 1.5 on an output at logical (100, 0).
        input.set_geometry(
            DesktopSize {
                width: 1920,
                height: 1080,
            },
            1.5,
            (100.0, 0.0),
        );
        input.mouse_event(MouseEvent::Button {
            x: 961,
            y: 541,
            button: MouseButton::Left,
            pressed: true,
        });
        input.mouse_event(MouseEvent::RelMove { x: 15, y: -30 });
        assert_eq!(input.point(), (976, 511));
        input.mouse_event(MouseEvent::ButtonRel {
            x: 0,
            y: 0,
            button: MouseButton::Left,
            pressed: false,
        });
        // Duplicate releases and unknown buttons send nothing.
        input.button(0x110, false);
        input.mouse_event(MouseEvent::Button {
            x: 0,
            y: 0,
            button: MouseButton::X2,
            pressed: true,
        });
        input.release_all();
        assert_eq!(
            sent(&connection, &mut compositor, &input),
            vec![
                (POINTER_MOTION_ABSOLUTE, vec![fixed(100.0), fixed(200.0)]),
                (POINTER_MOTION_ABSOLUTE, vec![fixed(1279.0), fixed(719.0)]),
                (
                    POINTER_MOTION_ABSOLUTE,
                    vec![fixed(961.0 / 1.5 + 100.0), fixed(541.0 / 1.5)]
                ),
                (BUTTON, vec![0x110, 1]),
                (POINTER_MOTION, vec![fixed(10.0), fixed(-20.0)]),
                (BUTTON, vec![0x110, 0]),
                (POINTER_MOTION_ABSOLUTE, vec![fixed(100.0), 0]),
                (BUTTON, vec![0x114, 1]),
                (BUTTON, vec![0x114, 0]),
            ]
        );
    }

    #[test]
    fn rdp_wheel_reaches_fake_input_at_10_per_notch() {
        let (mut compositor, connection, mut input) = wired("us");
        for event in [
            // RDP positive vertical rotation scrolls up (negative in Wayland).
            MouseEvent::VerticalScroll { value: 120 },
            MouseEvent::VerticalScroll { value: -120 },
            // High-resolution halves move proportionally and immediately.
            MouseEvent::VerticalScroll { value: 60 },
            MouseEvent::HorizontalScroll { value: 90 },
            // FreeRDP ainput wheel uses the same rotation units, bounded.
            MouseEvent::Scroll { x: 0, y: -240 },
            MouseEvent::Scroll { x: i32::MIN, y: 0 },
        ] {
            input.mouse_event(event);
        }
        let axis: Vec<_> = sent(&connection, &mut compositor, &input)
            .into_iter()
            .map(|(opcode, args)| {
                assert_eq!(opcode, AXIS);
                (args[0], args[1] as i32)
            })
            .collect();
        assert_eq!(
            axis,
            vec![
                (0, fixed(-10.0) as i32),
                (0, fixed(10.0) as i32),
                (0, fixed(-5.0) as i32),
                (1, fixed(7.5) as i32),
                (0, fixed(20.0) as i32),
                (1, fixed(-32767.0 * 10.0 / 120.0) as i32),
            ]
        );
    }
}
