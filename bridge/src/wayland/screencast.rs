//! Safe wrapper of the PipeWire screencast consumer in `src/screencast.c`.
//!
//! KWin publishes the output as a PipeWire node (`zkde_screencast_unstable_v1`
//! `stream_output`); the consumer connects to the session's private PipeWire
//! over the broker's descriptor. Content frames are taken one at a time and
//! hold their PipeWire buffer until the [`Held`] owner drops: SHM pixels are
//! copied into the worker's pool right away, a DMA-BUF stays held by its GPU
//! frame ticket while the encoder imports it.
use std::{
    ffi::{CStr, CString, c_char, c_int, c_void},
    os::fd::{BorrowedFd, IntoRawFd, OwnedFd},
};

const DAMAGE_MAX: usize = 16;
const CURSOR_MAX: usize = 256;

#[repr(C)]
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub(super) struct Rect {
    pub(super) x: i32,
    pub(super) y: i32,
    pub(super) width: u32,
    pub(super) height: u32,
}

#[repr(C)]
struct RawFrame {
    buffer: *mut c_void,
    width: u32,
    height: u32,
    stride: u32,
    offset: u32,
    size: u32,
    drm_format: u32,
    modifier: u64,
    dmabuf: i32,
    fd: i32,
    data: *const u8,
    sequence: u64,
    damage_full: i32,
    n_damage: u32,
    damage: [Rect; DAMAGE_MAX],
}

#[repr(C)]
struct RawCursor {
    serial: u64,
    id: u32,
    x: i32,
    y: i32,
    hot_x: i32,
    hot_y: i32,
    bitmap_serial: u64,
    bitmap_width: u32,
    bitmap_height: u32,
    bitmap_stride: u32,
    bitmap_format: u32,
    bitmap: [u8; CURSOR_MAX * CURSOR_MAX * 4],
}

unsafe extern "C" {
    fn kdesc_new(
        fd: c_int,
        serial: u64,
        node: u32,
        width: u32,
        height: u32,
        modifiers: *const u64,
        n_modifiers: u32,
        want_dmabuf: c_int,
        error: *mut c_char,
        length: usize,
    ) -> *mut c_void;
    fn kdesc_event_fd(stream: *mut c_void) -> c_int;
    fn kdesc_clear_event(stream: *mut c_void);
    fn kdesc_status(
        stream: *mut c_void,
        error: *mut c_char,
        length: usize,
        format_serial: *mut u64,
        buffers: *mut u32,
    ) -> c_int;
    fn kdesc_take(stream: *mut c_void, frame: *mut RawFrame) -> c_int;
    fn kdesc_release(buffer: *mut c_void);
    fn kdesc_cursor(stream: *mut c_void, cursor: *mut RawCursor, known_bitmap: u64) -> c_int;
    fn kdesc_renegotiate(stream: *mut c_void, width: u32, height: u32, want_dmabuf: c_int)
    -> c_int;
    fn kdesc_counters(stream: *mut c_void, content: *mut u64, cursor: *mut u64, merged: *mut u64);
    fn kdesc_free(stream: *mut c_void);
    fn kdesc_retarget(
        stream: *mut c_void,
        serial: u64,
        node: u32,
        width: u32,
        height: u32,
        want_dmabuf: c_int,
        error: *mut c_char,
        length: usize,
    ) -> c_int;
    fn kdesc_query_modifiers(
        node: *const c_char,
        fourcc: u32,
        out: *mut u64,
        max: c_int,
        error: *mut c_char,
        length: usize,
    ) -> c_int;
}

fn native_error(error: &[c_char]) -> String {
    // SAFETY: the C side always NUL-terminates within the buffer (snprintf),
    // and the buffer starts zeroed.
    unsafe { CStr::from_ptr(error.as_ptr()) }
        .to_string_lossy()
        .into_owned()
}

/// DRM fourcc XRGB8888, the layout the modifier list is queried for.
pub(super) const XRGB8888: u32 = 0x3432_5258;

/// DMA-BUF modifiers the render node imports for XRGB8888 (EGL query).
/// NVIDIA offers only block-linear modifiers and no LINEAR; KWin cannot
/// allocate implicit-modifier buffers there, so the list must be explicit.
pub(super) fn query_modifiers(node: &std::path::Path) -> anyhow::Result<Vec<u64>> {
    let path = CString::new(node.as_os_str().as_encoded_bytes())?;
    let mut out = vec![0u64; 64];
    let mut error = [0 as c_char; 256];
    // SAFETY: valid NUL-terminated path, `out` holds 64 entries, error buffer sized.
    let n = unsafe {
        kdesc_query_modifiers(
            path.as_ptr(),
            XRGB8888,
            out.as_mut_ptr(),
            out.len() as c_int,
            error.as_mut_ptr(),
            error.len(),
        )
    };
    anyhow::ensure!(n >= 0, "{}", native_error(&error));
    out.truncate(n as usize);
    Ok(out)
}

/// The PipeWire stream state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum StreamState {
    Connecting,
    Paused,
    Streaming,
}

/// One PipeWire consumer stream. Dropping it disconnects; held buffers stay
/// valid until their owners drop.
pub(super) struct Stream {
    raw: *mut c_void,
    cursor: Box<RawCursor>,
}
// SAFETY: every C entry point takes the PipeWire thread-loop lock; the handle
// itself is only used by the capture worker thread.
unsafe impl Send for Stream {}

impl Stream {
    /// Consume KWin's node `serial` (preferred) or `node` over `fd`.
    pub(super) fn connect(
        fd: OwnedFd,
        (serial, node): (u64, u32),
        (width, height): (u32, u32),
        modifiers: &[u64],
        want_dmabuf: bool,
    ) -> anyhow::Result<Self> {
        let mut error = [0 as c_char; 256];
        // SAFETY: the fd is transferred to (and always consumed by) the C
        // side; modifiers point to `len` entries; the error buffer is sized.
        let raw = unsafe {
            kdesc_new(
                fd.into_raw_fd(),
                serial,
                node,
                width,
                height,
                modifiers.as_ptr(),
                modifiers.len() as u32,
                c_int::from(want_dmabuf),
                error.as_mut_ptr(),
                error.len(),
            )
        };
        anyhow::ensure!(!raw.is_null(), "{}", native_error(&error));
        // SAFETY: RawCursor is plain old data; all-zero is a valid value.
        let cursor = unsafe { Box::<RawCursor>::new_zeroed().assume_init() };
        Ok(Self { raw, cursor })
    }
    /// Readable when a frame, cursor update or state change is pending.
    pub(super) fn wake(&self) -> BorrowedFd<'_> {
        // SAFETY: the eventfd lives as long as the stream.
        unsafe { BorrowedFd::borrow_raw(kdesc_event_fd(self.raw)) }
    }
    pub(super) fn clear_wake(&self) {
        // SAFETY: valid handle.
        unsafe { kdesc_clear_event(self.raw) }
    }
    /// The stream state, its format generation and its buffer count, or the
    /// stream's terminal error.
    pub(super) fn status(&self) -> anyhow::Result<(StreamState, u64, u32)> {
        let mut error = [0 as c_char; 256];
        let (mut serial, mut buffers) = (0, 0);
        // SAFETY: valid handle and buffers.
        let state = unsafe {
            kdesc_status(
                self.raw,
                error.as_mut_ptr(),
                error.len(),
                &mut serial,
                &mut buffers,
            )
        };
        Ok((
            match state {
                0 => StreamState::Connecting,
                1 => StreamState::Paused,
                2 => StreamState::Streaming,
                _ => anyhow::bail!("{}", native_error(&error)),
            },
            serial,
            buffers,
        ))
    }
    /// The newest content frame not taken yet, with the damage merged since
    /// the previous take.
    pub(super) fn take(&self) -> Option<Frame> {
        // SAFETY: RawFrame is plain old data and filled by the C side.
        let mut raw = unsafe { std::mem::MaybeUninit::<RawFrame>::zeroed().assume_init() };
        // SAFETY: valid handle; `raw` is writable.
        if unsafe { kdesc_take(self.raw, &mut raw) } != 1 {
            return None;
        }
        let held = Held { raw: raw.buffer };
        let damage = (raw.damage_full == 0)
            .then(|| raw.damage[..(raw.n_damage as usize).min(DAMAGE_MAX)].to_vec());
        Some(Frame {
            width: raw.width,
            height: raw.height,
            stride: raw.stride,
            offset: raw.offset,
            drm_format: raw.drm_format,
            modifier: raw.modifier,
            memory: if raw.dmabuf != 0 {
                Memory::DmaBuf(raw.fd)
            } else if raw.data.is_null() {
                Memory::Unmapped
            } else {
                Memory::Shm(raw.data)
            },
            damage,
            held,
        })
    }
    /// The latest cursor metadata; the bitmap is included only when it is
    /// newer than `known_bitmap`.
    pub(super) fn cursor(&mut self, known_bitmap: u64) -> Cursor<'_> {
        // SAFETY: valid handle; the boxed cursor is writable.
        let copied = unsafe { kdesc_cursor(self.raw, &mut *self.cursor, known_bitmap) } == 1;
        let c = &*self.cursor;
        let length = (c.bitmap_stride as usize * c.bitmap_height as usize).min(c.bitmap.len());
        Cursor {
            serial: c.serial,
            visible: c.id != 0,
            position: (c.x, c.y),
            hotspot: (c.hot_x, c.hot_y),
            bitmap_serial: c.bitmap_serial,
            bitmap: copied.then(|| Bitmap {
                width: c.bitmap_width,
                height: c.bitmap_height,
                stride: c.bitmap_stride,
                rgba: c.bitmap_format == RGBA_FORMAT,
                pixels: &c.bitmap[..length],
            }),
        }
    }
    /// Renegotiate the stream: a fixed size, and DMA-BUF or SHM buffers.
    pub(super) fn renegotiate(
        &self,
        (width, height): (u32, u32),
        dmabuf: bool,
    ) -> anyhow::Result<()> {
        // SAFETY: valid handle.
        let result = unsafe { kdesc_renegotiate(self.raw, width, height, c_int::from(dmabuf)) };
        anyhow::ensure!(result >= 0, "screencast renegotiation failed ({result})");
        Ok(())
    }
    /// Follow a new KWin node (a new zkde stream) on the same connection.
    pub(super) fn retarget(
        &self,
        (serial, node): (u64, u32),
        (width, height): (u32, u32),
        dmabuf: bool,
    ) -> anyhow::Result<()> {
        let mut error = [0 as c_char; 256];
        // SAFETY: valid handle and error buffer.
        let result = unsafe {
            kdesc_retarget(
                self.raw,
                serial,
                node,
                width,
                height,
                c_int::from(dmabuf),
                error.as_mut_ptr(),
                error.len(),
            )
        };
        anyhow::ensure!(result >= 0, "{}", native_error(&error));
        Ok(())
    }
    /// Content buffers, cursor-only buffers, and content buffers replaced by
    /// a newer one before they were taken.
    pub(super) fn counters(&self) -> (u64, u64, u64) {
        let (mut content, mut cursor, mut merged) = (0, 0, 0);
        // SAFETY: valid handle and outputs.
        unsafe { kdesc_counters(self.raw, &mut content, &mut cursor, &mut merged) };
        (content, cursor, merged)
    }
}
impl Drop for Stream {
    fn drop(&mut self) {
        // SAFETY: the handle is freed exactly once; held buffers keep the
        // C side alive until they are released.
        unsafe { kdesc_free(self.raw) }
    }
}

/// `SPA_VIDEO_FORMAT_RGBA`, KWin's cursor bitmap format.
const RGBA_FORMAT: u32 = 11;

/// One dequeued PipeWire content buffer; dropping it gives it back to KWin.
pub(crate) struct Held {
    raw: *mut c_void,
}
// SAFETY: kdesc_release takes the thread-loop lock and may run on any thread.
unsafe impl Send for Held {}
unsafe impl Sync for Held {}
impl Drop for Held {
    fn drop(&mut self) {
        // SAFETY: released exactly once.
        unsafe { kdesc_release(self.raw) }
    }
}

pub(super) enum Memory {
    /// MemFd pixels, readable while the frame is held.
    Shm(*const u8),
    /// A DMA-BUF descriptor, borrowed while the frame is held.
    DmaBuf(i32),
    /// MemFd whose mapping does not cover the chunk.
    Unmapped,
}

/// One content frame. Pixels or the DMA-BUF stay valid while `held` lives.
pub(super) struct Frame {
    pub(super) width: u32,
    pub(super) height: u32,
    pub(super) stride: u32,
    pub(super) offset: u32,
    pub(super) drm_format: u32,
    pub(super) modifier: u64,
    pub(super) memory: Memory,
    /// Changed rectangles since the previous taken frame; None is unknown.
    pub(super) damage: Option<Vec<Rect>>,
    pub(super) held: Held,
}
impl Frame {
    /// The MemFd pixels (`stride * height` bytes).
    pub(super) fn pixels(&self) -> Option<&[u8]> {
        let Memory::Shm(data) = self.memory else {
            return None;
        };
        let length = crate::buffer_length(self.width, self.height, self.stride).ok()?;
        // SAFETY: the C side checked that the mapping covers offset + stride *
        // height, and the buffer stays dequeued (KWin does not write it) while
        // `held` lives, which outlives this borrow.
        Some(unsafe { std::slice::from_raw_parts(data, length) })
    }
    /// A duplicate of the DMA-BUF descriptor.
    pub(super) fn dmabuf(&self) -> anyhow::Result<OwnedFd> {
        let Memory::DmaBuf(fd) = self.memory else {
            anyhow::bail!("not a DMA-BUF frame");
        };
        // SAFETY: the fd stays open while the buffer is held.
        Ok(unsafe { BorrowedFd::borrow_raw(fd) }.try_clone_to_owned()?)
    }
}

pub(super) struct Bitmap<'a> {
    pub(super) width: u32,
    pub(super) height: u32,
    pub(super) stride: u32,
    /// RGBA byte order (KWin); otherwise unsupported.
    pub(super) rgba: bool,
    pub(super) pixels: &'a [u8],
}

pub(super) struct Cursor<'a> {
    pub(super) serial: u64,
    pub(super) visible: bool,
    /// Output buffer pixels (RDP desktop pixels).
    pub(super) position: (i32, i32),
    pub(super) hotspot: (i32, i32),
    pub(super) bitmap_serial: u64,
    pub(super) bitmap: Option<Bitmap<'a>>,
}
