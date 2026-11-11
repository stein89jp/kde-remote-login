//! Bounded performance settings shared by capture and encoding.
use std::{
    collections::{HashMap, VecDeque},
    sync::atomic::{AtomicU8, AtomicU32, AtomicU64, Ordering},
    time::{Duration, Instant},
};

/// Keep interactive graphics within a short delivery budget. Feedback includes
/// network transit and client decoding, so this is not an audio/video clock.
///
/// The target moves between a few quantized levels: sustained delay lowers it
/// at most once a second, increases need sustained health. A single delayed
/// flight is not congestion (see [`Self::delayed`]). What the encoder is
/// actually configured with is decided by [`RateApplier`].
pub(crate) struct AdaptiveBitrate {
    ceiling: u32,
    levels: Vec<u32>,
    level: usize,
    sent: HashMap<u32, Instant>,
    suspended: bool,
    last_change: Instant,
    healthy_since: Option<Instant>,
    baseline: Option<Duration>,
    baseline_candidate: Option<(Instant, Duration, Duration, u32)>,
    fps_ceiling: u32,
    window: u32,
    last_window_change: Instant,
    last_window_reduction: Option<Instant>,
    last_queue: u32,
    last_ack: Option<Instant>,
    last_send: Option<Instant>,
    joint: bool,
    diagnostics: Telemetry,
    last_audio_congestion: Option<Instant>,
    /// Sound pressure cut the target and the encoder has not adopted a rate
    /// at or below it yet. Unlike `last_audio_congestion` this does not age:
    /// the cut reaches the encoder with the next changed frame, however much
    /// later that frame comes.
    audio_cut_pending: bool,
    /// The boundary of the delay episode in progress: when it was noticed,
    /// or the first acknowledgement after that if none was arriving then.
    /// Frames sent before it share its cause; one sent afterwards is fresh
    /// evidence.
    suspect: Option<Instant>,
    /// When the last delay episode ended with a healthy later frame.
    recovered: Option<Instant>,
}

impl Default for AdaptiveBitrate {
    fn default() -> Self {
        Self::new(12_000_000, Instant::now())
    }
}

impl AdaptiveBitrate {
    const BUDGET: Duration = Duration::from_millis(150);
    /// Minimum spacing between bitrate level decreases.
    const REDUCE_INTERVAL: Duration = Duration::from_secs(1);
    /// The credit window keeps its established reduction pace.
    const WINDOW_REDUCE_INTERVAL: Duration = Duration::from_millis(500);
    const RECOVER_INTERVAL: Duration = Duration::from_secs(3);
    /// Healthy acknowledgements required before raising the bitrate a level.
    const INCREASE_HOLD: Duration = Duration::from_secs(10);
    const LEVELS: u32 = 6;
    const FLOOR: u32 = 1_000_000;
    /// A frame outstanding this long past the delivery budget is congestion
    /// by itself. The link stalls measured through mstsc over the internet
    /// on 2026-11-11 delayed acknowledgements to 160-370 ms; 4 of 130
    /// delayed 5 s intervals that day exceeded 500 ms.
    const STALL_GRACE: Duration = Duration::from_millis(350);
    /// A delay episode beginning this soon after the last one ended is not
    /// isolated.
    const ISOLATION: Duration = Duration::from_millis(500);
    /// Unacknowledged frames a still-image refinement may add to.
    const REFINEMENT_FLIGHT: usize = 2;

    pub(crate) fn new(ceiling: u32, now: Instant) -> Self {
        Self {
            ceiling,
            levels: Self::levels(ceiling),
            level: 0,
            sent: HashMap::new(),
            suspended: false,
            last_change: now,
            healthy_since: None,
            baseline: None,
            baseline_candidate: None,
            fps_ceiling: 60,
            window: 2,
            last_window_change: now,
            last_window_reduction: None,
            last_queue: 0,
            last_ack: None,
            last_send: None,
            joint: false,
            diagnostics: Telemetry::from_env(),
            last_audio_congestion: None,
            audio_cut_pending: false,
            suspect: None,
            recovered: None,
        }
    }
    /// Geometric steps from the ceiling to the 1 Mbps floor, rounded to
    /// 100 kbps. Equal neighbours collapse, so a low ceiling has fewer levels.
    fn levels(ceiling: u32) -> Vec<u32> {
        let floor = Self::FLOOR.min(ceiling);
        let ratio = (f64::from(floor) / f64::from(ceiling)).powf(1.0 / f64::from(Self::LEVELS - 1));
        let mut levels: Vec<u32> = (0..Self::LEVELS)
            .map(|level| match level {
                0 => ceiling,
                level if level == Self::LEVELS - 1 => floor,
                level => {
                    let value = f64::from(ceiling) * ratio.powi(level as i32);
                    ((value / 100_000.0).round() as u32 * 100_000).clamp(floor, ceiling)
                }
            })
            .collect();
        levels.dedup();
        levels
    }
    /// The highest level not above `bitrate`, or the floor.
    fn level_at_most(&self, bitrate: u32) -> usize {
        self.levels
            .iter()
            .position(|&level| level <= bitrate)
            .unwrap_or(self.levels.len() - 1)
    }
    pub(crate) fn ceiling(&self) -> u32 {
        self.ceiling
    }
    pub(crate) fn current(&self) -> u32 {
        self.levels[self.level]
    }
    pub(crate) fn set_frame_rate_ceiling(&mut self, fps: u32) {
        self.fps_ceiling = fps.clamp(1, 60);
    }
    pub(crate) fn enable_joint_adaptation(&mut self, enabled: bool) {
        self.joint = enabled;
    }
    /// Scale cadence with available graphics capacity, keeping useful per-frame
    /// quality instead of spending a reduced rate on the original 60 frames.
    pub(crate) fn frame_rate(&self) -> u32 {
        if !self.joint {
            return self.fps_ceiling;
        }
        (u64::from(self.fps_ceiling) * u64::from(self.current()) / u64::from(self.ceiling)).clamp(
            u64::from(self.fps_ceiling.min(10)),
            u64::from(self.fps_ceiling),
        ) as u32
    }
    pub(crate) fn max_frames_in_flight(&self) -> u32 {
        self.window
    }
    /// Whether sound reported transport pressure within the recovery interval.
    pub(crate) fn audio_pressure(&self, now: Instant) -> bool {
        self.last_audio_congestion
            .is_some_and(|at| now.saturating_duration_since(at) < Self::RECOVER_INTERVAL)
    }
    /// Whether a decrease must reach the encoder with the next changed frame
    /// instead of waiting for the payload to exceed it: sound reported
    /// pressure recently, or a target it cut has not been adopted yet.
    pub(crate) fn audio_urgent(&self, now: Instant) -> bool {
        self.audio_cut_pending || self.audio_pressure(now)
    }
    /// The encoder is about to encode at `bitrate`. At or below the target,
    /// a cut made by sound pressure has arrived; a refinement frame held at
    /// a higher applied rate leaves it pending.
    pub(crate) fn rate_adopted(&mut self, bitrate: u32) {
        if bitrate <= self.current() {
            self.audio_cut_pending = false;
        }
    }
    /// Tracked frames sent but not yet acknowledged.
    pub(crate) fn in_flight(&self) -> usize {
        self.sent.len()
    }
    /// Whether a still image may be sharpened now: a quiet, healthy path,
    /// fewer than [`Self::REFINEMENT_FLIGHT`] frames unacknowledged, and
    /// always one credit left for a frame the desktop actually changed.
    /// Refinement is paced by the current target instead of being limited to
    /// high levels. During a delay episode it waits for the delayed flight;
    /// once nothing is in flight, one refinement frame is the later frame
    /// that resolves the episode, which a still screen would otherwise never
    /// send.
    pub(crate) fn permits_refinement(&self, now: Instant) -> bool {
        self.last_queue == 0
            && self.sent.len() < Self::REFINEMENT_FLIGHT
            && self.sent.len() + 1 < self.window as usize
            && (self.suspect.is_none() || self.sent.is_empty())
            && !self.audio_pressure(now)
            && (self.suspended
                || self
                    .last_ack
                    .is_some_and(|at| now.saturating_duration_since(at) < Self::RECOVER_INTERVAL))
    }
    pub(crate) fn reset_frames(&mut self) {
        self.sent.clear();
        self.suspended = false;
        self.healthy_since = None;
        self.baseline = None;
        self.baseline_candidate = None;
        self.last_ack = None;
        self.last_queue = 0;
        self.window = 2;
        self.last_window_reduction = None;
        self.last_send = None;
        self.suspect = None;
        self.recovered = None;
    }
    pub(crate) fn audio_congested(&mut self, now: Instant) {
        self.last_audio_congestion = Some(
            self.last_audio_congestion
                .map_or(now, |previous| previous.max(now)),
        );
        self.audio_cut_pending = true;
        // Sound can be queued even when graphics ACKs are healthy or suspended.
        // Use the same reduction cooldown, but keep recovery tied to both streams.
        self.congested(now);
    }
    pub(crate) fn clear_audio_congestion(&mut self) {
        self.last_audio_congestion = None;
        self.healthy_since = None;
    }
    pub(crate) fn inherit_audio_congestion(&mut self, previous: &Self, now: Instant) {
        if let Some(at) = previous.last_audio_congestion
            && now.saturating_duration_since(at) < Self::RECOVER_INTERVAL
        {
            // Sound negotiation can precede creation of the graphics context.
            // Preserve its pressure and target without carrying old frame IDs.
            self.last_audio_congestion = Some(at);
            self.audio_cut_pending = previous.audio_cut_pending;
            self.level = self.level.max(self.level_at_most(previous.current()));
            self.last_change = previous.last_change.min(now);
        }
    }
    fn delivery_budget(&self) -> Duration {
        // Transit time alone is not congestion. Detect delay above the fastest
        // observed ACK, while still reacting if a new connection never ACKs.
        self.baseline
            .map_or(Duration::from_millis(500), |baseline| {
                (baseline + Duration::from_millis(100)).max(Self::BUDGET)
            })
    }
    pub(crate) fn sent(&mut self, id: u32, now: Instant) {
        if let Some(previous) = self.last_send {
            self.diagnostics
                .sample("send_cadence", now.saturating_duration_since(previous));
            // An episode ends with a healthy later frame. When the desktop
            // went still instead, nothing resolved it; after an idle stream
            // it says nothing about the path this frame meets, so that
            // frame's delay would be a first one again.
            if self.sent.is_empty()
                && now.saturating_duration_since(previous) >= Self::RECOVER_INTERVAL
            {
                self.suspect = None;
            }
        }
        self.last_send = Some(now);
        self.diagnostics.count("sent_frames", 1);
        self.diagnostics
            .count("window_samples", u64::from(self.window));
        connection().record_window(self.window);
        if !self.suspended {
            // Track the same bounded window configured in the protocol server.
            if self.sent.len() >= self.window as usize {
                let oldest = *self.sent.iter().min_by_key(|(_, at)| **at).unwrap().0;
                self.sent.remove(&oldest);
            }
            self.sent.insert(id, now);
        }
    }
    fn congested(&mut self, now: Instant) {
        self.healthy_since = None;
        if self.window > 2
            && now.saturating_duration_since(self.last_window_change)
                >= Self::WINDOW_REDUCE_INTERVAL
        {
            self.window -= 1;
            self.last_window_change = now;
            self.last_window_reduction = Some(now);
        }
        if now.saturating_duration_since(self.last_change) >= Self::REDUCE_INTERVAL
            && self.level + 1 < self.levels.len()
        {
            self.level += 1;
            self.last_change = now;
            self.diagnostics.count("bitrate_level_decreases", 1);
        }
    }
    /// Delivery exceeded the budget for frames sent between `oldest` and
    /// `newest`. One late flight is tolerated: a link stall holds every frame
    /// in flight and releases them together, and no lower rate prevents it.
    /// It is congestion when a frame sent after the episode's boundary is
    /// late as well, when episodes follow each other closely, or when a frame
    /// has been outstanding far too long. While no acknowledgement has
    /// arrived since the episode was noticed, the stall may still be in
    /// progress and a frame sent meanwhile shares it: only the stall grace
    /// applies until the first acknowledgement sets the boundary.
    fn delayed(&mut self, oldest: Instant, newest: Instant, now: Instant) {
        let sustained = match self.suspect {
            Some(since) => newest >= since && self.last_ack.is_some_and(|at| at >= since),
            None => self
                .recovered
                .is_some_and(|at| now.saturating_duration_since(at) < Self::ISOLATION),
        };
        let stalled =
            now.saturating_duration_since(oldest) > self.delivery_budget() + Self::STALL_GRACE;
        if self.suspect.is_none() {
            self.suspect = Some(now);
            if !sustained && !stalled {
                self.diagnostics.count("tolerated_delay_episodes", 1);
            }
        }
        if sustained || stalled {
            self.congested(now);
        }
    }
    pub(crate) fn check_stall(&mut self, now: Instant) {
        let budget = self.delivery_budget();
        let mut overdue = self
            .sent
            .values()
            .copied()
            .filter(|at| now.saturating_duration_since(*at) > budget);
        if let Some(first) = overdue.next() {
            let (oldest, newest) = overdue.fold((first, first), |(oldest, newest), at| {
                (oldest.min(at), newest.max(at))
            });
            self.delayed(oldest, newest, now);
        }
    }
    pub(crate) fn acknowledged(&mut self, id: u32, queue_depth: u32, now: Instant) {
        // MS-RDPEGFX's suspension sentinel is a protocol mode, not a huge queue.
        if queue_depth == u32::MAX {
            self.sent.clear();
            self.suspended = true;
            self.suspect = None;
            self.healthy_since = None;
            self.baseline_candidate = None;
            self.last_queue = 0;
            self.last_ack = Some(now);
            return;
        }
        self.suspended = false;
        let Some(sent) = self.sent.remove(&id) else {
            return;
        };
        let elapsed = now.saturating_duration_since(sent);
        self.diagnostics.sample("graphics_send_ack", elapsed);
        self.diagnostics.count("ack_frames", 1);
        self.diagnostics
            .count("queued_graphics_bytes", u64::from(queue_depth));
        self.diagnostics
            .count("outstanding_frames", self.sent.len() as u64);
        self.diagnostics.report("graphics_feedback", now);
        // An episode noticed while nothing was arriving began no later than
        // this, the first acknowledgement since: frames sent during the
        // silence met the same stall.
        if let Some(since) = self.suspect
            && self.last_ack.is_none_or(|at| at < since)
        {
            self.suspect = Some(now);
        }
        self.last_ack = Some(now);
        self.last_queue = queue_depth;
        // A lifetime minimum mistakes a lasting transit change for congestion.
        // Relearn only from a narrow, empty-queue delay plateau lasting >=3s
        // and >=8 valid ACKs. Growing decoder/network queues, jitter and audio
        // pressure never raise the baseline. A quicker path relearns immediately.
        if queue_depth == 0
            && self
                .last_audio_congestion
                .is_none_or(|at| now.saturating_duration_since(at) >= Self::RECOVER_INTERVAL)
        {
            if self
                .baseline
                .is_some_and(|baseline| elapsed > baseline + Duration::from_millis(100))
            {
                let (since, low, high, samples) = self
                    .baseline_candidate
                    .get_or_insert((now, elapsed, elapsed, 0));
                *low = (*low).min(elapsed);
                *high = (*high).max(elapsed);
                *samples += 1;
                if *high - *low > (*low / 8).max(Duration::from_millis(10)) {
                    self.baseline_candidate = Some((now, elapsed, elapsed, 1));
                } else if *samples >= 8
                    && now.saturating_duration_since(*since) >= Self::RECOVER_INTERVAL
                {
                    self.baseline = Some(*low);
                    self.baseline_candidate = None;
                }
            } else {
                self.baseline_candidate = None;
            }
        } else {
            self.baseline_candidate = None;
        }
        self.baseline = Some(
            self.baseline
                .map_or(elapsed, |baseline| baseline.min(elapsed)),
        );
        let budget = self.delivery_budget();
        // QueueDepth is queued graphics bytes, not a count of frames. Permit
        // at most 150ms of payload at the current target rate.
        let queue_budget = u64::from(self.current()) * Self::BUDGET.as_millis() as u64 / 8_000;
        if u64::from(queue_depth) > queue_budget || elapsed > budget {
            self.delayed(sent, sent, now);
            return;
        }
        match self.suspect {
            // The path delivers again: the episode is over.
            Some(since) if sent >= since => {
                self.suspect = None;
                self.recovered = Some(now);
            }
            // A frame of the delayed flight arrived in time; growth waits
            // until a later frame resolves the episode.
            Some(_) => return,
            None => (),
        }
        let healthy = *self.healthy_since.get_or_insert(now);
        // Allow enough credits for the measured round trip at the target
        // cadence, but never more than eight. Two credits cap delivery at two
        // frames per ACK delay, about 20fps on a 100ms WAN path to mstsc. The
        // measured send rate is itself window-limited, so size from the target
        // FPS. Grow at once on a healthy link; after congestion shrank the
        // window, wait for a stable interval. No capture queue is introduced.
        let desired = (elapsed.as_micros() * u128::from(self.frame_rate()) / 1_000_000 + 2)
            .clamp(2, 8) as u32;
        if desired > self.window
            && self.last_window_reduction.is_none_or(|at| {
                now.saturating_duration_since(at) >= Self::RECOVER_INTERVAL
                    && now.saturating_duration_since(healthy) >= Self::RECOVER_INTERVAL
            })
            && self
                .last_audio_congestion
                .is_none_or(|at| now.saturating_duration_since(at) >= Self::RECOVER_INTERVAL)
        {
            self.window = desired;
            self.last_window_change = now;
        }
        if self.level > 0
            && now.saturating_duration_since(healthy) >= Self::INCREASE_HOLD
            && now.saturating_duration_since(self.last_change) >= Self::INCREASE_HOLD
            && self
                .last_audio_congestion
                .is_none_or(|at| now.saturating_duration_since(at) >= Self::RECOVER_INTERVAL)
        {
            self.level -= 1;
            self.last_change = now;
            self.diagnostics.count("bitrate_level_increases", 1);
        }
    }
}
/// Applies controller targets to the live encoder. FFmpeg's NVENC wrapper
/// resets rate control and forces an IDR on every rate change, which on a
/// settled page costs a visibly soft picture for a quarter of a second. A
/// target is therefore adopted for free whenever an IDR is due anyway
/// (refresh, resume, dropped frame, resize, cadence change); otherwise only
/// when it would change what the encoder sends: a decrease once the last
/// second of payload exceeded the lower target (or at once after sound
/// pressure, as every decrease used to),
/// an increase once `defer` has passed since the last IDR and the applied
/// rate is what limits the encoder. The credit window, not the encoder
/// rate, is the immediate congestion response.
pub(crate) struct RateApplier {
    applied: Option<(u32, u32)>,
    last_idr: Option<Instant>,
    defer: Duration,
    /// Submitted frames within the last [`Self::WINDOW`]: time and bytes.
    payload: VecDeque<(Instant, u64)>,
    payload_bytes: u64,
}
impl RateApplier {
    /// Span over which the submitted payload is compared with a target.
    const WINDOW: Duration = Duration::from_secs(1);
    pub(crate) fn new(defer: Duration) -> Self {
        Self {
            applied: None,
            last_idr: None,
            defer,
            payload: VecDeque::new(),
            payload_bytes: 0,
        }
    }
    /// Record one submitted frame's H.264 payload.
    pub(crate) fn submitted(&mut self, bytes: usize, now: Instant) {
        self.payload_rate(now);
        self.payload.push_back((now, bytes as u64));
        self.payload_bytes += bytes as u64;
    }
    /// Bits submitted during the last second.
    fn payload_rate(&mut self, now: Instant) -> u64 {
        while self
            .payload
            .front()
            .is_some_and(|(at, _)| now.saturating_duration_since(*at) >= Self::WINDOW)
        {
            self.payload_bytes -= self.payload.pop_front().unwrap().1;
        }
        self.payload_bytes * 8
    }
    /// The bitrate to configure for the next encoded frame at `fps`. `urgent`
    /// is sound pressure, recent or not yet adopted
    /// ([`AdaptiveBitrate::audio_urgent`]); `hold` keeps the applied rate (a
    /// refinement frame of a still image must never turn into an IDR).
    pub(crate) fn select(
        &mut self,
        target: u32,
        fps: u32,
        idr_due: bool,
        urgent: bool,
        hold: bool,
        now: Instant,
    ) -> u32 {
        let bitrate = match self.applied {
            Some((applied, applied_fps)) if applied_fps == fps && !idr_due && applied != target => {
                let rate = self.payload_rate(now);
                let reconfigure = if hold {
                    false
                } else if target < applied {
                    urgent || rate > u64::from(target)
                } else {
                    self.last_idr
                        .is_none_or(|at| now.saturating_duration_since(at) >= self.defer)
                        && rate * 4 >= u64::from(applied) * 3
                };
                if reconfigure { target } else { applied }
            }
            _ => target,
        };
        self.applied = Some((bitrate, fps));
        bitrate
    }
    pub(crate) fn idr_sent(&mut self, now: Instant) {
        self.last_idr = Some(now);
    }
    /// The cadence the encoder was last configured with. A refinement frame
    /// is encoded at it whatever the controller now asks for, because a
    /// cadence change recreates the encoder and starts with an IDR.
    pub(crate) fn applied_fps(&self) -> Option<u32> {
        self.applied.map(|(_, fps)| fps)
    }
}

/// Sharpening of a still screen. The encoder lowers its quantiser about one
/// step per frame, so a page that stops changing is left as soft as its last
/// frame. While no new capture arrives the retained image is encoded again
/// until the encoder has nothing left to add: the quantiser has reached its
/// floor and the frames stopped shrinking.
pub(crate) struct Refinement {
    pub(crate) due: Instant,
    expires: Instant,
    frames: u32,
    /// Size of the previous frame of this image if it was encoded at the floor.
    floor_bytes: Option<usize>,
    /// Lowest quantiser of this image so far, its previous frame's size, its
    /// smallest refinement frame, and the run of frames that improved nothing.
    lowest_qp: Option<u8>,
    previous_bytes: usize,
    smallest: usize,
    stagnant: u32,
}
impl Refinement {
    /// Quiet time after the last change before sharpening starts; shorter
    /// than this and 15-30 fps content would be refined between its frames.
    const SETTLE: Duration = Duration::from_millis(100);
    const EXPIRY: Duration = Duration::from_secs(6);
    /// Retry interval while the controller does not permit refinement.
    pub(crate) const RETRY: Duration = Duration::from_millis(250);
    /// Convergence took 15-30 frames at 12 Mbit/s and about 80 at 1 Mbit/s.
    const MAX_FRAMES: u32 = 120;
    /// Consecutive frames without a lower quantiser, no smaller than their
    /// predecessor and about as small as the smallest so far, after which the
    /// encoder evidently adds nothing. The average quantiser does not always
    /// reach the floor: with spatial AQ the floor clamps single macroblocks
    /// and the average settled two steps above it, and at low rates rate
    /// control held it near 40 on a still page (NVENC, 2026-11-11). Rate
    /// control pauses in mid-convergence at 1 Mbit/s lasted three or four
    /// such frames.
    const STAGNANT: u32 = 16;
    /// Arm after a changed frame of `bytes` at quantiser `qp` was sent.
    pub(crate) fn armed(qp: Option<u8>, bytes: usize, floor: u8, now: Instant) -> Self {
        Self {
            due: now + Self::SETTLE,
            expires: now + Self::EXPIRY,
            frames: 0,
            floor_bytes: qp.filter(|qp| *qp <= floor).map(|_| bytes),
            lowest_qp: qp,
            previous_bytes: bytes,
            smallest: usize::MAX,
            stagnant: 0,
        }
    }
    /// Only a clock: a capture skipped for want of a credit is waiting, and
    /// nothing else would wake the stream before the next capture.
    pub(crate) fn waiting(delay: Duration, now: Instant) -> Self {
        Self {
            due: now + delay,
            expires: now + Self::EXPIRY,
            frames: 0,
            floor_bytes: None,
            lowest_qp: None,
            previous_bytes: 0,
            smallest: usize::MAX,
            stagnant: 0,
        }
    }
    pub(crate) fn expired(&self, now: Instant) -> bool {
        now >= self.expires
    }
    /// Look again after `delay`; a timer that fired must always move on.
    pub(crate) fn postpone(&mut self, delay: Duration, now: Instant) {
        self.due = now + delay;
    }
    /// Look again no later than `delay` from now.
    pub(crate) fn hasten(&mut self, delay: Duration, now: Instant) {
        self.due = self.due.min(now + delay);
    }
    /// A refinement frame was sent. Returns whether the image still needs
    /// another, scheduled no faster than the cadence or the target bitrate.
    pub(crate) fn refined(
        &mut self,
        qp: Option<u8>,
        bytes: usize,
        floor: u8,
        target: u32,
        fps: u32,
        now: Instant,
    ) -> bool {
        self.frames += 1;
        // Without encoder statistics, keep the former single refinement.
        let Some(qp) = qp else { return false };
        let at_floor = qp <= floor;
        let converged = at_floor
            && self
                .floor_bytes
                .is_some_and(|previous| bytes * 10 >= previous * 9);
        self.floor_bytes = at_floor.then_some(bytes);
        let lower = self.lowest_qp.is_none_or(|lowest| qp < lowest);
        if lower {
            self.lowest_qp = Some(qp);
        }
        self.smallest = self.smallest.min(bytes);
        let flat =
            bytes * 10 >= self.previous_bytes * 9 && bytes <= self.smallest.saturating_mul(2);
        self.previous_bytes = bytes;
        self.stagnant = if !lower && flat { self.stagnant + 1 } else { 0 };
        let settled = converged || self.stagnant >= Self::STAGNANT;
        let spacing = Duration::from_secs_f64(1.0 / f64::from(fps)).max(Duration::from_micros(
            bytes as u64 * 8_000_000 / u64::from(target.max(1)),
        ));
        self.due = now + spacing;
        !settled && self.frames < Self::MAX_FRAMES && self.due < self.expires
    }
}

fn setting(name: &str, value: Option<&str>, default: u32, maximum: u32) -> anyhow::Result<u32> {
    let result = match value {
        Some(value) => value
            .parse::<u32>()
            .map_err(|_| anyhow::anyhow!("{name} must be an integer within 1..{maximum}"))?,
        None => default,
    };
    anyhow::ensure!(
        (1..=maximum).contains(&result),
        "{name} must be an integer within 1..{maximum}"
    );
    Ok(result)
}

fn environment(name: &str) -> anyhow::Result<Option<String>> {
    match std::env::var(name) {
        Ok(value) => Ok(Some(value)),
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(_) => anyhow::bail!("{name} must contain valid Unicode"),
    }
}

/// The headless output refreshes at 60 Hz. Slower links can request fewer frames.
pub fn frame_rate() -> anyhow::Result<u32> {
    let default = if std::env::var("KDE_RDP_ENCODER").as_deref() == Ok("nvenc") {
        60
    } else {
        30
    };
    setting(
        "KDE_RDP_FPS",
        environment("KDE_RDP_FPS")?.as_deref(),
        default,
        60,
    )
}

/// Adaptive H.264 bitrate ceiling in Mbps, bounded before crossing the ABI.
pub fn bitrate() -> anyhow::Result<u32> {
    let ceiling = setting(
        "KDE_RDP_BITRATE_MBPS",
        environment("KDE_RDP_BITRATE_MBPS")?.as_deref(),
        12,
        100,
    )? * 1_000_000;
    let link = environment("KDE_RDP_LINK_MBPS")?;
    let link = link
        .as_deref()
        .map(|value| setting("KDE_RDP_LINK_MBPS", Some(value), 100, 100))
        .transpose()?;
    effective_bitrate(ceiling, link)
}

fn effective_bitrate(ceiling: u32, link: Option<u32>) -> anyhow::Result<u32> {
    // 48kHz stereo S16LE payload is 1.536Mbps. Reserve 1.7Mbps for the
    // accepted PCM path plus framing instead of counting it as video capacity.
    if let Some(link) = link {
        anyhow::ensure!(
            (3..=100).contains(&link),
            "KDE_RDP_LINK_MBPS must be within 3..100 to reserve PCM audio capacity"
        );
        Ok(ceiling.min(link * 1_000_000 - 1_700_000))
    } else {
        Ok(ceiling)
    }
}

/// Defaults retain the measured release profile; additional profiles are opt-in
/// until matched text/scroll/video measurements justify promoting a new default.
pub(crate) fn quality_profile() -> anyhow::Result<(u32, bool)> {
    match environment("KDE_RDP_VIDEO_PROFILE")?
        .as_deref()
        .unwrap_or("speed")
    {
        "speed" => Ok((1, false)),
        "balanced" => Ok((4, true)),
        "quality" => Ok((5, true)),
        _ => anyhow::bail!("KDE_RDP_VIDEO_PROFILE must be speed, balanced or quality"),
    }
}
pub(crate) fn avc444_requested() -> anyhow::Result<bool> {
    match environment("KDE_RDP_VIDEO_CODEC")?
        .as_deref()
        .unwrap_or("avc420")
    {
        "avc420" => Ok(false),
        "auto" | "avc444" => Ok(true),
        _ => anyhow::bail!("KDE_RDP_VIDEO_CODEC must be avc420, auto or avc444"),
    }
}
/// NVENC defaults to `auto`: DMA-BUF import is used only when capture, GPU and
/// encoder support it, and SHM remains the fallback. Bitmap mode never encodes.
pub(crate) fn gpu_capture_requested() -> anyhow::Result<bool> {
    gpu_capture_setting(
        environment("KDE_RDP_GPU_CAPTURE")?.as_deref(),
        std::env::var("KDE_RDP_ENCODER").as_deref() == Ok("nvenc"),
    )
}
fn gpu_capture_setting(value: Option<&str>, nvenc: bool) -> anyhow::Result<bool> {
    match value.unwrap_or(if nvenc { "auto" } else { "shm" }) {
        "auto" => Ok(true),
        "shm" => Ok(false),
        _ => anyhow::bail!("KDE_RDP_GPU_CAPTURE must be auto or shm"),
    }
}
/// Seconds of encoded frames between periodic IDRs. Every recovery point
/// already forces one over reliable RDPGFX transport, and a periodic IDR on a
/// settled page costs a visibly soft picture, so the default is the maximum.
pub(crate) const DEFAULT_GOP_SECONDS: u32 = 3600;
pub(crate) fn gop_seconds() -> anyhow::Result<u32> {
    setting(
        "KDE_RDP_GOP_SECONDS",
        environment("KDE_RDP_GOP_SECONDS")?.as_deref(),
        DEFAULT_GOP_SECONDS,
        3600,
    )
}
/// Lowest quantiser the encoder may use. Below it NVENC spends most of its
/// bitrate on differences nobody can see; settled 1080p text measured a worst
/// 64x64 tile of 44-45 dB at 18 (16: 45, 20: 41-42, unfloored: 48).
pub(crate) const DEFAULT_MIN_QP: u32 = 18;
pub(crate) fn min_qp() -> anyhow::Result<u32> {
    setting(
        "KDE_RDP_MIN_QP",
        environment("KDE_RDP_MIN_QP")?.as_deref(),
        DEFAULT_MIN_QP,
        51,
    )
}
pub(crate) fn joint_adaptation_requested() -> anyhow::Result<bool> {
    match environment("KDE_RDP_ADAPTIVE_GRAPHICS")?
        .as_deref()
        .unwrap_or("0")
    {
        "0" => Ok(false),
        "1" => Ok(true),
        _ => anyhow::bail!("KDE_RDP_ADAPTIVE_GRAPHICS must be 0 or 1"),
    }
}

/// Encoder settings, parsed and validated once at startup instead of per frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct VideoConfig {
    pub(crate) fps: u32,
    pub(crate) bitrate: u32,
    pub(crate) preset: u32,
    pub(crate) spatial_aq: bool,
    pub(crate) avc444: bool,
    pub(crate) gpu_capture: bool,
    pub(crate) joint_adaptation: bool,
    pub(crate) gop_seconds: u32,
    pub(crate) min_qp: u32,
}
impl Default for VideoConfig {
    /// The NVENC defaults with no environment overrides.
    fn default() -> Self {
        Self {
            fps: 60,
            bitrate: 12_000_000,
            preset: 1,
            spatial_aq: false,
            avc444: false,
            gpu_capture: true,
            joint_adaptation: false,
            gop_seconds: DEFAULT_GOP_SECONDS,
            min_qp: DEFAULT_MIN_QP,
        }
    }
}
impl VideoConfig {
    pub(crate) fn from_env() -> anyhow::Result<Self> {
        let (preset, spatial_aq) = quality_profile()?;
        Ok(Self {
            fps: frame_rate()?,
            bitrate: bitrate()?,
            preset,
            spatial_aq,
            avc444: avc444_requested()?,
            gpu_capture: gpu_capture_requested()?,
            joint_adaptation: joint_adaptation_requested()?,
            gop_seconds: gop_seconds()?,
            min_qp: min_qp()?,
        })
    }
    /// How long a pending bitrate increase may wait for an IDR due anyway.
    pub(crate) fn rate_increase_defer(&self) -> Duration {
        Duration::from_secs(u64::from(self.gop_seconds.min(30)))
    }
}

/// Always-on per-connection counters for the one disconnect summary line.
/// Counts and sizes only: never desktop pixels, input, clipboard content,
/// credentials or user names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Counter {
    VideoFrames,
    VideoIdrFrames,
    VideoBytes,
    AudioWaves,
    AudioDiscardedBlocks,
    AudioCreditWaits,
    AudioCodecRecoveries,
    /// Audible 20 ms PCM blocks dropped by the earned clock-drift catch-up.
    AudioDriftSkips,
    /// PCM arrival-model runs dry of at least 100 ms: likely client underruns.
    AudioModelStarvations,
    /// Total length of those runs dry, in milliseconds.
    AudioModelStarvedMs,
    ClipboardToClient,
    ClipboardFromClient,
    ClipboardTimeouts,
    ClipboardOversize,
    /// Valid AUDIO_INPUT Data PDUs and their payload bytes.
    MicPackets,
    MicBytes,
    /// 20 ms microphone frames discarded in the frontend: queue overflow,
    /// expiry, or audio arriving while nothing records.
    MicDropped,
    /// Successful AUDIO_INPUT Opens.
    MicOpens,
    /// Rejected packets, failed Opens and helper connection failures.
    MicErrors,
}
impl Counter {
    const COUNT: usize = 19;
    /// Diagnostics counters that also feed the summary, so the opt-in
    /// telemetry and the always-on line share one increment site.
    fn from_telemetry(name: &str) -> Option<Self> {
        Some(match name {
            "delivered_frames" => Self::VideoFrames,
            "idr_frames" => Self::VideoIdrFrames,
            "h264_payload_bytes" => Self::VideoBytes,
            "waves_sent" => Self::AudioWaves,
            "discarded_blocks" => Self::AudioDiscardedBlocks,
            "credit_waits" => Self::AudioCreditWaits,
            "codec_recoveries" => Self::AudioCodecRecoveries,
            "drift_skips" => Self::AudioDriftSkips,
            _ => return None,
        })
    }
}

/// The negotiated RDPSND codec, if sound started on this connection.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum AudioCodec {
    #[default]
    None,
    Pcm,
    Aac,
    Adpcm,
}
impl AudioCodec {
    fn name(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Pcm => "pcm",
            Self::Aac => "aac",
            Self::Adpcm => "adpcm",
        }
    }
    fn from_u8(value: u8) -> Self {
        match value {
            1 => Self::Pcm,
            2 => Self::Aac,
            3 => Self::Adpcm,
            _ => Self::None,
        }
    }
}

/// Lock-free counters for the connection being served. The frontend serves
/// one connection at a time; [`ConnectionStats::take`] at disconnect starts
/// the next connection from zero.
pub struct ConnectionStats {
    counters: [AtomicU64; Counter::COUNT],
    audio_codec: AtomicU8,
    /// Last opened microphone format: rate << 8 | channels, 0 for none.
    microphone: AtomicU32,
    /// Client core Frame Acknowledge capability plus one, 0 when absent.
    client_frame_ack: AtomicU32,
    /// Client graphics request: 0 absent, else 1 << 31 | gfx << 16 | bpp.
    client_graphics: AtomicU32,
    /// RDPGFX capability exchange: a [`GraphicsChannel`] value.
    graphics_channel: AtomicU8,
    /// Graphics credit window: largest, and sum/count over sent frames.
    window_max: AtomicU32,
    window_sum: AtomicU64,
    window_sends: AtomicU64,
}
impl Default for ConnectionStats {
    fn default() -> Self {
        Self::new()
    }
}
impl ConnectionStats {
    pub const fn new() -> Self {
        Self {
            counters: [const { AtomicU64::new(0) }; Counter::COUNT],
            audio_codec: AtomicU8::new(0),
            microphone: AtomicU32::new(0),
            client_frame_ack: AtomicU32::new(0),
            client_graphics: AtomicU32::new(0),
            graphics_channel: AtomicU8::new(0),
            window_max: AtomicU32::new(0),
            window_sum: AtomicU64::new(0),
            window_sends: AtomicU64::new(0),
        }
    }
    pub fn add(&self, counter: Counter, value: u64) {
        // u64 counts and bytes cannot wrap within a connection's lifetime.
        self.counters[counter as usize].fetch_add(value, Ordering::Relaxed);
    }
    pub fn get(&self, counter: Counter) -> u64 {
        self.counters[counter as usize].load(Ordering::Relaxed)
    }
    pub fn set_audio_codec(&self, codec: AudioCodec) {
        self.audio_codec.store(codec as u8, Ordering::Relaxed);
    }
    /// Record the PCM format a microphone capture opened with.
    pub fn set_microphone_format(&self, rate: u32, channels: u8) {
        self.microphone
            .store(rate << 8 | u32::from(channels), Ordering::Relaxed);
    }
    /// Record the client's advertised core Frame Acknowledge capability
    /// (MS-RDPRFX maxUnacknowledgedFrameCount). It does not cap RDPGFX.
    pub fn set_client_frame_ack(&self, advertised: Option<u32>) {
        self.client_frame_ack.store(
            advertised.map_or(0, |count| count.saturating_add(1)),
            Ordering::Relaxed,
        );
    }
    /// Record what the client requested at activation: whether its Client
    /// Core Data offered RDPGFX and its Bitmap Capability Set color depth.
    pub fn set_client_graphics(&self, request: Option<ironrdp_server::ClientGraphicsCapability>) {
        self.client_graphics.store(
            request.map_or(0, |request| {
                1 << 31
                    | u32::from(request.gfx) << 16
                    | u32::from(request.bits_per_pixel.unwrap_or(0))
            }),
            Ordering::Relaxed,
        );
    }
    /// Record a completed RDPGFX capability exchange; the latest one wins.
    pub fn set_graphics_channel(&self, channel: GraphicsChannel) {
        self.graphics_channel
            .store(channel as u8, Ordering::Relaxed);
    }
    /// Record the graphics credit window in force for one sent frame.
    pub fn record_window(&self, window: u32) {
        self.window_max.fetch_max(window, Ordering::Relaxed);
        self.window_sum
            .fetch_add(u64::from(window), Ordering::Relaxed);
        self.window_sends.fetch_add(1, Ordering::Relaxed);
    }
    /// Read and reset every counter for the next connection.
    pub fn take(&self) -> ConnectionSummary {
        ConnectionSummary {
            counters: std::array::from_fn(|i| self.counters[i].swap(0, Ordering::Relaxed)),
            audio_codec: AudioCodec::from_u8(self.audio_codec.swap(0, Ordering::Relaxed)),
            microphone: self.microphone.swap(0, Ordering::Relaxed),
            client_frame_ack: self.client_frame_ack.swap(0, Ordering::Relaxed),
            client_graphics: self.client_graphics.swap(0, Ordering::Relaxed),
            graphics_channel: GraphicsChannel::from_u8(
                self.graphics_channel.swap(0, Ordering::Relaxed),
            ),
            window_max: self.window_max.swap(0, Ordering::Relaxed),
            window_sum: self.window_sum.swap(0, Ordering::Relaxed),
            window_sends: self.window_sends.swap(0, Ordering::Relaxed),
        }
    }
}

/// How far RDPGFX got: never negotiated (the client did not open the channel
/// or sent no CapsAdvertise), negotiated with AVC420, or negotiated with the
/// client's AVC_DISABLED flag (H.264 impossible, bitmap updates only).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum GraphicsChannel {
    #[default]
    None = 0,
    Avc420 = 1,
    AvcDisabled = 2,
}
impl GraphicsChannel {
    fn from_u8(value: u8) -> Self {
        match value {
            1 => Self::Avc420,
            2 => Self::AvcDisabled,
            _ => Self::None,
        }
    }
    fn name(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Avc420 => "avc420",
            Self::AvcDisabled => "avc_disabled",
        }
    }
}

static CONNECTION: ConnectionStats = ConnectionStats::new();
/// The process-wide counters of the connection currently being served.
pub fn connection() -> &'static ConnectionStats {
    &CONNECTION
}

/// A snapshot of one connection's counters, formatted at disconnect.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ConnectionSummary {
    counters: [u64; Counter::COUNT],
    audio_codec: AudioCodec,
    microphone: u32,
    client_frame_ack: u32,
    client_graphics: u32,
    graphics_channel: GraphicsChannel,
    window_max: u32,
    window_sum: u64,
    window_sends: u64,
}
impl ConnectionSummary {
    pub fn get(&self, counter: Counter) -> u64 {
        self.counters[counter as usize]
    }
    /// One payload-free line. `avc444` reports whether the graphics context
    /// negotiated AVC444 with it enabled; H.264 is reported only when frames
    /// were delivered, otherwise the connection used bitmap updates.
    pub fn line(
        &self,
        peer: std::net::SocketAddr,
        duration: Duration,
        failed: bool,
        avc444: bool,
    ) -> String {
        let frames = self.get(Counter::VideoFrames);
        let (codec, mode) = match (frames, avc444) {
            (0, _) => ("bitmap", "none"),
            (_, false) => ("h264", "avc420"),
            (_, true) => ("h264", "avc444"),
        };
        let millis = duration.as_millis().max(1);
        let kbps = u128::from(self.get(Counter::VideoBytes)) * 8 / millis;
        format!(
            "RDP connection summary peer={peer} duration_s={:.1} result={} \
             video_codec={codec} video_mode={mode} frames={frames} idr_frames={} \
             video_avg_kbps={kbps} audio_codec={} audio_waves={} audio_discarded_blocks={} \
             audio_credit_waits={} audio_codec_recoveries={} audio_drift_skips={} \
             audio_model_starvations={} audio_model_starved_ms={} \
             clipboard_to_client={} clipboard_from_client={} clipboard_timeouts={} clipboard_oversize_drops={} \
             mic={} mic_opens={} mic_packets={} mic_bytes={} mic_dropped={} mic_errors={} \
             client_frame_ack_limit={} window_max={} window_avg={:.1} \
             client_gfx={} client_bpp={} gfx_channel={}",
            duration.as_secs_f64(),
            if failed { "error" } else { "ok" },
            self.get(Counter::VideoIdrFrames),
            self.audio_codec.name(),
            self.get(Counter::AudioWaves),
            self.get(Counter::AudioDiscardedBlocks),
            self.get(Counter::AudioCreditWaits),
            self.get(Counter::AudioCodecRecoveries),
            self.get(Counter::AudioDriftSkips),
            self.get(Counter::AudioModelStarvations),
            self.get(Counter::AudioModelStarvedMs),
            self.get(Counter::ClipboardToClient),
            self.get(Counter::ClipboardFromClient),
            self.get(Counter::ClipboardTimeouts),
            self.get(Counter::ClipboardOversize),
            self.microphone_name(),
            self.get(Counter::MicOpens),
            self.get(Counter::MicPackets),
            self.get(Counter::MicBytes),
            self.get(Counter::MicDropped),
            self.get(Counter::MicErrors),
            match self.client_frame_ack {
                0 => "none".into(),
                value => (value - 1).to_string(),
            },
            self.window_max,
            self.window_sum as f64 / self.window_sends.max(1) as f64,
            match self.client_graphics {
                0 => "none",
                value if value & (1 << 16) != 0 => "yes",
                _ => "no",
            },
            match self.client_graphics & 0xffff {
                0 => "none".into(),
                bpp => bpp.to_string(),
            },
            self.graphics_channel.name(),
        )
    }
    fn microphone_name(&self) -> String {
        match (self.microphone >> 8, self.microphone & 0xff) {
            (0, _) => "none".into(),
            (rate, 1) => format!("pcm_{rate}_mono"),
            (rate, _) => format!("pcm_{rate}_stereo"),
        }
    }
}

/// Whether the opt-in `KDE_RDP_DIAGNOSTICS=1` receipts and detail lines are on.
pub fn diagnostics_enabled() -> bool {
    std::env::var("KDE_RDP_DIAGNOSTICS").as_deref() == Ok("1")
}

/// Optional bounded server-side timing receipts. These are durations/counters,
/// never desktop pixels, input text, clipboard contents or credentials. ACKs
/// include transport and decoding and do not measure physical presentation.
/// A few counters also feed the always-on [`ConnectionStats`] summary.
#[derive(Default)]
pub struct Telemetry {
    enabled: bool,
    began: Option<Instant>,
    samples: HashMap<&'static str, VecDeque<u64>>,
    counters: HashMap<&'static str, u64>,
}
impl Telemetry {
    pub fn enabled(&self) -> bool {
        self.enabled
    }
    pub fn from_env() -> Self {
        Self {
            enabled: diagnostics_enabled(),
            ..Self::default()
        }
    }
    pub fn sample(&mut self, stage: &'static str, duration: Duration) {
        if !self.enabled || (self.samples.len() >= 32 && !self.samples.contains_key(stage)) {
            return;
        }
        self.began.get_or_insert_with(Instant::now);
        let samples = self.samples.entry(stage).or_default();
        if samples.len() == 256 {
            samples.pop_front();
        }
        samples.push_back(duration.as_micros().min(u128::from(u64::MAX)) as u64);
    }
    pub fn count(&mut self, name: &'static str, value: u64) {
        if let Some(counter) = Counter::from_telemetry(name) {
            connection().add(counter, value);
        }
        if !self.enabled || (self.counters.len() >= 32 && !self.counters.contains_key(name)) {
            return;
        }
        self.began.get_or_insert_with(Instant::now);
        let counter = self.counters.entry(name).or_default();
        *counter = counter.saturating_add(value);
    }
    pub fn report(&mut self, component: &'static str, now: Instant) {
        let Some(began) = self.began else {
            return;
        };
        let elapsed = now.saturating_duration_since(began);
        if elapsed < Duration::from_secs(5) {
            return;
        }
        let mut timings = Vec::new();
        for (stage, samples) in &self.samples {
            let mut sorted: Vec<_> = samples.iter().copied().collect();
            sorted.sort_unstable();
            if sorted.is_empty() {
                continue;
            }
            timings.push(format!(
                "\"{stage}\":{{\"samples\":{},\"p50_us\":{},\"p95_us\":{},\"max_us\":{}}}",
                sorted.len(),
                sorted[(sorted.len() - 1) / 2],
                sorted[(sorted.len() - 1) * 95 / 100],
                sorted.last().unwrap()
            ));
        }
        let counters: Vec<_> = self
            .counters
            .iter()
            .map(|(name, value)| format!("\"{name}\":{value}"))
            .collect();
        eprintln!(
            "RDP performance {{\"component\":\"{component}\",\"interval_ms\":{},\"server_timings\":{{{}}},\"counters\":{{{}}},\"physical_presentation_measured\":false}}",
            elapsed.as_millis(),
            timings.join(","),
            counters.join(",")
        );
        self.samples.clear();
        self.counters.clear();
        self.began = Some(now);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn connection_summary_formats_every_counter_and_resets() {
        let stats = ConnectionStats::new();
        let counters = [
            (Counter::VideoFrames, 3600),
            (Counter::VideoIdrFrames, 4),
            (Counter::VideoBytes, 7_500_000),
            (Counter::AudioWaves, 3000),
            (Counter::AudioDiscardedBlocks, 5),
            (Counter::AudioCreditWaits, 6),
            (Counter::AudioCodecRecoveries, 7),
            (Counter::AudioDriftSkips, 17),
            (Counter::AudioModelStarvations, 18),
            (Counter::AudioModelStarvedMs, 1900),
            (Counter::ClipboardToClient, 8),
            (Counter::ClipboardFromClient, 9),
            (Counter::ClipboardTimeouts, 10),
            (Counter::ClipboardOversize, 11),
            (Counter::MicPackets, 12),
            (Counter::MicBytes, 13),
            (Counter::MicDropped, 14),
            (Counter::MicOpens, 15),
            (Counter::MicErrors, 16),
        ];
        for (counter, value) in counters {
            stats.add(counter, value);
        }
        stats.set_audio_codec(AudioCodec::Pcm);
        stats.set_microphone_format(48_000, 1);
        stats.set_client_frame_ack(Some(2));
        stats.set_client_graphics(Some(ironrdp_server::ClientGraphicsCapability {
            gfx: true,
            bits_per_pixel: Some(32),
        }));
        stats.set_graphics_channel(GraphicsChannel::Avc420);
        for window in [2, 4, 8, 8] {
            stats.record_window(window);
        }
        let summary = stats.take();
        let peer = "192.0.2.10:50123".parse().unwrap();
        assert_eq!(
            summary.line(peer, Duration::from_secs(60), false, false),
            "RDP connection summary peer=192.0.2.10:50123 duration_s=60.0 result=ok \
             video_codec=h264 video_mode=avc420 frames=3600 idr_frames=4 video_avg_kbps=1000 \
             audio_codec=pcm audio_waves=3000 audio_discarded_blocks=5 audio_credit_waits=6 \
             audio_codec_recoveries=7 audio_drift_skips=17 audio_model_starvations=18 \
             audio_model_starved_ms=1900 clipboard_to_client=8 \
             clipboard_from_client=9 clipboard_timeouts=10 clipboard_oversize_drops=11 mic=pcm_48000_mono mic_opens=15 \
             mic_packets=12 mic_bytes=13 mic_dropped=14 mic_errors=16 \
             client_frame_ack_limit=2 window_max=8 window_avg=5.5 \
             client_gfx=yes client_bpp=32 gfx_channel=avc420"
        );
        assert!(
            summary
                .line(peer, Duration::from_secs(60), true, true)
                .contains(" result=error video_codec=h264 video_mode=avc444 ")
        );
        // Take starts the next connection from zero, including the codec.
        let next = stats.take();
        assert_eq!(next, ConnectionSummary::default());
        let line = next.line(peer, Duration::ZERO, false, true);
        assert!(line.contains(" video_codec=bitmap video_mode=none frames=0 "));
        assert!(line.contains(" video_avg_kbps=0 audio_codec=none "));
        assert!(line.contains(
            " audio_codec_recoveries=0 audio_drift_skips=0 audio_model_starvations=0 \
                 audio_model_starved_ms=0 clipboard_to_client=0 "
        ));
        assert!(line.ends_with(
            " mic=none mic_opens=0 mic_packets=0 mic_bytes=0 mic_dropped=0 mic_errors=0 \
             client_frame_ack_limit=none window_max=0 window_avg=0.0 \
             client_gfx=none client_bpp=none gfx_channel=none"
        ));
        // A client that never asked for RDPGFX, or disabled AVC, explains a
        // bitmap-only connection without diagnostics.
        stats.set_client_graphics(Some(ironrdp_server::ClientGraphicsCapability {
            gfx: false,
            bits_per_pixel: Some(16),
        }));
        assert!(
            stats
                .take()
                .line(peer, Duration::ZERO, false, false)
                .ends_with(" client_gfx=no client_bpp=16 gfx_channel=none")
        );
        stats.set_graphics_channel(GraphicsChannel::AvcDisabled);
        assert!(
            stats
                .take()
                .line(peer, Duration::ZERO, false, false)
                .ends_with(" gfx_channel=avc_disabled")
        );
        stats.set_client_frame_ack(Some(0));
        assert!(
            stats
                .take()
                .line(peer, Duration::ZERO, false, false)
                .contains(" client_frame_ack_limit=0 ")
        );
        stats.set_microphone_format(44_100, 2);
        assert!(
            stats
                .take()
                .line(peer, Duration::ZERO, false, false)
                .contains(" mic=pcm_44100_stereo ")
        );
    }

    #[test]
    fn telemetry_feeds_summary_counters_even_when_diagnostics_are_off() {
        let mut telemetry = Telemetry::default();
        assert!(!telemetry.enabled());
        let names = [
            ("delivered_frames", Counter::VideoFrames),
            ("idr_frames", Counter::VideoIdrFrames),
            ("h264_payload_bytes", Counter::VideoBytes),
            ("waves_sent", Counter::AudioWaves),
            ("discarded_blocks", Counter::AudioDiscardedBlocks),
            ("credit_waits", Counter::AudioCreditWaits),
            ("codec_recoveries", Counter::AudioCodecRecoveries),
            ("drift_skips", Counter::AudioDriftSkips),
        ];
        // Other tests may count concurrently; this connection never resets.
        for (name, counter) in names {
            let before = connection().get(counter);
            telemetry.count(name, 2);
            assert!(connection().get(counter) >= before + 2, "{name}");
        }
        assert_eq!(Counter::from_telemetry("ack_frames"), None);
        assert!(telemetry.counters.is_empty(), "diagnostics stay off");
    }
    #[test]
    fn bitrate_reduces_on_repeated_delay_or_decoder_backlog_and_recovers_slowly() {
        let start = Instant::now();
        let mut rate = AdaptiveBitrate::new(12_000_000, start);
        rate.sent(0, start);
        rate.acknowledged(0, 0, start + Duration::from_millis(30));
        rate.sent(1, start + Duration::from_millis(1000));
        rate.acknowledged(1, 0, start + Duration::from_millis(1200));
        assert_eq!(rate.current(), 12_000_000, "one late frame is tolerated");
        rate.sent(2, start + Duration::from_millis(1210));
        rate.acknowledged(2, 1_000_000, start + Duration::from_millis(1240));
        assert_eq!(
            rate.current(),
            7_300_000,
            "a later frame's backlog confirms it"
        );
        rate.sent(3, start + Duration::from_millis(1250));
        rate.acknowledged(3, 1_000_000, start + Duration::from_millis(1280));
        assert_eq!(rate.current(), 7_300_000, "reductions have a cooldown");
        rate.sent(4, start + Duration::from_millis(2300));
        rate.acknowledged(4, 1_000_000, start + Duration::from_millis(2330));
        assert_eq!(rate.current(), 4_400_000, "backlog drops one level");
        let healthy = |ids: std::ops::RangeInclusive<u32>, rate: &mut AdaptiveBitrate| {
            for id in ids {
                let at = start + Duration::from_millis(u64::from(id) * 100);
                rate.sent(id, at);
                rate.acknowledged(id, 0, at + Duration::from_millis(30));
            }
        };
        // Healthy since 2430 ms: no increase (and so no IDR) for 10 s.
        healthy(24..=123, &mut rate);
        assert_eq!(rate.current(), 4_400_000, "increases need 10 s of health");
        healthy(124..=124, &mut rate);
        assert_eq!(rate.current(), 7_300_000, "recovery rises one level");
        healthy(125..=223, &mut rate);
        assert_eq!(rate.current(), 7_300_000, "each increase holds 10 s");
        healthy(224..=300, &mut rate);
        assert_eq!(rate.current(), 12_000_000);
    }

    #[test]
    fn bitrate_levels_are_quantized_bounded_and_descending() {
        let levels = AdaptiveBitrate::levels(12_000_000);
        assert_eq!(
            levels,
            [
                12_000_000, 7_300_000, 4_400_000, 2_700_000, 1_600_000, 1_000_000
            ]
        );
        for ceiling in [1_000_000, 1_300_000, 3_300_000, 12_000_000, 100_000_000] {
            let levels = AdaptiveBitrate::levels(ceiling);
            assert_eq!(levels[0], ceiling);
            assert_eq!(*levels.last().unwrap(), 1_000_000);
            assert!(levels.len() <= 6);
            assert!(levels.windows(2).all(|pair| pair[0] > pair[1]));
        }
        assert_eq!(AdaptiveBitrate::levels(1_000_000), [1_000_000]);
        let start = Instant::now();
        let mut rate = AdaptiveBitrate::new(1_000_000, start);
        rate.audio_congested(start + Duration::from_secs(2));
        assert_eq!(rate.current(), 1_000_000, "a single level cannot move");
        let rate = AdaptiveBitrate::new(12_000_000, start);
        assert_eq!(rate.level_at_most(7_300_000), 1);
        assert_eq!(rate.level_at_most(7_299_999), 2);
        assert_eq!(rate.level_at_most(1), 5);
    }

    #[test]
    fn the_encoder_is_reconfigured_only_when_the_new_rate_changes_its_output() {
        let start = Instant::now();
        let at = |ms| start + Duration::from_millis(ms);
        let mut applier = RateApplier::new(Duration::from_secs(30));
        let select = |applier: &mut RateApplier, target, idr, ms| {
            applier.select(target, 60, idr, false, false, at(ms))
        };
        assert_eq!(select(&mut applier, 12_000_000, true, 0), 12_000_000);
        applier.idr_sent(at(0));
        // 60 frames of 10 kB within a second: 4.8 Mbit/s.
        for frame in 0..60 {
            applier.submitted(10_000, at(1_000 + frame * 16));
        }
        assert_eq!(
            select(&mut applier, 7_300_000, false, 2_000),
            12_000_000,
            "4.8 Mbit/s fits the lower target: no IDR for it"
        );
        assert_eq!(
            select(&mut applier, 4_400_000, false, 2_000),
            4_400_000,
            "the payload exceeds this target"
        );
        applier.idr_sent(at(2_000));
        // The payload window forgets after a second.
        assert_eq!(select(&mut applier, 2_700_000, false, 3_100), 4_400_000);
        assert_eq!(
            select(&mut applier, 2_700_000, true, 3_200),
            2_700_000,
            "an IDR due anyway adopts the target for free"
        );
        applier.idr_sent(at(3_200));
        // Increases: wait out the hold, and for a rate that limits the encoder.
        for ms in [4_000, 20_000, 33_199, 33_200, 60_000] {
            assert_eq!(
                select(&mut applier, 12_000_000, false, ms),
                2_700_000,
                "{ms}"
            );
        }
        for frame in 0..60 {
            applier.submitted(4_500, at(60_000 + frame * 16));
        }
        assert_eq!(
            select(&mut applier, 12_000_000, false, 60_990),
            12_000_000,
            "2.16 of 2.7 Mbit/s: the applied rate is the limit"
        );
        applier.idr_sent(at(60_990));
        assert_eq!(select(&mut applier, 7_300_000, false, 61_500), 12_000_000);
        assert_eq!(
            applier.select(7_300_000, 30, false, false, false, at(61_600)),
            7_300_000,
            "a cadence change recreates the encoder anyway"
        );
    }

    #[test]
    fn sound_pressure_lowers_the_encoder_rate_at_once_and_refinement_never_reconfigures() {
        let start = Instant::now();
        let mut applier = RateApplier::new(Duration::from_secs(30));
        assert_eq!(
            applier.select(12_000_000, 60, true, false, false, start),
            12_000_000
        );
        applier.idr_sent(start);
        let later = start + Duration::from_secs(40);
        for frame in 0..60 {
            applier.submitted(20_000, later - Duration::from_millis(frame * 16));
        }
        assert_eq!(
            applier.select(7_300_000, 60, false, false, true, later),
            12_000_000,
            "a refinement frame keeps the applied rate even above the target"
        );
        let quiet = later + Duration::from_secs(2);
        assert_eq!(
            applier.select(7_300_000, 60, false, false, false, quiet),
            12_000_000
        );
        assert_eq!(
            applier.select(7_300_000, 60, false, true, false, quiet),
            7_300_000,
            "sound pressure applies the decrease with no payload at all"
        );
    }

    /// Encoder output measured on NVENC (p1, ull, VBR, one-frame VBV, floor 18)
    /// after a 1080p page switch: (average QP, bytes) per frame.
    const SWITCH_12M: [(u8, usize); 22] = [
        (42, 39_660),
        (34, 23_427),
        (32, 6_739),
        (30, 4_920),
        (29, 7_815),
        (28, 4_089),
        (27, 4_504),
        (26, 4_786),
        (25, 4_493),
        (24, 3_969),
        (22, 7_020),
        (21, 9_021),
        (20, 4_189),
        (19, 6_670),
        (18, 6_662),
        (18, 1_853),
        (18, 1_095),
        (18, 191),
        (18, 101),
        (18, 81),
        (18, 81),
        (18, 81),
    ];
    /// The same at 1 Mbit/s: rate control pauses with tiny frames far above
    /// the floor, which are not convergence.
    const SWITCH_1M_TAIL: [(u8, usize); 14] = [
        (38, 705),
        (38, 105),
        (38, 79),
        (38, 79),
        (37, 3_640),
        (36, 3_452),
        (34, 4_230),
        (36, 121),
        (37, 84),
        (37, 79),
        (36, 79),
        (34, 3_910),
        (33, 4_136),
        (31, 4_229),
    ];

    #[test]
    fn a_still_image_is_refined_until_the_encoder_has_nothing_left_to_add() {
        let start = Instant::now();
        let (qp, bytes) = SWITCH_12M[0];
        let mut refinement = Refinement::armed(Some(qp), bytes, 18, start);
        assert_eq!(refinement.due, start + Duration::from_millis(100));
        let mut now = refinement.due;
        let mut frames = 0;
        for (qp, bytes) in SWITCH_12M[1..].iter().copied() {
            frames += 1;
            if !refinement.refined(Some(qp), bytes, 18, 12_000_000, 60, now) {
                break;
            }
            assert!(refinement.due >= now + Duration::from_micros(16_666));
            now = refinement.due;
        }
        assert_eq!(frames, 20, "stops at the second 81-byte frame at the floor");
        // Tiny frames above the floor are rate-control pauses.
        let mut refinement = Refinement::armed(Some(44), 35_267, 18, start);
        for (qp, bytes) in SWITCH_1M_TAIL {
            assert!(refinement.refined(Some(qp), bytes, 18, 1_000_000, 60, start));
        }
        // Paced by the target: a 4 kB frame at 1 Mbit/s buys 32 ms.
        assert!(refinement.refined(Some(30), 4_000, 18, 1_000_000, 60, start));
        assert_eq!(refinement.due, start + Duration::from_millis(32));
        // A small change on a settled page needs two tiny frames to prove it.
        let mut refinement = Refinement::armed(Some(18), 217, 18, start);
        assert!(refinement.refined(Some(18), 104, 18, 12_000_000, 60, start));
        assert!(!refinement.refined(Some(18), 104, 18, 12_000_000, 60, start));
        // Without encoder statistics: one refinement frame, as before.
        let mut refinement = Refinement::armed(None, 40_000, 18, start);
        assert!(!refinement.refined(None, 20_000, 18, 12_000_000, 60, start));
        // Above the floor it ends once the encoder stopped improving. With
        // spatial AQ the average quantiser settled at 20 over a floor of 18,
        // with frames of about 100 bytes.
        let mut refinement = Refinement::armed(Some(31), 14_000, 18, start);
        let mut frames = 0;
        for (qp, bytes) in [(26, 3_900), (22, 1_400), (20, 310)]
            .into_iter()
            .chain(std::iter::repeat((20, 101)))
        {
            frames += 1;
            if !refinement.refined(Some(qp), bytes, 18, 12_000_000, 60, start) {
                break;
            }
        }
        assert_eq!(frames, 3 + 1 + 16, "sixteen frames that added nothing");
        // At 2 Mbit/s rate control held a still page near QP 40, wavering.
        let mut refinement = Refinement::armed(Some(44), 30_000, 18, start);
        let mut frames = 0;
        for qp in [41, 39, 40, 41, 39].into_iter().cycle() {
            frames += 1;
            if !refinement.refined(Some(qp), 80, 18, 2_000_000, 60, start) {
                break;
            }
        }
        assert_eq!(frames, 2 + 16, "two new lows, then sixteen without one");
        // Without a floor (1) a still frame kept costing 9 kB at QP 3: the
        // same rule stops paying for it.
        let mut refinement = Refinement::armed(Some(30), 9_000, 1, start);
        let mut frames = 0;
        while refinement.refined(Some(3), 9_000, 1, 12_000_000, 60, start) {
            frames += 1;
        }
        assert_eq!(frames, 16);
        // Bounded by frame count and by the expiry when nothing settles:
        // alternating sizes never count as stagnant.
        let mut refinement = Refinement::armed(Some(30), 9_000, 1, start);
        let mut frames = 1;
        while refinement.refined(
            Some(3),
            [9_000, 4_000][frames % 2],
            1,
            12_000_000,
            60,
            start,
        ) {
            frames += 1;
        }
        assert_eq!(frames, 120);
        let mut refinement = Refinement::armed(Some(30), 9_000, 18, start);
        assert!(!refinement.expired(start + Duration::from_millis(5_999)));
        assert!(refinement.expired(start + Duration::from_secs(6)));
        assert!(!refinement.refined(
            Some(30),
            9_000,
            18,
            12_000_000,
            60,
            start + Duration::from_millis(5_990)
        ));
    }

    #[test]
    fn video_config_defaults_gop_and_gpu_capture() {
        let config = VideoConfig::default();
        assert_eq!(config.gop_seconds, 3600, "no periodic IDR within an hour");
        assert_eq!(config.rate_increase_defer(), Duration::from_secs(30));
        assert_eq!(config.min_qp, 18);
        assert!(config.gpu_capture);
        let short = VideoConfig {
            gop_seconds: 5,
            ..config
        };
        assert_eq!(short.rate_increase_defer(), Duration::from_secs(5));
        let long = VideoConfig {
            gop_seconds: 3600,
            ..config
        };
        assert_eq!(long.rate_increase_defer(), Duration::from_secs(30));
        assert!(gpu_capture_setting(None, true).unwrap());
        assert!(!gpu_capture_setting(None, false).unwrap());
        assert!(!gpu_capture_setting(Some("shm"), true).unwrap());
        assert!(gpu_capture_setting(Some("auto"), false).unwrap());
        for value in ["", "AUTO", "gpu", " shm"] {
            assert!(gpu_capture_setting(Some(value), true).is_err());
        }
        for value in ["1", "30", "3600"] {
            assert!(setting("KDE_RDP_GOP_SECONDS", Some(value), 30, 3600).is_ok());
        }
        for value in ["0", "3601", "-1", "infinite"] {
            assert!(setting("KDE_RDP_GOP_SECONDS", Some(value), 30, 3600).is_err());
        }
        for value in ["1", "18", "51"] {
            assert!(setting("KDE_RDP_MIN_QP", Some(value), DEFAULT_MIN_QP, 51).is_ok());
        }
        for value in ["0", "52", "-1", "off"] {
            assert!(setting("KDE_RDP_MIN_QP", Some(value), DEFAULT_MIN_QP, 51).is_err());
        }
    }

    #[test]
    fn stable_long_rtt_does_not_cause_bitrate_collapse() {
        let start = Instant::now();
        let mut rate = AdaptiveBitrate::new(12_000_000, start);
        for id in 0..30 {
            let at = start + Duration::from_millis(u64::from(id) * 500);
            rate.sent(id, at);
            rate.check_stall(at + Duration::from_millis(220));
            rate.acknowledged(id, 0, at + Duration::from_millis(250));
        }
        assert_eq!(rate.current(), 12_000_000);
        rate.sent(30, start + Duration::from_secs(15));
        rate.acknowledged(30, 0, start + Duration::from_millis(15400));
        assert_eq!(rate.current(), 12_000_000, "one late frame is tolerated");
        rate.sent(31, start + Duration::from_millis(15500));
        rate.acknowledged(31, 0, start + Duration::from_millis(15900));
        assert_eq!(
            rate.current(),
            7_300_000,
            "delay above baseline reduces bitrate"
        );
    }

    #[test]
    fn sustained_empty_queue_path_change_relearns_and_recovers_both_directions() {
        let start = Instant::now();
        let mut rate = AdaptiveBitrate::new(12_000_000, start);
        rate.sent(0, start);
        rate.acknowledged(0, 0, start + Duration::from_millis(20));
        for id in 1..=150 {
            let at = start + Duration::from_millis(u64::from(id) * 500);
            rate.sent(id, at);
            rate.acknowledged(id, 0, at + Duration::from_millis(250));
        }
        assert_eq!(rate.baseline, Some(Duration::from_millis(250)));
        assert_eq!(
            rate.current(),
            12_000_000,
            "stable changed path must recover useful quality"
        );
        rate.sent(151, start + Duration::from_secs(76));
        rate.acknowledged(151, 0, start + Duration::from_millis(76020));
        assert_eq!(rate.baseline, Some(Duration::from_millis(20)));
        rate.sent(152, start + Duration::from_secs(77));
        rate.acknowledged(152, 0, start + Duration::from_millis(77250));
        rate.sent(153, start + Duration::from_secs(78));
        rate.acknowledged(153, 0, start + Duration::from_millis(78250));
        assert!(
            rate.current() < rate.ceiling(),
            "renewed excess delay must remain visible"
        );
    }

    #[test]
    fn backlog_jitter_and_audio_cannot_raise_the_baseline() {
        for queued in [true, false] {
            let start = Instant::now();
            let mut rate = AdaptiveBitrate::new(12_000_000, start);
            rate.sent(0, start);
            rate.acknowledged(0, 0, start + Duration::from_millis(20));
            for id in 1..40 {
                let at = start + Duration::from_millis(u64::from(id) * 500);
                rate.sent(id, at);
                let delay = if queued || id % 2 == 0 { 250 } else { 450 };
                rate.acknowledged(
                    id,
                    if queued { 100_000 } else { 0 },
                    at + Duration::from_millis(delay),
                );
            }
            assert_eq!(rate.baseline, Some(Duration::from_millis(20)));
            assert_eq!(rate.current(), 1_000_000);
        }
        let start = Instant::now();
        let mut rate = AdaptiveBitrate::new(12_000_000, start);
        rate.sent(0, start);
        rate.acknowledged(0, 0, start + Duration::from_millis(20));
        for id in 1..30 {
            let at = start + Duration::from_millis(u64::from(id) * 500);
            rate.audio_congested(at);
            rate.sent(id, at);
            rate.acknowledged(id, 0, at + Duration::from_millis(250));
        }
        assert_eq!(rate.baseline, Some(Duration::from_millis(20)));
        rate.reset_frames();
        assert!(rate.baseline.is_none());
        assert!(rate.last_audio_congestion.is_some());
    }

    #[test]
    fn cadence_and_window_are_bounded_and_follow_pressure() {
        let start = Instant::now();
        let mut rate = AdaptiveBitrate::new(12_000_000, start);
        rate.set_frame_rate_ceiling(30);
        rate.enable_joint_adaptation(true);
        for id in 0..60 {
            let at = start + Duration::from_millis(u64::from(id) * 500);
            rate.sent(id, at);
            rate.acknowledged(id, 0, at + Duration::from_millis(100));
            assert!(rate.sent.len() <= 8);
            assert!((2..=8).contains(&rate.max_frames_in_flight()));
        }
        assert_eq!(rate.frame_rate(), 30);
        assert_eq!(
            rate.max_frames_in_flight(),
            5,
            "credits cover the 100ms ACK delay at the 30fps target"
        );
        rate.audio_congested(start + Duration::from_secs(31));
        assert!(rate.frame_rate() < 30);
        assert!(!rate.permits_refinement(start + Duration::from_secs(31)));
        rate.reset_frames();
        assert_eq!(rate.max_frames_in_flight(), 2);
    }

    /// Model a fixed ACK delay at a 60fps source and return frames delivered
    /// in the final ten seconds of a 30-second run.
    fn delivered_over_wan(rate: &mut AdaptiveBitrate, start: Instant, ack_ms: u64) -> u32 {
        let mut flight = VecDeque::new();
        let (mut id, mut next, mut delivered) = (0, 0, 0);
        for ms in 0..30_000 {
            let now = start + Duration::from_millis(ms);
            while flight.front().is_some_and(|&(_, ack)| ack <= ms) {
                let (id, _) = flight.pop_front().unwrap();
                rate.acknowledged(id, 0, now);
                if ms >= 20_000 {
                    delivered += 1;
                }
            }
            rate.check_stall(now);
            if ms >= next && flight.len() < rate.max_frames_in_flight() as usize {
                rate.sent(id, now);
                flight.push_back((id, ms + ack_ms));
                id += 1;
                next = ms + 17;
            }
            assert!(flight.len() <= 8);
        }
        delivered
    }

    #[test]
    fn default_window_grows_past_two_credits_on_a_healthy_wan_path() {
        // Two fixed credits measured 18-24fps from Windows 10 mstsc at ~33ms
        // RTT. Without joint FPS adaptation, the window still follows the
        // ACK delay so delivery reaches the 60fps target.
        for ack_ms in [33, 60, 100] {
            let start = Instant::now();
            let mut rate = AdaptiveBitrate::new(12_000_000, start);
            rate.set_frame_rate_ceiling(60);
            let delivered = delivered_over_wan(&mut rate, start, ack_ms);
            assert!(rate.max_frames_in_flight() > 2, "{ack_ms}ms");
            assert!(
                delivered >= 550,
                "{ack_ms}ms ACK delay delivered only {delivered} frames in 10s"
            );
            assert_eq!(rate.frame_rate(), 60, "FPS coordination stays opt-in");
        }
    }

    #[test]
    fn default_window_grows_promptly_and_congestion_shrinks_it() {
        let start = Instant::now();
        let mut rate = AdaptiveBitrate::new(12_000_000, start);
        rate.set_frame_rate_ceiling(60);
        rate.sent(0, start);
        rate.acknowledged(0, 0, start + Duration::from_millis(100));
        assert_eq!(rate.max_frames_in_flight(), 8, "first healthy ACK sizes it");
        // A stalled frame shrinks the window one step per reduction interval.
        rate.sent(1, start + Duration::from_millis(200));
        rate.check_stall(start + Duration::from_millis(800));
        assert_eq!(rate.max_frames_in_flight(), 7);
        rate.check_stall(start + Duration::from_millis(900));
        assert_eq!(rate.max_frames_in_flight(), 7, "reduction has a cooldown");
        rate.check_stall(start + Duration::from_millis(1300));
        assert_eq!(rate.max_frames_in_flight(), 6);
        // After congestion, a healthy ACK alone does not regrow it at once.
        rate.sent(2, start + Duration::from_millis(1400));
        rate.acknowledged(2, 0, start + Duration::from_millis(1500));
        assert_eq!(rate.max_frames_in_flight(), 6);
        for (id, ms) in (3..).zip((1600..=5000).step_by(100)) {
            rate.sent(id, start + Duration::from_millis(ms));
            rate.acknowledged(id, 0, start + Duration::from_millis(ms + 100));
        }
        assert_eq!(rate.max_frames_in_flight(), 8, "stable health regrows it");
        rate.reset_frames();
        assert_eq!(rate.max_frames_in_flight(), 2);
    }

    #[test]
    fn full_cadence_delivery_grows_a_bounded_window_without_a_capture_queue() {
        let start = Instant::now();
        let mut rate = AdaptiveBitrate::new(12_000_000, start);
        rate.enable_joint_adaptation(true);
        rate.set_frame_rate_ceiling(60);
        let mut flight = VecDeque::new();
        let (mut id, mut next, mut delivered) = (0, 0, 0);
        for ms in 0..30_000 {
            let now = start + Duration::from_millis(ms);
            while flight.front().is_some_and(|&(_, ack)| ack <= ms) {
                let (id, _) = flight.pop_front().unwrap();
                rate.acknowledged(id, 0, now);
                if ms >= 20_000 {
                    delivered += 1;
                }
            }
            rate.check_stall(now);
            if ms >= next && flight.len() < rate.max_frames_in_flight() as usize {
                rate.sent(id, now);
                flight.push_back((id, ms + 120));
                id += 1;
                next = ms + 17;
            }
            assert!(flight.len() <= 8);
            assert!(rate.sent.len() <= 8);
        }
        assert!(
            delivered >= 500,
            "100ms path must recover >50fps with fresh frames"
        );
        assert_eq!(rate.current(), 12_000_000);
        assert_eq!(rate.max_frames_in_flight(), 8);
    }

    #[test]
    fn audio_congestion_reduces_video_despite_healthy_graphics_and_recovers_afterward() {
        let start = Instant::now();
        let mut rate = AdaptiveBitrate::new(12_000_000, start);
        rate.sent(0, start);
        rate.acknowledged(0, 0, start + Duration::from_millis(20));
        for id in 1..=10 {
            let at = start + Duration::from_millis(u64::from(id) * 500);
            rate.audio_congested(at);
            let reduced = rate.current();
            rate.sent(id, at);
            rate.acknowledged(id, 0, at + Duration::from_millis(20));
            assert_eq!(
                rate.current(),
                reduced,
                "healthy graphics cannot undo sound pressure"
            );
        }
        assert_eq!(rate.current(), 1_000_000);
        let quiet = start + Duration::from_millis(5020);
        let acknowledge = |ids: std::ops::RangeInclusive<u32>, rate: &mut AdaptiveBitrate| {
            for id in ids {
                let at = quiet + Duration::from_millis(u64::from(id - 10) * 100);
                rate.sent(id, at);
                rate.acknowledged(id, 0, at + Duration::from_millis(20));
            }
        };
        acknowledge(11..=109, &mut rate);
        assert_eq!(rate.current(), 1_000_000, "recovery holds for 10 s");
        acknowledge(110..=130, &mut rate);
        assert_eq!(
            rate.current(),
            1_600_000,
            "recovery resumes one level after sound pressure ends"
        );
    }

    #[test]
    fn audio_pressure_applies_without_graphics_acks_and_shares_the_reduction_cooldown() {
        let start = Instant::now();
        let mut rate = AdaptiveBitrate::new(12_000_000, start);
        rate.sent(0, start);
        rate.acknowledged(0, u32::MAX, start + Duration::from_millis(20));
        rate.audio_congested(start + Duration::from_millis(500));
        assert_eq!(rate.current(), 12_000_000, "first reduction waits 1 s");
        rate.audio_congested(start + Duration::from_millis(1000));
        assert_eq!(rate.current(), 7_300_000);
        rate.audio_congested(start + Duration::from_millis(1001));
        rate.check_stall(start + Duration::from_millis(1900));
        assert_eq!(
            rate.current(),
            7_300_000,
            "audio reports do not bypass the cooldown"
        );
        for half_second in 4..30 {
            rate.audio_congested(start + Duration::from_millis(half_second * 500));
        }
        assert_eq!(rate.current(), 1_000_000);
        assert!(
            rate.sent.is_empty(),
            "audio pressure needs no graphics frame sample"
        );
        rate.reset_frames();
        assert!(
            rate.last_audio_congestion.is_some(),
            "graphics reactivation retains audio pressure"
        );
    }

    #[test]
    fn replacement_controllers_inherit_only_recent_audio_pressure() {
        let start = Instant::now();
        let mut previous = AdaptiveBitrate::new(12_000_000, start);
        previous.sent(99, start);
        previous.audio_congested(start + Duration::from_millis(1000));
        let now = start + Duration::from_millis(1100);
        let mut replacement = AdaptiveBitrate::new(12_000_000, now);
        replacement.inherit_audio_congestion(&previous, now);
        assert_eq!(replacement.current(), 7_300_000);
        assert!(replacement.last_audio_congestion.is_some());
        assert!(replacement.sent.is_empty());
        assert!(replacement.baseline.is_none());
        let stale = start + Duration::from_secs(4);
        let mut replacement = AdaptiveBitrate::new(12_000_000, stale);
        replacement.inherit_audio_congestion(&previous, stale);
        assert_eq!(replacement.current(), 12_000_000);
        assert!(replacement.last_audio_congestion.is_none());
        previous.clear_audio_congestion();
        let mut replacement = AdaptiveBitrate::new(12_000_000, now);
        replacement.inherit_audio_congestion(&previous, now);
        assert_eq!(replacement.current(), 12_000_000);
    }

    #[test]
    fn missing_acknowledgements_reduce_to_floor_with_bounded_tracking() {
        let start = Instant::now();
        let mut rate = AdaptiveBitrate::new(12_000_000, start);
        rate.sent(0, start);
        for half_second in 1..30 {
            rate.check_stall(start + Duration::from_millis(half_second * 500 + 1));
        }
        assert_eq!(rate.current(), 1_000_000);
        for id in 1..100 {
            rate.sent(id, start + Duration::from_secs(16));
        }
        assert_eq!(rate.sent.len(), 2);
        rate.reset_frames();
        rate.check_stall(start + Duration::from_secs(30));
        assert!(rate.sent.is_empty());
    }

    #[test]
    fn suspension_and_unknown_acknowledgements_do_not_create_congestion_samples() {
        let start = Instant::now();
        let mut rate = AdaptiveBitrate::new(12_000_000, start);
        rate.sent(0, start);
        rate.acknowledged(0, u32::MAX, start + Duration::from_secs(1));
        for id in 1..100 {
            rate.sent(id, start + Duration::from_secs(2));
        }
        rate.check_stall(start + Duration::from_secs(5));
        assert_eq!(rate.current(), 12_000_000);
        assert!(rate.sent.is_empty());
        rate.acknowledged(99, 0, start + Duration::from_secs(6));
        rate.acknowledged(1000, 99, start + Duration::from_secs(7));
        assert_eq!(rate.current(), 12_000_000);
        rate.sent(100, start + Duration::from_secs(7));
        assert_eq!(rate.sent.len(), 1, "resume enables tracking again");
    }
    /// A source of one frame per `interval` ms (60 fps unless changed) over
    /// a path with a fixed ACK delay, limited by the credit window. `stall` = (first_ms, period_ms, length_ms): nothing is
    /// delivered or acknowledged during a stall; held frames are released
    /// when it ends. `slow` = (since_ms, by_ms) delays every frame sent from
    /// then on and reports a small decoder queue, so the delay is not a
    /// plateau the baseline may relearn. Returns the frames acknowledged.
    struct Path {
        ack_ms: u64,
        stall: Option<(u64, u64, u64)>,
        slow: Option<(u64, u64)>,
        flight: VecDeque<(u32, u64, u32)>,
        id: u32,
        next: u64,
        interval: u64,
    }
    impl Path {
        fn new(ack_ms: u64) -> Self {
            Self {
                ack_ms,
                stall: None,
                slow: None,
                flight: VecDeque::new(),
                id: 0,
                next: 0,
                interval: 17,
            }
        }
        fn release(&self, at: u64) -> u64 {
            match self.stall {
                Some((first, period, length)) if at >= first && (at - first) % period < length => {
                    at - (at - first) % period + length
                }
                _ => at,
            }
        }
        fn run(&mut self, rate: &mut AdaptiveBitrate, start: Instant, from: u64, to: u64) -> u32 {
            let mut delivered = 0;
            for ms in from..to {
                let now = start + Duration::from_millis(ms);
                while self.flight.front().is_some_and(|&(_, ack, _)| ack <= ms) {
                    let (id, _, queue) = self.flight.pop_front().unwrap();
                    rate.acknowledged(id, queue, now);
                    delivered += 1;
                }
                rate.check_stall(now);
                if ms >= self.next && self.flight.len() < rate.max_frames_in_flight() as usize {
                    rate.sent(self.id, now);
                    let slow = self
                        .slow
                        .filter(|&(since, _)| ms >= since)
                        .map_or(0, |(_, by)| by);
                    // The frame reaches the client after any stall, and its
                    // acknowledgement returns after any stall as well.
                    let arrival = self.release(ms + self.ack_ms / 2 + slow);
                    let ack = self.release(arrival + self.ack_ms - self.ack_ms / 2);
                    let ack = self
                        .flight
                        .back()
                        .map_or(ack, |&(_, last, _)| ack.max(last));
                    self.flight
                        .push_back((self.id, ack, if slow > 0 { 10_000 } else { 0 }));
                    self.id += 1;
                    self.next = ms + self.interval;
                }
            }
            delivered
        }
    }

    #[test]
    fn isolated_link_stalls_do_not_latch_the_window_or_the_floor() {
        // 2026-11-11, Windows mstsc over the internet: the client's link went
        // silent for 165-230 ms at a time (every 60 s all day, every 1-3 s for
        // minutes), then delivered everything at once. Each stall reset the
        // health timers, which pinned the window at 2 and the level at the
        // floor on an otherwise healthy 35 ms path.
        let start = Instant::now();
        let mut rate = AdaptiveBitrate::new(12_000_000, start);
        rate.set_frame_rate_ceiling(60);
        let mut path = Path::new(35);
        path.run(&mut rate, start, 0, 1_000);
        // Real congestion first: every frame is 400 ms late for six seconds.
        path.slow = Some((1_000, 400));
        path.run(&mut rate, start, 1_000, 7_000);
        assert_eq!(rate.current(), 1_000_000);
        assert_eq!(rate.max_frames_in_flight(), 2);
        // The path heals, but stalls for 180 ms every 2 s.
        path.slow = None;
        path.stall = Some((8_000, 2_000, 180));
        path.run(&mut rate, start, 7_000, 12_000);
        assert!(
            rate.max_frames_in_flight() >= 4,
            "the window regrows within 5 s despite the stalls: {}",
            rate.max_frames_in_flight()
        );
        let mut level = rate.current();
        for second in 12..70 {
            path.run(&mut rate, start, second * 1_000, (second + 1) * 1_000);
            assert!(rate.current() >= level, "no reduction at {second} s");
            assert!(rate.max_frames_in_flight() >= 4, "window at {second} s");
            level = rate.current();
        }
        assert_eq!(rate.current(), 12_000_000, "five levels regained in 50 s");
        let delivered = path.run(&mut rate, start, 70_000, 80_000);
        assert!(delivered >= 500, "only {delivered} frames in 10 s");
    }

    #[test]
    fn one_stalled_flight_changes_nothing_on_a_healthy_stream() {
        let start = Instant::now();
        let mut rate = AdaptiveBitrate::new(12_000_000, start);
        rate.set_frame_rate_ceiling(60);
        let mut path = Path::new(100);
        path.run(&mut rate, start, 0, 5_000);
        assert_eq!(rate.max_frames_in_flight(), 8);
        // 366 ms was the longest ordinary stall of the day.
        path.stall = Some((5_000, 60_000, 366));
        let delivered = path.run(&mut rate, start, 5_000, 15_000);
        assert_eq!(rate.max_frames_in_flight(), 8);
        assert_eq!(rate.current(), 12_000_000);
        assert!(delivered >= 550, "{delivered}");
    }

    #[test]
    fn sustained_delay_is_congestion_as_soon_as_a_later_frame_is_late() {
        for ack_ms in [35, 100] {
            let start = Instant::now();
            let mut rate = AdaptiveBitrate::new(12_000_000, start);
            rate.set_frame_rate_ceiling(60);
            let mut path = Path::new(ack_ms);
            path.run(&mut rate, start, 0, 5_000);
            let window = rate.max_frames_in_flight();
            path.slow = Some((5_000, 400));
            path.run(&mut rate, start, 5_000, 5_400);
            assert_eq!(
                rate.current(),
                12_000_000,
                "the first late flight is not enough"
            );
            path.run(&mut rate, start, 5_400, 5_750);
            assert!(
                rate.max_frames_in_flight() < window && rate.current() == 7_300_000,
                "{ack_ms} ms: window {} target {}",
                rate.max_frames_in_flight(),
                rate.current()
            );
            path.run(&mut rate, start, 5_750, 11_000);
            assert_eq!(rate.current(), 1_000_000, "one level per second after that");
            assert_eq!(rate.max_frames_in_flight(), 2);
        }
    }

    #[test]
    fn a_frame_outstanding_far_past_the_budget_is_congestion_by_itself() {
        let start = Instant::now();
        let mut rate = AdaptiveBitrate::new(12_000_000, start);
        rate.sent(0, start);
        rate.acknowledged(0, 0, start + Duration::from_millis(30));
        let sent = start + Duration::from_secs(2);
        rate.sent(1, sent);
        rate.check_stall(sent + Duration::from_millis(151));
        rate.check_stall(sent + Duration::from_millis(500));
        assert_eq!(rate.current(), 12_000_000, "within the stall grace");
        rate.check_stall(sent + Duration::from_millis(501));
        assert_eq!(rate.current(), 7_300_000, "150 ms budget plus 350 ms");
        // The same for a late acknowledgement with nothing sent since.
        let mut rate = AdaptiveBitrate::new(12_000_000, start);
        rate.sent(0, start);
        rate.acknowledged(0, 0, start + Duration::from_millis(30));
        rate.sent(1, sent);
        rate.acknowledged(1, 0, sent + Duration::from_millis(501));
        assert_eq!(rate.current(), 7_300_000);
    }

    #[test]
    fn delay_episodes_in_quick_succession_are_congestion() {
        for (gap_ms, reduced) in [(499, true), (500, false)] {
            let start = Instant::now();
            let at = |ms| start + Duration::from_millis(ms);
            let mut rate = AdaptiveBitrate::new(12_000_000, start);
            rate.sent(0, at(0));
            rate.acknowledged(0, 0, at(30));
            rate.sent(1, at(2_000));
            rate.acknowledged(1, 0, at(2_200));
            rate.sent(2, at(2_210));
            rate.acknowledged(2, 0, at(2_240));
            assert_eq!(rate.current(), 12_000_000);
            rate.sent(3, at(2_240 + gap_ms - 200));
            rate.acknowledged(3, 0, at(2_240 + gap_ms));
            assert_eq!(rate.current() < 12_000_000, reduced, "{gap_ms} ms");
        }
    }

    #[test]
    fn a_tolerated_episode_holds_growth_until_a_later_frame_is_healthy() {
        let start = Instant::now();
        let at = |ms| start + Duration::from_millis(ms);
        let mut rate = AdaptiveBitrate::new(12_000_000, start);
        rate.set_frame_rate_ceiling(60);
        rate.sent(0, at(0));
        rate.acknowledged(0, 0, at(20));
        assert_eq!(rate.max_frames_in_flight(), 3);
        rate.sent(1, at(1_000));
        rate.sent(2, at(1_100));
        rate.check_stall(at(1_160));
        // Both frames were in flight when the stall was noticed; the second
        // then arrives within the budget.
        rate.acknowledged(1, 0, at(1_200));
        rate.acknowledged(2, 0, at(1_205));
        assert_eq!(
            rate.max_frames_in_flight(),
            3,
            "no growth from a late flight"
        );
        rate.sent(3, at(1_210));
        rate.acknowledged(3, 0, at(1_310));
        assert_eq!(
            rate.max_frames_in_flight(),
            8,
            "the next healthy frame sizes it"
        );
        assert_eq!(rate.current(), 12_000_000);
    }
    #[test]
    fn a_stall_before_the_screen_goes_still_is_resolved_by_a_refinement_frame() {
        let start = Instant::now();
        let at = |ms| start + Duration::from_millis(ms);
        let mut rate = AdaptiveBitrate::new(12_000_000, start);
        rate.set_frame_rate_ceiling(60);
        rate.sent(0, at(0));
        rate.acknowledged(0, 0, at(35));
        let window = rate.max_frames_in_flight();
        // The last two frames before the desktop stops changing meet a stall.
        rate.sent(1, at(1_000));
        rate.sent(2, at(1_016));
        rate.check_stall(at(1_160));
        assert!(
            !rate.permits_refinement(at(1_160)),
            "not while the delayed flight is outstanding"
        );
        rate.acknowledged(1, 0, at(1_200));
        assert!(!rate.permits_refinement(at(1_200)));
        rate.acknowledged(2, 0, at(1_205));
        // Nothing else will be sent on a still screen: the refinement frame
        // is the later frame that shows whether the path recovered.
        assert!(rate.permits_refinement(at(1_300)));
        rate.sent(3, at(1_300));
        assert!(
            !rate.permits_refinement(at(1_310)),
            "one probe at a time until the episode is resolved"
        );
        rate.acknowledged(3, 0, at(1_335));
        assert!(rate.permits_refinement(at(1_340)));
        rate.sent(4, at(1_340));
        assert!(
            rate.permits_refinement(at(1_350)),
            "resolved: two in flight again"
        );
        rate.acknowledged(4, 0, at(1_375));
        assert_eq!(rate.current(), 12_000_000);
        assert!(rate.max_frames_in_flight() >= window);
        // Had the probe been late as well, that is sustained delay.
        let mut rate = AdaptiveBitrate::new(12_000_000, start);
        rate.set_frame_rate_ceiling(60);
        rate.sent(0, at(0));
        rate.acknowledged(0, 0, at(35));
        rate.sent(1, at(1_000));
        rate.acknowledged(1, 0, at(1_200));
        assert_eq!(rate.current(), 12_000_000);
        assert!(rate.permits_refinement(at(1_300)));
        rate.sent(2, at(1_300));
        rate.acknowledged(2, 0, at(1_500));
        assert_eq!(rate.current(), 7_300_000);
    }

    #[test]
    fn an_unresolved_delay_episode_does_not_outlive_an_idle_stream() {
        let start = Instant::now();
        let at = |ms| start + Duration::from_millis(ms);
        let stalled_then_next_frame_after = |idle_ms: u64| {
            let mut rate = AdaptiveBitrate::new(12_000_000, start);
            rate.set_frame_rate_ceiling(60);
            rate.sent(0, at(0));
            rate.acknowledged(0, 0, at(35));
            // A late frame, then the desktop goes still with no refinement.
            rate.sent(1, at(1_000));
            rate.acknowledged(1, 0, at(1_200));
            // The next frame is late too.
            rate.sent(2, at(1_200 + idle_ms));
            rate.acknowledged(2, 0, at(1_200 + idle_ms + 200));
            rate.current()
        };
        assert_eq!(
            stalled_then_next_frame_after(1_000),
            7_300_000,
            "a later frame late soon afterwards is sustained delay"
        );
        assert_eq!(
            stalled_then_next_frame_after(60_000),
            12_000_000,
            "after an idle stream a late frame is a first one again"
        );
    }

    #[test]
    fn a_stall_while_frames_are_sparse_is_tolerated_at_any_frame_rate() {
        // Typing at about 6 fps over a 35 ms path, with a 330 ms link stall:
        // the stall is noticed while it lasts, and the next keystroke's frame
        // is sent into the same silence. It is not a later, independently
        // late frame; the first acknowledgement after the stall is where the
        // episode's evidence starts.
        for interval in [333, 167, 100, 67, 33, 17] {
            let start = Instant::now();
            let mut rate = AdaptiveBitrate::new(12_000_000, start);
            rate.set_frame_rate_ceiling(60);
            let mut path = Path::new(35);
            path.interval = interval;
            path.run(&mut rate, start, 0, 5_000);
            let window = rate.max_frames_in_flight();
            path.stall = Some((5_000, 60_000, 330));
            path.run(&mut rate, start, 5_000, 9_000);
            assert_eq!(rate.current(), 12_000_000, "{interval} ms between frames");
            assert_eq!(rate.max_frames_in_flight(), window, "{interval} ms");
            assert!(rate.suspect.is_none(), "{interval} ms: the episode ended");
        }
    }

    #[test]
    fn sustained_delay_is_still_congestion_when_frames_are_sparse() {
        // Acknowledgements keep arriving, 400 ms late: the first one sets
        // the boundary, and a frame sent after it that is late as well
        // confirms. With frames this far apart that is one frame interval
        // and one budget after the first late acknowledgement.
        for (interval, by_ms) in [(167, 5_900), (100, 5_800), (33, 5_750)] {
            let start = Instant::now();
            let mut rate = AdaptiveBitrate::new(12_000_000, start);
            rate.set_frame_rate_ceiling(60);
            let mut path = Path::new(35);
            path.interval = interval;
            path.run(&mut rate, start, 0, 5_000);
            path.slow = Some((5_000, 400));
            path.run(&mut rate, start, 5_000, 5_400);
            assert_eq!(
                rate.current(),
                12_000_000,
                "{interval} ms: nothing acknowledged yet"
            );
            path.run(&mut rate, start, 5_400, by_ms);
            assert_eq!(rate.current(), 7_300_000, "{interval} ms between frames");
            path.run(&mut rate, start, by_ms, 12_000);
            assert_eq!(
                rate.current(),
                1_000_000,
                "{interval} ms: down to the floor"
            );
        }
        // An outage with no acknowledgement at all is caught by the stall
        // grace alone, sparse or not.
        let start = Instant::now();
        let at = |ms| start + Duration::from_millis(ms);
        let mut rate = AdaptiveBitrate::new(12_000_000, start);
        rate.sent(0, at(0));
        rate.acknowledged(0, 0, at(35));
        rate.sent(1, at(2_000));
        rate.check_stall(at(2_160));
        rate.sent(2, at(2_167));
        rate.check_stall(at(2_330));
        assert_eq!(rate.current(), 12_000_000, "a frame sent into the silence");
        rate.check_stall(at(2_501));
        assert_eq!(rate.current(), 7_300_000, "budget plus stall grace");
    }

    #[test]
    fn a_sound_pressure_cut_stays_urgent_until_the_encoder_adopts_it() {
        let start = Instant::now();
        let at = |ms| start + Duration::from_millis(ms);
        let mut rate = AdaptiveBitrate::new(12_000_000, start);
        let mut applier = RateApplier::new(Duration::from_secs(30));
        assert_eq!(
            applier.select(
                rate.current(),
                60,
                true,
                rate.audio_urgent(at(0)),
                false,
                at(0)
            ),
            12_000_000
        );
        applier.idr_sent(at(0));
        assert!(!rate.audio_urgent(at(0)));
        rate.audio_congested(at(1_000));
        assert_eq!(rate.current(), 7_300_000);
        // The screen is still. 3.5 s later the pressure itself has aged out,
        // but the cut has not reached the encoder.
        assert!(!rate.audio_pressure(at(4_500)) && rate.audio_urgent(at(4_500)));
        // A refinement frame is held at the applied rate and consumes nothing.
        let held = applier.select(rate.current(), 60, false, true, true, at(4_500));
        assert_eq!(held, 12_000_000);
        rate.rate_adopted(held);
        assert!(rate.audio_urgent(at(4_500)));
        // Neither a new sound backend nor a graphics reset undoes the cut.
        rate.clear_audio_congestion();
        rate.reset_frames();
        assert_eq!(rate.current(), 7_300_000);
        assert!(rate.audio_urgent(at(60_000)));
        // The next changed frame, a minute later and with no payload to
        // measure, is encoded at the reduced rate.
        let urgent = rate.audio_urgent(at(60_000));
        let adopted = applier.select(rate.current(), 60, false, urgent, false, at(60_000));
        assert_eq!(adopted, 7_300_000);
        rate.rate_adopted(adopted);
        assert!(!rate.audio_urgent(at(60_000)), "delivered once");
        // A cut made while the encoder is already at or below the target is
        // settled by the next frame without a reconfiguration.
        let mut rate = AdaptiveBitrate::new(1_000_000, start);
        rate.audio_congested(at(1_000));
        assert!(rate.audio_urgent(at(9_000)));
        rate.rate_adopted(1_000_000);
        assert!(!rate.audio_urgent(at(9_000)));
        // A replacement controller inherits an undelivered cut with the
        // pressure and the level.
        let mut previous = AdaptiveBitrate::new(12_000_000, start);
        previous.audio_congested(at(1_000));
        let mut replacement = AdaptiveBitrate::new(12_000_000, at(1_100));
        replacement.inherit_audio_congestion(&previous, at(1_100));
        assert_eq!(replacement.current(), 7_300_000);
        assert!(replacement.audio_urgent(at(60_000)));
        // The applier reports the cadence a refinement frame must keep.
        assert_eq!(applier.applied_fps(), Some(60));
        assert_eq!(RateApplier::new(Duration::ZERO).applied_fps(), None);
    }

    #[test]
    fn performance_settings_are_bounded_and_strict() {
        for maximum in [60, 100] {
            assert_eq!(setting("test", None, 30, maximum).unwrap(), 30);
            assert_eq!(setting("test", Some("1"), 30, maximum).unwrap(), 1);
            assert_eq!(
                setting("test", Some(&maximum.to_string()), 30, maximum).unwrap(),
                maximum
            );
            for value in ["0", "-1", "1.5", "", " 30", "4294967296"] {
                assert!(setting("test", Some(value), 30, maximum).is_err());
            }
            assert!(setting("test", Some(&(maximum + 1).to_string()), 30, maximum).is_err());
        }
    }
    #[test]
    fn explicit_link_budget_reserves_pcm_without_raising_user_video_ceiling() {
        assert_eq!(effective_bitrate(12_000_000, Some(5)).unwrap(), 3_300_000);
        assert_eq!(effective_bitrate(1_000_000, Some(5)).unwrap(), 1_000_000);
        assert_eq!(effective_bitrate(12_000_000, None).unwrap(), 12_000_000);
        for link in [0, 1, 2, 101, u32::MAX] {
            assert!(effective_bitrate(12_000_000, Some(link)).is_err());
        }
    }
}
