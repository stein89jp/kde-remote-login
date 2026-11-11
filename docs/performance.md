# Performance controls and reproducible validation

The remote desktop keeps one display and bounded latest-frame delivery. Since
2026-10-20 the audio baseline prefers 44.1 kHz stereo S16LE PCM in 20 ms blocks
(about 1.41 Mbit/s, resampled from the 48 kHz capture because mstsc played
48 kHz PCM about 9% slow), then 48 kHz stereo PCM (mono PCM is opt-in; it
sounded pitched-up and robotic in mstsc), with both
RDPSND timestamps zero and an earned 20 ms drift catch-up during continuous PCM sound (since
2026-10-20 a drop needs an estimated client-queue surplus above a 60 ms jitter
reserve, replacing a blind drop every 30 s; `KDE_RDP_AUDIO_CHANNELS`,
`KDE_RDP_AUDIO_RATE` and `KDE_RDP_AUDIO_DRIFT_PPM` default to that baseline;
[session audio](../bridge/README.md#session-audio)); a 20-minute
continuous YouTube playback in actual Windows 10 mstsc was clean and in sync on
2026-10-20 (longer, constrained-bandwidth and reconnect listening remain
[acceptance](acceptance.md) work). Audio and video measurements use internal monotonic
time; graphics acknowledgements do not establish physical presentation or an
audio/video clock.

## Configuration

Set frontend environment variables in `/etc/kde-rdp/frontend.conf`, then apply
them during your planned reinstall or service restart. Configuration is validated
before desktop capture starts.

| Variable | Values and precedence |
| --- | --- |
| `KDE_RDP_ENCODER` | `bitmap` (default), `nvenc`; NVENC validates the hardware encoder at startup, and clients without a compatible graphics codec receive bitmaps |
| `KDE_RDP_FPS` | 1–60; a ceiling, default 30 for bitmap and 60 for NVENC |
| `KDE_RDP_BITRATE_MBPS` | 1–100; video ceiling, default 12. Adaptation uses 6 quantized levels down to 1 Mbps; sustained delivery delay lowers the level at most once per second (one delayed flight alone does not), 10 s of healthy delivery raises it. The encoder is reconfigured, which costs an IDR, only when the new level changes what it sends (see [rate levels](#rate-levels-and-encoder-reconfiguration)) |
| `KDE_RDP_LINK_MBPS` | Optional known total link budget, 3–100; reserves 1.7 Mbps for PCM plus framing and sets the video target below the remaining budget |
| `KDE_RDP_VIDEO_PROFILE` | `speed` (existing p1 default), `balanced` (p4 with spatial AQ), `quality` (p5 with spatial AQ); B frames and lookahead remain disabled |
| `KDE_RDP_VIDEO_CODEC` | `avc420` (default), `auto`, `avc444`; negotiated client support determines the usable codec, with AVC420 and bitmap fallbacks |
| `KDE_RDP_GPU_CAPTURE` | `auto` (default with NVENC) or `shm` to force CPU readback; DMA-BUF import/conversion is selected only with a supported capture, GPU and encoder combination; SHM remains the fallback, and bitmap mode always uses SHM |
| `KDE_RDP_GOP_SECONDS` | 1–3600, default 3600 (30 until 2026-11-11); seconds of encoded frames between periodic NVENC IDRs, so by default there is effectively none. Refresh, resize, resume, frame drops and other recovery points still force IDRs immediately; a periodic IDR recovers nothing over reliable RDPGFX and leaves a settled page visibly soft until it is refined again |
| `KDE_RDP_MIN_QP` | 1–51, default 18; the lowest H.264 quantiser the encoder may use, for I and P frames alike. Lower values spend bitrate on differences that are not visible, higher values save a little more and soften text: 16 and 20 are the neighbours to compare (see [quantiser floor](#quantiser-floor)). `1` is effectively no floor; still-screen refinement then ends on its frame cap instead of at the floor |
| `KDE_RDP_GPU_DEVICE` | Optional NVIDIA DRM render node, for example `/dev/dri/renderD128`; otherwise readable render nodes are probed in order. Applies to `auto` GPU capture |
| `KDE_RDP_SCALE` | `client` or explicit 1.0–5.0; an explicit value takes precedence over client Display Control scale metadata |
| `KDE_RDP_ADAPTIVE_GRAPHICS` | `1` opts into coordinated FPS, which lowers the cadence with the bitrate level (not below 10); `0` (default) keeps the configured FPS. Either way the graphics credit window follows the measured ACK delay (2–8 frames, see below) |
| `KDE_RDP_MICROPHONE` | `auto` (default) offers mstsc microphone redirection (MS-RDPEAI `AUDIO_INPUT`, 48 kHz mono PCM preferred, about 0.8 Mbit/s upload while a remote application records); `off` offers no channel. Separate from playback; see [remote microphone](install.md#remote-microphone) |
| `KDE_RDP_AUDIO_CHANNELS` | `stereo` (default) offers stereo PCM first and no mono; `mono` is an opt-in that offers mono PCM (downmixed before resampling) first at the selected rate, then stereo PCM, but 48 kHz mono sounded pitched-up and robotic in Windows 10 mstsc (2026-10-20). Compressed fallbacks are unchanged. Invalid values warn and use `stereo` |
| `KDE_RDP_AUDIO_RATE` | `44100` (default) offers 44.1 kHz PCM (stereo about 1.41 Mbit/s, 3528-byte 20 ms waves resampled from the 48 kHz capture) first, then 48 kHz stereo PCM; `48000` restores the former 48 kHz-first offer, which mstsc played about 8.8% slow (25 s behind video after 5 minutes). Invalid values warn and use `44100` |
| `KDE_RDP_AUDIO_DRIFT_PPM` | 0–2000, default 100; the PCM drift skip's allowance for Windows audio clock drift (20 ms per 200 s of continuous sound at 100). Larger values clamp to 2000; invalid values warn and use 100 |
| `KDE_RDP_DIAGNOSTICS` | `1` enables bounded timing/counter reports, including `idr_frames`, `refinement_frames`, `deferred_rate_increase_frames`, `deferred_rate_decrease_frames` (frames encoded while a level change was not applied to the encoder), `bitrate_level_decreases`, `bitrate_level_increases` and `tolerated_delay_episodes`; ordinary operation avoids diagnostic aggregation |

H.264 frames are limited by a graphics credit window: frames sent but not yet
acknowledged by the client. It starts at 2 per graphics context and, on each
healthy acknowledgement, grows to cover the measured ACK delay at the target
FPS plus two, at most 8. Congestion shrinks it one frame per 500 ms; after a
reduction it regrows only after 3 s of healthy delivery and no recent audio
congestion. The client's core Frame Acknowledge capability (MS-RDPRFX surface
commands) does not cap it: RDPGFX has its own acknowledgements. Two fixed
credits limited Windows 10 mstsc over a ~33 ms RTT internet path to about
20 fps.

A frame acknowledged later than the delivery budget (the fastest observed
acknowledgement plus 100 ms, at least 150 ms), a client decoder backlog above
150 ms of payload, or a frame still unacknowledged past the budget starts a
delay episode. Since 2026-11-11 one such episode alone is not congestion:
frames sent before its boundary share its cause. The boundary is the moment
the episode was noticed or, when no acknowledgement was arriving then, the
first acknowledgement after it: a stall noticed while it lasts is still in
progress, and a frame sent into that silence (the next keystroke while
typing) met the same stall. The episode becomes congestion, shrinking the
window and lowering the bitrate level, when a frame sent after the boundary is
late as well, when a new episode begins within 500 ms of the previous one
ending, or when a frame stays unacknowledged for the budget plus 350 ms, which
also catches an outage with no acknowledgement at all. A healthy
acknowledgement of a later frame ends the episode; on a screen that went
still, that later frame is one refinement frame sent once nothing is in
flight, and an episode nothing resolved is dropped when the next frame is
sent after 3 s without one. A tolerated episode leaves the 3 s and 10 s
recovery timers running and is counted as `tolerated_delay_episodes`. The
reason is a measurement, not
a preference: the link of a Windows mstsc client over the internet went silent
for 80–170 ms about once a minute, and earlier that day for a few minutes
every one to three seconds, then delivered everything at once
(acknowledgements arrived up to 370 ms late). No lower rate prevents that, yet
each stall used to cost a window step and a bitrate level, and frequent ones
held an otherwise healthy 35 ms path at two credits and 1 Mbit/s. The price is
a later first reaction to sustained delay: in the delivery model
(`controller-bench`) the first reduction after a sudden, lasting 400 ms delay
comes 0.4–0.75 s after it starts instead of about 0.2 s (up to 0.9 s when
frames are 100 ms or more apart, because it needs a frame sent after the
first late acknowledgement), and the reductions after it keep their pace. Sound pressure reported by the audio path is not
filtered this way: it shrinks the window and lowers the level at once, as
before, so link stalls during playback still reduce video through that path.

### Quantiser floor

Without a floor NVENC keeps lowering the quantiser while the picture is easy
to encode and spends most of its bitrate on differences nobody can see.
Measured on an RTX 4070 Ti at 1920×1080, 60 fps and the 12 Mbit/s ceiling
(AVC420, p1, one-frame VBV, synthetic desktop pages and clips, every frame
decoded and compared with the encoder's input; `Y` is luma PSNR, a tile is
64×64 pixels):

| `KDE_RDP_MIN_QP` | Settled grey text, worst tile Y | Settled coloured text, worst tile Y / chroma | Photo pan, Mbit/s (mean Y) | Scrolling text, Mbit/s | Bytes to settle after a text page switch |
| --- | --- | --- | --- | --- | --- |
| none | 47.7–48.6 dB | 46.6 / 54.4 dB | 5.76 (55.0 dB) | 2.04 | 179 kB |
| 16 | 44.9–45.3 dB | 43.5 / 43.1 dB | 1.15 (53.8 dB) | 1.65 | 146 kB |
| **18** | 43.9–44.7 dB | 42.7 / 39.3 dB | 1.05 (53.7 dB) | 1.57 | 141 kB |
| 20 | 41.4–42.2 dB | 40.3 / 38.5 dB | 1.00 (52.5 dB) | 1.45 | 127 kB |

Typing on the settled coloured page cost 4.41 Mbit/s without a floor (each
frame re-coded about 9 kB of invisible residue) and 0.10 Mbit/s with one.
Magnified crops of settled coloured text at none, 18, 22 and 26 could not be
told apart in this check; that is not a substitute for looking at text through
mstsc. With AVC444 the floor applies to both views (coloured page at 18: main
view worst tile 42.8 dB, auxiliary view 43.0 dB, 5.9 instead of 11.7 Mbit/s
over a page switch). These are encoder measurements: they exclude capture,
the network and the client's decoder.

### Still-screen refinement

After a change the encoder lowers its quantiser by about one step per frame.
A page switch is a single capture, so its first frame (29 dB in the
measurement above, small text partly unreadable) used to stay on screen until
one refinement frame 750 ms later (37 dB) and was never sharpened further.
Since 2026-11-11 the frontend keeps encoding the retained image while nothing
new is captured:

- It starts 100 ms after the last change, so content changing more often
  than that is never refined between its own frames.
- It continues until the frame's average quantiser has reached the floor and
  the frame is no smaller than 0.9 times the previous one at the floor: the
  encoder has nothing left to add. Frame size alone is not enough, because
  at low bitrates the encoder pauses with tiny frames far above the floor.
- The average quantiser does not always reach the floor: with spatial AQ
  (`balanced`, `quality`) it settled two steps above it, and at 2 Mbit/s rate
  control held a still page near QP 40. Refinement therefore also ends after
  16 consecutive frames that bring no new lowest quantiser, are not more than
  10% smaller than their predecessor and are at most twice the smallest
  refinement frame so far. The rate-control pauses in the measured
  convergence runs lasted at most 8 such frames.
- Without encoder statistics there is one refinement frame, as before.
- It is bounded by 6 s since the last change and 120 frames and sends no
  faster than the frame rate or the current bitrate level allows. It needs a
  quiet path: no decoder backlog, no sound pressure within 3 s, fewer than
  two unacknowledged frames, one credit left free for a frame the desktop
  actually changes, and an acknowledgement within the last 3 s (or a client
  that suspended acknowledgements). During a delay episode it waits until
  nothing is in flight. It no longer requires a high bitrate level: it runs
  at every level including the 1 Mbit/s floor, paced at that level. On a
  constrained link this is traffic next to the PCM audio for up to 6 s after
  each change.
- A refinement frame is never an IDR, never reconfigures the encoder and is
  encoded at the cadence the encoder has, also when coordinated FPS
  (`KDE_RDP_ADAPTIVE_GRAPHICS=1`) has changed the cadence meanwhile.
- A capture that was skipped because the credit window was full is kept and
  sent by the same clock at the next frame interval with a free credit,
  before any refinement: the client is not left with, or refined towards, an
  older picture until the next capture or the one-second heartbeat. Only a
  newer capture or a resize replaces it; pointer updates and the expiry of
  the refinement do not.

On NVENC a 1080p text page switch settled after 20 refinement frames and
102 kB at the 12 Mbit/s level (worst tile 44.7 dB), a coloured page after 27
frames and 291 kB; at the 1 Mbit/s level after 52 and 82 frames (119 and
302 kB), paced by that level. AVC444 took 15 and 32 frames. `refinement_frames`
counts them. The retained image is the DMA-BUF or snapshot the stream already
holds while idle; refinement holds nothing extra.

### Rate levels and encoder reconfiguration

FFmpeg's NVENC wrapper resets rate control and forces an IDR whenever the
bitrate changes. On a settled text page that IDR measured 29 dB (worst tile
17 dB) and needed 24–28 frames and about 150 kB to recover, while a lower
bitrate barely shrinks the frame that a scene change produces (39.7 kB at the
12 Mbit/s level, 35.0 kB at 4.4, 35.3 kB at 1). The bitrate level is therefore
applied to the encoder only when that changes what it sends:

- for free whenever an IDR is due anyway (refresh, resume, dropped frame,
  resize, cadence change) or a new encoder is created;
- a decrease, when the H.264 payload of the last second exceeded the lower
  level; after the audio path reported pressure, with the next changed frame
  whatever the payload, and however much later that frame comes (the urgency
  lasts until the encoder has adopted a rate at or below the level; refinement
  frames do not count);
- an increase, when 30 s have passed since the last IDR (less with a shorter
  `KDE_RDP_GOP_SECONDS`) and the payload of the last second reached three
  quarters of the applied rate, so the applied rate is what limits the encoder.

Until then the encoder keeps its rate and the frames are counted as
`deferred_rate_decrease_frames` or `deferred_rate_increase_frames`. The credit
window remains the immediate response to congestion.

This has a limit that is accepted, not solved. The decrease gate compares one
second of payload with the level, so without sound pressure a burst smaller
than one second of the level (a 1080p page switch is about 1.1 Mbit with its
refinement) is encoded with the per-frame budget the encoder still has, and
when content becomes heavy while a decrease is pending the encoder exceeds the
level for up to about a second before it is reconfigured. On a link that the
credit window, not the bitrate, is limiting, the encoder keeps the old frame
budget until the level has fallen below what the link carries. The measurement
behind accepting this is the one above: reconfiguring barely shrinks a
scene-change frame (39.7 kB at 12 Mbit/s, 35.0 kB at 4.4, 35.3 kB at 1) and
costs an IDR.

GPU capture needs the packaged NVIDIA frontend drop-in described in
[the installation instructions](install.md#optional-nvidia-encoding). It sets
`PrivateDevices=no`, allows only the NVIDIA and DRM device groups under the base
unit's `DevicePolicy=closed`, and adds the `video` and `render` groups, making
DRM render nodes and NVIDIA driver nodes available to the service. Re-copy it
after upgrading; older copies lack the `DeviceAllow` lines. The allocator reads
`/sys/dev/char/MAJOR:MINOR/device/vendor` to identify NVIDIA nodes; the service's
read-only sysfs policy permits this lookup. Keep that metadata readable when
adding local sandbox overrides. On systems with several GPUs, choose the render
node explicitly and ensure its NVIDIA driver device nodes exist; the packaged
device helper prepares GPU minor 0, control, and UVM nodes. Unsupported or
inaccessible GPU capture falls back to SHM. Installed service-account access
remains a separate acceptance check.

SHM capture keeps a pool of up to three shared capture buffers per layout plus
one private overflow buffer, so a 3840×2160 desktop can hold four 33 MB buffers
(about 133 MB) in the worst case. Snapshots of a pooled buffer share its pixels
instead of copying them; a buffer still held as a diff base or encoder input is
not handed back to the compositor, and capture then uses the overflow buffer and
copies. Compositor damage limits pixel comparisons to the damaged 64-pixel
tiles; once per second a full comparison audits that damage, and a change found
outside it forces a full refresh.

With GPU capture, the worker releases the SHM pool when the first DMA-BUF frame
arrives: unheld buffers are unmapped at once, and a buffer still held as a diff
base or encoder input is unmapped when its last holder drops it. SHM frames
that arrive between the renegotiation request and the first DMA-BUF still use
the pool; a fallback to SHM (GPU failure, resize, minimize) builds a new one on
its first frame.

With `KDE_RDP_DIAGNOSTICS=1`, capture reports these counters:

| Counter | Meaning |
| --- | --- |
| `capture_damage_limited_compares` | Frames compared only within compositor-damaged tiles |
| `capture_full_compares` | SHM frames compared in full: no usable damage, or the once-per-second audit |
| `capture_changed_tiles` | Changed 64-pixel tiles found by those comparisons |
| `capture_damage_missed` | Audits that found changed pixels outside the reported damage; each forces a full refresh and should stay 0 |
| `capture_shm_pool_releases` | SHM capture pools released because GPU (DMA-BUF) capture took effect; about one per switch to GPU capture |

Each served connection, including failed TLS handshakes and abrupt TCP drops,
ends with one always-on `RDP connection summary` journal line, independent of
`KDE_RDP_DIAGNOSTICS`:

```text
RDP connection summary peer=192.0.2.10:50123 duration_s=60.0 result=ok video_codec=h264 video_mode=avc420 frames=3600 idr_frames=4 video_avg_kbps=1000 audio_codec=pcm audio_waves=3000 audio_discarded_blocks=5 audio_credit_waits=6 audio_codec_recoveries=7 audio_drift_skips=0 audio_model_starvations=0 audio_model_starved_ms=0 clipboard_to_client=8 clipboard_from_client=9 clipboard_timeouts=10 clipboard_oversize_drops=11 mic=pcm_48000_mono mic_opens=2 mic_packets=1306 mic_bytes=2507520 mic_dropped=0 mic_errors=0 client_frame_ack_limit=2 window_max=8 window_avg=7.8 client_gfx=yes client_bpp=32 gfx_channel=avc420
```

It records the peer address, duration, `result=ok` or `error`, video codec and
mode, H.264 frames, IDR frames, average H.264 payload kbps, audio codec, waves,
discarded capture blocks, credit waits, codec recoveries and drift skips (audible
20 ms PCM blocks dropped by the earned drift catch-up: about one per 200 s of
continuous sound after the first 800 s, plus any repaid burst surplus or
starvation credit above the reserve), arrival-model starvations (the PCM model
of the client queue ran dry for at least 100 ms, a likely client underrun) and
their total milliseconds, clipboard transfers each way, clipboard timeouts and oversize drops. The microphone fields give the
recording format (`mic=none` when Windows never recorded), successful opens,
packets and bytes received, audio frames dropped by the bounded queues, and
failed or unanswered opens, invalid packets and helper connection errors.
`client_frame_ack_limit` is the client's advertised core Frame Acknowledge
capability (`none` when absent); it is logged only and does not limit RDPGFX.
`window_max` and `window_avg` give the largest and mean graphics credit window
over sent H.264 frames (0 when none were sent). `video_codec=bitmap
video_mode=none` means no H.264 frames were sent; bitmap frames are not counted.
The last three fields explain a bitmap-only connection. `client_gfx` says
whether the client's Client Core Data offered the graphics pipeline
(`RNS_UD_CS_SUPPORT_DYNVC_GFX_PROTOCOL`; `no` means the client never opens
RDPGFX, for example mstsc set to fewer than 32-bit colors) and `client_bpp` is
its Bitmap Capability Set color depth (`none` when absent). `gfx_channel` is
`none` when no RDPGFX capability exchange completed, `avc420` when one allowed
H.264, and `avc_disabled` when the client advertised AVC_DISABLED. Every
completed exchange also logs `RDP graphics pipeline ready: <capability set>, AVC420
available` (or `disabled by the client; bitmap updates only`), without
`KDE_RDP_DIAGNOSTICS`.
The line carries counts, durations and the peer address only: no usernames,
error text, clipboard content or credentials.

The frontend reads the separate cursor through standard ext-image-copy-capture
cursor sessions. It selects it using negotiated pointer support and falls back to
an embedded cursor when the client cannot represent the shape, including shapes
over 384 px (a drag-and-drop icon can make the shape large). Supported clients
move the cursor locally; ordinary remote input is not echoed back as an RDP
position update, which would force mstsc's newer local pointer backward. That
suppression needs the maintained compositor's no-echo patch (0011); with stock
Niri the cursor would snap back while moving, so the frontend and compositor are
packaged together. Patch 0012 includes the drag-and-drop icon in the cursor
image. Animated cursors re-render each animation step. Actual
compositor warps still send position updates, including repeated warps to the
same target. Shape, visibility and fallback changes do not replay old positions.
When the client negotiates a pointer cache, each activation keeps up to
min(cache size, 32) recent shapes and sends a repeated shape as a cached-pointer
reference instead of its full bitmap; a reconnect, resize or reactivation starts
with an empty cache, and a cache size of 0 sends full shapes as before.
The clipboard carries Unicode text, HTML and images in both directions, confined
to the authenticated compositor connection with finite transfer deadlines.
Windows text, `HTML Format` (CF_HTML, offered as `text/html`) and one image (PNG,
then DIBV5, then DIB, offered as `image/png`) reach Linux; an image is fetched
only when the copy has no text. Linux `image/png` reaches Windows as CF_DIB plus
a registered `PNG` format, and `text/html` as `HTML Format`. Limits are 1 MiB of
UTF-8 text, 4 MiB of HTML, and 32 MiB and 32768 pixels per side for images.
Enable clipboard in the RDP client's local-resource controls. Files and
host-local selection access are outside this feature: a Windows copy with no
usable format, such as files, leaves the Linux clipboard unchanged.
Native oversized or unusable offers are ignored so a later selection can recover.
Local text, HTML or image data that exceeds its limit while being read logs one
warning naming only the format and limit, is counted as an oversize drop, and is
not offered to the client.
A well-formed incoming RDP clipboard message larger than 32 MiB + 4104 bytes is
skipped without buffering and logged once with only its declared size; the
session continues, and a skipped paste response becomes a failed response, so
that paste simply fails. Malformed or compressed clipboard framing still closes
the connection before fragment assembly; reconnect restores the clipboard channel.

Private audio helper retries are finite. An unavailable audio descriptor permits
a usable desktop; after an active capture fails, reconnect obtains fresh format
and capture state. TCP keepalive and a 20-second outstanding-write timeout detect
silent loss independently of idle desktop pixels or suspended graphics ACKs.
The broker's retention (`grace_seconds`, default 45 seconds) still starts at
observed lease detachment.

### Process memory

The frontend fixes glibc's `M_MMAP_THRESHOLD` at its 128 KiB default at
startup, so buffers above it are mmapped and returned to the kernel when freed.
Otherwise the NVENC preflight and each encoder close raise glibc's dynamic
threshold, and frame-sized buffers then stay resident in the heap after they
are freed. The frontend's `kde-rdp-memory` thread also runs `malloc_trim(0)`
after the preflight and about 2 s after a client disconnect, at most once per
30 s, never per frame and never on the RDP runtime thread.

With `KDE_RDP_DIAGNOSTICS=1`, that thread logs `RDP performance` lines with
`"component":"memory"`: `"event":"start"` after the preflight, `"periodic"`
every 60 s, and `"disconnect"` with each trim (about 2 s after a disconnect, or
up to 30 s when disconnects follow each other).

| Object | Source | Fields |
| --- | --- | --- |
| `process` | `/proc/self/status` | `rss_bytes`, `rss_anon_bytes`, `rss_file_bytes`, `rss_shmem_bytes`, `rss_peak_bytes` (VmHWM), `swap_bytes`, `threads` |
| `heap` | glibc `mallinfo2` | `arena_bytes`, `in_use_bytes`, `free_bytes`, `top_releasable_bytes`, `mmapped_bytes`, `mmapped_chunks` |
| `cgroup` | the service's cgroup | `current_bytes`, `peak_bytes`, `max_bytes`, `high_bytes`, `swap_bytes`; from `memory.stat`: `anon_bytes`, `anon_thp_bytes`, `file_bytes`, `file_mapped_bytes`, `shmem_bytes`, `kernel_bytes`, `sock_bytes`; from `memory.events`: `events_high`, `events_max`, `events_oom_kill` |
| `trim` | start and disconnect lines only | `released_bytes` (RssAnon before minus after the trim), `duration_us` |

Sizes are bytes; `null` means unreadable and `"max"` means no limit.
`MemoryMax` (1536M in the packaged unit) limits the cgroup values, not the RSS:
`rss_shmem_bytes` includes KWin's screencast buffers, which KWin's cgroup pays
for, and `kernel_bytes` (driver memory) is charged to the service but is not in
its RSS. `free_bytes` counts free heap chunks whether or not they are resident,
so it stays high after a trim; read retained memory from `rss_anon_bytes` and
`anon_bytes`. On a healthy host `events_max` and `events_oom_kill` stay 0 and
`anon_bytes` returns to about its start value after each disconnect.

## Measurements

Run `scripts/headless-performance.py` through `lab/run-isolated.sh`. It launches
only a private compositor, synthetic application and loopback client, records
separate frontend/compositor/application CPU, capture/video timing, frame cadence,
client decoded updates and a synthetic PPM screenshot. Its `--link-mbps` and
`--delay-ms` options use a bounded duplex relay on that private loopback.
The relay records wire bytes and keeps two queued 16 KiB chunks per direction;
it is a userspace link model, not a physical LAN or kernel-qdisc measurement. An idle desktop
should send initial pixels and recovery/control traffic; repeating identical
desktop frames is unnecessary.
These graphics workloads do not play audio; the configured PCM reserve is a
numeric bitrate budget. Simultaneous audio/video listening and synchronization
remain the post-reinstall checks below.

Prepare the patched compositor, release frontend, `lab/dynamic-app`, and
`artifacts/rdp-desktop-client` as described in the component READMEs. FreeRDP and
its dependencies may be extracted into `lab/sysroot`; they need not be installed
on the host. The desktop client fixture uses FreeRDP 3.27+ codec-statistics APIs;
these measurements use FreeRDP 3.32.2 with its displayless GDI frontend.

```sh
lab/run-isolated.sh python3 scripts/headless-performance.py \
  --width 1920 --workload colored-text --encoder bitmap \
  --report artifacts/performance/1080p-text-bitmap.json
lab/run-isolated.sh --nvenc python3 scripts/headless-performance.py \
  --width 3840 --workload scroll --encoder nvenc \
  --link-mbps 10 --delay-ms 50 \
  --report artifacts/performance/4k-scroll-shaped.json
lab/run-isolated.sh --nvidia-renderer python3 scripts/headless-performance.py \
  --width 3840 --workload colored-text --encoder nvenc \
  --codec avc444 --capture auto \
  --report artifacts/performance/4k-text-avc444-gpu.json
```

Repeat idle, typing, scrolling, colored small text, window movement, video and
resize at 1920×1080 and 3840×2160, using bitmap, NVENC and NVENC-configured bitmap
fallback. Compare unshaped and constrained links at matched targets. NVENC rate control
is a target rather than a physical wire cap. Native benchmark payload rates
project bytes per frame at the requested FPS; they are not measured wire rates,
and an encoder that misses its frame budget cannot sustain that projection.
Reserve additional headroom on constrained links and
check actual payload and wire-byte receipts alongside audio behavior. Use the
native `scripts/encoder-bench.c` matrix for conversion/encode and decoded image
error; it excludes capture, networking and client decoding. Keep those scopes
separate. EndPaint/update rates count decoded updates, which can include partial
rectangles; they are not monitor refresh or physical presentation FPS.
The server's historical `delivered_frames` counter counts successful GFX
submission to its event queue, before TCP write or client acknowledgement.
Use the client codec counts and decoded images to establish receipt/decoding.
Each short run includes connection startup and initial image delivery. A shaped
link may fail to deliver the requested codec within that window; its receipt sets
`decoded_path_within_window=false`, which is a delivery stall rather than a
successful codec performance result. Unshaped runs require decoded pixels and,
for NVENC, actual AVC commands. Increase `--seconds` for sustained measurements.
Use `--adaptive-graphics` for matched opt-in controller comparisons and use twice
`--delay-ms` as the relay's added nominal round-trip delay.
The Mbps setting is a ceiling. At larger delays the finite relay queue can
limit bulk throughput below that ceiling; use measured TLS payload bytes and
allow initial bitmap delivery to complete before assessing the codec policy.

Compare saved colored-text screenshots with
`scripts/compare-desktop-quality.py` (requires Python NumPy), selecting `--bitmap-dir`, `--shm-dir`,
`--gpu-dir` and optional `--avc444-dir` from matched workload directories.
The helper first proves the lossless reference crop and repeat consistency,
then reports RGB/luma error and separate black/colored glyph and surrounding
pixel errors. It rejects missing, blank, mirrored or incomplete references;
it does not align or blur away codec errors. These are decoded synthetic pixels,
not a substitute for viewing text through mstsc.

Record machine/kernel, Rust/FFmpeg/FreeRDP versions, revisions, binary/package
hashes, renderer, GPU/driver, settings and link conditions beside every receipt.
The initial budgets are bounded state and stale-frame prevention: latest capture,
finite graphics window, bounded frame age, no persistent backlog, no lost input
releases, and the accepted audio FIFO/expiry/credit controls. Comparative quality,
latency and audio evidence is required before changing profile or codec
defaults. NVENC's GPU-capture default keeps automatic SHM fallback.

## Windows checks after reinstall

Use `docs/uninstall.md` and `docs/install.md` for the clean installation, then
record the remaining gates in `docs/acceptance.md`. Test Windows 100/150/200% DPI,
accurate corners/center/drag, cursor shape/hotspot/hide/animation, Japanese/emoji
text, HTML and image clipboard in both directions, portrait/landscape resize, minimize/resume,
blackholed network loss and retained-desktop reconnect. Compare colored one-pixel
edges and settled text with AVC444 and AVC420 fallback.

The 2026-11-11 video changes (quantiser floor, still-screen refinement, no
periodic IDR, gated encoder reconfiguration, tolerated delay episodes) have
encoder, unit and model evidence only. Their mstsc checks are listed in
[acceptance](acceptance.md) and in the
[validation entry](validation.md#video-rate-latch-quantiser-floor-still-screen-refinement-and-keyframes-2026-11-11).

Keep the established PCM combination and listen to speech/music after reconnect.
Run at least 30 minutes each of Firefox/YouTube and VLC flash/tone content,
including pause/resume, limited bandwidth and short stalls. Measure initial/final
audio/video offset and recovery. Installed service-account GPU access, cold boot
without local login, monitor off/disconnected, helper recovery and session cleanup
remain actual-machine checks. Automated decoder or server timing results cannot
check these gates off.
