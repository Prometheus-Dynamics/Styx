# Frame Planning

`FrameRequirements` (in `styx-core`) describes the frames a consumer needs. `styx::planner`
turns those requirements into a concrete capture, decode and preparation plan, then runs it.

```rust
use std::time::Duration;
use styx::planner::plan_best;
use styx::prelude::*;

let requirements = FrameRequirements::luma()   // Y8
    .stride_alignment(64)                      // 64-byte rows and base addresses
    .pyramid(2)                                // attach ½ and ¼ companions
    .min_resolution(1280, 720)
    .min_fps(25)
    .priority(Priority::Latency);              // or Throughput / Power

let plan = plan_best(&probe_all(), &requirements)?;
println!("{plan}");                            // steps, where they run, costs, rejected options

let mut frames = plan.start()?;
let roi = frames.roi();                        // change the region of interest while running
while let RecvOutcome::Data(frame) = frames.next_frame(Duration::from_millis(500)) {
    let rows = frame.luma_rows()?;             // Y8, 64-byte aligned
    let quarter = frame.pyramid_level(2);      // same timestamp as `frame`
    let origin = frame.meta().crop;            // where an ROI view sits in the full frame
    let latency = frame.meta().latency();      // sensor -> now breakdown
    // ...
    roi.set(Some(FrameRect::new(320, 200, 640, 400)));
}
# Ok::<(), Box<dyn std::error::Error>>(())
```

## What the planner considers

For every device, backend and mode it builds the cheapest route to the requested output:

| Capture format | Route (luma output) | Pyramid |
|---|---|---|
| GREY / R8 | direct (libcamera colour sensors deliver the ISP's Y plane zero-copy) | ISP second output (Raspberry Pi), else box filter |
| NV12, YUV420, NV16, ... | zero-copy Y-plane view | ISP second output (Raspberry Pi), else box filter |
| MJPEG | hardware Y8 decoder if one opened at runtime, else turbojpeg-luma (multi-core with restart markers) | box filter |
| YUYV | luma copy | box filter |
| H.264 / H.265 | FFmpeg hardware Y8 decoder if available | box filter |
| RGB | luma conversion | box filter |

Candidates are filtered by `min/max_resolution`, `min_fps` and the overrides, then ranked:

1. resolution: the smallest mode at or above `min_resolution` when it is set, else the smallest
   mode covering `output_resolution` when that is set, otherwise the largest;
2. estimated cost for the requested `Priority`;
3. frame rate;
4. V4L2 over libcamera's UVC pipeline for the same USB camera.

Costs come from measurements on a Raspberry Pi CM5; absolute numbers differ by machine but the
ranking holds. `Priority` also sets decode threads per frame (latency: automatic, up to four) and
queue depth (latency 1, i.e. newest frame only; throughput 4; power 3). `PlanOverrides::decode_threads` and
`queue_depth` override them.

## Overrides and hardware

- `PlanOverrides::backend`: only consider `"libcamera"`, `"v4l2"`, ...
- `PlanOverrides::decoder`: use exactly this decoder implementation (e.g. `"turbojpeg-luma"`).
- `PlanOverrides::forbid`: never use these decoder implementations.
- `PlanOverrides::hardware`:
  - `Auto` (default) uses hardware decoders whose Cargo feature is enabled, whose device exists
    and which open. The device check needs no FFmpeg; opening happens on the first lookup of
    that input format.
  - `Disabled` keeps everything on the CPU.
  - `Required` fails planning for CPU-only paths.
- `PyramidSource`:
  - `PreferHardware` (default) takes level 1 from the ISP's second output where possible and
    computes deeper levels with box filters.
  - `HardwareOnly` fails without an ISP output. The ISP provides a single level.
  - `Software` always uses box filters.
- `strict()` is reserved for turning fallbacks into errors.

## Output size

`FrameRequirements::output_resolution(w, h)` is the size the consumer works at, for example
320x180 for a detector that would downsize anyway. The planner then prefers the smallest mode
that covers it, and MJPEG decoded with turbojpeg (luma or RGB) is decoded straight to ½, ¼ or ⅛
size in the DCT domain: the smallest of those that still covers the request. Frames are never
upscaled. `FramePlan::output_resolution()` gives the size frames arrive at, and the plan says
when its route cannot scale.

On a Raspberry Pi the ISP scales instead (libcamera's output size; a `scale` step in the plan).
It keeps the mode's aspect ratio so the field of view is not cropped: 320x180 from a 16:10
OV9782 arrives at 320x200. Modes are ranked by the size of the frames they deliver, so a wide
mode scaled by the ISP beats a smaller 4:3 mode. On a CM5, OV9782 luma at 320x200 runs at
30 fps with 1.5% CPU, delivered straight from the ISP. `HardwarePolicy::Disabled` turns ISP
scaling off.

On a Raspberry Pi CM5, Logitech C270 MJPEG at 1280x720 delivered at 320x180:

| Output | Full size | 320x180 |
|---|---|---|
| Luma | 0.87 ms, 3.95 MB | 0.64 ms, 2.42 MB |
| RGB | 6.03 ms, 12.75 MB | 1.97 ms, 2.34 MB |

Decode time per frame (p50) and the process's memory while capturing (PSS). Scaled decodes of
frames with restart markers are split across cores like full-size ones. The full-size RGB plan
decodes with FFmpeg, whose libraries add to its memory; asking for a smaller size selects a
decoder that can scale.

## Several consumers of one camera

`plan_many(device, &[detector, recorder])` plans one capture for several consumers, for example
Y8 for a detector and MJPEG for a recorder. It returns a `SharedFramePlan`: the mode and
interval every consumer can use, plus a `FramePlan` per consumer. `start()` returns a
`PlannedFrames` per consumer, each prepared its own way (decoder, pyramid, ROI, output size).

```rust
use styx::planner::plan_many;

let plan = plan_many(&device, &[
    FrameRequirements::luma().output_resolution(320, 180),
    FrameRequirements::formats([FourCc::MJPG]),
])?;
println!("{plan}");
let mut consumers = plan.start()?;
let recorder = consumers.pop().unwrap();
let detector = consumers.pop().unwrap();
```

- **Mode:** the smallest mode that covers every consumer's output size (the largest if any
  consumer states none); then the lowest combined cost.
- **Two ISP sizes (Raspberry Pi):** the ISP has two outputs. Consumers wanting two different
  sizes get the larger from the main output and the smaller from the second, both scaled in
  hardware from the same exposure. With a third size, or when an ISP pyramid uses the second
  output, the ISP delivers the mode's size and those consumers scale on their own.
- **Decoding once:** consumers with the same requirements apart from the region of interest
  share their preparation: each frame is decoded (and its pyramid built) once, and each
  consumer's region is a zero-copy crop of the shared result. The plan notes which consumers
  share.
- **Frames:** the capture is read by whichever consumer asks first, and every other consumer
  gets a zero-copy share of the same frame. Each consumer gets the newest frame it has not had
  yet, so a slow consumer skips frames instead of building a backlog (counted as
  `CaptureQueueEviction` in its health report) and never holds the others up.
- **Lifetime:** dropping a consumer releases its queued frames; the capture stops when the last
  one goes.

On a CM5:

| Consumers | Frames | CPU |
|---|---|---|
| C270 1280x960 MJPEG: 320x180 luma detector + raw MJPEG | 15 fps each (the camera's rate) | 1.7% |
| OV9782: luma at 320x180 + full-size luma | 320x200 (second output) and 1280x800, 30 fps each, same exposure | 1.2% |

Three identical 320x180 luma consumers of the C270 fixture take 1.0 ms per frame together
(one decode), and use 377 KB of heap against 319 KB for one consumer.

## Stopping idle cameras

`FramePlan::stop_when_idle(duration)` (or `SharedFramePlan::stop_when_idle`, or
`StyxConfig::stop_when_idle` for a plain capture) stops streaming once nobody has asked for a
frame for that long. The next request starts the camera again, with the same mode and controls.
`pause_when_idle` keeps a libcamera camera configured instead, so it starts again in ~0.1 s
rather than ~1.4 s, at the cost of keeping its buffers. See
[reconnect.md](reconnect.md#stopping-idle-cameras).

## Encoded output

A consumer may ask for compressed frames the camera does not produce
(`FrameRequirements::formats([FourCc::H264])`, also H.265 and MJPEG). The planner adds an encode
step, preferring hardware encoders (VA-API, V4L2 mem2mem) to libx264/libx265, and a decode step
first for MJPEG cameras. On a CM5, OV9782 NV12 at 640x360 encodes in ~3.7 ms per frame with
libx264 at low latency. Each plan (or group of shared consumers asking for the same stream) gets
an encoder of its own; `FrameMeta::delta` marks inter-coded packets, and
`PlannedFrames::request_keyframe` asks for an IDR frame with the stream headers. Pyramids are
refused for encoded output.

## Async

With the `async` feature, `PlannedFrames::next_frame_async` awaits the next frame on Tokio,
also on a shared capture. The frame is prepared (decoded, scaled) on the awaiting task, as
`MediaPipeline::next_async_receive` does; heavy plans belong on a blocking task.

## Other processes

`styx::ipc::CameraService` serves a camera to other processes: each asks for the frames it
needs, and the service plans one shared capture for all of them. `FramePlan::exportable` (and
`SharedFramePlan::exportable`) decode into memfds so frames reach other processes without
copying. See [frame-server.md](frame-server.md).

`FramePlan::capture_into(CaptureBuffers)` goes the other way: the caller owns a fixed set of
buffers (memfds or dma-bufs, e.g. a PipeWire node's pool) and the camera captures into them, when
the plan passes the camera's frames through unchanged and the backend imports buffers (V4L2
`V4L2_MEMORY_DMABUF`, the virtual camera). `CaptureBuffers::index_of` names the buffer a frame is
in; a buffer goes back to the camera when its frame is dropped. Elsewhere frames come in the
capture's own buffers and `CaptureBuffers::in_use` stays false. See
[ecosystem.md](ecosystem.md#buffers-and-zero-copy).

## Region of interest

`FrameRequirements::roi` sets an initial region; `PlannedFrames::roi()` returns a handle to
change it per frame. Regions are full-frame pixel coordinates of the capture, also when frames
are decoded at a smaller output size.

- **ISP and raw paths:** frames become zero-copy crop views. The region's left edge is moved down
  to the stride alignment so rows stay aligned.
- **MJPEG:** the decoder decodes only the region. Rows below it are skipped entirely; rows above
  must still be entropy-decoded.
- **Companions:** pyramid companions are cropped to the same region at their scale.
- **Mapping back:** `FrameMeta::crop` gives each view's position in the full frame.

## Limits

- Hardware decode paths other than the Raspberry Pi ISP (Rockchip MPP, VA-API, Jetson) are
  implemented but not yet validated on hardware.
- A consumer sharing its preparation gets its region cropped from the full decoded frame; alone,
  an MJPEG decode skips the rows below the region.
