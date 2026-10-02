# The native processing pipeline (`styx-pipeline`)

Raw frames from a sensor Styx drives itself, a 3A loop closed over the sensor's frame-exact
controls, and an ISP: the PiSP on the CM5, the software ISP anywhere else. No libcamera.

## What connects to what

```
styx-native  NativeCamera ── raw frames + FrameControls (exposure, gain, frame duration;
     │                       read back from embedded data) ──────────────────────────┐
     │  configure_external / start_external: sensor side only, the PiSP owns the nodes │
     ▼                                                                                 │
styx-pisp    FrontEndDevice (fe_stats every frame, fe_image0 held, FE config queue)    │
     │       BackEndStream  (fe_image0 buffers imported as dma-bufs, output0 + output1) │
     │                                                                                 │
styx-softisp SoftIsp (raw → NV12/RGB/luma + statistics in one pass)                    │
     │                                                                                 │
styx-pipeline stats::from_pisp / from_softisp ─► Controller (styx-algo Pipeline) ◄──────┘
     │          SensorRequest ─► CameraControls::request_at(landing frame)
     │          IspSettings  ─► BE (BLC, LSC, WBG × digital gain, CCM, gamma), FE (BLA/BLC, RGB→Y)
     │                       ─► software ISP (black level, WB, digital gain, LSC, CCM, tone)
     ├─ device::PispPipeline, device::SoftPipeline   (feature `device`)
     ├─ rawrec (raw recordings), replay::VirtualSensor (host closed loop)
     ▼
styx         native backend: NV12 / RG24 modes (property `isp` = pisp | software),
             planner prices them; capture leases PiSP output buffers (dma-buf) or heap frames
tools/native-pipeline   pisp | soft | replay, with the measurements below
```

## The loop

* **Statistics.** PiSP: the 32x32 AWB zones (sums of 16-bit samples after the statistics
  black level, ÷ 65536) become the colour zones, the 1024-bin Y histogram the histogram, the
  CDAF figures of merit the focus values. Software ISP: its zone sums are divided by the white
  balance and digital gains it applied and rescaled to the PiSP convention (black removed, not
  stretched); luma zones keep the white balance; the histogram is resampled.
* **Frame metadata.** What produced frame F comes from `styx-native`: the control schedule's
  prediction, replaced by the values read back from the frame's embedded data (every frame on
  the OV9782 with the `-emb` overlay).
* **Sensor requests.** AE names the frame its request lands on: `F + issue latency +
  max delay` (OV9782: exposure/gain 2, frame length 1). `ControlHandle::request_at_now` hands
  it to the scheduler, which writes each control `delay` frames earlier (group hold), and
  writes what is due in the current frame at once while at least 4 ms of it are left
  (`DEFAULT_WRITE_MARGIN`; frame starts carry their `FRAME_SYNC` time). Measured on the OV9782
  (`native-pipeline latch`, 30 fps): a group-held exposure write that ends up to 1 ms before
  the next frame start lands 2 frames after the frame it was written in, later ones 3; the
  sensor latches at the frame start. So the issue latency is 0 on the PiSP path at 30 and
  60 fps (statistics of F are ready 7.3 ms readout + ~1 ms after F starts: written during F,
  landing on F + 2) and 1 at 120 fps (`SensorInfo::issue_latency`); without frame-start events
  2. The software ISP path uses at least 1. A request whose values repeat the previous one is
  not re-sent; frame 0's values are written before `STREAMON`, not in the bridge's start
  acknowledgement.
* **ISP settings.** White balance (green normalised to 1; green's gain goes to the digital
  gain), CCM, tone curve, black level, lens shading. The ISP digital gain for a frame is the
  algorithms' total exposure divided by what the sensor delivered for that frame (clamped to
  1..max), so the picture follows the target while a new exposure is in flight, as the
  Raspberry Pi IPA does.
* **Timing.** PiSP: frame F's statistics arrive with its raw frame; F goes through the back
  end at once with the settings computed from F − 1 (its digital gain recomputed for what F
  got) while F's statistics go through the algorithms, so the settings from F process F + 1
  and the algorithms' time is hidden behind the back end job (see "PiSP path"). Sensor
  requests go out as early as before. While AE is locked and AWB converged the algorithms
  run at 15 Hz (`PispOptions::settled_rate_hz`). The FE's RGB→Y weights and black levels
  follow on the config it takes next (configs are queued two ahead). Software ISP: statistics
  come out of processing F, so the settings from F process F + 1.
* **Determinism.** `Controller::record_to` writes a `styx-algo` replay; replaying it gives the
  same outputs bit for bit (tested), and the virtual sensor replays raw recordings.

## Start-up: open → first frame → good image

Where the time goes (`native-pipeline pisp` prints it; `PispPipeline::startup`,
`NativeCamera::bring_up_times`), OV9782 1280x800 at 30 fps, PiSP path, cold start:

| phase | before | after | what changed |
|---|---|---|---|
| power-up | 6.8 ms | 2.9 ms | bridge power settle 5 → 1 ms (`CameraOptions::power_settle`; 0, 0.5, 1 and 2 ms all brought the sensor up with every register reading back over 20 power cycles; the sequence's own 2 × 600 µs stay) |
| chip id + init registers | 26.1 ms | 21.0 ms | consecutive registers as one auto-incrementing I²C burst (`burst_writes` in the description; one message per transfer, so not the repeated-start problem of earlier batching); verified by reading back all 93 registers (`native-pipeline regcheck`: identical with and without bursts; 0x0101 and 0x1000 read 0 either way) |
| mode registers, initial controls | 16.0 ms | 10.0 ms | bursts (0x3800..0x380b, 0x3810..0x3815, ...) |
| front end + back end open | 7.1 ms after the sensor | 7.4 ms, in parallel | opened on the calling thread while the sensor is brought up on another |
| start values | in the start acknowledgement | 1.5 ms before `STREAMON` | frame 0's exposure/gain/frame length written right away |
| `STREAMON` (receiver, bridge round trip, stream-on write) | 8.2 ms | 6.7 ms | |
| `STREAMON` returned → first frame | 9.3 ms | 9.3 ms | the sensor's start; grows with the first exposure (≈ 8 ms + exposure: 25 ms warm at 17 ms) |
| **open → first frame** | **74.6 ms** | **52.6 ms** | |

The I²C bus (`i2c@88000` on RP1) runs at 100 kHz (device tree `clock-frequency`): one register
write is 0.4 ms, and the 93 bring-up registers are still 31 ms of the 52.6. At 400 kHz (the
OV9782's SCCB allows fast mode) the bring-up would be about 4 times shorter; that is the board's
device tree, not changed here.

Getting to a good image (AE locked: no change beyond 5% in flight and the frame within 5% of
the target, two frames in a row):

* **Same-frame writes** (above): a change lands 2 frames after the frame that asked for it
  instead of 4.
* **Model-based steps**: the target is a total exposure computed from what produced each frame
  (embedded data), so every frame's statistics count; a frame exposed before a change lands
  asks for the same total again (nothing new is sent), a clearly different one (the scene
  changed) replaces the change in flight. Changes above `full_step` (8%) go straight to the
  target; after such a step lands, what is left (clipped zones, the black level offset) is
  corrected at once. Damping stays for small changes. Once locked AE does not hunt within the
  5% tolerance.
* **Unsettled frames**: the OV9782's first frame after a start reads a higher black level
  (+0.013..0.019 of full scale at gain 1.5; at 120 fps, where the exposure fills the frame, it
  is nearly black), so AE and AWB leave it out (`settle_frames = 1` in the description's black
  level → `CameraConfig::unsettled_frames`).
* **Warm starts**: a pipeline that stops remembers what AE and AWB settled on per camera
  (`styx_pipeline::warm`, in memory, and on disk under `STYX_STATE_DIR`); the next start begins
  there, the total exposure re-split for the new mode's limits (30 → 120 fps: 17 ms × 1.5 →
  8.2 ms × 3.1).
* **Restarts without a bring-up**: `PispPipeline::stop` leaves the camera powered in standby;
  `start` again (or `reconfigure` for another frame rate first) reopens only the ISP; a sensor
  already in the mode keeps its registers.
* AWB estimates from frame 1 (the first settled one) at full speed; its start-up only counts
  frames whose exposure is usable.

## PiSP path

`FrontEndDevice::open` (with `keep_embedded`) routes `csi2 → pisp-fe`, `NativeCamera::
configure_external` sets the sensor and bridge up without touching links or nodes,
`start_external` starts the embedded data node and the event threads (frame starts from
`rp1-cfe-fe_image0`'s `FRAME_SYNC` events), then the front end streams. Each frame
(`PispPipeline::next`):

1. `next_held_raw` dequeues the statistics and the raw buffer (not copied); the statistics are
   copied out of their uncached buffer in one go (14 µs), and only on frames the algorithms run.
2. The back end config is patched with the newest settings (`BeConfigBuilder`: tiles and
   everything geometric are prepared once, again only when lens shading switches on or off;
   per frame only the blocks whose register values changed, lens shading tables resampled and
   gamma converted only when they change: 4-7 µs) and the job is queued on the raw buffer,
   imported as a dma-buf (`BackEndStream::process_queued`).
3. While the back end works (0.84 ms), the statistics are converted in place
   (`stats::from_pisp_raw`) and run through the algorithms; their sensor request goes to the
   control schedule. Settled (AE locked, AWB converged), the algorithms run at 15 Hz.
4. `wait_job` returns output 0 (e.g. NV12 1280x800) and output 1 (e.g. RGB 640x400) in buffers
   the caller holds until it releases them (each output on its own: `release_output`); the raw
   buffer goes back to the front end.

The back end's outputs are cached dma-heap buffers (`linux,cma`) imported with
`V4L2_MEMORY_DMABUF` (`OutputMemory::CachedHeap`, the default): vb2 does no cache maintenance
on imported buffers, so a CPU reader brackets its reads with `DMA_BUF_IOCTL_SYNC`
(`PispPipeline::sync_output`; the Styx lease does it when its pixels are first read) and reads
at memory speed; the driver's own buffers (`OutputMemory::Driver`) are mapped uncached. The
config buffer comes from a cached heap too (the driver copies it with the CPU at `QBUF`).

### PiSP path performance

CM5, OV9782 1280x800, the scene above, HeliOS tuning, 300 frames, 2026-10-02. "Before" is
`582d885` on the same device (the tool; through the Styx API the old code no longer runs on
today's bridge, its last measurement is quoted). CPU is per frame for the whole process
(scheduler run time of every thread); latency is the frame's capture timestamp (frame start)
to the frame being in the consumer's hands.

| | before | after |
|---|---|---|
| `native-pipeline pisp`, 30 fps, reading the NV12 output (the tool's mean) | 3.1 ms CPU (9.2% of a core) | 0.47 ms (1.4%) |
| same, not reading the output | 1.2 ms (3.5%) | 0.26 ms (0.8%) |
| same at 120 fps, not reading | — | 0.22 ms (2.6%) |
| latency, median / p95 (30 fps) | 9.21 / 9.85 ms | 8.35 / 8.39 ms |
| dequeue → outputs ready, median / p95 | 1.71 / 2.35 ms | 0.86 / 0.89 ms |
| reading the 1280x800 Y plane on the CPU | 1.87 ms | 0.22 ms (cached buffers, sync included) |
| Styx API, NV12 1280x800, one consumer, 30 fps | — | 0.26 ms (0.8%), latency 8.28 / 8.32 ms |
| same at 120 fps | 1.25 ms (15% at 120.6 fps, `native_processed`) | 0.19 ms (2.3%), latency 8.36 / 8.38 ms |
| same, the consumer reading every pixel (30 fps) | — | 0.55 ms (1.6%) |
| Styx API, NV12 1280x800 + RG24 640x400 (two consumers, one PiSP pass), 30 fps | no plan (no NV12 → RG24 converter) | 0.32 ms (1.0%), both 8.36 / 8.40 ms |
| same at 120 fps | — | 0.23 ms (2.7%), both 8.43 / 8.46 ms |
| same, both consumers reading every pixel (30 fps) | — | 0.81 ms (2.4%) |
| camera service + two client processes (NV12, RG24 640x400), 30 fps: service | — | 0.24 ms (0.7%), nothing copied |
| clients receiving (dma-bufs) / reading every pixel | — | 0 / 0.37 ms (NV12), 0.20 ms (RGB); latency 8.38-8.40 ms |
| peak RSS (Styx API process) | 27.4 MiB (tool) | 17.8 MiB |
| software ISP mode through the Styx API (`STYX_NATIVE_ISP=software`), 30 fps | 15.8 ms CPU, 22.4 ms latency (1 thread) | 6.9 ms, 9.5 ms (4 threads) |

These were measured without temporal denoise; with it (the default when the tuning has
`rpi.denoise.tdn`, see "Quality vs libcamera") the back end job takes 2.3 ms and the latency
9.9 ms, the CPU is unchanged.

`native_isp_bench single|shared FPS FRAMES [--read]`, `native_isp_bench serve|client` and
`native-pipeline pisp [--no-read] [--profile] [--every-frame] [--driver-buffers]` measure
these; `--profile` (`styx_pisp::device::profile`) times every device call per frame and the
tool prints each thread's CPU and wake-ups.

Where the 3.1 ms and 9.2 ms went before: 1.87 ms the tool reading the uncached NV12 output;
0.79 ms (on the frame path, so latency too) decoding the embedded data line: all 16 KiB of
its uncached buffer were unpacked twice per frame although the layout reads 25 bytes (fixed in
`styx-sensor`); 0.13 ms the back end config `QBUF`; 0.11 ms the algorithms (0.65 ms on the
frames AWB and lens shading run, before the back end job); 0.05 ms rebuilding the back end
config and tiles; 0.02 ms decoding statistics; the rest ioctls and wake-ups. The latency was
7.4 ms sensor readout + 1.8 ms after the dequeue.

Where the 0.26 ms per frame go now (Styx API, NV12, 30 fps; `--profile`, per-thread CPU):

| | per frame |
|---|---|
| back end config `QBUF`: the `pispbe` driver writes the whole `pisp_be_config` to the hardware registers (MMIO) for every job | 0.12 ms |
| algorithms: 0.13 ms per run, at 15 Hz while settled (0.11 ms per frame when run on every frame) | 0.07 ms |
| ~20 V4L2 ioctls (1 µs each) and two waits of the pipeline thread | 0.025 ms |
| event thread: frame-start events, embedded data (two wake-ups) | 0.013 ms |
| the consumer: queue hand-off, lease, its wake-up | 0.009 ms |
| statistics copy (every other frame) and conversion | 0.007 ms |
| back end config patch | 0.007 ms |
| embedded data decode | 0.005 ms |
| control schedule, frame metadata | ~0.005 ms |

The latency, 8.3 ms, is 7.4 ms of sensor readout (the timestamp is the frame start, the front
end's buffers complete at its end), 0.84 ms back end job (hardware; the algorithms run inside
it) and about 0.05 ms on the host.

## Software path

`SoftPipeline` captures packed RAW10 from `rp1-cfe-csi2_ch0` (or any raw node), the software
ISP writes NV12/RGB/luma and the statistics in one pass (bilinear demosaic, full-range BT.601
YUV like the PiSP's "jpeg" encoding). Raw recordings (`--record`) store each frame with its
sensor values; `replay::VirtualSensor` re-exposes a recorded frame for whatever the loop asks
(linear above black, clipped), with requests landing on their frames, so AE runs closed-loop on
a host: `native-pipeline replay --recording <base>`, or `STYX_RAW_RECORDING=<base> cargo test
-p styx-pipeline --test replay_loop`.

### Software path performance

`SoftLoop::new` / `SoftPipeline::open` take a thread count (`--threads N` in the tool; 0 is one
per CPU). The ISP splits each frame into two row bands per thread, claimed by whichever thread
is free, on helper threads it starts once and keeps asleep between frames (no per-frame spawn,
no rayon). Input rows are copied 16 KiB at a time into a cached buffer before unpacking, since
the receiver's MMAP buffers are mapped uncached (`SoftIsp::set_copy_input`).

CM5, OV9782 1280x800 RAW10 into RGB24 with the HeliOS tuning (lens shading 32x32, CCM, gamma,
statistics 16x12 zones every second quad row), 300 frames, the scene above, 2026-10-02. Before
is the native-stack branch as of `582d885`; CPU is the whole tool process (capture, 3A, the
tool's per-frame output mean) per frame.

| | before, 1 thread | after, 1 thread | after, 4 threads |
|---|---|---|---|
| 30 fps, MMAP buffers: CPU per frame | 15.6 ms (47%) | 7.7 ms (23%) | 8.1 ms (24%) |
| 30 fps, MMAP: dequeue -> output | 14.1 ms | 6.2 ms | 2.1 ms |
| 30 fps, MMAP: sensor timestamp -> output | 22.4 ms | 14.4 ms | 10.3 ms |
| 30 fps, CMA buffers: CPU / dequeue -> output / latency | 10.6 / 9.1 / 17.4 ms | 7.1 / 5.6 / 13.8 ms | 8.1 / 2.1 / 10.4 ms |
| 120 fps, MMAP: frame rate | 64.9 fps (falls behind) | 120.0 fps | 120.0 fps |
| 120 fps, MMAP: CPU / dequeue -> output / latency | 15.7 / 14.1 / 57 ms | 7.7 / 6.1 / 14.5 ms (91% of a core) | 8.1 / 2.1 / 10.5 ms |
| peak RSS (MMAP / CMA) | 12.8 / 7.8 MiB | 12.9 / 7.9 MiB | 13.0 / 8.2 MiB |

About 8.2 ms of the latency is the sensor read-out (timestamp at frame start, dequeue at its
end). Four threads cost 0.4 ms more CPU per frame than one (wake-ups, band edges) and cut
the processing time by 3x; with the copy, MMAP and CMA buffers now cost the same.

The ISP alone on a recorded frame (`styx-softisp`, same settings, one thread unless noted):

| | before | after |
|---|---:|---:|
| RGB24, frame in write-combined memory (as MMAP) | 10.7 ms | 5.6 ms |
| RGB24, frame in cached memory | 5.9 ms | 5.0 ms |
| RGB24, write-combined, 2 / 3 / 4 threads | (no threads without rayon) | 2.8 / 1.9 / 1.5 ms |
| tone curve kernel, 3 channels of a frame | 2.31 ms | 1.54 ms |
| statistics (their share of the frame) | 0.57 ms | 0.39 ms |
| lens shading tables (`set_params`, on every frame whose gains change) | 3.19 ms | 0.31 ms |
| replay of 60 recorded frames with the 3A loop, per frame (1 / 4 threads) | 9.15 ms | 5.63 / 2.24 ms |

Quality: unchanged bit for bit. `styx-softisp`'s `tests/golden.rs` hashes every output (RGB24,
NV12, I420, luma, both demosaics, both scales, lens shading, statistics) at 1 and 3 threads
against the hashes of the code before this work (identical on x86 and the A76); the replay
of the 60 recorded frames gives the same final image (md5) and the same AE/AWB trajectory as
before at 1 and 4 threads (max error 0 per channel, PSNR infinite). Per-stage numbers, x86 and
what was tried and dropped are in `crates/softisp/PERFORMANCE.md`.

## Planner

The native backend lists `NV12` and `RG24` modes at each sensor size (all rates of the raw
mode) with property `isp`. Costs (`planner/cost.rs`, from the measurements here): PiSP adds
0.9 ms latency and 0.3 ms CPU to the native capture; the software ISP 5.5 ms/MP of CPU
(+0.4 ms with helper threads) + 0.3 ms 3A, and 5.5 ms/MP divided over its threads of latency
(it runs on min(4, cores) threads, `StyxConfig::native_soft_threads`). A native PiSP scales
like libcamera's ISP (`output_resolution` → `NativeIspConfig::output_size`), and a shared
capture uses both back end outputs: consumers are served by size and format (the PiSP makes
either processed format on either output; the second output is attached to each frame as a
`CompanionKind::Scaled` companion), so NV12 1280x800 + RG24 640x400 come from one pass with
no conversion; a third size or format falls back to the mode's size first, then to a CPU
conversion. Shared plans of a sensor Styx drives run at exactly the rate asked when saving
power, as single plans do. Every output is handed out as a dma-buf, in process and to other
processes through the camera service (planes exported with their offsets). Raw native modes are not routed through a Bayer decoder when an ISP route exists
(that route has no 3A); Bayer decoders are priced at the software ISP's cost. `plan_frames`
for NV12, RG24 or luma (NV12's Y plane) on the native OV9782 picks the PiSP mode on the CM5
and the software ISP mode elsewhere. Tuning: `STYX_TUNING`, else the description's `tuning`
file in `/etc/styx/tuning`, `/usr/{local/,}share/styx/tuning`, libcamera's
`/usr/{local/,}share/libcamera/ipa/rpi/pisp` (read at run time), else the defaults.

## Results on the CM5 (OV9782 1280x800 at 30 fps, bridge with embedded data)

Scene: a ceiling lit by a warm ceiling lamp, a blue LED and posters. Tuning: the HeliOS
`ov9782.json` read from the device at run time. "Brightness steps" force the exposure to ¼
(darker) or 3× (brighter) of the converged value for 10 frames and then hand back to AE (as
AE sees it, a step of the scene's brightness). Settling = exposure × gain (from embedded data)
within 5% of its final value; output level = mean of the output luma.

### Start-up and convergence (2026-10-02)

PiSP path, `native-pipeline pisp --perturb 60:0.25 --perturb 110:3`, before (the code at
`582d885`, 3 runs) and after (5 runs, `--cold`: no remembered state); frames counted from the
start or from the end of the forced step; "locked" as each version defines it (before:
Raspberry Pi's five frames of stable exposure; after: see above).

| | before | after |
|---|---|---|
| open → first frame | 74.3 / 74.5 / 74.6 ms | 52.5 – 52.7 ms (5 of 5) |
| open → first AE-locked frame | 474.3 – 474.6 ms (frame 12) | 252.5 – 252.7 ms (frame 6, 5 of 5) |
| AE start: exposure / output within 5%, locked | 8 / 8 / 12, 8 / 8 / 12, 12 / 8 / 12 | 5 / 3 / 6 (5 of 5) |
| darker step (¼): exposure / output within 5%, locked | 12 / 7 / 14–15 | 2 / 0 / 3 (5 of 5) |
| brighter step (3×): exposure / output within 5%, locked | 13 / 11–12 / 15 | 2 / 2 / 3 (5 of 5) |
| overshoot after settling (start, ¼, 3×) | 0–3.9%, 3.4–4.4%, 2.5–3.5% | 0–2.5%, 0–2.9%, 3.3–4.2% |
| dark start (forced 100 µs × 1) | | within 5% after 7 frames, locked frame 8 (318 ms) |
| bright start (forced 33 ms × 8, saturated) | | within 5% after 9 frames, locked frame 10 (385 ms) |
| 120 fps cold start | | locked frame 8, 119 ms after open; ¼ step locked after 4 |
| restart, camera kept open (`--keep-open`): stop → first frame / locked | | 42 ms / frame 2, 109 ms (30 fps); 24 ms / frame 2, 41 ms (switch to 120 fps) |
| restart, camera closed and reopened, warm: open → first frame / locked | | 69 ms / frame 2, 136 ms (30 fps); 51 ms / frame 2, 68 ms (120 fps) |

Warm restarts start at the remembered exposure and do not change it (exposure within 5% from
frame 0; output within 5% from frame 1, frame 0 being the unsettled one). The first frame of a
warm start comes later than a cold one because it exposes for 17 ms instead of 1 ms.

Through the Styx API (`styx-compare run --format NV12 --fps 30 --frames 90`, native with
`--ae-state-control 0xF4000010`, the AE state the processed native modes now publish;
libcamera with `systemctl stop styx-bridge` and `helios-peripherals` stopped, same scene):

| | native, cold (3 processes) | native, 4 opens in one process | libcamera (3 processes) | libcamera, 4 opens in one process |
|---|---|---|---|---|
| `start()` returns | 45.2 – 45.4 ms | 45.1 ms | 119.6 – 131.3 ms | 108.3 ms (95.3 – 128.9) |
| open → first frame | 56.4 – 56.5 ms | 72.5 ms (56.4 – 72.5) | 130.6 – 142.1 ms | 118.2 ms (106.0 – 139.7) |
| open → AE converged | 256.7 – 256.8 ms | 139.7 ms (139.6 – 258.9) | 663.8 – 675.3 ms | 644.6 ms (584.9 – 672.8) |
| open → exposure settled | 223.4 – 290.0 ms | 72.5 ms (72.5 – 323.3) | 530.4 – 541.9 ms | 578.0 ms (518.3 – 606.0) |

(Medians and ranges over the opens; in one process the native opens after the first start
from the remembered state.)

| | PiSP | software ISP |
|---|---|---|
| frame rate (160 frames) | 30.000 fps, no gaps | 30.000 fps, no gaps |
| open → first frame | 52.6 ms (see above; 74.6 before) | 69.9 ms (before the start-up work) |
| open → first AE-locked frame | 252.5 ms, frame 6 (see above) | 670 / 503 / 670 ms, frame 18 / 13 / 18 (before) |
| AE start: exposure / output within 5%, locked | 5 / 3 / 6 frames (see above) | 17 / 13 / 17, 3.6% (before) |
| darker step (¼): exposure / output, locked | 2 / 0, 3 | 13 / 8, 14; 4.7% (before) |
| brighter step (3×): exposure / output, locked | 2 / 2, 3 | 19 / 18, 21; 4.9% (before) |
| sensor timestamp (frame start) → output ready | 8.35 ms median, 8.39 p95 (9.21 / 9.85 before, see "PiSP path performance") | 22.4 ms median, 22.8 p95 (17.4 with CMA buffers; before perf-soft) |
| dequeue → output ready | 0.86 ms (back end job 0.84; 1.71 before) | 14.2 ms (9.1 with CMA buffers) |
| CPU per frame (whole process) | 0.26 ms (0.8% of a core); 0.47 ms reading the output (3.1 ms before) | 15.8 ms (47%); 10.7 ms (32%) with CMA buffers |
| peak RSS | 17.8 MiB (Styx API; 27.4 before) | 12.7 MiB (7.9 with CMA buffers) |
| AWB (Bayesian, HeliOS tuning) | 2533 K, output R/G 1.19 B/G 0.91 | 2557 K, R/G 1.24 B/G 0.90 |

Run of 2026-10-01 with lens shading on both paths (the PiSP's LSC block since then), after the
fixes below; an earlier run without PiSP lens shading gave the same rates and convergence
(PiSP AE start 12 frames, CPU 3.7 ms) and is in the git history of this file. "CMA buffers":
`--heap linux,cma`, capture into cached dma-heap buffers instead of the driver's MMAP ones,
which the CPU reads uncached: reading the raw frame is a third of the software path's time.

* Sensor values were read back from embedded data on every frame; statistics and raw frames
  always had the same sequence; no frame was dropped.
* At 60 and 120 fps the PiSP path keeps up (2.9 and 3.0 ms CPU per frame then, 0.22 ms at
  120 fps now); the software ISP
  path needs 15 ms per frame on one core (the CPU figures include the tool's own per-frame
  mean of the output and, for the software path, writing the raw recording).
* The light is warm: the Bayesian AWB (CT curve from the tuning) keeps some of it, grey world
  (default tuning, host replay of the recorded frames) ends at R/G 0.997, B/G 1.006.
* Through the Styx API (`examples/native_processed.rs`): `plan_best` for NV12, luma and RG24
  on the native OV9782 picks the native NV12 / RG24 modes with the PiSP (the raw modes are
  rejected: "raw pBAA would need a decoder without 3A"); capture runs at 120.625 fps (the plan
  takes the fastest rate for latency), start → first frame 78-80 ms, exposure settled in 12
  frames, 14.7-15.9% of a core, saved frames `native-processed-{nv12,luma,rgb}`.
* libcamera on the same device and scene (the compare harness, `tools/compare` on
  `native/compare`, Styx's libcamera backend, NV12, median of 3): open → first frame 103 ms,
  open → AE converged 635 ms (raw BYR2: 568 ms). Native then: 76 ms and 476 ms (PiSP); now
  see "Start-up and convergence" above.
* Host replay of the recorded frames (software ISP, one x86 core): 1.9 ms per frame, AE
  settles in 12 frames, locks at 13.
* Images: `pisp-nv12-rgb` (output 0, NV12 converted), `pisp-rgb-half` (output 1),
  `soft-rgb`, `replay-soft-rgb` (PPM and PNG in the run's output directory).

Found on the way: with the kernel driver's default flips (both on) the OV9782's order is RGGB
and the picture is turned against libcamera's, which runs them off; the description now
defaults to flips off (BGGR, upright). A frame lasts VTS + 1 lines (measured at 30/60/120 fps):
`controls.frame_length_extra_lines = 1`. Every stop request timed out (1 s, "stop request
failed: -110"), first because the bridge was served through the reactor that blocks in
`vb2_fop_poll` while `STREAMOFF` holds the node lock (fixed on native/harden: own `poll`,
quiesced around `STREAMON`/`STREAMOFF`), then because rp1-cfe stops the receiver when the
*last* node stops, which with embedded data is the embedded node, stopped after the event
thread had gone (fixed here: stop it first). No stop timeouts since, on either path.

## Quality vs libcamera

CM5, OV9782 1280x800 at 30 fps, 2026-10-02 (device clock: Aug 17), the HeliOS tuning
(`/usr/share/libcamera/ipa/rpi/pisp/ov9782.json`, the same as libcamera's
`src/ipa/rpi/pisp/data/ov9782.json`). Scene: a dim room lit by a warm lamp and a blue LED
(AE ends at 33 ms × 10). Both paths at a fixed 33 ms × 8, NV12 output 0 saved packed:
libcamera through Styx's libcamera backend (`styx-compare run --backend libcamera --format
NV12 --set 8=i:1 --set 7=i:33000 --set 10=i:1 --set 9=f:8 --save ...`, AWB auto: through
Styx `AwbEnable`/`ColourTemperature` did not take, the metadata kept reporting AWB's 2660 K),
native with `native-pipeline pisp --fixed 33000:8` (AWB auto). Compared with
`tools/compare/quality.py` (8x5 zones, the reference is libcamera; noise is the standard
deviation of luma minus its 5x5 mean in the flattest 10% of 16x16 blocks, 0..255; sharpness
the mean gradient of the 5% strongest-gradient pixels and the 10-90% rise of strong edges).

| | libcamera | native now | native, no TDN | native before (`4b003cb` blocks) |
|---|---|---|---|---|
| AWB (auto) | 2660-2679 K | 2693 K (2702 K) | (2860 K fixed) | (2860 K fixed) |
| grey world R/G, B/G of the output | 1.139, 1.599 | 1.160, 1.599 | 1.206, 1.564 | 1.178, 1.508 |
| per-zone R/G vs libcamera: mean (min..max) | 1 | 1.020 (0.994..1.043) | 1.066 | 1.033 (0.994..1.052) |
| per-zone B/G vs libcamera | 1 | 1.002 (0.988..1.024) | 0.977 | 0.928 (0.848..0.988) |
| mean luma | 0.236 | 0.232 | 0.228 | 0.242 |
| luma, corners vs centre relative to libcamera | 1 | 1.05, 1.06, 1.03, 1.01 | 1.07, 1.06, 1.02, 1.02 | 0.99, 1.04, 1.01, 1.03 |
| per-zone luma vs libcamera (min..max) | 1 | 0.98..1.11 | 0.99..1.08 | 0.98..1.07 |
| tone: output luma where libcamera's is 0.15 / 0.34 / 0.53 / 0.72 / 0.91 | | 0.146 / 0.332 / 0.526 / 0.717 / 0.894 | 0.143 / 0.325 / 0.517 / 0.711 / 0.886 | 0.154 / 0.356 / 0.544 / 0.728 / 0.922 |
| flat-area noise (0..255) | 0.40 | **0.39** | 1.57 | **8.78** |
| sharpness: top-5% gradient / HF energy share | 10.5 / 0.0011 | 10.6 / 0.0011 | 11.3 / 0.0015 | 25.5 / 0.0181 |
| edge 10-90% rise | 6.0 px | 6.2 px | 6.2 px | 5.9 px |

(The "no TDN" and "before" columns ran AWB at a fixed 2860 K, so their colour columns are not
comparable; the before column is the current binary with the tuning's `rpi.noise`,
`rpi.denoise`, `rpi.sdn`, `rpi.geq`, `rpi.dpc` and `rpi.sharpen` removed, i.e. what the back
end did until now.) Images (libcamera, native now, no TDN, before; full frame at half size,
then 256x256 crops at 2x): `target/quality-full.png`, `quality-centre.png`,
`quality-corner.png`, `quality-flat.png` (the noise), `quality-edges.png`.

What was ours and is fixed:

* **Denoise and sharpening.** The back end ran libpisp's default sharpening at full strength
  on undenoised data and no denoise at all: 22x libcamera's noise in flat areas and 2.4x its
  edge gradients (sharpened noise). Now `styx-algo`'s `Denoise` (ported from `noise.cpp`,
  `denoise.cpp`, `geq.cpp`, `dpc.cpp`, `sharpen.cpp`) drives DPC, GEQ, SDN, CDN, TDN and the
  sharpening scale from the tuning with gain-dependent strength (noise profile × √gain; SDN
  and CDN start at their no-TDN strengths and back off while TDN builds up), and the back end
  runs temporal denoise with its long-term buffers: noise and sharpness now match libcamera.
* **AWB** (see [algorithms.md](algorithms.md)): continuous and hysteretic; in this scene 2693 K
  against libcamera's 2660-2679 K, R/G of the output within 2%, B/G equal.
* **Gains below 1.** With warm light the red gain is below 1 (0.98 here) and the back end
  applied it as is, so saturated highlights turned cyan; the channel gains now get libcamera's
  extra 1 / min(gain) (`IspSettings::channel_gains`), also +1-2% of brightness here.

Left (within the measurement, or not ours):

* Luma shading: the edge zones come out 1-6% (one zone 11%) brighter than libcamera's,
  relative to the centre. Calibration interpolation, resampling, luminance strength (0.8),
  packing and grid steps are the same as libcamera's (checked against its source); the
  colour residual (±3-4% per zone) is what libcamera's adaptive ALSC (not ported: Gauss-Seidel
  refinement of the R and B tables from the statistics) would correct. Not resolved here.
* Tone: 1-2% darker in the highlights with the same gamma curve and the same contrast
  enhancement (`contrast.cpp`'s stretch, ported as is); the stretch follows the front end's
  histogram, which both configure the same way except for the AGC weights (uniform here).
* Cost: temporal denoise reads and writes a 16-bit average of the frame every job: the back
  end job goes from 0.84-0.88 ms to 2.3 ms at 1280x800, so the latency from the frame start
  to the outputs from 8.35 to 9.9 ms (30 and 120 fps); CPU unchanged (0.6-0.7 ms per frame).
  `PispOptions::temporal_denoise = false` (`native-pipeline --no-tdn`) keeps the old latency
  with 4x libcamera's noise (SDN and CDN at their no-TDN strengths).

## Gaps

* PiSP lens shading: the ALSC tables resampled to the back end's 33x33 grid, packed as the
  Raspberry Pi IPA does; compared against libcamera's output in "Quality vs libcamera" (edges
  1-6% brighter, the adaptive part of ALSC not ported).
* The front end statistics set-up is fixed (uniform AGC weights; AGC meters the AWB zones).
* PiSP CPU left (see "PiSP path performance"): half of it is the `pispbe` driver writing the
  whole back end config to the hardware by MMIO on every job (0.12 ms; the driver could write
  only changed blocks, as it already does for the front end). The event thread wakes twice
  per frame and the pipeline thread twice (front end, back end); one wait per frame would
  need the event thread's work on the pipeline thread (`styx-native`). Pyramid companions
  from the PiSP's second output are libcamera-only so far.
* Algorithms at 15 Hz while settled: a scene change is seen up to one 15 Hz period later
  (`settled_rate_hz: None` runs them on every frame).
* Brightness changes were forced exposure steps, not changes of the light.
* The tool's software runs use one thread unless `--threads` says otherwise (the `styx`
  native backend's software mode uses min(4, cores), and the planner prices it as measured).
