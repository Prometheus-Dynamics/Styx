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
* **Sensor requests.** AE names the frame its request lands on: `F + issue latency (2) +
  max delay` (OV9782: exposure/gain 2, frame length 1 → F + 4). `ControlHandle::request_at`
  hands it to the scheduler, which writes each control `delay` frames earlier (group hold).
  A request whose values repeat the previous one is not re-sent.
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

| | PiSP | software ISP |
|---|---|---|
| frame rate (160 frames) | 30.000 fps, no gaps | 30.000 fps, no gaps |
| open → first frame | 75.6 ms | 70.3 ms |
| open → first AE-locked frame | 475.6 ms (frame 12) | 670.3 ms (frame 18) |
| AE start: exposure / output within 5%, locked | 12 / 6 / 12 frames, overshoot 0% | 17 / 12 / 18, 4.5% |
| darker step (¼): exposure / output, locked | 12 / 7 frames, 14; overshoot 4.5% | 12 / 8, 14; 4.6% |
| brighter step (3×): exposure / output, locked | 15 / 14 frames, 17; 3.7% | 20 / 20, 22; 4.4% |
| sensor timestamp (frame start) → output ready | 9.79 ms median, 10.55 p95 | 23.4 ms median, 25.4 p95 |
| dequeue → output ready | 2.26 ms (back end job 0.84, config + tiles 0.024) | 15.0 ms |
| CPU per frame (whole process) | 3.7 ms (11% of a core) | 18.7 ms (56%) |
| peak RSS | 27.5 MiB | 12.8 MiB |
| AWB (Bayesian, HeliOS tuning) | 2533 K, output R/G 1.10 B/G 0.87 | 3127 K, R/G 1.31 B/G 0.87 |

* Sensor values were read back from embedded data on every frame; statistics and raw frames
  always had the same sequence; no frame was dropped.
* At 60 and 120 fps the PiSP path keeps up (2.9 and 3.0 ms CPU per frame); the software ISP
  path needs 15 ms per frame on one core (the CPU figures include the tool's own per-frame
  mean of the output and, for the software path, writing the raw recording).
* The light is warm: the Bayesian AWB (CT curve from the tuning) keeps some of it, grey world
  (default tuning, host replay of the recorded frames) ends at R/G 0.997, B/G 1.006.
* libcamera on the same device and scene (the compare harness, `tools/compare` on
  `native/compare`, Styx's libcamera backend, NV12, median of 3): open → first frame 103 ms,
  open → AE converged 635 ms (raw BYR2: 568 ms). Native: 76 ms and 476 ms (PiSP).
* Host replay of the recorded frames (software ISP, one x86 core): 1.9 ms per frame, AE
  settles in 12 frames, locks at 13.
* Images: `pisp-nv12-rgb` (output 0, NV12 converted), `pisp-rgb-half` (output 1),
  `soft-rgb`, `replay-soft-rgb` (PPM and PNG in the run's output directory).

Found on the way: with the kernel driver's default flips (both on) the OV9782's order is RGGB
and the picture is turned against libcamera's, which runs them off; the description now
defaults to flips off (BGGR, upright). A frame lasts VTS + 1 lines (measured at 30/60/120 fps):
`controls.frame_length_extra_lines = 1`. Every stop request timed out (1 s, "stop request
failed: -110") in the runs above: the bridge was served through the reactor, which blocks in
`vb2_fop_poll` while `STREAMON`/`STREAMOFF` hold the receiver's node lock. native/harden's event
thread (own `poll`, quiesced around `STREAMON`/`STREAMOFF`) fixes that; the external route
quiesces it around the front end's `STREAMON`/`STREAMOFF` too (`start_external` spawns it
quiesced, `resume_external` after the front end started, `quiesce_external` before it stops).
Not yet re-run on the device.

## Gaps

* PiSP lens shading is applied since the device runs above (the ALSC tables resampled to the
  back end's 33x33 grid, packed as the Raspberry Pi IPA does; tested on the host, not yet on
  the device). TDN/sharpening strength/denoise follow libpisp defaults, not the tuning.
* The front end statistics set-up is fixed (uniform AGC weights; AGC meters the AWB zones).
* Styx capture of processed native modes (`native_isp.rs`) is planned and unit-tested on the
  host, not yet run on the device; it uses output 0 only (the tool uses both outputs).
* Brightness changes were forced exposure steps, not changes of the light.
* The software ISP runs single-threaded here (`rayon` off).
