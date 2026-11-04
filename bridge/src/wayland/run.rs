//! The capture worker: Wayland and PipeWire startup, the event loop and
//! input teardown.
//!
//! KWin pushes frames: `zkde_screencast_unstable_v1.stream_output` publishes
//! the output as a PipeWire node, which `screencast.c` consumes over the
//! broker's PipeWire descriptor. A quiet desktop sends no frames at all
//! (0 fps is normal), so liveness comes from `wl_display.sync` health probes
//! and the retained desktop is re-sent as a heartbeat once per second.
//! Resizing renegotiates the stream size, which KWin's virtual output
//! follows; output management is never used (docs/kde-findings.md section 4).
use super::capture::{CapturePool, damage_tiles};
use super::cursor::{CursorPublisher, CursorTracker, CursorUpdate};
use super::kwin_input::KwinInput;
use super::protocols::screencast::{
    zkde_screencast_stream_unstable_v1::ZkdeScreencastStreamUnstableV1,
    zkde_screencast_unstable_v1::{Pointer, ZkdeScreencastUnstableV1},
};
use super::pump::{pump, pump_wake};
use super::screencast::{self, Frame, Memory, Rect, Stream};
use super::state::{State, StreamEvents};
use super::{
    CaptureMetadata, Command, Commands, CursorSnapshot, GpuCapture, GpuFrame, LatestFrames,
    OutputStatus,
};
use crate::TileSet;
use ironrdp_server::{DesktopSize, DisplayUpdate};
use std::{
    os::{fd::OwnedFd, unix::net::UnixStream},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU32, Ordering},
        mpsc,
    },
    time::{Duration, Instant},
};
use wayland_client::{Connection, EventQueue, QueueHandle, protocol::wl_output};

pub(super) struct WorkerChannels {
    pub(super) commands: Arc<Commands>,
    pub(super) frames: LatestFrames,
    pub(super) ready: mpsc::SyncSender<anyhow::Result<()>>,
    pub(super) alive: Arc<AtomicBool>,
    pub(super) frame_rate: Arc<AtomicU32>,
    pub(super) gpu: Arc<GpuCapture>,
    pub(super) cursor_tx: tokio::sync::watch::Sender<CursorSnapshot>,
    pub(super) capture_metadata: CaptureMetadata,
    pub(super) status: Arc<OutputStatus>,
}

/// Heartbeat and compositor health probe interval while nothing changes.
const HEARTBEAT: Duration = Duration::from_secs(1);
/// A `wl_display.sync` must complete within this.
const HEALTH_TIMEOUT: Duration = Duration::from_secs(3);
/// KWin must create the stream and the first frame within this.
const STARTUP_TIMEOUT: Duration = Duration::from_secs(8);

pub(super) fn run(
    socket: UnixStream,
    pipewire: OwnedFd,
    name: &str,
    target: DesktopSize,
    scale: f64,
    worker: WorkerChannels,
) -> anyhow::Result<()> {
    let mut worker = CaptureLoop::start(socket, pipewire, name, target, scale, worker)?;
    let result = worker.capture();
    worker.finish();
    result
}

/// The stream size to negotiate: KWin accepts 200x200..10000x10000; the
/// bridge's own limit is MAX_DIMENSION. A smaller client is told the real
/// size through the usual Resize derived from the frames.
fn stream_size(target: DesktopSize) -> (u32, u32) {
    let clamp = |value: u16| u32::from(value).clamp(200, crate::MAX_DIMENSION);
    (clamp(target.width), clamp(target.height))
}

/// How the event loop continues.
enum Flow {
    Next,
    /// The desktop stopped; the worker ends successfully.
    Stop,
}

/// How KWin reports the cursor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CursorMode {
    /// SPA_META_Cursor; the RDP client draws the pointer.
    Metadata,
    /// Drawn into the frames, for clients without pointer support.
    Embedded,
}
impl CursorMode {
    fn for_client(max: u16) -> Self {
        if max == 0 {
            Self::Embedded
        } else {
            Self::Metadata
        }
    }
    fn pointer(self) -> Pointer {
        match self {
            Self::Metadata => Pointer::Metadata,
            Self::Embedded => Pointer::Embedded,
        }
    }
}

/// The DMA-BUF path: KWin's render node and the modifiers it can import.
struct GpuPath {
    device: u64,
    modifiers: Vec<u64>,
}
impl GpuPath {
    fn probe() -> anyhow::Result<Self> {
        let node = crate::gpu_capture::render_node()?;
        let modifiers = screencast::query_modifiers(&node.path)?;
        anyhow::ensure!(
            !modifiers.is_empty(),
            "{} offers no XRGB8888 DMA-BUF modifiers",
            node.path.display()
        );
        Ok(Self {
            device: node.device,
            modifiers,
        })
    }
}

struct CaptureLoop {
    target: DesktopSize,
    scale: f64,
    c: Connection,
    q: EventQueue<State>,
    h: QueueHandle<State>,
    s: State,
    output: wl_output::WlOutput,
    screencast: ZkdeScreencastUnstableV1,
    zkde: Option<ZkdeScreencastStreamUnstableV1>,
    stream: Stream,
    cursor_mode: CursorMode,
    input: KwinInput,
    geometry: (DesktopSize, f64, (f64, f64)),
    clipboard: Option<crate::clipboard::ClipboardAttachment>,
    telemetry: crate::performance::Telemetry,
    commands: Arc<Commands>,
    frames: LatestFrames,
    ready: Option<mpsc::SyncSender<anyhow::Result<()>>>,
    alive: Arc<AtomicBool>,
    frame_rate: Arc<AtomicU32>,
    gpu: Arc<GpuCapture>,
    gpu_path: Option<GpuPath>,
    /// Whether the stream currently offers DMA-BUF buffers.
    stream_dmabuf: bool,
    capture_metadata: CaptureMetadata,
    status: Arc<OutputStatus>,
    cursor: CursorTracker,
    cursor_publisher: CursorPublisher,
    cursor_max: u16,
    known_bitmap: u64,
    known_cursor: u64,
    refresh_requested: bool,
    // Capture loop state.
    started: Instant,
    shm: Option<CapturePool>,
    gpu_sequence: u64,
    next_capture: Instant,
    frame_waiting: bool,
    heartbeat_at: Instant,
    health_pending: Option<Instant>,
    latest: Option<Arc<DisplayUpdate>>,
    /// Whether the stream's damage for the next frame is relative to the
    /// pixels of `latest`: every frame since was taken, none was dropped.
    damage_continuous: bool,
    /// A full comparison periodically audits compositor damage.
    damage_audit_at: Instant,
    format_serial: u64,
    stream_counters: (u64, u64, u64),
    logged_scale: bool,
}

/// Wait for a condition while dispatching, with a monotonic deadline.
fn roundtrip_until(
    c: &Connection,
    q: &mut EventQueue<State>,
    s: &mut State,
    what: &str,
    mut done: impl FnMut(&State) -> bool,
) -> anyhow::Result<()> {
    let started = Instant::now();
    while !done(s) {
        pump(c, q, s)?;
        anyhow::ensure!(
            started.elapsed() < Duration::from_secs(5),
            "{what} timed out"
        );
    }
    Ok(())
}

/// Ask KWin for a stream of `output` and wait for its PipeWire node.
fn create_stream(
    c: &Connection,
    q: &mut EventQueue<State>,
    s: &mut State,
    screencast: &ZkdeScreencastUnstableV1,
    output: &wl_output::WlOutput,
    mode: CursorMode,
) -> anyhow::Result<(ZkdeScreencastStreamUnstableV1, (u64, u32))> {
    s.stream = StreamEvents::default();
    let h = q.handle();
    let stream = screencast.stream_output(output, mode.pointer().into(), &h, ());
    roundtrip_until(c, q, s, "KWin screencast stream", |s| {
        s.stream.node.is_some() || s.stream.failed.is_some() || s.stream.closed
    })?;
    if let Some(error) = s.stream.failed.take() {
        stream.close();
        anyhow::bail!("KWin refused the screencast: {error}");
    }
    anyhow::ensure!(!s.stream.closed, "KWin closed the screencast stream");
    Ok((
        stream,
        (s.stream.serial.unwrap_or(0), s.stream.node.unwrap_or(0)),
    ))
}

impl CaptureLoop {
    /// Bind the compositor globals, the fake-input device and the screencast
    /// stream of the selected output.
    fn start(
        socket: UnixStream,
        pipewire: OwnedFd,
        name: &str,
        target: DesktopSize,
        scale: f64,
        worker: WorkerChannels,
    ) -> anyhow::Result<Self> {
        let WorkerChannels {
            commands,
            frames,
            ready,
            alive,
            frame_rate,
            gpu,
            cursor_tx,
            capture_metadata,
            status,
        } = worker;
        let c = Connection::from_socket(socket)?;
        let mut q = c.new_event_queue();
        let h = q.handle();
        let mut s = State {
            registry: Some(c.display().get_registry(&h, ())),
            ..State::default()
        };
        c.display().sync(&h, ());
        roundtrip_until(&c, &mut q, &mut s, "Wayland registry", |s| s.synced)?;
        // Output names and xdg-output geometry follow the globals.
        s.synced = false;
        c.display().sync(&h, ());
        roundtrip_until(&c, &mut q, &mut s, "Wayland output metadata", |s| {
            s.synced && s.find_output(name).is_some()
        })
        .map_err(|_| {
            anyhow::anyhow!(
                "requested output {} unavailable",
                if name.is_empty() { "(first)" } else { name }
            )
        })?;
        let record = s.find_output(name).unwrap();
        anyhow::ensure!(
            record.scale > 0 && record.normal_transform,
            "bridge requires positive scale and normal transform"
        );
        let (global, output) = (record.global, record.output.clone());
        let output_scale = record.effective_scale();
        let origin = record.origin();
        s.selected_output = Some(global);
        let screencast = s
            .screencast
            .clone()
            .ok_or_else(|| anyhow::anyhow!("missing zkde_screencast_unstable_v1 v6"))?;
        let (fake_global, fake_version) = s
            .fake_input
            .ok_or_else(|| anyhow::anyhow!("missing org_kde_kwin_fake_input"))?;
        let seat = s
            .seat
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("missing Wayland seat"))?
            .clone();
        s.seat_keyboard = Some(seat.get_keyboard(&h, ()));
        roundtrip_until(&c, &mut q, &mut s, "compositor keyboard map", |s| {
            s.seat_keymap.is_some() || s.keymap_error.is_some()
        })?;
        if let Some(error) = s.keymap_error.take() {
            anyhow::bail!(error);
        }
        let mut input = KwinInput::bind(
            s.registry.as_ref().unwrap(),
            fake_global,
            fake_version,
            &h,
            s.seat_keymap.take().unwrap(),
            target,
            output_scale,
        )?;
        input.set_geometry(target, output_scale, origin);
        let cursor_mode = CursorMode::Metadata;
        let (zkde, node) = create_stream(&c, &mut q, &mut s, &screencast, &output, cursor_mode)?;
        let gpu_path = match GpuPath::probe() {
            Ok(path) => Some(path),
            Err(error) => {
                eprintln!("DMA-BUF screencast unavailable; using SHM: {error}");
                None
            }
        };
        let stream = Stream::connect(
            pipewire,
            node,
            stream_size(target),
            gpu_path.as_ref().map_or(&[][..], |path| &path.modifiers),
            false,
        )?;
        let telemetry = crate::performance::Telemetry::from_env();
        let now = Instant::now();
        Ok(Self {
            target,
            scale,
            c,
            q,
            h,
            s,
            output,
            screencast,
            zkde: Some(zkde),
            stream,
            cursor_mode,
            input,
            geometry: (target, output_scale, origin),
            clipboard: None,
            telemetry,
            commands,
            frames,
            ready: Some(ready),
            alive,
            frame_rate,
            gpu,
            gpu_path,
            stream_dmabuf: false,
            capture_metadata,
            status,
            cursor: CursorTracker::new(),
            cursor_publisher: CursorPublisher::new(cursor_tx),
            cursor_max: 0,
            known_bitmap: 0,
            known_cursor: 0,
            refresh_requested: false,
            started: now,
            shm: None,
            gpu_sequence: 0,
            next_capture: now,
            frame_waiting: true,
            heartbeat_at: now + HEARTBEAT,
            health_pending: None,
            latest: None,
            damage_continuous: false,
            damage_audit_at: now,
            format_serial: 0,
            stream_counters: (0, 0, 0),
            logged_scale: false,
        })
    }

    /// Run until the desktop stops or an error ends the worker.
    fn capture(&mut self) -> anyhow::Result<()> {
        loop {
            if let Flow::Stop = self.iteration()? {
                return Ok(());
            }
        }
    }

    fn iteration(&mut self) -> anyhow::Result<Flow> {
        if !self.alive.load(Ordering::Acquire) {
            return Ok(Flow::Stop);
        }
        let timeout = self.timeout();
        pump_wake(
            &self.c,
            &mut self.q,
            &mut self.s,
            timeout,
            Some(&self.commands),
            Some(self.stream.wake()),
        )?;
        self.stream.clear_wake();
        self.frame_waiting = true;
        self.check_compositor()?;
        if let Flow::Stop = self.drain_commands()? {
            return Ok(Flow::Stop);
        }
        self.update_gpu()?;
        self.update_cursor();
        if self.refresh_requested {
            if self.latest.is_some() {
                self.frames.send_replace(self.latest.clone());
            }
            self.refresh_requested = false;
        }
        let now = Instant::now();
        let suppressed = self.status.suppressed.load(Ordering::Acquire) && self.latest.is_some();
        // While the client suppresses output, KWin's newest frame waits in
        // the stream (one buffer) and only heartbeats are sent.
        if !suppressed && now >= self.next_capture {
            self.frame_waiting = false;
            if let Some(frame) = self.stream.take() {
                self.publish(frame)?;
            }
        }
        self.heartbeat(now)?;
        if self.ready.is_some() {
            anyhow::ensure!(
                self.started.elapsed() < STARTUP_TIMEOUT,
                "no screencast frame from KWin within {STARTUP_TIMEOUT:?}"
            );
        }
        Ok(Flow::Next)
    }

    /// Wait until the next heartbeat, health deadline or capture time.
    fn timeout(&self) -> i32 {
        let now = Instant::now();
        let mut until = self.heartbeat_at;
        if let Some(sent) = self.health_pending {
            until = until.min(sent + HEALTH_TIMEOUT);
        }
        if self.frame_waiting {
            until = until.min(self.next_capture.max(now));
        }
        if self.ready.is_some() {
            until = until.min(now + Duration::from_millis(100));
        }
        until.saturating_duration_since(now).as_millis().min(1000) as i32
    }

    /// Stream, output and keymap changes reported by KWin and PipeWire.
    fn check_compositor(&mut self) -> anyhow::Result<()> {
        if let Some(error) = self.s.stream.failed.take() {
            anyhow::bail!("KWin screencast failed: {error}");
        }
        anyhow::ensure!(!self.s.stream.closed, "KWin closed the screencast stream");
        let (_, format_serial, buffers) = self.stream.status()?;
        if format_serial != self.format_serial {
            // New buffers: damage history starts over.
            self.format_serial = format_serial;
            self.damage_continuous = false;
            self.telemetry.count("capture_stream_formats", 1);
            self.telemetry
                .count("capture_stream_buffers", u64::from(buffers));
        }
        if let Some(text) = self.s.seat_keymap.take() {
            self.input.update_keymap(text)?;
        }
        if let Some(error) = self.s.keymap_error.take() {
            anyhow::bail!(error);
        }
        if let Some(output) = self.s.output() {
            let geometry = (self.target, output.effective_scale(), output.origin());
            if geometry != self.geometry {
                self.geometry = geometry;
                self.input.set_geometry(geometry.0, geometry.1, geometry.2);
            }
            if !self.logged_scale && (geometry.1 - self.scale).abs() > 1.0 / 256.0 {
                // Scale belongs to the Plasma display settings: changing it
                // would need output management, which pins the mode.
                eprintln!(
                    "RDP client scale {:.2} differs from the KWin output scale {:.2}; keeping KWin's",
                    self.scale, geometry.1
                );
                self.logged_scale = true;
            }
        }
        Ok(())
    }

    fn wants_gpu(&self) -> bool {
        self.gpu.requested.load(Ordering::Acquire)
            && !self.gpu.failed.load(Ordering::Acquire)
            && self.gpu_path.is_some()
    }

    /// Follow GPU capture requests by renegotiating DMA-BUF or SHM buffers.
    fn update_gpu(&mut self) -> anyhow::Result<()> {
        if self.gpu.requested.load(Ordering::Acquire) && self.gpu_path.is_none() {
            // The encoder falls back to SHM input for the whole session.
            self.gpu.failed.store(true, Ordering::Release);
            self.gpu.requested.store(false, Ordering::Release);
        }
        let wants = self.wants_gpu();
        if wants != self.stream_dmabuf {
            self.stream_dmabuf = wants;
            if !wants {
                self.gpu.latest.lock().unwrap().take();
            }
            self.stream.renegotiate(stream_size(self.target), wants)?;
            self.damage_continuous = false;
            self.telemetry.count("capture_gpu_switches", 1);
        }
        Ok(())
    }

    /// Adjacent resizes were already collapsed in the bounded mailbox.
    fn drain_commands(&mut self) -> anyhow::Result<Flow> {
        for _ in 0..256 {
            if !self.alive.load(Ordering::Acquire) {
                return Ok(Flow::Stop);
            }
            match self.commands.pop() {
                Some(Command::Resize(next, next_scale)) => {
                    self.scale = next_scale;
                    if next != self.target {
                        self.target = next;
                        self.stream
                            .renegotiate(stream_size(next), self.stream_dmabuf)?;
                        self.input
                            .set_geometry(next, self.geometry.1, self.geometry.2);
                        self.geometry.0 = next;
                        self.telemetry.count("capture_resizes", 1);
                    }
                }
                Some(Command::Status) => self.refresh_requested = true,
                Some(Command::Cursor(max)) => self.set_cursor_max(max)?,
                Some(Command::Key(event)) => self.input.key_event(event),
                Some(Command::Mouse(event)) => {
                    let relative = matches!(
                        event,
                        ironrdp_server::MouseEvent::RelMove { .. }
                            | ironrdp_server::MouseEvent::ButtonRel { .. }
                    );
                    self.input.mouse_event(event);
                    let now = Instant::now();
                    if relative {
                        self.cursor.relative(now);
                    } else {
                        self.cursor.injected(self.input.point(), now);
                    }
                }
                Some(Command::Release) => self.input.release_all(),
                Some(Command::Clipboard(next)) => {
                    if next.attach_connection(self.c.clone()) {
                        self.clipboard = Some(next);
                    }
                }
                Some(Command::Stop) => return Ok(Flow::Stop),
                None => break,
            }
        }
        Ok(Flow::Next)
    }

    /// A client activation: its pointer limit selects how KWin reports the
    /// cursor, and the client's pointer cache starts empty.
    fn set_cursor_max(&mut self, max: u16) -> anyhow::Result<()> {
        self.cursor_max = max;
        let mode = CursorMode::for_client(max);
        if mode != self.cursor_mode {
            self.restart_stream(mode)?;
        }
        self.cursor
            .activate(max, &mut self.cursor_publisher.snapshot);
        self.cursor_publisher.publish(true);
        Ok(())
    }

    /// A new zkde stream (cursor mode is fixed per stream) on the same
    /// PipeWire connection.
    fn restart_stream(&mut self, mode: CursorMode) -> anyhow::Result<()> {
        if let Some(old) = self.zkde.take() {
            old.close();
        }
        let (zkde, node) = create_stream(
            &self.c,
            &mut self.q,
            &mut self.s,
            &self.screencast,
            &self.output,
            mode,
        )?;
        self.zkde = Some(zkde);
        self.cursor_mode = mode;
        self.stream
            .retarget(node, stream_size(self.target), self.stream_dmabuf)?;
        self.cursor.restart(&mut self.cursor_publisher.snapshot);
        self.known_bitmap = 0;
        self.damage_continuous = false;
        self.next_capture = Instant::now();
        self.telemetry.count("capture_stream_restarts", 1);
        Ok(())
    }

    /// Map the stream's latest cursor metadata onto the snapshot.
    fn update_cursor(&mut self) {
        let desktop = (self.target.width, self.target.height);
        let cursor = self.stream.cursor(self.known_bitmap);
        if cursor.serial == self.known_cursor && cursor.bitmap.is_none() {
            return;
        }
        self.known_cursor = cursor.serial;
        self.known_bitmap = cursor.bitmap_serial;
        let bitmap = cursor
            .bitmap
            .as_ref()
            .filter(|bitmap| bitmap.rgba)
            .map(|bitmap| (bitmap.pixels, bitmap.width, bitmap.height, bitmap.stride));
        let update = CursorUpdate {
            visible: cursor.visible,
            position: cursor.position,
            hotspot: cursor.hotspot,
            bitmap,
        };
        if self.cursor_mode == CursorMode::Metadata {
            self.cursor.update(
                &update,
                desktop,
                &mut self.cursor_publisher.snapshot,
                Instant::now(),
            );
            self.cursor_publisher.publish(false);
        }
    }

    /// Re-send the retained desktop and probe compositor health.
    fn heartbeat(&mut self, now: Instant) -> anyhow::Result<()> {
        if self.s.synced {
            self.health_pending = None;
        }
        anyhow::ensure!(
            self.health_pending
                .is_none_or(|sent| sent.elapsed() < HEALTH_TIMEOUT),
            "compositor health check timed out"
        );
        if now < self.heartbeat_at {
            return Ok(());
        }
        if self.latest.is_some() {
            self.frames.send_replace(self.latest.clone());
        }
        if self.health_pending.is_none() {
            self.s.synced = false;
            self.c.display().sync(&self.h, ());
            self.health_pending = Some(now);
        }
        self.heartbeat_at = now + HEARTBEAT;
        self.telemetry.count(
            if self.status.suppressed.load(Ordering::Acquire) {
                "capture_suppressed_heartbeats"
            } else {
                "capture_idle_heartbeats"
            },
            1,
        );
        let counters = self.stream.counters();
        let (content, cursor, merged) = (
            counters.0 - self.stream_counters.0,
            counters.1 - self.stream_counters.1,
            counters.2 - self.stream_counters.2,
        );
        self.stream_counters = counters;
        self.telemetry
            .count("capture_stream_content_buffers", content);
        self.telemetry
            .count("capture_stream_cursor_buffers", cursor);
        self.telemetry
            .count("capture_stream_merged_buffers", merged);
        self.telemetry.report("capture", now);
        Ok(())
    }

    /// Snapshot a content frame, publish a changed desktop and schedule the
    /// next take.
    fn publish(&mut self, frame: Frame) -> anyhow::Result<()> {
        let captured_at = Instant::now();
        let (width, height) = (frame.width, frame.height);
        crate::buffer_length(width, height, width * 4)?;
        let desktop = DesktopSize {
            width: width as u16,
            height: height as u16,
        };
        if desktop != self.target {
            // KWin follows the negotiated size; a frame of another size is
            // from before the renegotiation took effect, or KWin chose it.
            self.telemetry.count("capture_size_mismatch", 1);
        }
        let damage = frame.damage.clone();
        let (image, owner, gpu) = match frame.memory {
            Memory::DmaBuf(_) if self.stream_dmabuf => {
                (self.gpu_ticket(frame, captured_at)?, None, true)
            }
            Memory::DmaBuf(_) => {
                // A DMA-BUF from before the switch to SHM: tiled, unreadable.
                self.telemetry.count("capture_dropped_dmabuf", 1);
                self.damage_continuous = false;
                return Ok(());
            }
            Memory::Unmapped => anyhow::bail!("unmappable SHM screencast buffer"),
            Memory::Shm(_) => {
                let pixels = frame
                    .pixels()
                    .ok_or_else(|| anyhow::anyhow!("invalid SHM screencast layout"))?;
                let layout = (width, height, frame.stride);
                if self.shm.as_ref().is_none_or(|pool| pool.layout != layout) {
                    self.shm = Some(CapturePool::new(layout));
                }
                let (_, image, owner) = self.shm.as_mut().unwrap().capture(pixels, desktop)?;
                // The copy is done: KWin gets its buffer back now.
                drop(frame);
                (image, owner, false)
            }
        };
        let (changed, changes) = self.compare_with_latest(&image, gpu, damage.as_deref());
        self.damage_continuous = true;
        self.telemetry
            .sample("capture_snapshot", captured_at.elapsed());
        self.telemetry.count(
            if changed {
                "capture_frames"
            } else {
                "capture_suppressed"
            },
            1,
        );
        if changed {
            self.capture_metadata
                .lock()
                .unwrap()
                .get_or_insert_with(Default::default)
                .push(&image, captured_at, changes, owner);
            self.latest = Some(Arc::new(DisplayUpdate::Bitmap(image)));
            self.frames.send_replace(self.latest.clone());
        }
        if let Some(ready) = self.ready.take() {
            let _ = ready.send(Ok(()));
        }
        let fps = self.frame_rate.load(Ordering::Acquire).clamp(1, 60);
        self.next_capture = captured_at + Duration::from_secs_f64(1.0 / f64::from(fps));
        self.heartbeat_at = captured_at + HEARTBEAT;
        Ok(())
    }

    /// Hold the DMA-BUF in a GPU frame ticket for the encoder, and publish a
    /// token bitmap that names it.
    fn gpu_ticket(
        &mut self,
        frame: Frame,
        captured_at: Instant,
    ) -> anyhow::Result<ironrdp_server::BitmapUpdate> {
        let device = self.gpu_path.as_ref().map_or(0, |path| path.device);
        anyhow::ensure!(
            frame.drm_format != 0,
            "unsupported DMA-BUF screencast format"
        );
        self.gpu_sequence = self.gpu_sequence.wrapping_add(1);
        let fd = frame.dmabuf()?;
        let (width, height) = (frame.width, frame.height);
        let ticket = Arc::new(GpuFrame {
            fd,
            stride: frame.stride,
            fourcc: frame.drm_format,
            modifier: frame.modifier,
            width,
            height,
            offset: frame.offset,
            device,
            sequence: self.gpu_sequence,
            inverted: false,
            captured_at,
            _owner: Arc::new(frame.held),
        });
        *self.gpu.latest.lock().unwrap() = Some(ticket);
        self.telemetry.count(
            "gpu_capture_stride_bytes",
            u64::from(frame.stride) * u64::from(height),
        );
        let mut token = b"NIRIGPU1".to_vec();
        token.extend_from_slice(&self.gpu_sequence.to_le_bytes());
        Ok(ironrdp_server::BitmapUpdate {
            x: u16::MAX,
            y: u16::MAX,
            width: std::num::NonZeroU16::new(width as u16).unwrap(),
            height: std::num::NonZeroU16::new(height as u16).unwrap(),
            format: ironrdp_server::PixelFormat::BgrX32,
            data: token.into(),
            stride: std::num::NonZeroUsize::new(width as usize * 4).unwrap(),
        })
    }

    /// Whether a snapshot differs from `latest`, and exactly which tiles do.
    /// KWin's damage limits the comparison when it is relative to `latest`;
    /// a periodic full comparison audits it. Consumers reuse the recorded
    /// tiles instead of comparing complete desktops on the RDP runtime thread.
    fn compare_with_latest(
        &mut self,
        image: &ironrdp_server::BitmapUpdate,
        gpu: bool,
        damage: Option<&[Rect]>,
    ) -> (bool, Option<TileSet>) {
        let previous = self.latest.as_deref().and_then(|latest| match latest {
            DisplayUpdate::Bitmap(previous) if crate::comparable(previous, image) => Some(previous),
            _ => None,
        });
        let Some(previous) = previous else {
            return (true, None);
        };
        if gpu {
            // GPU tickets carry a sequence, never pixels.
            return (!crate::same_pixels(previous, image, None), None);
        }
        let hint = self
            .damage_continuous
            .then_some(damage)
            .flatten()
            .and_then(|damage| damage_tiles(image, damage));
        let now = Instant::now();
        let audit = hint.is_some() && now >= self.damage_audit_at;
        let tiles = if audit {
            self.damage_audit_at = now + Duration::from_secs(1);
            crate::changed_tiles(previous, image, None)
        } else {
            crate::changed_tiles(previous, image, hint.as_ref())
        };
        self.telemetry.count(
            if hint.is_some() && !audit {
                "capture_damage_limited_compares"
            } else {
                "capture_full_compares"
            },
            1,
        );
        self.telemetry
            .count("capture_changed_tiles", tiles.len() as u64);
        if audit && hint.as_ref().is_some_and(|hint| !tiles.is_subset(hint)) {
            // Changed pixels outside the reported damage: an earlier limited
            // comparison may have missed changes too, and a diff base may
            // already contain one the client never received. Unknown history
            // forces full comparisons; a refresh epoch resends full desktops.
            self.telemetry.count("capture_damage_missed", 1);
            self.status.refresh_epoch.fetch_add(1, Ordering::AcqRel);
            return (true, None);
        }
        (!tiles.is_empty(), Some(tiles))
    }

    /// Release clipboard and every held input, close the stream, then wait
    /// for KWin to process them before the worker closes its socket.
    fn finish(&mut self) {
        if let Some(clipboard) = self.clipboard.take() {
            clipboard.clear();
        }
        self.input.release_all();
        if let Some(zkde) = self.zkde.take() {
            zkde.close();
        }
        self.shm = None;
        // A successful flush only hands bytes to the socket. Await
        // wl_display.sync so KWin processes the releases before this client
        // socket closes; otherwise a hangup may discard final releases.
        self.s.synced = false;
        self.c.display().sync(&self.h, ());
        let cleanup_deadline = Instant::now();
        while !self.s.synced && cleanup_deadline.elapsed() < Duration::from_millis(500) {
            if pump(&self.c, &mut self.q, &mut self.s).is_err() {
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stream_size_stays_within_kwins_range() {
        let size = |width, height| DesktopSize { width, height };
        assert_eq!(stream_size(size(1920, 1080)), (1920, 1080));
        assert_eq!(stream_size(size(100, 4096)), (200, 4096));
        assert_eq!(stream_size(size(1, 1)), (200, 200));
    }
    #[test]
    fn cursor_mode_follows_client_pointer_support() {
        assert_eq!(CursorMode::for_client(0), CursorMode::Embedded);
        assert_eq!(CursorMode::for_client(32), CursorMode::Metadata);
        assert_eq!(CursorMode::Metadata.pointer(), Pointer::Metadata);
        assert_eq!(CursorMode::Embedded.pointer(), Pointer::Embedded);
    }
}
