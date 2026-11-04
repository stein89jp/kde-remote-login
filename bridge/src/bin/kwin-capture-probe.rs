//! Disposable lab probe: real KWin screencast capture through `Desktop`, for
//! `lab/kwin/bridge-integration.sh`. Prints one JSON line per check.
//!
//!   kwin-capture-probe /abs/wayland-socket [output]
//!
//! PipeWire: `KDE_RDP_PIPEWIRE_SOCKET`, else `pipewire-0` next to the Wayland
//! socket. `KDE_RDP_PROBE_SAVE=PREFIX` writes PPM frames. `KDE_RDP_PROBE_SIZE`
//! (default 1280x720) is the start size; `KDE_RDP_PROBE_RESIZE=WxH[,WxH...]`
//! resizes through the RDP display-control path; `KDE_RDP_PROBE_GPU=1`
//! switches to DMA-BUF tickets and back. Needs an animated window on screen,
//! unless `KDE_RDP_PROBE_STATIC=1` (renegotiation on a quiet desktop).
use ironrdp_displaycontrol::pdu::DisplayControlMonitorLayout;
use ironrdp_server::{
    BitmapUpdate, DesktopSize, DisplayUpdate, MouseEvent, RdpServerDisplay, RdpServerInputHandler,
};
use kde_rdp_bridge::wayland::Desktop;
use std::{
    io::Write,
    os::unix::net::UnixStream,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

fn size(text: &str) -> anyhow::Result<DesktopSize> {
    let (w, h) = text
        .split_once('x')
        .ok_or_else(|| anyhow::anyhow!("size must be WxH"))?;
    Ok(DesktopSize {
        width: w.parse()?,
        height: h.parse()?,
    })
}

fn bitmap(d: &Desktop) -> Option<BitmapUpdate> {
    match d.snapshot()?.as_ref() {
        DisplayUpdate::Bitmap(image) => Some(image.clone()),
        _ => None,
    }
}

fn wait<T>(what: &str, timeout: Duration, mut f: impl FnMut() -> Option<T>) -> anyhow::Result<T> {
    let started = Instant::now();
    loop {
        if let Some(value) = f() {
            return Ok(value);
        }
        anyhow::ensure!(started.elapsed() < timeout, "{what} timed out");
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn save(prefix: Option<&str>, name: &str, image: &BitmapUpdate) -> anyhow::Result<()> {
    let Some(prefix) = prefix else {
        return Ok(());
    };
    let (w, h) = (
        usize::from(image.width.get()),
        usize::from(image.height.get()),
    );
    let mut out = std::fs::File::create(format!("{prefix}-{name}.ppm"))?;
    write!(out, "P6\n{w} {h}\n255\n")?;
    let mut rgb = Vec::with_capacity(w * h * 3);
    for row in image.data.chunks(image.stride.get()).take(h) {
        for px in row[..w * 4].as_chunks::<4>().0 {
            rgb.extend_from_slice(&[px[2], px[1], px[0]]);
        }
    }
    out.write_all(&rgb)?;
    Ok(())
}

fn main() -> anyhow::Result<()> {
    let args: Vec<_> = std::env::args().collect();
    anyhow::ensure!(
        (2..=3).contains(&args.len()),
        "usage: kwin-capture-probe /abs/wayland-socket [output]"
    );
    let wayland = Path::new(&args[1]);
    anyhow::ensure!(wayland.is_absolute(), "explicit absolute socket required");
    let pipewire = std::env::var_os("KDE_RDP_PIPEWIRE_SOCKET")
        .map(PathBuf::from)
        .unwrap_or_else(|| wayland.with_file_name("pipewire-0"));
    let prefix = std::env::var("KDE_RDP_PROBE_SAVE").ok();
    let start = size(&std::env::var("KDE_RDP_PROBE_SIZE").unwrap_or_else(|_| "1280x720".into()))?;
    let started = Instant::now();
    let mut d = Desktop::start(
        UnixStream::connect(wayland)?,
        Some(UnixStream::connect(&pipewire)?.into()),
        args.get(2).cloned().unwrap_or_default(),
        start,
        None,
    )?;
    let first = bitmap(&d).ok_or_else(|| anyhow::anyhow!("no first frame"))?;
    println!(
        "{{\"probe\":\"start\",\"ms\":{},\"width\":{},\"height\":{},\"gpu_token\":{}}}",
        started.elapsed().as_millis(),
        first.width,
        first.height,
        d.is_gpu_bitmap(&first)
    );
    save(prefix.as_deref(), "start", &first)?;
    // Frames keep arriving while the test window animates.
    let mut frames = 0u32;
    let mut last = first.data.as_ptr();
    let counted = Instant::now();
    while counted.elapsed() < Duration::from_secs(2) {
        if let Some(image) = bitmap(&d)
            && image.data.as_ptr() != last
        {
            last = image.data.as_ptr();
            frames += 1;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    println!("{{\"probe\":\"frames\",\"seconds\":2,\"distinct_snapshots\":{frames}}}");
    if std::env::var("KDE_RDP_PROBE_STATIC").as_deref() != Ok("1") {
        anyhow::ensure!(frames > 0, "no new frames while the window animates");
    }
    // Pointer input reaches KWin and comes back as cursor metadata.
    d.set_cursor_max_size(96);
    d.mouse(MouseEvent::Move { x: 200, y: 150 });
    std::thread::sleep(Duration::from_millis(300));
    anyhow::ensure!(d.alive(), "worker died after cursor activation");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()?;
    if let Ok(list) = std::env::var("KDE_RDP_PROBE_RESIZE") {
        for next in list.split(',') {
            let next = size(next)?;
            let mut updates = d.updates_full_send();
            let asked = Instant::now();
            d.request_layout(DisplayControlMonitorLayout::new_single_primary_monitor(
                u32::from(next.width),
                u32::from(next.height),
                None,
                None,
            )?);
            let announced = runtime.block_on(async {
                tokio::time::timeout(Duration::from_secs(5), async {
                    loop {
                        match updates.next_update().await? {
                            Some(DisplayUpdate::Resize(size)) => return anyhow::Ok(size),
                            None => anyhow::bail!("capture ended during resize"),
                            _ => {}
                        }
                    }
                })
                .await?
            })?;
            let image = wait("resized frame", Duration::from_secs(5), || {
                bitmap(&d).filter(|image| {
                    (image.width.get(), image.height.get()) == (next.width, next.height)
                })
            })?;
            println!(
                "{{\"probe\":\"resize\",\"width\":{},\"height\":{},\"announced\":\"{}x{}\",\"ms\":{}}}",
                next.width,
                next.height,
                announced.width,
                announced.height,
                asked.elapsed().as_millis()
            );
            anyhow::ensure!(announced == next, "Resize announced a different size");
            anyhow::ensure!(d.size().await_size() == next, "desktop size not updated");
            if !d.is_gpu_bitmap(&image) {
                save(
                    prefix.as_deref(),
                    &format!("{}x{}", next.width, next.height),
                    &image,
                )?;
            }
        }
    }
    if std::env::var("KDE_RDP_PROBE_GPU").as_deref() == Ok("1") {
        let asked = Instant::now();
        d.set_gpu_capture(true);
        let frame = wait("DMA-BUF ticket", Duration::from_secs(5), || {
            d.set_gpu_capture(true);
            bitmap(&d)
                .filter(|image| d.is_gpu_bitmap(image))
                .and_then(|image| d.gpu_frame_for(&image))
        })?;
        println!(
            "{{\"probe\":\"gpu\",\"ms\":{},\"width\":{},\"height\":{},\"fourcc\":\"0x{:08x}\",\"modifier\":\"0x{:016x}\",\"stride\":{},\"offset\":{},\"device\":{}}}",
            asked.elapsed().as_millis(),
            frame.width,
            frame.height,
            frame.fourcc,
            frame.modifier,
            frame.stride,
            frame.offset,
            frame.device
        );
        drop(frame);
        let asked = Instant::now();
        d.set_gpu_capture(false);
        let image = wait("SHM after DMA-BUF", Duration::from_secs(5), || {
            bitmap(&d).filter(|image| !d.is_gpu_bitmap(image))
        })?;
        println!(
            "{{\"probe\":\"gpu_off\",\"ms\":{},\"width\":{},\"height\":{}}}",
            asked.elapsed().as_millis(),
            image.width,
            image.height
        );
        save(prefix.as_deref(), "after-gpu", &image)?;
    }
    // A quiet desktop (the window stopped animating): KWin sends no frames,
    // yet the worker stays alive on wl_display.sync health probes.
    if let Ok(seconds) = std::env::var("KDE_RDP_PROBE_IDLE_SECONDS") {
        let seconds: u64 = seconds.parse()?;
        std::thread::sleep(Duration::from_secs(seconds));
        let mut changes = 0u32;
        let mut last = bitmap(&d).map(|image| image.data.as_ptr());
        let counted = Instant::now();
        while counted.elapsed() < Duration::from_secs(3) {
            let now = bitmap(&d).map(|image| image.data.as_ptr());
            if now != last {
                changes += 1;
                last = now;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        anyhow::ensure!(
            d.alive(),
            "quiet desktop ended the worker: {:?}",
            d.failure()
        );
        println!(
            "{{\"probe\":\"idle\",\"after_seconds\":{seconds},\"new_frames_in_3s\":{changes},\"alive\":true}}"
        );
    }
    // Cursor metadata: once the window (which sets no cursor) is gone, KWin's
    // default cursor arrives as an RDP pointer shape, and the bridge's own
    // motion is never echoed back as a server warp.
    if std::env::var("KDE_RDP_PROBE_CURSOR").as_deref() == Ok("1") {
        let mut updates = d.updates_send();
        let asked = Instant::now();
        let mut step = 0u16;
        let shape = runtime.block_on(async {
            loop {
                anyhow::ensure!(asked.elapsed() < Duration::from_secs(20), "no cursor shape");
                step = step.wrapping_add(1);
                d.mouse(MouseEvent::Move {
                    x: 100 + step % 200,
                    y: 100 + step % 150,
                });
                match tokio::time::timeout(Duration::from_millis(50), updates.next_update()).await {
                    Ok(Ok(Some(DisplayUpdate::RGBAPointer(p)))) => {
                        return Ok((p.width, p.height, p.hot_x, p.hot_y));
                    }
                    Ok(Ok(Some(DisplayUpdate::LargePointer(p)))) => {
                        return Ok((p.width, p.height, p.hot_x, p.hot_y));
                    }
                    Ok(Ok(None)) => anyhow::bail!("capture ended while waiting for a cursor"),
                    _ => {}
                }
            }
        })?;
        let mut echoes = 0;
        runtime.block_on(async {
            // Earlier real warps (KWin keeps the pointer's relative position
            // when the output resizes) may still be queued on this stream.
            while let Ok(Ok(Some(_))) =
                tokio::time::timeout(Duration::from_millis(200), updates.next_update()).await
            {
            }
            for i in 0..40u16 {
                d.mouse(MouseEvent::Move {
                    x: 50 + i * 10,
                    y: 60 + i * 5,
                });
                if let Ok(Ok(Some(DisplayUpdate::PointerPosition(_)))) =
                    tokio::time::timeout(Duration::from_millis(25), updates.next_update()).await
                {
                    echoes += 1;
                }
            }
        });
        println!(
            "{{\"probe\":\"cursor\",\"ms\":{},\"width\":{},\"height\":{},\"hot_x\":{},\"hot_y\":{},\"motion_echoes\":{echoes}}}",
            asked.elapsed().as_millis(),
            shape.0,
            shape.1,
            shape.2,
            shape.3
        );
        anyhow::ensure!(echoes == 0, "own motion echoed as server warps");
    }
    // A client without pointer support: a new KWin stream with the cursor
    // embedded in the frames, and back to metadata for the next activation.
    if std::env::var("KDE_RDP_PROBE_EMBEDDED").as_deref() == Ok("1") {
        for (max, mode) in [(0u16, "embedded"), (96, "metadata")] {
            let before = bitmap(&d).map(|image| image.data.as_ptr());
            let asked = Instant::now();
            d.set_cursor_max_size(max);
            let image = wait(
                "frame from the restarted stream",
                Duration::from_secs(5),
                || bitmap(&d).filter(|image| Some(image.data.as_ptr()) != before),
            )?;
            anyhow::ensure!(
                d.alive(),
                "stream restart ended the worker: {:?}",
                d.failure()
            );
            println!(
                "{{\"probe\":\"cursor_mode\",\"mode\":\"{mode}\",\"ms\":{},\"width\":{},\"height\":{}}}",
                asked.elapsed().as_millis(),
                image.width,
                image.height
            );
            if max == 0 {
                save(prefix.as_deref(), "embedded-cursor", &image)?;
            }
        }
    }
    d.stop();
    // Let the worker finish its teardown: exiting while it still releases
    // PipeWire DMA-BUFs crashed in NVIDIA's EGL exit handlers in the lab.
    let delay = std::env::var("KDE_RDP_PROBE_EXIT_DELAY_MS").unwrap_or_else(|_| "1000".into());
    std::thread::sleep(Duration::from_millis(delay.parse()?));
    println!("{{\"probe\":\"done\",\"ok\":true}}");
    Ok(())
}

/// `RdpServerDisplay::size` is async; the probe polls it synchronously.
trait AwaitSize {
    fn await_size(self) -> DesktopSize;
}
impl<F: std::future::Future<Output = DesktopSize>> AwaitSize for F {
    fn await_size(self) -> DesktopSize {
        tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(self)
    }
}
