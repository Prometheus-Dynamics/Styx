# Examples

Examples are organized as the top-level `styx-examples` package:

- `00_quickstart`: listing cameras, capturing processed frames, first-run pipelines
- `01_capture`: async capture, controls and metadata, shared captures, raw frames and
  recordings, hotplug, file/netcam/simulation inputs
- `02_graph`: Daedalus graph integration and `FrameLease` graph flow
- `03_codecs`: decode/encode examples
- `04_performance`: benchmarks, zero-copy validation, the native stack's measurements
- `05_apps`: camera service, frame socket, preview and recording applications
- `06_new_camera`: adding a camera (a sensor description, a kernel-driver data file, a checker)

Every example says at the top what it shows and how to run it. Features pick the camera
backends: `v4l2` (USB and other V4L2 cameras, with the CPU converters for YUYV), `native`
(sensors Styx drives itself, the PiSP or the software ISP, 3A in Rust) and `libcamera`
(optional, needs libcamera installed). None of the examples below needs libcamera.

## Using a camera with Styx

`...` is `cargo run -p styx-examples --features native,v4l2`.

| Example | Shows | Run |
|---|---|---|
| [`list_cameras`](00_quickstart/list_cameras.rs) | Every camera through every backend: modes, formats, frame rates (the exact range for sensors Styx drives), controls | `... --bin list_cameras` |
| [`capture_frames`](00_quickstart/capture_frames.rs) | NV12 / RGB / luma at a size and frame rate; the planner picks camera and route and prints its plan | `... --bin capture_frames -- nv12 1280x800 30 90 [out.pgm]` |
| [`async_capture`](01_capture/async_capture.rs) | Awaiting frames under Tokio, and the same future on a ten-line `std` executor | `cargo run -p styx-examples --features async,native,v4l2 --bin async_capture [-- --no-runtime]` |
| [`camera_controls`](01_capture/camera_controls.rs) | AE and AWB on and off, manual exposure and gain, EV, colour temperature and gains, denoise, frame rate; the frame each change lands on, from per-frame metadata; V4L2 controls by name | `... --bin camera_controls` |
| [`two_consumers`](01_capture/two_consumers.rs) | One capture, two consumers at different sizes and formats (both PiSP outputs from one pass); a slow consumer drops only its own frames | `... --bin two_consumers -- [seconds]` |
| [`raw_processing`](01_capture/raw_processing.rs) | Raw Bayer frames through your own processing (the software ISP and a white balance loop written in the example), recorded to MCAP and replayed anywhere | `cargo run -p styx-examples --features native,replay-mcap --bin raw_processing -- record /tmp/raw.mcap 60` / `-- replay /tmp/raw.mcap` |
| [`hotplug`](01_capture/hotplug.rs) | Cameras appearing and disappearing; a capture per camera | `cargo run -p styx-examples --features hotplug,native,v4l2 --bin hotplug -- [seconds]` |
| [`camera_service`](05_apps/camera_service.rs) | One camera for many processes; each client asks for its own format and size; frames passed as dma-bufs | `... --bin camera_service -- serve` and `-- client luma 320x200` |
| [`frame_socket`](05_apps/frame_socket.rs) | The latest frame for other processes, leased: its buffer is not reused while a consumer holds it | `... --bin frame_socket -- serve /tmp/cam.sock` and `-- fetch /tmp/cam.sock 10 300` |
| [`sensor_check`](06_new_camera/sensor_check.rs) | Validating a new sensor description or kernel-driver data file, and what Styx makes of it (modes, exact rates, exposure limits) | `cargo run -p styx-examples --features native --bin sensor_check -- examples/06_new_camera/example_sensor.toml` |

How these compare with libcamera, raw V4L2 and GStreamer code for the same tasks:
[docs/comparison.md](../docs/comparison.md).

Adding a camera: [`example_sensor.toml`](06_new_camera/example_sensor.toml) (a sensor Styx
drives through its sensor bridge) or [`example_sensor.kernel.toml`](06_new_camera/example_sensor.kernel.toml)
(a sensor with its own kernel driver), checked with `sensor_check`; then on the board
`list_cameras`, `native-pipeline regcheck` (`tools/native-pipeline`) and `landing`
(`crates/native/examples`). The full guide:
[docs/native-stack/adding-a-camera.md](../docs/native-stack/adding-a-camera.md).

### Where they ran

2026-10-02, release builds: a Raspberry Pi CM5 (HeliOS image) with an OV9782 behind the Styx
sensor bridge (PiSP, embedded data) and a Logitech C270 on USB, in a dim room; and an x86
desktop with a v4l2loopback camera fed by GStreamer (`gst-launch-1.0 videotestsrc ! v4l2sink`,
`STYX_V4L2_ALLOW_VIRTUAL=1`). All of them ran on the CM5; on the desktop all but
`camera_service` (`camera_controls` there has only its V4L2 part, `hotplug` saw no unplug,
`raw_processing` replayed the CM5's recording). Output, trimmed:

<details><summary>CM5: <code>list_cameras</code></summary>

```text
ov9782 (styx bridge i2c 10-0060)
  keys: bridge:/dev/v4l-subdev2, i2c:10-0060, native:ov9782
  backend native: bridge=/dev/v4l-subdev2 backend=bridge sensor=ov9782 description=embedded:ov9782 ... isp=pisp
    pBAA 1280x800  60.280, 120.626 fps (any rate 2.100..120.626 fps)
    BA81 1280x800  72.336, 144.751 fps (any rate 2.519..144.751 fps)
    pBAA  640x400  77.224, 259.788 fps (any rate 2.116..259.788 fps)
    NV12 1280x800  60.280, 120.626 fps (any rate 2.100..120.626 fps)
    RG24 1280x800  60.280, 120.626 fps (any rate 2.100..120.626 fps)
    NV12  640x400  77.224, 259.788 fps (any rate 2.116..259.788 fps)
    ...
    control exposure_time_us             9..476184 (default 5842)
    control gain                         1..15.9375 (default 1)
    control frame_rate                   2.0995035..311.7446 (default 60.27982)
    control ae_enable                    false..true (default true)
    control awb_enable                   false..true (default true)
    control colour_temperature           1000..20000 (default 0)
    control ae_state                     0..2 (default 1)
046d:0825
  keys: UVC Camera (046d:0825), 046d:0825, uvcvideo, usb-xhci-hcd.1-1
  backend v4l2: path=/dev/video8 name=UVC Camera (046d:0825) driver=uvcvideo ...
    YUYV  640x480  30.000, 25.000, 20.000, 15.000, 10.000, 5.000 fps
    YUYV 1280x960  7.500, 5.000 fps
    MJPG 1280x960  30.000, 25.000, 20.000, 15.000, 10.000, 5.000 fps
    ...
    control Brightness                   0..255 (default 128)
    control Auto Exposure                0..3 (default 3)
    control Exposure Time, Absolute      1..10000 (default 166)
```

In libcamera mode (`camera-mode.sh libcamera`: the OV9782 under its `ov9282` kernel driver), a
build with `--features native,v4l2,libcamera` lists the same sensor through both backends:

```text
ov9782 (kernel driver i2c 10-0060)
  keys: sensor:/dev/v4l-subdev2, i2c:10-0060, native:ov9782, /base/axi/pcie@1000120000/rp1/i2c@88000/ov9782@60, ov9782
  backend native: subdev=/dev/v4l-subdev2 backend=kernel sensor=ov9782 description=kernel driver + builtin:ov9782 ... isp=pisp
    pBAA 1280x800  14.949, 120.626 fps (any rate 2.100..120.626 fps)
    NV12 1280x800  14.949, 120.626 fps (any rate 2.100..120.626 fps)
    ...
  backend libcamera: ... Model=ov9782 id=/base/axi/pcie@1000120000/rp1/i2c@88000/ov9782@60
    ...
```

</details>

<details><summary>CM5: <code>capture_frames</code></summary>

`nv12 1280x800 30 90` and `rgb 640x400 60 120` (cold start: AE begins at 1 ms):

```text
frame plan for ov9782 (styx bridge i2c 10-0060) via native NV12 1280x800 @ 30 fps: ~9.7 ms latency, ~0.30 ms CPU per frame
  1. capture    hardware    9.69 ms  native NV12 1280x800 (PiSP front end statistics and back end, raw frames as dma-bufs, 3A in Styx)
  rejected (38 total, first 8):
    - native pBAA 1280x800: raw pBAA would need a decoder without 3A; the camera's processed modes run AE/AWB
    ...
delivers 1280x800
frame 0: NV12 1280x800, 2 plane(s), t=21263.713 s, exposure 1.00 ms x gain 1.00
frame 89: NV12 1280x800, 2 plane(s), t=21266.680 s, exposure 33.22 ms x gain 10.00
90 frames, open -> first frame 33.8 ms, 30.000 fps by sensor timestamps

frame plan for ov9782 (styx bridge i2c 10-0060) via native RG24 640x400 @ 60 fps: ~5.2 ms latency, ~0.30 ms CPU per frame
120 frames, open -> first frame 29.3 ms, 59.983 fps by sensor timestamps
```

The same request with `STYX_NATIVE_ISP=software` (the software ISP instead of the PiSP), the
USB camera (`STYX_CAMERA=uvc ... nv12 640x480 30 60`), and in libcamera mode the OV9782 under
its kernel driver (`STYX_CAMERA=sensor:`):

```text
frame plan for ov9782 (styx bridge i2c 10-0060) via native NV12 1280x800 @ 30 fps: ~10.1 ms latency, ~4.17 ms CPU per frame
  1. capture    cpu        10.06 ms  native NV12 1280x800 (software ISP and 3A in Styx)
90 frames, open -> first frame 34.7 ms, 30.000 fps by sensor timestamps

frame plan for 046d:0825 via v4l2 YUYV 640x480 @ 30 fps: ~26.8 ms latency, ~0.11 ms CPU per frame
  1. capture    zero-copy  26.67 ms  v4l2 YUYV 640x480 (camera exposure, encode and transfer)
  2. decode     cpu         0.11 ms  YUYV -> NV12 via yuyv-nv12
60 frames, open -> first frame 623.4 ms, 13.397 fps by sensor timestamps

frame plan for ov9782 (kernel driver i2c 10-0060) via native NV12 1280x800 @ 30 fps: ~9.7 ms latency, ~0.30 ms CPU per frame
90 frames, open -> first frame 41.2 ms, 30.000 fps by sensor timestamps
```

(The C270's own auto exposure stretched its frames in the dim room: 13.4 fps.)

</details>

<details><summary>CM5: <code>async_capture</code>, <code>camera_controls</code>, <code>two_consumers</code></summary>

```text
Tokio, current-thread runtime
frame plan for ov9782 (styx bridge i2c 10-0060) via native NV12 640x400 @ 30 fps: ~5.2 ms latency, ~0.30 ms CPU per frame
  frame 89 640x400, mean wait 33.0 ms
90 frames; the timer ticked 6 times meanwhile

no runtime: std block_on
  frame 89 640x400, mean wait 33.0 ms
90 frames
```

`camera_controls` (in the dim room AE ends at its limits, 33 ms x 10, and keeps searching):

```text
== processed NV12, 3A in Rust ==
AE searching at frame 90, 3001 ms after open: frame   89 t=21280.8869 s: exposure 33.215 ms, gain 10.00 (analogue 10.00 x digital 1.00), frame 33.33 ms (read back)
AWB: Uint(2563) K
manual: exposure 10 ms, gain 2.0
  landed 3 frame(s) after the request: frame   93 t=21281.0202 s: exposure 10.001 ms, gain 2.00 (analogue 2.00 x digital 1.00), frame 33.33 ms (read back)
exposure fixed at 5 ms, gain automatic (0), +1 stop of exposure compensation
  landed 3 frame(s) after the request: frame   97 t=21281.1535 s: exposure  4.996 ms, gain 10.00 (analogue 10.00 x digital 1.00), frame 33.33 ms (read back)
white balance: AWB off, 5000 K; then red 1.8 / blue 1.4
  colour temperature now Uint(5000)
  gains red Float(1.8) blue Float(1.4)
everything automatic again
frame rate control: control apply failed: a processed native capture keeps the frame rate it started with (AE chooses exposures within it); start it again at another rate (CaptureHandle::reconfigure)
restarted at 60 fps: first frame 72 ms after the restart began
  frame    0 t=21282.0930 s: exposure 16.553 ms, gain 3.00 (analogue 3.00 x digital 1.00), frame 16.67 ms (read back)

== raw pBAA, controls straight to the sensor ==
exposure 6 ms, gain 3.0
  landed 2 frame(s) after the request: frame    7 t=21282.3829 s: exposure  5.997 ms, gain 3.00 (analogue 3.00 x digital 1.00), frame 33.33 ms (read back)
frame rate 60 fps
  frame    9 t=21282.4496 s: exposure  5.997 ms, gain 3.00 (...), frame 16.67 ms (read back) (interval 33.33 ms)
  frame   10 t=21282.4663 s: exposure  5.997 ms, gain 3.00 (...), frame 16.67 ms (read back) (interval 16.67 ms)

== V4L2: 046d:0825 ==
  Auto Exposure                        now Some(Int(3))
  Exposure Time, Absolute              now Some(Int(667))
  Brightness: Int(128) -> Int(255)
```

`two_consumers 5` (the detector spends 50 ms per frame):

```text
shared capture of ov9782 (styx bridge i2c 10-0060) via native NV12 1280x800 for 2 consumers
consumer 0: frame plan for ov9782 (styx bridge i2c 10-0060) via native NV12 1280x800 @ 30 fps: ~9.7 ms latency, ~0.30 ms CPU per frame
consumer 1: frame plan for ov9782 (styx bridge i2c 10-0060) via native RG24 1280x800 @ 30 fps: ~9.7 ms latency, ~0.30 ms CPU per frame
  1. capture    hardware    9.69 ms  native RG24 1280x800 (PiSP front end statistics and back end, raw frames as dma-bufs, 3A in Styx)
  2. scale      hardware    0.00 ms  640x400 from the ISP (the mode's field of view), on its second output
  note: RG24 from the ISP's output, no conversion
recorder: 151 frames NV12 1280x800 (dma-buf), 30.2 fps, 0 dropped
detector: 100 frames RG24 640x400 (dma-buf), 20.0 fps, 50 dropped
```

</details>

<details><summary>CM5: <code>raw_processing</code>, <code>hotplug</code>, <code>frame_socket</code>, <code>camera_service</code>, <code>landing</code></summary>

`raw_processing record /tmp/ex/out/raw.mcap 60 20000 4`, then `replay` (on the CM5, and the
same recording on the desktop: identical numbers, 1.0 ms per frame there):

```text
ov9782 (styx bridge i2c 10-0060) via native: pBAA 1280x800
frame    0 pBAA 1280x800, sensor 20.00 ms x 4.00: luma 0.047, white balance R 0.91 B 1.33, digital gain 2.06
frame   15 pBAA 1280x800, sensor 20.00 ms x 4.00: luma 0.167, white balance R 0.96 B 1.31, digital gain 8.00
60 frames, software ISP + statistics 1.75 ms per frame
recorded 60 frames to /tmp/ex/out/raw.mcap

replaying ov9782 (styx bridge i2c 10-0060) (replay)
frame    0 pBAA 1280x800: luma 0.047, white balance R 0.91 B 1.33, digital gain 2.06
frame   15 pBAA 1280x800: luma 0.167, white balance R 0.96 B 1.31, digital gain 8.00
60 frames, software ISP + statistics 2.67 ms per frame
```

`hotplug 16`, the C270 de-authorized at 4 s and back at 9 s:

```text
  capturing from ov9782 (styx bridge i2c 10-0060)
  capturing from 046d:0825
watching for 16 s
  4.46 s removed 046d:0825
  its capture ended after 33 frames
  9.80 s added 046d:0825
  capturing from 046d:0825
frames so far: 046d:0825: 52, ov9782 (styx bridge i2c 10-0060): 599
```

`frame_socket serve /tmp/ex/cam.sock 10` and, in another process, `fetch /tmp/ex/cam.sock 6 300`:

```text
NV12 1280x800 t=21308.460 s via Some(Dmabuf), 2 planes, fetched in 0.12 ms, 39.9 ms old, unchanged after 300 ms: true
NV12 1280x800 t=21309.693 s via Some(Dmabuf), 2 planes, fetched in 0.06 ms, 10.1 ms old, unchanged after 300 ms: true
FrameSocketStats { published: 301, copied: 0, served: 6, leases: 0, held_frames: 0, revoked: 0, unserved: 0 }
```

`camera_service serve ov9782` with two clients at once, `client luma 320x200 5` and
`client rgb 640x400 5` (no rate asked: 30 fps, `planner::DEFAULT_FPS`; before that default
the plan took the mode's fastest, 260 fps at 640x400, 0.4% and 0.8% CPU per client):

```text
shared capture of ov9782 (styx bridge i2c 10-0060) via native NV12 640x400 for 2 consumers
consumer 0: frame plan for ov9782 (styx bridge i2c 10-0060) via native NV12 640x400 @ 30 fps: ~5.2 ms latency, ~0.30 ms CPU per frame
  2. luma view  zero-copy   0.00 ms  Y plane of NV12
  3. scale      hardware    0.00 ms  320x200 from the ISP (the mode's field of view), on its second output
consumer 1: frame plan for ov9782 (styx bridge i2c 10-0060) via native RG24 640x400 @ 30 fps: ~5.2 ms latency, ~0.30 ms CPU per frame
  note: RG24 from the ISP's output, no conversion
149 frames 320x200 via dma-buf: 29.8 fps, 149 keyframes, 15251 kbit/s, age p50 4.6 ms, CPU 0.0%
150 frames 640x400 via dma-buf: 30.0 fps, 150 keyframes, 184316 kbit/s, age p50 4.6 ms, CPU 0.0%
```

`landing 30 1` (`crates/native/examples`: the check of a new camera's control delays; image
levels were off by up to 14% in this run from 100 Hz light on a dark scene, the steps are what
it checks):

```text
30.000 fps, delays exposure 2 gain 2 frame length 1
47 frame periods: within 1% of the reported frame duration on 47 (0 off)
  exposure x2            asked in frame   13 (next), predicted   16 (+3), changed    16 ok
  gain x2                asked in frame   25 (next), predicted   28 (+3), changed    28 ok
  rate /1.5              asked in frame   49 (next), predicted   51 (+2), changed    51 ok
8 of 8 steps landed on the predicted frame
```

</details>

<details><summary>Desktop: v4l2loopback YUYV 1280x720, and <code>sensor_check</code></summary>

```text
$ capture_frames nv12 1280x720 30 90
frame plan for platform:v4l2loopback-000 via v4l2 YUYV 1280x720 @ 30 fps: ~27.0 ms latency, ~0.32 ms CPU per frame
  1. capture    zero-copy  26.67 ms  v4l2 YUYV 1280x720 (camera exposure, encode and transfer)
  2. decode     cpu         0.32 ms  YUYV -> NV12 via yuyv-nv12
90 frames, open -> first frame 14.2 ms, 30.000 fps by sensor timestamps

$ two_consumers 4
recorder: 122 frames NV12 1280x720 (memory), 30.5 fps, 0 dropped
detector: 80 frames RG24 1280x720 (memory), 20.0 fps, 39 dropped

$ frame_socket fetch target/host-frame.sock 4 200     # serve running in another process
NV12 1280x720 t=231.341 s via Some(HostExternal), 2 planes, fetched in 0.09 ms, unchanged after 200 ms: true
FrameSocketStats { published: 182, copied: 0, served: 4, leases: 0, held_frames: 0, revoked: 0, unserved: 0 }

$ sensor_check examples/06_new_camera/example_sensor.toml
sensor example (vendor not given), I2C address 0x10, 16-bit register addresses, chip id 0x0219 at 0x0000
pixel array 3296x2480, active 3280x2464, Rggb, black level Some((64, 10))
analogue gain 1.00x..10.67x; delays (frames) exposure 2, gain 1, frame length 2; group hold yes; embedded data no
mode 1640x1232 1640x1232 raw10 (10-bit, 182.4 MHz pixel rate): 0.807..42.800 fps, default 19.724
       15 fps: VTS 3527 (vblank 2295) -> 14.9986 fps; exposure 0.076..66.597 ms
       30 fps: VTS 1763 (vblank 531) -> 30.0058 fps; exposure 0.076..33.251 ms

$ sensor_check            # the built-in description and data files
== built-in description ov9782
mode 1280x800 1280x800 raw10 (10-bit, 160.0 MHz pixel rate): 2.100..120.626 fps, default 60.280
       30 fps: VTS 3662 (vblank 2862) -> 30.0000 fps; exposure 0.009..33.215 ms
      120 fps: VTS 915 (vblank 115) -> 119.9674 fps; exposure 0.009..8.217 ms
```

</details>

## Other examples

Run examples through the `styx-examples` package with explicit features:

```bash
cargo run -p styx-examples --bin quickstart_capture_virtual
cargo run -p styx-examples --bin quickstart_runtime_memory_report
cargo run -p styx-examples --features camera-graph --bin camera_graph_metrics
```

Canonical examples for the intended facade:

- `capture_virtual`: smallest blocking capture request example.
- `runtime_memory_report`: process-level and pipeline-attached memory telemetry report.
- `low_latency_preview`: latest-frame preview path with queue depth tuned for freshness.
- `reliable_recording`: record every frame to disk with queue and pool sizing biased toward completeness.
- `latest_frame_fanout`: split one source into multiple latest-only consumers without adding backpressure.
- `file_replay`: replay recorded files through the same capture facade.
- `netcam_capture`: ingest MJPEG over HTTP with explicit timeout and backoff tuning.

Specialized examples remain for feature-specific surfaces such as:

- `async_pipeline`
- `probe_and_select`
- `graph_fanout`
- `v4l2_hardware_bench`
- `pipeline_health`
- `ffmpeg_scale`
- `libcamera_ffmpeg_preview`
- `uvc_capture` (`04_performance`, `--features uvc`): USB cameras through the userspace UVC
  backend against `uvcvideo` (rate, timestamp jitter, latency, CPU, controls, replug), as
  quoted in [docs/uvc.md](../docs/uvc.md); `crates/uvc/examples/uvc_raw.rs` uses `styx-uvc` alone
- `native_capture`, `native_processed`, `native_isp_bench` (`04_performance`): the native
  stack's rates, latency, CPU and memory, as quoted in
  [docs/native-stack/pipeline.md](../docs/native-stack/pipeline.md)

CI builds the portable subset with `async`, `file-backend` and `netcam`, and every example
above with `native,v4l2,async,hotplug,replay-mcap` in a job without libcamera installed.
Hardware, preview window, FFmpeg, libcamera and simulation examples stay behind their matching
feature flags.
