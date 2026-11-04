//! The separate RDP pointer from PipeWire `SPA_META_Cursor` metadata.
//!
//! KWin's stream in `pointer=metadata` mode reports the cursor with every
//! buffer that carries it: id (0 = hidden or off this output), the hotspot
//! position in output buffer pixels (RDP desktop pixels), the hotspot, and a
//! premultiplied RGBA bitmap only when the cursor image changed. Cursor
//! motion alone produces cursor-only buffers (docs/kde-findings.md section 3).
//!
//! `CursorTracker` maps that state onto the `CursorSnapshot` without any
//! PipeWire objects, so the rules are unit-tested:
//! - the first position of a stream is a baseline, never a warp;
//! - KWin reports every position, including those the bridge's own fake
//!   input caused. A position within one pixel of a recently injected point,
//!   or shortly after relative motion (whose result the bridge cannot know),
//!   is a baseline; any other new position is a compositor warp, which
//!   advances the position generation and moves the client's local pointer;
//! - the worker publishes the snapshot once per batch of updates.
use super::CursorSnapshot;
use ironrdp_server::DisplayUpdate;
use std::{
    collections::VecDeque,
    sync::Arc,
    time::{Duration, Instant},
};

/// KWin's metadata bitmap limit (`bitmapSize` 256x256).
pub(super) const CURSOR_LIMIT: u32 = 256;
/// Larger shapes need the Large Pointer update.
const RGBA_POINTER_LIMIT: u32 = 96;
/// Fractional-scale rounding moves the compositor's position by at most this.
const ECHO_TOLERANCE: i64 = 1;
/// How long an injected point can take to come back as a cursor position.
const ECHO_WINDOW: Duration = Duration::from_millis(750);
/// Bound on own points remembered.
const INJECTED_HISTORY: usize = 256;

/// Convert a premultiplied RGBA cursor (R, G, B, A bytes, top-down) to the
/// straight-alpha, bottom-up BGRA of MS-RDPBCGR Color/New Pointer masks.
/// `None` means the image is fully transparent.
pub(super) fn cursor_image(
    bytes: &[u8],
    width: u32,
    height: u32,
    stride: u32,
) -> anyhow::Result<Option<Vec<u8>>> {
    anyhow::ensure!(
        (1..=CURSOR_LIMIT).contains(&width) && (1..=CURSOR_LIMIT).contains(&height),
        "invalid cursor geometry"
    );
    let row = width as usize * 4;
    anyhow::ensure!(
        stride as usize >= row && stride.is_multiple_of(4),
        "invalid cursor stride"
    );
    anyhow::ensure!(
        bytes.len() == stride as usize * height as usize,
        "cursor image length mismatch"
    );
    let mut bgra = Vec::with_capacity(row * height as usize);
    let mut opaque = false;
    for line in bytes.chunks_exact(stride as usize).rev() {
        for pixel in line[..row].as_chunks::<4>().0 {
            let alpha = u32::from(pixel[3]);
            opaque |= alpha != 0;
            let color = |channel: u8| {
                (u32::from(channel) * 255 + alpha / 2)
                    .checked_div(alpha)
                    .unwrap_or(0)
                    .min(255) as u8
            };
            bgra.extend_from_slice(&[color(pixel[2]), color(pixel[1]), color(pixel[0]), pixel[3]]);
        }
    }
    Ok(opaque.then_some(bgra))
}

fn pointer_update(width: u32, height: u32, hotspot: (u16, u16), data: Vec<u8>) -> DisplayUpdate {
    let (width, height) = (width as u16, height as u16);
    let (hot_x, hot_y) = hotspot;
    if u32::from(width) > RGBA_POINTER_LIMIT || u32::from(height) > RGBA_POINTER_LIMIT {
        DisplayUpdate::LargePointer(ironrdp_server::LargePointer {
            cache_index: 0,
            width,
            height,
            hot_x,
            hot_y,
            data,
        })
    } else {
        DisplayUpdate::RGBAPointer(ironrdp_server::RGBAPointer {
            cache_index: 0,
            width,
            height,
            hot_x,
            hot_y,
            data,
        })
    }
}

/// Crop a bottom-up BGRA image to its top-left `max`x`max` pixels.
fn crop(data: &[u8], (width, height): (u32, u32), max: u32) -> (Vec<u8>, (u32, u32)) {
    let (w, h) = (width.min(max), height.min(max));
    if (w, h) == (width, height) {
        return (data.to_vec(), (width, height));
    }
    let row = width as usize * 4;
    let mut out = Vec::with_capacity(w as usize * h as usize * 4);
    // Bottom-up: the top rows are the last ones.
    for line in data.chunks_exact(row).skip((height - h) as usize) {
        out.extend_from_slice(&line[..w as usize * 4]);
    }
    (out, (w, h))
}

/// One cursor metadata update, decoupled from PipeWire.
pub(super) struct CursorUpdate<'a> {
    pub(super) visible: bool,
    pub(super) position: (i32, i32),
    pub(super) hotspot: (i32, i32),
    /// A new premultiplied RGBA bitmap: pixels, width, height, stride.
    pub(super) bitmap: Option<(&'a [u8], u32, u32, u32)>,
}

/// The PipeWire-free mapping from cursor metadata to the snapshot.
#[derive(Debug)]
pub(super) struct CursorTracker {
    /// The client's pointer limit; 0 means it draws no pointer.
    max: u16,
    /// The converted bitmap (bottom-up straight BGRA) and its size, or None
    /// while it is transparent or unknown.
    image: Option<(Vec<u8>, (u32, u32))>,
    have_bitmap: bool,
    hotspot: (i32, i32),
    visible: bool,
    last_position: Option<(u16, u16)>,
    /// Points the bridge injected, with when.
    injected: VecDeque<((u32, u32), Instant)>,
    relative_until: Option<Instant>,
}

impl CursorTracker {
    pub(super) fn new() -> Self {
        Self {
            max: 0,
            image: None,
            have_bitmap: false,
            hotspot: (0, 0),
            visible: false,
            last_position: None,
            injected: VecDeque::new(),
            relative_until: None,
        }
    }
    /// A new client activation: its pointer limit, and a fresh baseline.
    pub(super) fn activate(&mut self, max: u16, snapshot: &mut CursorSnapshot) {
        self.max = max;
        self.last_position = None;
        // Re-resolve the shape for the new limit.
        snapshot.shape = None;
        self.rebuild(snapshot);
        self.apply(snapshot);
    }
    /// A new stream: KWin resends the bitmap; everything is a baseline.
    pub(super) fn restart(&mut self, snapshot: &mut CursorSnapshot) {
        self.image = None;
        self.have_bitmap = false;
        self.visible = false;
        self.last_position = None;
        snapshot.shape = None;
        self.apply(snapshot);
    }
    /// Record where the bridge's fake input moved the cursor.
    pub(super) fn injected(&mut self, point: (u32, u32), now: Instant) {
        while self
            .injected
            .front()
            .is_some_and(|(_, at)| now.duration_since(*at) > ECHO_WINDOW)
            || self.injected.len() >= INJECTED_HISTORY
        {
            self.injected.pop_front();
        }
        self.injected.push_back((point, now));
    }
    /// Relative motion: the resulting point is unknown until KWin reports it.
    pub(super) fn relative(&mut self, now: Instant) {
        self.relative_until = Some(now + ECHO_WINDOW);
    }
    pub(super) fn update(
        &mut self,
        update: &CursorUpdate<'_>,
        desktop: (u16, u16),
        snapshot: &mut CursorSnapshot,
        now: Instant,
    ) {
        if let Some((pixels, width, height, stride)) = update.bitmap {
            self.have_bitmap = true;
            self.image = match cursor_image(pixels, width, height, stride) {
                Ok(image) => image.map(|data| (data, (width, height))),
                Err(error) => {
                    eprintln!("ignoring cursor bitmap: {error}");
                    None
                }
            };
            snapshot.shape = None;
        }
        if update.hotspot != self.hotspot {
            self.hotspot = update.hotspot;
            snapshot.shape = None;
        }
        self.visible = update.visible;
        if snapshot.shape.is_none() {
            self.rebuild(snapshot);
        }
        if update.visible {
            self.position(update.position, desktop, snapshot, now);
        }
        self.apply(snapshot);
    }
    fn position(
        &mut self,
        (x, y): (i32, i32),
        desktop: (u16, u16),
        snapshot: &mut CursorSnapshot,
        now: Instant,
    ) {
        let clamp = |value: i32, size: u16| value.clamp(0, i32::from(size.max(1)) - 1) as u16;
        let point = (clamp(x, desktop.0), clamp(y, desktop.1));
        let Some(previous) = self.last_position.replace(point) else {
            snapshot.position = point;
            return;
        };
        if previous == point {
            return;
        }
        let echo = self.injected.iter().any(|&((ix, iy), at)| {
            now.duration_since(at) <= ECHO_WINDOW
                && (i64::from(ix) - i64::from(point.0)).abs() <= ECHO_TOLERANCE
                && (i64::from(iy) - i64::from(point.1)).abs() <= ECHO_TOLERANCE
        }) || self.relative_until.is_some_and(|until| now < until);
        snapshot.position = point;
        if !echo {
            snapshot.position_generation = snapshot.position_generation.wrapping_add(1);
        }
    }
    fn rebuild(&self, snapshot: &mut CursorSnapshot) {
        let Some((data, size)) = &self.image else {
            snapshot.shape = None;
            return;
        };
        let max = u32::from(self.max).clamp(1, CURSOR_LIMIT);
        let (data, (width, height)) = crop(data, *size, max);
        let hotspot = (
            self.hotspot.0.clamp(0, width as i32 - 1) as u16,
            self.hotspot.1.clamp(0, height as i32 - 1) as u16,
        );
        snapshot.shape = Some(Arc::new(pointer_update(width, height, hotspot, data)));
    }
    /// Derive the snapshot's separation and visibility.
    fn apply(&self, snapshot: &mut CursorSnapshot) {
        snapshot.separate = self.max != 0 && self.have_bitmap;
        snapshot.visible = snapshot.separate && self.visible && snapshot.shape.is_some();
    }
}

/// The snapshot and the copy last sent to the RDP streams.
pub(super) struct CursorPublisher {
    pub(super) tx: tokio::sync::watch::Sender<CursorSnapshot>,
    pub(super) snapshot: CursorSnapshot,
    published: CursorSnapshot,
}
impl CursorPublisher {
    pub(super) fn new(tx: tokio::sync::watch::Sender<CursorSnapshot>) -> Self {
        Self {
            tx,
            snapshot: CursorSnapshot::default(),
            published: CursorSnapshot::default(),
        }
    }
    /// Whether the snapshot differs from the published one in anything the
    /// RDP streams act on. A baseline position alone is not sent: streams
    /// read positions only with a new position generation (a warp).
    fn changed(&self) -> bool {
        let (next, sent) = (&self.snapshot, &self.published);
        (next.separate, next.visible, next.position_generation)
            != (sent.separate, sent.visible, sent.position_generation)
            || match (&next.shape, &sent.shape) {
                (Some(a), Some(b)) => !same_shape(a, b),
                (a, b) => a.is_some() != b.is_some(),
            }
    }
    /// Publish once per batch, and only real changes.
    pub(super) fn publish(&mut self, force: bool) {
        if !force && !self.changed() {
            return;
        }
        // Keep the published Arc for an identical rebuilt shape, so streams
        // do not resend it.
        if let (Some(next), Some(sent)) = (&self.snapshot.shape, &self.published.shape)
            && !Arc::ptr_eq(next, sent)
            && same_shape(next, sent)
        {
            self.snapshot.shape = Some(sent.clone());
        }
        self.snapshot.generation = self.snapshot.generation.wrapping_add(1);
        self.published = self.snapshot.clone();
        self.tx.send_replace(self.snapshot.clone());
    }
}

fn same_shape(a: &DisplayUpdate, b: &DisplayUpdate) -> bool {
    match (a, b) {
        (DisplayUpdate::RGBAPointer(a), DisplayUpdate::RGBAPointer(b)) => {
            (a.width, a.height, a.hot_x, a.hot_y) == (b.width, b.height, b.hot_x, b.hot_y)
                && a.data == b.data
        }
        (DisplayUpdate::LargePointer(a), DisplayUpdate::LargePointer(b)) => {
            (a.width, a.height, a.hot_x, a.hot_y) == (b.width, b.height, b.hot_x, b.hot_y)
                && a.data == b.data
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 24x24 premultiplied RGBA cursor whose top row differs from the rest.
    fn rgba(value: u8) -> Vec<u8> {
        let mut pixels = vec![0u8; 24 * 24 * 4];
        for (index, pixel) in pixels.as_chunks_mut::<4>().0.iter_mut().enumerate() {
            let alpha = if index < 24 { 255 } else { 128 };
            pixel.copy_from_slice(&[value / 2, value / 2, value / 2, alpha]);
        }
        pixels
    }
    fn update(bitmap: Option<&[u8]>, position: (i32, i32)) -> CursorUpdate<'_> {
        CursorUpdate {
            visible: true,
            position,
            hotspot: (3, 1),
            bitmap: bitmap.map(|pixels| (pixels, 24, 24, 96)),
        }
    }
    const DESKTOP: (u16, u16) = (640, 480);
    fn shown(snapshot: &mut CursorSnapshot, now: Instant) -> CursorTracker {
        let mut tracker = CursorTracker::new();
        tracker.activate(96, snapshot);
        let pixels = rgba(10);
        tracker.update(&update(Some(&pixels), (100, 100)), DESKTOP, snapshot, now);
        assert!(snapshot.separate && snapshot.visible);
        tracker
    }

    #[test]
    fn cursor_image_converts_premultiplied_rgba_to_straight_bottom_up_bgra() {
        // Two rows of one pixel: R, G, B, A premultiplied, top-down.
        let image = cursor_image(&[30, 20, 10, 255, 20, 10, 5, 128], 1, 2, 4).unwrap();
        assert_eq!(image, Some(vec![10, 20, 40, 128, 10, 20, 30, 255]));
        // Row padding past the width is skipped.
        let image = cursor_image(&[3, 2, 1, 255, 9, 9, 9, 9], 1, 1, 8).unwrap();
        assert_eq!(image, Some(vec![1, 2, 3, 255]));
        assert_eq!(cursor_image(&[0; 16], 2, 2, 8).unwrap(), None);
        let bytes = vec![255u8; 257 * 4];
        for (width, height, stride, length) in [
            (0, 1, 4, 0),
            (257, 1, 257 * 4, 257 * 4),
            (1, 257, 4, 257 * 4),
            (2, 1, 4, 4),
            (1, 1, 6, 6),
            (1, 2, 4, 4),
        ] {
            assert!(
                cursor_image(&bytes[..length], width, height, stride).is_err(),
                "{width}x{height} stride {stride} length {length}"
            );
        }
    }

    #[test]
    fn first_position_is_a_baseline_and_foreign_moves_are_warps() {
        let now = Instant::now();
        let mut snapshot = CursorSnapshot::default();
        let mut tracker = shown(&mut snapshot, now);
        assert_eq!(snapshot.position, (100, 100));
        assert_eq!(snapshot.position_generation, 0);
        // An application warp.
        tracker.update(&update(None, (700, -5)), DESKTOP, &mut snapshot, now);
        assert_eq!(snapshot.position, (639, 0));
        assert_eq!(snapshot.position_generation, 1);
        // An unchanged position is not a new warp.
        tracker.update(&update(None, (700, -5)), DESKTOP, &mut snapshot, now);
        assert_eq!(snapshot.position_generation, 1);
        // A new activation starts with a baseline again.
        tracker.activate(96, &mut snapshot);
        tracker.update(&update(None, (30, 40)), DESKTOP, &mut snapshot, now);
        assert_eq!(snapshot.position_generation, 1);
    }

    #[test]
    fn echoes_of_injected_and_relative_motion_are_not_warps() {
        let now = Instant::now();
        let mut snapshot = CursorSnapshot::default();
        let mut tracker = shown(&mut snapshot, now);
        // Several injected points come back late, one rounded by a pixel.
        for x in 101..110 {
            tracker.injected((x, 100), now);
        }
        for x in [103, 106, 110] {
            tracker.update(&update(None, (x, 100)), DESKTOP, &mut snapshot, now);
        }
        assert_eq!(snapshot.position_generation, 0);
        assert_eq!(snapshot.position, (110, 100));
        // A stale injected point no longer explains a position.
        let later = now + ECHO_WINDOW * 2;
        tracker.update(&update(None, (105, 100)), DESKTOP, &mut snapshot, later);
        assert_eq!(snapshot.position_generation, 1);
        // Relative motion lands somewhere the bridge cannot predict.
        tracker.relative(later);
        tracker.update(&update(None, (300, 300)), DESKTOP, &mut snapshot, later);
        assert_eq!(snapshot.position_generation, 1);
    }

    #[test]
    fn hidden_cursor_and_transparent_bitmap_hide_the_pointer() {
        let now = Instant::now();
        let mut snapshot = CursorSnapshot::default();
        let mut tracker = shown(&mut snapshot, now);
        let mut hidden = update(None, (0, 0));
        hidden.visible = false;
        tracker.update(&hidden, DESKTOP, &mut snapshot, now);
        assert!(snapshot.separate && !snapshot.visible);
        // Reappearing elsewhere without own motion is a warp.
        tracker.update(&update(None, (50, 60)), DESKTOP, &mut snapshot, now);
        assert!(snapshot.visible);
        assert_eq!(snapshot.position_generation, 1);
        let transparent = vec![0u8; 24 * 24 * 4];
        tracker.update(
            &update(Some(&transparent), (50, 60)),
            DESKTOP,
            &mut snapshot,
            now,
        );
        assert!(snapshot.separate && !snapshot.visible);
    }

    #[test]
    fn hotspot_change_rebuilds_the_shape_and_client_limit_crops() {
        let now = Instant::now();
        let mut snapshot = CursorSnapshot::default();
        let mut tracker = shown(&mut snapshot, now);
        let first = snapshot.shape.clone().unwrap();
        let mut moved = update(None, (100, 100));
        moved.hotspot = (5, 6);
        tracker.update(&moved, DESKTOP, &mut snapshot, now);
        let second = snapshot.shape.clone().unwrap();
        assert!(!same_shape(&first, &second));
        match &*second {
            DisplayUpdate::RGBAPointer(p) => assert_eq!((p.hot_x, p.hot_y), (5, 6)),
            _ => panic!("expected an RGBA pointer"),
        }
        // A 16-pixel client limit crops the 24x24 shape to its top left.
        tracker.activate(16, &mut snapshot);
        match snapshot.shape.as_deref() {
            Some(DisplayUpdate::RGBAPointer(p)) => {
                assert_eq!((p.width, p.height), (16, 16));
                assert_eq!(p.data.len(), 16 * 16 * 4);
                // The top row (alpha 255) is the last bottom-up row.
                assert_eq!(p.data[p.data.len() - 1], 255);
                assert_eq!(p.data[3], 128);
            }
            _ => panic!("expected an RGBA pointer"),
        }
        // A client without pointer support gets the cursor in the video.
        tracker.activate(0, &mut snapshot);
        assert!(!snapshot.separate && !snapshot.visible);
    }

    #[test]
    fn publisher_sends_only_changes_and_keeps_identical_shapes() {
        let (tx, rx) = tokio::sync::watch::channel(CursorSnapshot::default());
        let mut publisher = CursorPublisher::new(tx);
        let now = Instant::now();
        let mut tracker = CursorTracker::new();
        tracker.activate(96, &mut publisher.snapshot);
        let pixels = rgba(10);
        tracker.update(
            &update(Some(&pixels), (1, 1)),
            DESKTOP,
            &mut publisher.snapshot,
            now,
        );
        publisher.publish(false);
        let first = rx.borrow().clone();
        assert_eq!(first.generation, 1);
        // The same bitmap again (a new stream resends it) is no change.
        tracker.update(
            &update(Some(&pixels), (1, 1)),
            DESKTOP,
            &mut publisher.snapshot,
            now,
        );
        publisher.publish(false);
        assert_eq!(rx.borrow().generation, 1);
        // A baseline-only move is not sent either.
        tracker.injected((2, 2), now);
        tracker.update(&update(None, (2, 2)), DESKTOP, &mut publisher.snapshot, now);
        publisher.publish(false);
        assert_eq!(rx.borrow().generation, 1);
        tracker.update(
            &update(None, (300, 2)),
            DESKTOP,
            &mut publisher.snapshot,
            now,
        );
        publisher.publish(false);
        let warped = rx.borrow().clone();
        assert_eq!(warped.generation, 2);
        assert!(Arc::ptr_eq(
            warped.shape.as_ref().unwrap(),
            first.shape.as_ref().unwrap()
        ));
    }
}
