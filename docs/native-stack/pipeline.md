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
* **Timing.** PiSP: frame F's statistics arrive with its raw frame, so the back end processes
  F with the settings computed from F. The FE's RGB→Y weights and black levels follow on the
  config it takes next (configs are queued two ahead). Software ISP: statistics come out of
  processing F, so the settings from F process F + 1.
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
`rp1-cfe-fe_image0`'s `FRAME_SYNC` events), then the front end streams. Each frame:
`next_held` dequeues the statistics and the raw buffer (not copied), the loop runs, a fresh
back end config is prepared (template + settings, 12-24 µs with tiling) and the back end job
runs on the raw buffer imported as a dma-buf (`BackEndStream`), writing output 0 (e.g. NV12
1280x800) and output 1 (e.g. RGB 640x400, resampler) into buffers the caller holds until it
releases them. The raw buffer goes back to the front end after the job.

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
mode) with property `isp`. Costs (`planner/cost.rs`, from the measurements below): PiSP adds
1.7 ms latency and 1 ms CPU to the native capture; the software ISP 10 ms/MP + 0.3 ms 3A (CPU
and latency). Raw native modes are not routed through a Bayer decoder when an ISP route exists
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
| sensor timestamp (frame start) → output ready | 9.21 ms median, 9.85 p95 | 22.4 ms median, 22.8 p95 (17.4 with CMA buffers) |
| dequeue → output ready | 1.71 ms (back end job 0.84, config + tiles 0.045) | 14.2 ms (9.1 with CMA buffers) |
| CPU per frame (whole process) | 3.1 ms (9.2% of a core) | 15.8 ms (47%); 10.7 ms (32%) with CMA buffers |
| peak RSS | 27.4 MiB | 12.7 MiB (7.9 with CMA buffers) |
| AWB (Bayesian, HeliOS tuning) | 2533 K, output R/G 1.19 B/G 0.91 | 2557 K, R/G 1.24 B/G 0.90 |

Run of 2026-10-01 with lens shading on both paths (the PiSP's LSC block since then), after the
fixes below; an earlier run without PiSP lens shading gave the same rates and convergence
(PiSP AE start 12 frames, CPU 3.7 ms) and is in the git history of this file. "CMA buffers":
`--heap linux,cma`, capture into cached dma-heap buffers instead of the driver's MMAP ones,
which the CPU reads uncached: reading the raw frame is a third of the software path's time.

* Sensor values were read back from embedded data on every frame; statistics and raw frames
  always had the same sequence; no frame was dropped.
* At 60 and 120 fps the PiSP path keeps up (2.9 and 3.0 ms CPU per frame); the software ISP
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

## Gaps

* PiSP lens shading: the ALSC tables resampled to the back end's 33x33 grid, packed as the
  Raspberry Pi IPA does; runs on the device, not compared against libcamera's output. TDN/sharpening strength/denoise follow libpisp defaults, not the tuning.
* The front end statistics set-up is fixed (uniform AGC weights; AGC meters the AWB zones).
* Styx capture of processed native modes uses output 0 only (the tool uses both outputs); the
  planner's second-output / pyramid logic is libcamera-only.
* Brightness changes were forced exposure steps, not changes of the light.
* The tool's software runs use one thread unless `--threads` says otherwise; the `styx`
  native backend's software mode (`capture_api/native_isp.rs`) opens it with one thread, and
  the planner still prices the software ISP at 10 ms/MP (now about 5 ms/MP on one A76 core,
  1.5 ms/MP on four).
