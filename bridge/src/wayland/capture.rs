//! Pooled capture slots that SHM screencast frames are copied into.
//!
//! KWin's PipeWire stream has only two to four buffers and skips frames when
//! the consumer holds them, so a MemFd frame is copied into a pooled slot at
//! once and its PipeWire buffer returned. Snapshots then share the slot
//! without copying, exactly like the earlier compositor-written SHM pool.
use super::screencast::Rect;
use crate::{SnapshotOwner, TileSet, buffer_length};
use ironrdp_server::{BitmapUpdate, DesktopSize};
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

/// Width, height and stride of the copied frames.
pub(super) type ShmLayout = (u32, u32, u32);

/// One private mapping the worker copies frames into. Snapshots share it
/// without copying through `Bytes::from_owner`, which keeps an `Arc` clone
/// alive for as long as any slice of the snapshot lives. The worker writes
/// the slot again only once the pool holds its sole reference.
pub(super) struct ShmSlot {
    mapping: *mut libc::c_void,
    length: usize,
    /// Advanced whenever the worker may write the slot; a capture log
    /// record is trusted only while its generation is still current.
    generation: Arc<AtomicU64>,
}
// SAFETY: only the single capture worker writes the slot, and only while no
// snapshot references it (strong count one); everyone else reads.
unsafe impl Send for ShmSlot {}
unsafe impl Sync for ShmSlot {}
impl ShmSlot {
    fn new(length: usize) -> anyhow::Result<Self> {
        // SAFETY: a fresh private anonymous mapping of `length` bytes.
        let mapping = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                length,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        anyhow::ensure!(
            mapping != libc::MAP_FAILED,
            "capture mmap failed: {}",
            std::io::Error::last_os_error()
        );
        Ok(Self {
            mapping,
            length,
            generation: Arc::new(AtomicU64::new(0)),
        })
    }
    fn bytes(&self) -> &[u8] {
        // SAFETY: mapping covers length readable bytes for this object's lifetime.
        unsafe { std::slice::from_raw_parts(self.mapping.cast(), self.length) }
    }
    /// Copy `height` rows of `row` bytes from `source` (rows `stride` apart)
    /// into the slot, tightly packed.
    ///
    /// # Safety
    /// Nobody else may reference the slot (strong count one).
    unsafe fn write(&self, source: &[u8], stride: usize, row: usize, height: usize) {
        // SAFETY: the caller guarantees exclusive access; length checked by
        // the pool against row * height.
        let target =
            unsafe { std::slice::from_raw_parts_mut(self.mapping.cast::<u8>(), self.length) };
        if stride == row {
            target[..row * height].copy_from_slice(&source[..row * height]);
        } else {
            for (line, out) in source
                .chunks(stride)
                .zip(target.chunks_exact_mut(row))
                .take(height)
            {
                out.copy_from_slice(&line[..row]);
            }
        }
    }
}
impl Drop for ShmSlot {
    fn drop(&mut self) {
        self.generation.store(u64::MAX, Ordering::Release);
        // SAFETY: this object owns the exact mapping returned by mmap.
        unsafe { libc::munmap(self.mapping, self.length) };
    }
}

/// The `Bytes` owner of a zero-copy snapshot.
struct SlotView(Arc<ShmSlot>);
impl AsRef<[u8]> for SlotView {
    fn as_ref(&self) -> &[u8] {
        self.0.bytes()
    }
}

/// Where the next frame is copied.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ShmTarget {
    /// A pooled slot; snapshots of it share its pixels.
    Shared(usize),
    /// Every slot is held: the snapshot is a private copy.
    Scratch,
}

/// Shared slots. Diff bases and the encoder normally hold at most two
/// snapshots, so a third slot keeps capture from copying twice.
pub(super) const SHARED_SLOTS: usize = 3;

/// Capture slots for one layout. A slot still referenced by a delivered
/// snapshot (the latest desktop, a diff base or an encoder input) is never
/// written again; when every slot is held, the frame is copied into a fresh
/// private allocation instead.
pub(super) struct CapturePool {
    pub(super) layout: ShmLayout,
    slots: Vec<Arc<ShmSlot>>,
}
impl CapturePool {
    pub(super) fn new(layout: ShmLayout) -> Self {
        Self {
            layout,
            slots: Vec::new(),
        }
    }
    /// Choose a slot nobody reads and advance its generation.
    fn acquire(&mut self) -> anyhow::Result<ShmTarget> {
        let (width, height, _) = self.layout;
        let target = if let Some(index) = self
            .slots
            .iter()
            .position(|slot| Arc::strong_count(slot) == 1)
        {
            ShmTarget::Shared(index)
        } else if self.slots.len() < SHARED_SLOTS {
            self.slots.push(Arc::new(ShmSlot::new(buffer_length(
                width,
                height,
                width * 4,
            )?)?));
            ShmTarget::Shared(self.slots.len() - 1)
        } else {
            return Ok(ShmTarget::Scratch);
        };
        if let ShmTarget::Shared(index) = target {
            // The last snapshot clone released the slot with a Release
            // decrement; order its reads before the slot is written again.
            std::sync::atomic::fence(Ordering::Acquire);
            self.slots[index].generation.fetch_add(1, Ordering::AcqRel);
        }
        Ok(target)
    }
    /// Copy one frame (`stride * height` bytes in the pool's layout) and
    /// snapshot it. A pooled slot is shared zero-copy and returns the owner
    /// that vouches for the slot's change history.
    pub(super) fn capture(
        &mut self,
        pixels: &[u8],
        desktop: DesktopSize,
    ) -> anyhow::Result<(ShmTarget, BitmapUpdate, Option<SnapshotOwner>)> {
        let (width, height, stride) = self.layout;
        anyhow::ensure!(
            pixels.len() >= buffer_length(width, height, stride)?,
            "SHM frame shorter than its layout"
        );
        let target = self.acquire()?;
        let ShmTarget::Shared(index) = target else {
            let image = crate::capture_bitmap(
                &pixels[..buffer_length(width, height, stride)?],
                width,
                height,
                stride,
                false,
                desktop,
                false,
            )?;
            return Ok((target, image, None));
        };
        let slot = &self.slots[index];
        // SAFETY: acquire() selected a slot only the pool references.
        unsafe { slot.write(pixels, stride as usize, width as usize * 4, height as usize) };
        let data = bytes::Bytes::from_owner(SlotView(slot.clone()));
        let image =
            crate::shared_capture_bitmap(data, width, height, width * 4, false, desktop, false)?;
        // A converted desktop is a separate allocation and releases the slot.
        let owner = (Arc::strong_count(slot) > 1).then(|| SnapshotOwner {
            generation: slot.generation.clone(),
            value: slot.generation.load(Ordering::Acquire),
        });
        Ok((target, image, owner))
    }
    #[cfg(test)]
    fn slots(&self) -> usize {
        self.slots.len()
    }
    #[cfg(test)]
    fn busy(&self, target: ShmTarget) -> bool {
        match target {
            ShmTarget::Shared(index) => Arc::strong_count(&self.slots[index]) > 1,
            ShmTarget::Scratch => false,
        }
    }
}

/// Damage reported for one capture as the tiles it may have changed. None
/// when the report cannot bound the change.
pub(super) fn damage_tiles(image: &BitmapUpdate, damage: &[Rect]) -> Option<TileSet> {
    if damage.is_empty() {
        return None;
    }
    let mut tiles = TileSet::for_image(image);
    for rect in damage {
        // Clip to the frame; negative origins shrink the rectangle.
        let (x, width) = clip(rect.x, rect.width);
        let (y, height) = clip(rect.y, rect.height);
        if width > 0 && height > 0 {
            tiles.insert_rect(x, y, width, height);
        }
    }
    Some(tiles)
}

fn clip(origin: i32, length: u32) -> (u32, u32) {
    if origin >= 0 {
        (origin as u32, length)
    } else {
        (0, length.saturating_sub(origin.unsigned_abs()))
    }
}

/// Changed-tile comparisons that use the capture log instead of reading the
/// complete desktops on the RDP runtime thread.
impl super::Desktop {
    fn capture_changes(&self, previous: &BitmapUpdate, image: &BitmapUpdate) -> Option<TileSet> {
        self.inner
            .capture_metadata
            .lock()
            .unwrap()
            .as_ref()?
            .changes_between(previous, image)
    }
    /// `bitmap_damage` limited to the tiles the capture worker saw change.
    pub(crate) fn capture_damage(
        &self,
        previous: Option<&BitmapUpdate>,
        image: &BitmapUpdate,
    ) -> Vec<BitmapUpdate> {
        let hint = previous.and_then(|previous| self.capture_changes(previous, image));
        crate::bitmap_damage_hinted(previous, image, hint.as_ref())
    }
    /// Pixel equality limited to the tiles the capture worker saw change.
    pub(crate) fn same_capture(&self, previous: &BitmapUpdate, image: &BitmapUpdate) -> bool {
        let hint = self.capture_changes(previous, image);
        crate::same_pixels(previous, image, hint.as_ref())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    const WIDTH: u32 = 128;
    const HEIGHT: u32 = 128;
    const STRIDE: u32 = WIDTH * 4;
    const DESKTOP: DesktopSize = DesktopSize {
        width: WIDTH as u16,
        height: HEIGHT as u16,
    };

    fn capture(
        pool: &mut CapturePool,
        value: u8,
    ) -> (ShmTarget, BitmapUpdate, Option<SnapshotOwner>) {
        let (_, _, stride) = pool.layout;
        let frame = vec![value; (stride * HEIGHT) as usize];
        pool.capture(&frame, DESKTOP).unwrap()
    }
    fn filled(image: &BitmapUpdate, value: u8) -> bool {
        image.data.iter().all(|byte| *byte == value)
    }
    fn pool() -> CapturePool {
        CapturePool::new((WIDTH, HEIGHT, STRIDE))
    }

    #[test]
    fn pooled_slot_is_never_rewritten_while_a_diff_base_holds_it() {
        let mut pool = pool();
        let (first, base, owner) = capture(&mut pool, 1);
        assert!(owner.is_some(), "a native snapshot shares its slot");
        // The bases the RDP side keeps: VideoUpdates::last_image and
        // fallback_previous, Updates::previous, and a queued partial tile.
        let last_image = base.clone();
        let fallback_previous = base.clone();
        let previous = base.clone();
        let partial = crate::bitmap_damage(None, &base)[0]
            .sub(
                64,
                64,
                std::num::NonZeroU16::new(64).unwrap(),
                std::num::NonZeroU16::new(64).unwrap(),
            )
            .unwrap();
        drop(base);
        let mut latest: Option<BitmapUpdate> = None;
        for frame in 0..(SHARED_SLOTS as u8 * 4) {
            // The worker still holds `latest` while the next frame is copied.
            let (target, image, _) = capture(&mut pool, 10 + frame);
            assert_ne!(target, first, "a held slot was written again");
            assert!(filled(&image, 10 + frame));
            for held in [&last_image, &fallback_previous, &previous] {
                assert!(filled(held, 1));
                assert!(!crate::same_pixels(held, &image, None));
                let damage = crate::bitmap_damage(Some(held), &image);
                assert_eq!(damage.len(), 1);
                assert_eq!(damage[0].width.get(), WIDTH as u16);
            }
            assert!(filled(&partial, 1));
            if let Some(latest) = &latest {
                assert!(filled(latest, 9 + frame));
            }
            latest = Some(image);
        }
        assert!(latest.is_some());
        drop((last_image, fallback_previous, previous, partial));
        drop(latest);
        // With every holder gone the first slot is reused, not a new one.
        let (target, image, _) = capture(&mut pool, 99);
        assert_eq!(target, first);
        assert!(filled(&image, 99));
        assert_eq!(pool.slots(), SHARED_SLOTS);
    }

    #[test]
    fn exhausted_pool_copies_privately_without_touching_held_slots() {
        let mut pool = pool();
        let held: Vec<_> = (0..SHARED_SLOTS as u8)
            .map(|value| capture(&mut pool, value).1)
            .collect();
        let (target, copied, owner) = capture(&mut pool, 77);
        assert_eq!(target, ShmTarget::Scratch);
        assert!(owner.is_none(), "a private copy cannot vouch for history");
        assert!(filled(&copied, 77));
        let (target, next, _) = capture(&mut pool, 78);
        assert_eq!(target, ShmTarget::Scratch);
        assert!(filled(&copied, 77));
        assert!(filled(&next, 78));
        for (value, image) in held.iter().enumerate() {
            assert!(filled(image, value as u8));
        }
        assert_eq!(pool.slots(), SHARED_SLOTS);
        drop(held);
        assert!(matches!(capture(&mut pool, 5).0, ShmTarget::Shared(_)));
    }

    #[test]
    fn padded_kwin_rows_are_packed_into_the_native_layout() {
        let mut pool = CapturePool::new((WIDTH, HEIGHT, STRIDE + 64));
        let mut frame = vec![0xEEu8; ((STRIDE + 64) * HEIGHT) as usize];
        for row in frame.as_chunks_mut::<{ (STRIDE + 64) as usize }>().0 {
            row[..STRIDE as usize].fill(3);
        }
        let (target, image, owner) = pool.capture(&frame, DESKTOP).unwrap();
        assert!(matches!(target, ShmTarget::Shared(_)) && owner.is_some());
        assert_eq!(image.stride.get(), STRIDE as usize);
        assert!(filled(&image, 3));
        assert!(pool.capture(&frame[..100], DESKTOP).is_err());
    }

    #[test]
    fn slot_reuse_retires_capture_log_history() {
        let mut pool = pool();
        let mut log = crate::CaptureLog::default();
        let now = Instant::now();
        let (first, image, owner) = capture(&mut pool, 1);
        log.push(&image, now, None, owner);
        let (_, next, owner) = capture(&mut pool, 2);
        log.push(&next, now, Some(TileSet::for_image(&next)), owner);
        assert!(log.changes_between(&image, &next).is_some());
        let address = image.data.as_ptr();
        drop(image);
        // The released slot is rewritten: a snapshot at the same address is a
        // different desktop, so the old record must not describe it.
        let (target, reused, owner) = capture(&mut pool, 3);
        assert_eq!(target, first);
        assert_eq!(reused.data.as_ptr(), address);
        assert!(log.changes_between(&reused, &next).is_none());
        log.push(&reused, now, None, owner);
        assert!(log.changes_between(&next, &reused).is_none());
        drop(reused);
        assert!(!pool.busy(first));
    }

    #[test]
    fn kwin_damage_maps_to_tiles_and_is_clipped() {
        let image = capture(&mut pool(), 0).1;
        assert!(damage_tiles(&image, &[]).is_none());
        let tiles = damage_tiles(
            &image,
            &[
                Rect {
                    x: -10,
                    y: -10,
                    width: 20,
                    height: 20,
                },
                Rect {
                    x: 0,
                    y: 0,
                    width: 0,
                    height: 5,
                },
            ],
        )
        .unwrap();
        assert_eq!(tiles.len(), 1);
        let all = damage_tiles(
            &image,
            &[Rect {
                x: 0,
                y: 0,
                width: WIDTH,
                height: HEIGHT,
            }],
        )
        .unwrap();
        let mut full = TileSet::for_image(&image);
        full.insert_rect(0, 0, WIDTH, HEIGHT);
        assert!(full.len() > 1);
        assert_eq!(all.len(), full.len());
    }
}
