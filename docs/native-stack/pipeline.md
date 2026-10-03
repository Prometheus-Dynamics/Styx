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
`start_external_driven` starts the embedded data node and the event thread (bridge requests
only; frame starts from `rp1-cfe-fe_image0`'s `FRAME_SYNC` events are read on the pipeline
thread, see "Wake-ups" below), then the front end streams. Each frame
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

Output buffers are reused in release order, and a buffer a consumer holds (a Styx lease, a
frame server's latest frame, a frame another process has not released) is never written. When
consumers hold every buffer of an output, `next` drops the frame (`PipelineError::OutputsHeld`:
no job queued, the raw buffer goes back) and the Styx capture moves on to the next frame: the
camera never waits for consumers. Each output has 4 buffers (`PispOptions::be_buffers`; through
Styx `StyxConfig::native_output_buffers`). On the CM5 with the frame socket
(`styx::ipc::FrameSocket`, NV12 1280x800 at 30 fps, 20 s each): two consumers holding a frame
for 500 ms each cost frames with 4 buffers (17.3 fps delivered) and none with 8 (30.03 fps, no
sequence gap); three holding 1 s each with 4 buffers left 3.6 fps; no held frame changed in
any run.

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
| frame starts and embedded data, read on the pipeline thread when it wakes (was the event thread, two wake-ups: 0.013 ms) | ~0.005 ms |
| the consumer: queue hand-off, lease, its wake-up | 0.009 ms |
| statistics copy (every other frame) and conversion | 0.007 ms |
| back end config patch | 0.007 ms |
| embedded data decode | 0.005 ms |
| control schedule, frame metadata | ~0.005 ms |

Wake-ups. The PiSP path starts the sensor side "driven"
(`NativeCamera::start_external_driven`): the event thread serves only the bridge's start and
stop requests and sleeps while streaming, and the pipeline thread feeds the frame-start events
and embedded data to the control schedule itself when it wakes for a frame
(`SensorStream::service`, at the statistics dequeue and again before the algorithms' request,
so the request knows how much of the current frame is left). It waits for a frame start only
while a write waits for one (`SensorStream::frame_start_fd`: a request whose controls land a
frame apart, e.g. frame length after exposure); exposure and gain requests from the statistics
of frame F are still written during F with ≥ 4 ms left. Measured (CM5, 300 frames, HeliOS
tuning, 2026-10-02, `native-pipeline pisp --no-read` / `native_isp_bench single`):

| | before | after |
|---|---|---|
| waits per frame, 30 fps: pipeline thread + event thread | 2.05 + 2.05 | 2.09 + 0 |
| same, 120 fps | 2.04 + 2.04 | 2.14 + 0 (the extra 0.1: frame-start waits while AE converges) |
| CPU per frame, tool, 30 / 120 fps (pipeline + event thread) | 0.262 / 0.213 ms | 0.257 / 0.209 ms |
| Styx API, 30 / 120 fps: process CPU, latency median | 0.26 ms, 8.28 ms / 0.19 ms, 8.36 ms | 0.26 ms, 8.28 ms / 0.19 ms, 8.36 ms |
| AE (cold, ¼ and 3× steps), 30 fps: locked after | 6 / 3 / 3 frames | 6 / 3 / 3 frames |

The latency, 8.3 ms, is 7.4 ms of sensor readout (the timestamp is the frame start, the front
end's buffers complete at its end), 0.84 ms back end job (hardware; the algorithms run inside
it) and about 0.05 ms on the host.

### Controls through the Styx API

A processed capture's 3A loop takes the application's controls
(`styx::capture_api::native_controls`, applied to `Controller::set_controls` before the next
frame): `EXPOSURE_TIME_US` / `GAIN` fix that value for AE (both fixed is manual exposure; 0
hands it back), `AE_ENABLE` (off holds the current values), `EXPOSURE_VALUE` (stops),
`AWB_ENABLE`, `COLOUR_TEMPERATURE` (used while AWB is off; read: AWB's estimate for the latest
frame), `RED_GAIN` / `BLUE_GAIN`; `AE_STATE` reads 1 (searching) or 2 (converged). Controls
given with the `CaptureRequest` apply from frame 0. The frame rate is the capture's: `FRAME_RATE`
and `FRAME_DURATION_US` are refused (restart at another rate); on raw captures they, and
exposure and gain, go to the sensor's control schedule. `examples/01_capture/camera_controls.rs`
shows each, with the frame it landed on.

### Denoise settings

Through the Styx API (`StyxConfig`, also read from a serialised config): 
`NativeIspConfig::temporal_denoise` (`StyxConfig::native_temporal_denoise(bool)`, default
`true`) runs the back end's temporal denoise when the tuning has `rpi.denoise.tdn` (flat-area
noise as libcamera's, about 1.5 ms more back end time per 1280x800 frame: latency, not CPU);
`false` gives spatial and colour denoise at their no-TDN strengths and the shorter latency.
`NativeIspConfig::spatial_denoise_percent` (`native_spatial_denoise(percent)`, default 100)
scales the SDN noise model and the CDN threshold (0 turns both off). Both are per Styx
instance (camera service): every capture of a native PiSP camera opened through it uses
them. They are not per capture: a shared capture serves several consumers from one back end
pass, so per-consumer denoise would need a merge rule, and the planner's `PlanOverrides`
(and the IPC wire format) do not carry ISP settings. In `styx-pipeline`:
`PispOptions::temporal_denoise` / `spatial_denoise`, `Controller::set_spatial_denoise`,
`IspSettings::with_spatial_denoise`; `native-pipeline pisp --no-tdn` / `--spatial-denoise K`.

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
is free, on helper threads it starts once and keeps asleep between frames. What makes the path
cheap (details and kernel timings in `crates/softisp/PERFORMANCE.md`):

* **fp16 arithmetic** (`styx_softisp::Arithmetic::Half`, chosen automatically on CPUs with FP16
  arithmetic: the A76): front end fused with the RAW10 unpacking, demosaic and colour matrix
  in one pass, the tone curve as 48 segments looked up by the fp16 exponent with `tbl`, 4:2:0
  output from registers. The integer path stays the reference (x86, Cortex-A72/A53).
* **Cached capture buffers**: the raw frames go into `linux,cma` dma-heap buffers when the heap
  exists (`device::soft_capture_memory`; styx's processed native modes and `native-pipeline
  soft` do this unless `driver_buffers` / `--driver-buffers`), 0.6 ms less ISP time per frame
  than the receiver's uncached MMAP buffers. Rows are still staged 16 KiB at a time.
* **Statistics on every fourth quad row** (65 536 quads, 340 per zone: 0.23 ms instead of
  0.92 ms on every row), and **statistics and algorithms at 15 Hz once settled** (AE locked
  and AWB converged, as the PiSP path; `SoftLoop::set_settled_rate(None)` /
  `--every-frame` keeps every frame).
* **Lens shading tables** kept while the grid moves less than 0.25% (`LSC_TOLERANCE`; adaptive
  lens shading moves it on most frames, a rebuild costs 0.3 ms), and gain changes no longer
  rebuild them (fp16: 12 us of `set_params`).
* Embedded data decoded from only the bytes the layout reads (`styx-sensor`, the PiSP round):
  the event thread went from 1.3-1.5 ms per frame to 0.03 ms on this path too.

CM5, OV9782 1280x800 RAW10 with the HeliOS tuning (lens shading, CCM, adaptive contrast,
statistics), `native-pipeline soft --quiet`, 150 frames at 30 fps / 600 at 120 fps, 2026-10-02
(device time 2026-08-17). CPU is the whole process per frame (capture, ISP, 3A, the tool's
per-frame output level on every eighth row and column); latency is the sensor's frame-start
timestamp to the output being ready (about 8.2 ms of it the readout). Before: the
native-stack branch at `0d8638a` (MMAP buffers, integer ISP, event thread 1.4 ms).

| 30 fps | before, 1 thread | now, 1 thread | before, 4 threads | now, 4 threads |
|---|---|---|---|---|
| NV12 1280x800: CPU per frame (% of a core) | 7.3 ms (21.8%) | 3.4 ms (10.2%) | 7.5 ms (22.6%) | 4.0 ms (12.0%) |
| RGB24 1280x800 | 7.2 ms (21.6%) | 3.1 ms (9.4%) | 7.7 ms (23.2%) | 4.5 ms (13.7%) |
| RGB24 640x400 (binned) | 4.2 ms (12.6%) | 1.7 ms (5.0%) | 4.3 ms (13.0%) | 2.1 ms (6.4%) |
| luma 1280x800 | 4.5 ms (13.6%) | 2.1 ms (6.4%) | 4.7 ms (14.2%) | 2.7 ms (8.2%) |
| NV12: dequeue -> output / latency | 6.2 / 14.5 ms | 3.2 / 10.8 ms | 1.9 / 10.3 ms | 1.1 / 8.7 ms |
| RGB24: dequeue -> output / latency | 6.0 / 14.3 ms | 2.9 / 10.4 ms | 1.9 / 10.3 ms | 1.2 / 8.8 ms |
| peak RSS, NV12 / RGB24 | 11.6 / 12.9 MiB | 6.8 / 8.4 MiB | 11.7 / 13.2 MiB | 7.2 / 8.6 MiB |

| 120 fps | before, 1 thread | now, 1 thread | before, 4 threads | now, 4 threads |
|---|---|---|---|---|
| NV12 1280x800: CPU per frame (% of a core) | 7.1 ms (84.9%) | 3.3 ms (39.5%) | 7.3 ms (87.7%) | 4.0 ms (47.5%) |
| RGB24 1280x800 | 7.1 ms (84.7%) | 3.0 ms (36.3%) | 7.6 ms (90.5%) | 4.4 ms (52.9%) |
| RGB24 640x400 (binned) | 4.1 ms (48.7%) | 1.6 ms (19.2%) | 4.2 ms (50.3%) | 2.1 ms (25.4%) |
| luma 1280x800 | 4.5 ms (53.3%) | 2.1 ms (24.8%) | 4.6 ms (54.7%) | 2.6 ms (31.0%) |
| NV12: dequeue -> output / latency | 6.2 / 14.5 ms | 3.2 / 10.8 ms | 1.9 / 10.3 ms | 1.1 / 8.7 ms |

All runs held their rate (30.000 / 119.967 fps) with no sequence gaps; AWB settled at the same
temperature before and now (2533 K in one session, 2435 K in a later one: the light changed).
Of the 3.9 ms saved per NV12 frame, 1.3 ms is the embedded-data fix of the PiSP round; the
rest is this round's (fp16 ISP, cached capture buffers 0.6 ms, statistics and settings). Four threads cut the processing time to a third but cost 0.6
(NV12) to 1.4 ms (RGB24) more CPU: the cores share the memory bandwidth for the input and the
output (helper threads 0.9-1.0 ms each against a quarter of 2.7 ms).

Where the 3.1 ms of an RGB24 frame go now (30 fps, one thread):

| | ms |
|---|---:|
| ISP: RAW10 unpack, black level, gains, lens shading (fp16) | 0.5 |
| ISP: demosaic, colour matrix, tone curve, RGB24 | 1.6 |
| ISP: statistics (0.23 ms on the frames that take them: every second once settled) | 0.12 |
| ISP: staging the raw rows out of the CMA buffer, row loop, band edges | 0.5 |
| settings: gains (every frame), tone curve refit (when adaptive contrast moved it) | 0.07 |
| algorithms (every second frame once settled) and statistics conversion | 0.07 |
| dequeue / requeue, dma-buf cache maintenance, the tool's per-frame bookkeeping | 0.3 |
| event thread (frame starts, embedded data, control writes) | 0.03 |

NV12 costs 0.3 ms more than RGB24 (luma and chroma), luma alone 1 ms less, the binned half
size 1.4 ms less. A frame at 120 fps costs the same as at 30 fps; once settled the statistics
and algorithms run on every eighth frame there.

Quality against the integer arithmetic (the previous output): PSNR 53.9-55.3 dB per channel
on 55 recorded frames with the loop's settings, at most 2 codes apart (one sample in a
million more than 1); 54.7-61 dB on a synthetic chart (`crates/softisp/tests/quality.rs`).
Both arithmetics' outputs are pinned bit for bit (`tests/golden.rs`). `native-pipeline quality
--recording BASE` repeats the comparison on any recording. On the replay of the 60 recorded
frames the AE trajectory is unchanged but AWB ends at 4463 K instead of 2533 K with fp16: the
Bayesian AWB is bistable on this scene (warm lamp, blue LED). The integer path flips the same
way when its statistics are scaled by 1.0007 (fp16's differ by up to 0.05% per zone); live,
both settle at the same temperature. That is a sensitivity of the AWB search, not of the ISP.

## Planner

The native backend lists `NV12` and `RG24` modes at each sensor size (all rates of the raw
mode) with property `isp`. Costs (`planner/cost.rs`, from the measurements here): PiSP adds
0.9 ms latency and 0.3 ms CPU to the native capture; the software ISP 2.9 ms/MP of CPU
(+0.9 ms with helper threads) + 0.3 ms 3A, and 2.9 ms/MP divided over its threads (x1.3) of
latency (it runs on min(4, cores) threads, `StyxConfig::native_soft_threads`). Without a PiSP
the native backend also lists binned `NV12` / `RG24` modes at half each sensor size (each 2x2
quad a pixel, no demosaic, the sensor at full size), priced at 1.2 ms per raw megapixel: a
consumer asking for 640x400 gets one at about 40% of the full-size CPU. A native PiSP scales
like libcamera's ISP (`output_resolution` → `NativeIspConfig::output_size`), and a shared
capture uses both back end outputs: consumers are served by size and format (the PiSP makes
either processed format on either output; the second output is attached to each frame as a
`CompanionKind::Scaled` companion), so NV12 1280x800 + RG24 640x400 come from one pass with
no conversion; a third size or format falls back to the mode's size first, then to a CPU
conversion. Pyramid companions come from the second output as with libcamera: the first level
of an NV12 mode (`PyramidSource::PreferHardware` / `HardwareOnly`) is the back end's output 1
at half the main size (`NativeIspConfig::pyramid_level`, set by the plan), attached as
`CompanionKind::Pyramid { level: 1 }` (a GREY view for luma consumers); further levels are box
filtered from it. Measured (`native_isp_bench pyramid 30 300`, luma 1280x800 + 2 levels):
0.42 ms process CPU per frame with the hardware level, 0.55 ms with both levels on the CPU
(the consumer thread 0.07 ms vs 0.22 ms), latency 8.50 vs 8.56 ms. Shared plans of a sensor Styx drives run at exactly the rate asked when saving
power, as single plans do. Every output is handed out as a dma-buf, in process and to other
processes through the camera service (planes exported with their offsets). Raw native modes are not routed through a Bayer decoder when an ISP route exists
(that route has no 3A); Bayer decoders are priced at the software ISP's cost. `plan_frames`
for NV12, RG24 or luma (NV12's Y plane) on the native OV9782 picks the PiSP mode on the CM5
and the software ISP mode elsewhere. Tuning (`styx_pipeline::tuning`): `STYX_TUNING` (one file), else the
description's `tuning` file in `$STYX_TUNING_PATH`, `~/.config/styx/tuning`, `/etc/styx/tuning`,
`/usr/{local/,}share/styx/tuning`, then the tunings built into `styx-pipeline` (`tuning/`: the
HeliOS `ov9782.json`), then libcamera's `/usr/{local/,}share/libcamera/ipa/rpi/pisp` (read at
run time), else Styx's built-in generic tuning (`tuning/generic.toml`: grey world,
centre-weighted AE, default tone curve, spatial and colour denoise, no TDN).

The same pipeline runs unchanged on sensors with an upstream kernel driver (controls through
V4L2, values predicted from the delays when the driver sends no embedded data): the OV9782
under `ov9282` against the bridge, same scene and session, is in
[adding-a-camera.md](adding-a-camera.md#the-ov9782-on-the-cm5-kernel-driver-against-the-bridge)
(30/60/120 fps exact, every exposure, gain and frame length change on its predicted frame,
AE at 30 and 60 fps as on the bridge, CPU the same, first frame 8 ms later).

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
* Through the Styx API (`examples/04_performance/native_processed.rs`): `plan_best` for NV12, luma and RG24
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

Left after the first round, and what the second round found (below): the luma shading (edge
zones 1-6% brighter), the colour residual (±3-4% per zone, libcamera's adaptive ALSC not
ported) and the tone (1-2% darker highlights).

### Second round: the back end config byte for byte

libcamera dumps the back end config it programs when `LIBCAMERA_RPI_PISP_CONFIG_DUMP=<file>`
is set (libpisp's `GetJsonConfig`, the first frame after each configure; `--repeat 2` gives
the first frame of a second session, started from the first one's AWB and ALSC state);
`native-pipeline pisp` now writes ours (`pisp-be-config-first.bin`, `-last.bin`, the raw
`pisp_be_tiles_config`). Compared field by field with `tools/compare/be_config_diff.py`
(offsets from libpisp's own field table, `be_fields.json`; `native-pipeline be-replay --raw
FRAME --configs A.bin,B.bin` runs one raw frame through the back end with given configs, for
trying a block of libcamera's in ours on identical input):

* **Lens shading**: grid steps (6553, 10485), tile grid offsets and the LUT packing equal.
  libpisp's dump holds only the first row of the 33x33 table (its field table declares
  `lut_packed` as 33 entries): on that row, which has both top corners, our green gains equal
  libcamera's to one LSB (0.1-0.2%) at every vertex, red and blue within 1.5% (the start-up
  colour temperature: ours the tuning's `default_ct`, libcamera's AWB's first estimate).
  The calibration, resampling, luminance strength and packing were therefore right; the
  table is not what made the edges brighter.
* **Gamma**: equal byte for byte (the tuning's curve on the first frame, before the
  histogram stretch).
* **Output colour space**: the same full-range BT.601 matrix, offsets and 0..255 clipping
  (libcamera `sYCC` on the stream; libcamera programs it on output 1 and keeps output 0 off,
  ours on output 0); the internal YCbCr round trip identical too.
* **Differences that matter**: the front end's **luma histogram** (what the contrast stretch
  and the AGC constraints read) was uniform here, while libcamera weights it with the
  metering mode's 15x15 weights (centre-weighted by default: zero in the corners), and builds
  the front end's RGB-to-Y from the YCbCr luma row times the white balance gains with the
  1/min extra gain. In a scene with a lamp in a corner the uniform histogram's 95% point sat
  higher, so the stretch lifted the highlights less: the 1-2% darker highlights, and through
  the tone curve's slope a few percent on the zones away from the centre. Both now as
  libcamera (`Params::histogram_weights` from AGC, `IspSettings::apply_fe`).
* Not differences: SDN/CDN/TDN/GEQ values follow gain and the frame (the dump is libcamera's
  frame 0 at gain 1 with TDN reset); libcamera feeds the back end compressed raw (`PC1B`,
  8-bit with `DECOMPRESS`) where we feed 16-bit, and enables `DEBIN` (no effect at 1x1).

Raw frames at the same exposure (libcamera `BYR2` vs `native-pipeline soft --record`) match
within 0.1-0.5 codes at 2x gain; at 8x gain, in one dim run, ours were 2.3-2.4 codes lower
(of 10-bit) uniformly, which looks like a black level difference between the kernel driver's
and our register set at high gain; not settled (needs a covered lens).

Results (`tools/compare/zone_spread.py` over `quality.py`'s zones; CM5, the room lit this time; two scenes by exposure: dim 6 ms x 2, mean luma 0.20,
and bright 33 ms x 3.5, luma 0.78; AWB auto in both stacks; two sessions of each, the zones
where libcamera's two sessions differ by more than 3% (a blinking LED) left out; spread is
half the 5-95% range of the per-zone ratio to libcamera's first session, normalised to its
median, with the largest deviation in brackets):

| dim scene (36 of 40 zones) | libcamera, 2nd session | native now (two sessions) | previous round (`d79951f`) | now, no TDN |
|---|---|---|---|---|
| luma shading spread | ±0.8% (1.6%) | ±1.7% (4.9%), ±1.9% (4.5%) | ±2.0% (7.6%), ±2.2% (3.6%) | ±0.9% (5.2%) |
| R/G per zone: median, spread | 1.002, ±0.7% | 1.023 / 1.031, ±1.5-1.8% (3.1%) | 1.035 / 1.033, ±1.7-1.8% (2.9%) | 1.032, ±1.5% |
| B/G per zone | 0.999, ±0.5% | 0.990 / 0.980, ±1.0-1.1% (1.9%) | 0.981 / 0.976, ±0.9-1.3% (3.1%) | 0.986, ±0.9% |
| AWB | 2488-2491 K | 2543 K | 2542-2543 K | 2543 K |
| mean luma | 0.198 / 0.198 | 0.199 / 0.199 | 0.195 / 0.202 | 0.198 |
| tone: where libcamera's output is 0.104 / 0.212 / 0.342 / 0.464 | 0.105 / 0.212 / 0.341 / 0.461 | 0.105 / 0.216 / 0.345 / 0.464 | 0.103 / 0.210 / 0.337 / 0.456 | 0.105 / 0.212 / 0.342 / 0.465 |
| flat-area noise (0..255) | 0.12 / 0.28 | 0.22 / 0.23 | 0.18 / 0.25 | 0.65 |

| bright scene (40 zones) | libcamera, 2nd session | native now | previous round | now, no TDN |
|---|---|---|---|---|
| luma shading spread | ±0.5% (0.9%) | ±0.5-0.6% (1.0%) | ±0.5% (0.7%) | ±0.6% (0.8%) |
| R/G per zone: median, spread | 0.999, ±0.5% | 1.008 / 1.009, ±0.7% (1.7%) | 1.014 / 1.015, ±0.8-0.9% (1.7%) | 1.009, ±0.6% |
| B/G per zone | 1.002, ±0.5% | 0.994, ±0.9-1.2% (1.9%) | 0.993, ±1.0-1.1% (1.8%) | 0.994, ±1.1% |
| AWB | 2596-2602 K | 2643 K | 2639 K | 2643 K |
| tone: where libcamera's output is 0.464 / 0.668 / 0.843 / 0.982 | 0.458 / 0.661 / 0.837 / 0.981 | 0.459 / 0.663 / 0.839 / 0.980 | 0.461 / 0.664 / 0.842 / 0.980 | 0.459 / 0.661 / 0.839 / 0.979 |
| flat-area noise | 0.22 / 0.24 | 0.23 | 0.24 | 0.63 |

So in both scenes luma shading and tone are within about twice libcamera's own session-to-
session difference, and the earlier edge and highlight differences do not reproduce (the
scene was lit differently: the first round's dim room had the lamp in a corner, where the
uniform histogram mattered most). The colour medians follow AWB (ours 40-50 K warmer this
time, i.e. R/G +1-3%); with adaptive ALSC the per-zone colour spread is ±0.7-1.8%, against
±0.5-0.7% between libcamera's sessions (it was ±3-4% in the first round's scene, and
±0.8-1.8% with the previous binary here). Images (libcamera, native now, no TDN, previous
round; dim scene; full frame at half size, then 256x256 crops at 2x): `target/quality-full.png`,
`quality-centre.png`, `quality-corner.png`, `quality-flat.png`, `quality-edges.png`; the bright
scene in `target/quality-bright/`. Cost on the device: the same process CPU and algorithm
times as the previous binary within a session (0.61-0.72 ms per frame, algorithms p95
0.70-1.10 ms depending on the session, alike for both binaries).

Cost of temporal denoise (unchanged): the back end job goes from 0.84-0.88 ms to 2.3 ms at
1280x800, so the latency from the frame start to the outputs from 8.35 to 9.9 ms (30 and 120
fps); CPU unchanged. It is a Styx setting now (see "Denoise settings").

## Gaps

* PiSP lens shading: the ALSC tables (with the adaptive refinement) resampled to the back
  end's 33x33 grid, packed as the Raspberry Pi IPA does; checked against libcamera's
  programmed table (first row, the only one its dump holds) in "Quality vs libcamera".
* Raw input: libcamera feeds the back end compressed raw (`PC1B`), we feed 16-bit (twice the
  memory traffic between front and back end).
* The front end statistics set-up is fixed except for the histogram's zone weights (the
  metering mode's, as libcamera); AGC meters the AWB zones.
* PiSP CPU left (see "PiSP path performance"): half of it is the `pispbe` driver writing the
  whole back end config to the hardware by MMIO on every job (0.12 ms; the driver could write
  only changed blocks, as it already does for the front end). The pipeline thread waits
  twice per frame, for the front end's statistics and 0.84 ms later for the back end job; the
  two cannot share a wait without delaying the frame.
* Algorithms at 15 Hz while settled: a scene change is seen up to one 15 Hz period later
  (`settled_rate_hz: None` runs them on every frame).
* Brightness changes were forced exposure steps, not changes of the light.
* The tool's software runs use one thread unless `--threads` says otherwise (the `styx`
  native backend's software mode uses min(4, cores), and the planner prices it as measured).
  On the CM5 one thread is the cheapest in CPU (four cost 0.6-1.4 ms more per frame); the
  default favours latency.
* The software ISP's dma-heap capture buffers still need their rows staged into a cached
  buffer: read directly they measured 1 ms slower per frame (16-byte loads at a 10-byte stride
  straight from DRAM).
