//! Deterministic delivery-model measurements of the actual graphics controller.
//! Synthetic ACK/serialization timing is not a network or presentation result.
//!
//! `stall_ms` models a link outage starting 12 s into the run: nothing is
//! serialized or acknowledged while it lasts, and everything held is released
//! when it ends. With `stall_period_ms` the outage repeats at that period,
//! like the 60-second and clustered client-link stalls measured on 2026-11-11.
#[allow(dead_code)]
#[path = "../performance.rs"]
mod performance;
use performance::AdaptiveBitrate;
use std::{
    collections::VecDeque,
    time::{Duration, Instant},
};
struct Frame {
    id: u32,
    sent: u64,
    ack: u64,
}
const STALL_START: u64 = 12_000_000;
fn main() -> anyhow::Result<()> {
    let args: Vec<_> = std::env::args().collect();
    anyhow::ensure!(
        (5..=7).contains(&args.len()),
        "usage: controller-bench rtt_ms link_mbps joint_0_or_1 stall_ms [stall_period_ms [seconds]]"
    );
    let rtt: u64 = args[1].parse()?;
    let link: u64 = args[2].parse()?;
    let joint: u32 = args[3].parse()?;
    let stall: u64 = args[4].parse()?;
    let period: u64 = args.get(5).map_or(Ok(0), |value| value.parse())?;
    let seconds: u64 = args.get(6).map_or(Ok(30), |value| value.parse())?;
    anyhow::ensure!(
        (1..=2000).contains(&rtt)
            && (3..=100).contains(&link)
            && joint <= 1
            && stall <= 5000
            && (period == 0 || (period > stall && period <= 600_000))
            && (20..=3600).contains(&seconds),
        "invalid model settings"
    );
    // The end of the outage containing `us`, or `us` itself outside one.
    let released = |us: u64| -> u64 {
        if stall == 0 || us < STALL_START {
            return us;
        }
        let into = match period {
            0 => us - STALL_START,
            period => (us - STALL_START) % (period * 1000),
        };
        if into < stall * 1000 {
            us - into + stall * 1000
        } else {
            us
        }
    };
    let start = Instant::now();
    let ceiling = ((link * 1_000_000 - 1_700_000).min(12_000_000)) as u32;
    let mut controller = AdaptiveBitrate::new(ceiling, start);
    controller.set_frame_rate_ceiling(60);
    controller.enable_joint_adaptation(joint == 1);
    let mut flight = VecDeque::<Frame>::new();
    let (mut id, mut next_send, mut wire_free, mut peak) = (0, 0, 0, 0);
    let mut ages = Vec::new();
    let mut final_frames = 0;
    // From the first outage on: window and target sums, and time at the floor.
    let (mut samples, mut windows, mut targets, mut floor_ms) = (0u64, 0u64, 0u64, 0u64);
    let end = seconds * 1_000_000;
    for us in (0..end).step_by(1000) {
        let now = start + Duration::from_micros(us);
        while flight.front().is_some_and(|frame| frame.ack <= us) {
            let frame = flight.pop_front().unwrap();
            controller.acknowledged(frame.id, 0, now);
            ages.push(us - frame.sent);
            if us >= end - 10_000_000 {
                final_frames += 1;
            }
        }
        controller.check_stall(now);
        if us >= STALL_START {
            samples += 1;
            windows += u64::from(controller.max_frames_in_flight());
            targets += u64::from(controller.current());
            floor_ms += u64::from(controller.current() == 1_000_000 && ceiling > 1_000_000);
        }
        if us < next_send || flight.len() >= controller.max_frames_in_flight() as usize {
            continue;
        }
        let fps = controller.frame_rate();
        let serialized = u64::from(controller.current()) / u64::from(fps) * 1_000_000
            / (link * 1_000_000 - 1_700_000);
        let wire_start = released(us.max(wire_free));
        wire_free = wire_start + serialized;
        // Acknowledgements are held by an outage too, and arrive in order.
        let ack = released(wire_free + rtt * 1000 + 3000);
        let ack = flight.back().map_or(ack, |last| ack.max(last.ack));
        flight.push_back(Frame { id, sent: us, ack });
        controller.sent(id, now);
        id += 1;
        peak = peak.max(flight.len());
        next_send = us + 1_000_000 / u64::from(fps);
    }
    ages.sort_unstable();
    anyhow::ensure!(!ages.is_empty(), "model delivered no frames");
    println!(
        "{{\"scope\":\"controller_delivery_model\",\"physical_presentation_measured\":false,\"rtt_ms\":{rtt},\"link_mbps_including_pcm\":{link},\"pcm_reserve_mbps\":1.7,\"joint_adaptation\":{},\"stall_ms\":{stall},\"stall_period_ms\":{period},\"seconds\":{seconds},\"delivered_frames\":{},\"last_10s_fps\":{},\"frame_age_p50_ms\":{},\"frame_age_p95_ms\":{},\"frame_age_max_ms\":{},\"peak_outstanding_frames\":{peak},\"final_target_bps\":{},\"final_target_fps\":{},\"final_window\":{},\"mean_window_after_12s\":{:.1},\"mean_target_bps_after_12s\":{},\"seconds_at_floor\":{:.1}}}",
        joint == 1,
        ages.len(),
        f64::from(final_frames) / 10.0,
        ages[ages.len() / 2] as f64 / 1000.0,
        ages[ages.len() * 95 / 100] as f64 / 1000.0,
        ages.last().unwrap().to_owned() as f64 / 1000.0,
        controller.current(),
        controller.frame_rate(),
        controller.max_frames_in_flight(),
        windows as f64 / samples.max(1) as f64,
        targets / samples.max(1),
        floor_ms as f64 / 1000.0
    );
    Ok(())
}
