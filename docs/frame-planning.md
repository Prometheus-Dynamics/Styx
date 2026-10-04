# Frame Planning

A consumer says what frames it wants with `Frames` (a `FrameRequest`); `styx::planner` turns
that into a concrete capture, decode and preparation plan for a camera, explains it, and runs
it.

```rust
use std::time::Duration;
use styx::prelude::*;

let mut frames = Frames::nv12()              // or rgb(), gray(), formats([..]), any()
    .size(1280, 800)                         // the size you work at; never upscaled
    .fps(30)                                 // exactly 30 fps
    .open_best(&probe_all())?;               // or .open(&camera) for one probed camera
println!("{}", frames.plan());               // steps, where they run, costs, rejected options

while let RecvOutcome::Data(frame) = frames.next_frame(Duration::from_millis(500)) {
    let meta = frame.meta();                 // timestamp, sequence, exposure and gain
    let latency = meta.latency();            // sensor -> now breakdown
    // ...
}
# Ok::<(), Box<dyn std::error::Error>>(())
```

`camera.frames().nv12().size(1280, 800).fps(30).open()?` is the same for one probed camera.
`request.plan(&camera)?` (or `plan_best`) returns the `FramePlan` without starting it, to print
or adjust (`config`, `stop_when_idle`, `exportable`, `capture_into`) before `start()`.

## What you can ask for

Each choice has one meaning.

| Choice | Methods | Meaning |
|---|---|---|
| Format | `Frames::nv12()`, `rgb()` (RGB24), `gray()` (8-bit luma), `formats([..])`, `any()` | the pixel format delivered; `formats` lists acceptable ones, most preferred first; `any` takes the camera's own |
| Size | `size(w, h)` | the size you work at: the smallest mode covering it, scaled down by the ISP or the JPEG decoder where they can; never upscaled |
| | `size_at_least(w, h)`, `size_at_most(w, h)` | bounds on the capture mode |
| Frame rate | none | the camera's default: 30 fps (`planner::DEFAULT_FPS`) on a camera that runs at any rate in a range (a sensor Styx drives), clamped to its range; on a camera with a list of rates (USB), the listed rate closest to 30 |
| | `fps(x)` | exactly `x`: within a range, `x` itself; on a list, a listed rate within 1% (29.97 counts as 30). Planning fails with `PlanError::FrameRate`, naming the rates the modes have, when none can |
| | `fps_at_least(x)` | the fastest rate of the chosen mode, which must reach `x` |
| | `fps_between(a, b)` | the fastest rate from `a` to `b` |
| Delivery | `latest()` (default) | the newest frame only: a frame not taken before the next arrives is dropped; the lowest latency |
| | `every_frame(n)` | frames queue up to `n` while the consumer is busy; any dropped beyond that are counted (`Frames::dropped`, the health report) |

Advanced, for consumers with special needs (a feature detector, SIMD code):

| Method | Meaning |
|---|---|
| `pyramid(levels)`, `pyramid_source(..)` | ½, ¼, ... companions with each frame (`frame.pyramid_level(n)`), the first from the ISP's second output where there is one |
| `roi(rect)` | deliver only this region (full-frame pixels); `Frames::roi()` changes it per frame. A native camera's PiSP crops it at full resolution in any format; elsewhere luma frames are views of it |
| `regions([rect, ..])` | several regions (up to 16): the first is the frame, the others its `frame.region(i)` companions, all from the same capture; `Frames::roi().set_regions(..)` moves them |
| `skip_stale_regions()` | skip frames the ISP cropped before a region last moved, instead of delivering them |
| `overview(w, h)` | the whole frame at about `w`x`h` with every frame (`frame.overview()`), to find the next region in; from the PiSP's second output where there is one, else box-filtered on the CPU |
| `row_alignment(bytes)` | rows (and the buffer start) aligned, e.g. 64 for SIMD loads; copied only when the camera's rows are not |

```rust
let mut frames = Frames::gray()
    .size(1280, 720)
    .pyramid(2)                              // ½ and ¼ companions
    .row_alignment(64)
    .open(&camera)?;
let roi = frames.roi();                      // change the region of interest while running
if let RecvOutcome::Data(frame) = frames.next_frame(Duration::from_millis(500)) {
    let rows = frame.luma_rows()?;           // Y8, 64-byte aligned
    let quarter = frame.pyramid_level(2);    // same timestamp as `frame`
    let origin = frame.meta().crop;          // where an ROI view sits in the full frame
    roi.set(Some(FrameRect::new(320, 200, 640, 400)));
}
```

Route control, only when the planner's choice is not wanted: `backend(BackendKind::V4l2)`,
`hardware(Hardware::Auto | Required | Off)`, `decoder("turbojpeg-luma")`, `forbid("ffmpeg")`,
`decode_threads(n)`.

The frames returned (`Frames`) are the running stream: `next_frame`, `next_frame_async`, the
iterator, `set_control` / `get_control` (exposure, gain, white balance, ...), `capture()` for the
camera's `CaptureHandle`, `plan()`, `roi()`, `dropped()`, `health_report()`.

## What the planner considers

For every camera, backend and mode it builds the cheapest route to the requested format:

| Capture format | Route (gray output) | Pyramid |
|---|---|---|
| GREY / R8 | direct (libcamera colour sensors deliver the ISP's Y plane zero-copy) | ISP second output (Raspberry Pi), else box filter |
| NV12, YUV420, NV16, ... | zero-copy Y-plane view | ISP second output (Raspberry Pi), else box filter |
| MJPEG | hardware Y8 decoder if one opened at runtime, else turbojpeg-luma (multi-core with restart markers) | box filter |
| YUYV | luma copy | box filter |
| H.264 / H.265 | FFmpeg hardware Y8 decoder if available | box filter |
| RGB | luma conversion | box filter |

Candidates are filtered by the size bounds, the frame rate and the route choices, then ranked:

1. size: the smallest mode at or above `size_at_least` when it is set, else the route
   delivering the smallest frames that cover `size` when that is set, otherwise the largest mode;
2. cost: CPU milliseconds plus half the latency milliseconds per frame. A hardware block (ISP,
   hardware decoder) that adds a few milliseconds but almost no CPU beats the same work on the
   CPU; between CPU routes the faster one wins. Every plan prints its cost and this rule;
3. frame rate: the faster mode;
4. V4L2 over libcamera's UVC pipeline for the same USB camera.

A mode is rejected for its rate only when it could deliver the frames otherwise, so a
`PlanError::FrameRate` lists the rates of modes that would have worked. Costs come from
measurements on a Raspberry Pi CM5; absolute numbers differ by machine but the ranking holds.

Delivery sets the queue between capture and consumer (`latest`: 1, newest wins;
`every_frame(n)`: `n`) and decode threads per frame (`latest`: automatic, up to four cores per
JPEG with restart markers; `every_frame`: one), unless `decode_threads` says otherwise.

## Route control and hardware

- `backend(kind)`: only consider this backend (`BackendKind::V4l2`, `Native`, `Uvc`, ...). The
  userspace UVC backend is only considered when asked for or when nothing else has the camera.
- `decoder(name)`: use exactly this decoder implementation (e.g. `"turbojpeg-luma"`).
- `forbid(name)`: never use this decoder or encoder implementation.
- `hardware(..)`:
  - `Hardware::Auto` (default) uses hardware decoders whose Cargo feature is enabled, whose
    device exists and which open. The device check needs no FFmpeg; opening happens on the
    first lookup of that input format.
  - `Hardware::Off` keeps everything on the CPU (no ISP scaling either).
  - `Hardware::Required` fails planning for CPU-only paths.
- `PyramidSource`:
  - `PreferHardware` (default) takes level 1 from the ISP's second output where possible and
    computes deeper levels with box filters.
  - `HardwareOnly` fails without an ISP output. The ISP provides a single level.
  - `Software` always uses box filters.

## The previous request

`FrameRequirements` and `Priority` (styx-core) are deprecated and kept for one release: every
planner function and `FrameClient::request` still take them, converted with
`FrameRequest::from`. `Priority` mixed three things: route scoring (now one score for all),
queue depth (now delivery) and what `min_fps` meant (now the rate methods). The conversion
keeps the meaning: `Latency` → `latest()`, `Throughput` → `every_frame(4)`, `Power` →
`every_frame(3)` (each with the decode threads it had); `min_fps(x)` → `fps_at_least(x)`, or
`fps(x)` with `Priority::Power`; `output_resolution` / `min_resolution` / `max_resolution` →
`size` / `size_at_least` / `size_at_most`; `stride_alignment` → `row_alignment`; the overrides →
the route methods. Two things differ: on a camera with a list of rates, `Power` + `min_fps(x)`
took the slowest listed rate at or above `x` and is now exactly `x` (an error when not listed;
use `fps_at_least` for any rate above); and with no rate a list-rate camera runs at the listed
rate closest to 30 instead of its fastest.

## Output size

`size(w, h)` is the size the consumer works at, for example
320x180 for a detector that would downsize anyway. The planner then prefers the smallest mode
that covers it, and MJPEG decoded with turbojpeg (luma or RGB) is decoded straight to ½, ¼ or ⅛
size in the DCT domain: the smallest of those that still covers the request. Frames are never
upscaled. `FramePlan::output_resolution()` gives the size frames arrive at, and the plan says
when its route cannot scale.

## What arrives, and strict requests

`FramePlan::delivered()` says what the frames are before the first one arrives: format, size,
frame rate, pyramid levels (and the deepest one the ISP makes; the rest are box-filtered on the
CPU), whether they are inter-coded packets, and `unmet`: the parts of the request the plan does
not meet (`Unmet::Size { wanted, delivered }` when no route scales to the size asked for, so
frames arrive larger in both dimensions; covering it in one dimension at the camera's aspect
ratio is meeting it). Consumers can size buffers and regions once, at plan time.

By default a plan delivers the nearest it can and reports what it missed. `.strict()` refuses
instead: planning skips candidates that would miss part of the request and fails, naming what
they would miss, when none meets it. On a shared capture a strict consumer also refuses a setup
another consumer would force on it.

On a Raspberry Pi the ISP scales instead (libcamera's output size; a `scale` step in the plan).
It keeps the mode's aspect ratio so the field of view is not cropped: 320x180 from a 16:10
OV9782 arrives at 320x200. Modes are ranked by the size of the frames they deliver, so a wide
mode scaled by the ISP beats a smaller 4:3 mode. On a CM5, OV9782 luma at 320x200 runs at
30 fps with 1.5% CPU, delivered straight from the ISP. `Hardware::Off` turns ISP
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
grey frames for a detector and MJPEG for a recorder. It returns a `SharedFramePlan`: the mode and
interval every consumer can use, plus a `FramePlan` per consumer. `start()` returns `Frames`
per consumer, each prepared its own way (decoder, pyramid, ROI, output size) and delivered its
own way (`latest` or `every_frame(n)`).

```rust
use styx::planner::plan_many;

let plan = plan_many(&device, &[
    Frames::gray().size(320, 180),
    Frames::formats([FourCc::MJPG]).every_frame(8),
])?;
println!("{plan}");
let mut consumers = plan.start()?;
let recorder = consumers.pop().unwrap();
let detector = consumers.pop().unwrap();
```

- **Mode:** the smallest mode that covers every consumer's size (the largest if any consumer
  states none); then the lowest combined cost.
- **Rate:** one for the capture, the rate every consumer accepts: an exact rate if one asks for
  it (two different exact rates cannot share a capture: `PlanError::FrameRate`), else the
  fastest within every consumer's bounds, else the camera's default.
- **Two ISP sizes (Raspberry Pi):** the ISP has two outputs. Consumers wanting two different
  sizes get the larger from the main output and the smaller from the second, both scaled in
  hardware from the same exposure. With a third size, or when an ISP pyramid uses the second
  output, the ISP delivers the mode's size and those consumers scale on their own.
- **Regions:** each consumer's regions of interest come from the one PiSP: the main output's
  crop, the second output's, extra back end passes, or views of the whole frame (see
  [Region of interest](#region-of-interest)).
- **Decoding once:** consumers with the same request apart from the region of interest and the
  rate share their preparation: each frame is decoded (and its pyramid built) once, and each
  consumer's region is a zero-copy crop of the shared result. The plan notes which consumers
  share.
- **Frames:** the capture is read by whichever consumer asks first, and every other consumer
  gets a zero-copy share of the same frame. A `latest` consumer gets the newest frame it has not
  had yet, so a slow one skips frames instead of building a backlog (counted as
  `CaptureQueueEviction` in its health report, `Frames::dropped`) and never holds the others
  up; an `every_frame(n)` consumer has up to `n` waiting (each holding a camera buffer).
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
(`Frames::formats([FourCc::H264])`, also H.265 and MJPEG). The planner adds an encode
step, preferring hardware encoders (VA-API, V4L2 mem2mem) to libx264/libx265, and a decode step
first for MJPEG cameras. On a CM5, OV9782 NV12 at 640x360 encodes in ~3.7 ms per frame with
libx264 at low latency. Each plan (or group of shared consumers asking for the same stream) gets
an encoder of its own; `FrameMeta::delta` marks inter-coded packets, and
`Frames::request_keyframe` asks for an IDR frame with the stream headers. Pyramids are
refused for encoded output.

## Async

With the `async` feature, `Frames::next_frame_async` awaits the next frame on Tokio,
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

`FrameRequest::roi` sets an initial region; `Frames::roi()` returns a handle to change it per
frame (`FrameClient::set_roi` through a camera service). `FrameRequest::regions([..])` asks for
several (up to 16, `MAX_REGIONS`): region 0 is the frame itself (`roi`), regions 1, 2, ... come
with it as `CompanionKind::Region { index }` companions (`frame.region(i)`, `frame.regions()`),
cut from the same capture, so they share its timestamp and metadata. `RoiHandle::set_regions`
/ `set_region(i, ..)` (`FrameClient::set_regions`) move them while running; `None` for a region
after the first means none (no companion). Regions are full-frame pixel coordinates of the
capture, also when frames are decoded at a smaller output size, and every frame or region says
where it is (`FrameMeta::crop`). `Delivered::regions` says how each is made (`Delivered::roi` is
region 0); a region a route cannot apply is unmet (`Unmet::Roi`: NV12 or RGB frames without a
PiSP to crop them, or more regions than the capture has slots for).

How regions are made (`RoiCrop`):

- **`Isp`, a native camera's PiSP in the frame's own pass:** the back end's main output is the
  region at full resolution, in any format, rounded out to even pixels (at least 16x16): frames
  are the region's size, so nothing outside it is processed further or handed on, and the pass
  reads only the input around it (two 128x128 crops side by side: 0.27 ms of back end time
  against 2.26 ms for the whole 1280x800 frame, with temporal denoise). The second output can crop another region the same way when the main output is the
  whole frame (its pass then covers the region already: free).
- **`IspPass`, an extra back end pass over the same raw frame:** the back end works memory to
  memory, so after the frame's pass it runs again over the raw frame (still held) for each
  further region, into the main output's buffers (`NativeIspConfig::regions`). Measured on the
  CM5 (OV9782 1280x800, 30 fps): 0.03 ms of back end time per pass plus 1.35 ns per pixel of the
  region (128x128: 0.05 ms, 640x400: 0.41 ms, the whole frame 1.39 ms), on the frame's path
  (the frame comes with all its regions), and about 0.03 ms of CPU per pass. Through the planner
  (`native_regions bench`, regions moving 2 px every frame, one gray consumer): 1 region 0.294 ms
  of CPU per frame and 8.94 ms from frame start to delivery, 2 regions 0.300 ms / 9.01 ms, 4
  regions 0.385 ms / 9.14 ms. The old fallback, views of a whole frame a viewer shares, costs
  0.27-0.28 ms and 9.56 ms (the frame's pass processes the whole frame). Each pass's region
  against the same pixels of a
  viewer's frame from the same capture: mean |difference| 0.2 levels (temporal denoise, below);
  the second output's crop and views are identical.
- **`View`:** luma frames become zero-copy crop views of the captured (or decoded) frame. The
  region's left edge is moved down to the stride alignment so rows stay aligned. An MJPEG
  decode skips the rows below region 0 (rows above must still be entropy-decoded) unless other
  regions or the overview need the whole frame.

The planner prices the choices ([`cost`](../crates/styx/src/planner/cost.rs) `pisp_pass`): a
consumer alone on its capture gets region 0 as the main output's crop and the others as extra
passes. On a shared capture (or a camera service with several clients):

- when no consumer needs the whole frame (only trackers), the first tracker's region 0 is the
  main output's crop and every other region an extra pass;
- otherwise the main output is the whole frame: luma regions are views of it (free, cheaper than
  a pass), others the second output's crop (when nothing else takes the second output) and
  extra passes, in the main output's format;
- the capture has 15 region slots; regions beyond them fall back to views (luma, whole frame) or
  are unmet;
- moving a region never restarts the capture: the capture's set-up key has where each
  consumer's regions are made, not where they are.

A new region applies from the next frame the ISP processes (about 60 µs to re-plan the back end's
tiles); frames already processed arrive first, one or two after a change.
`FrameRequest::skip_stale_regions()` skips those instead: a frame whose ISP-cropped regions do
not cover the regions as set now is not delivered (views are always current). On the CM5,
regions jumping every 4 frames: 28 of 120 frames showed stale regions without it, none with it
(at 24 instead of 30 frames per second).

Temporal denoise keeps a running average of the frame in the back end; the frame's own pass
reads and writes it. Extra passes read the average the frame's pass just wrote and write
nothing, so the region is denoised as the main output is and the main stream is untouched
(`native-pipeline pisp --passes`, a static 256x256 region, mean squared difference between
consecutive frames: the pass 30.5 against the main output's 30.1 on the same frames; with
temporal denoise off in the pass 34.1 against 29.1; the main output without passes 28.0-28.7,
within the spread between runs). The main pass updates the average only where its tiles
read: a pass outside them (the main output a crop elsewhere) runs without, and a main crop that
jumps mostly outside what the last frame covered starts the average over.

The capture's `OUTPUT_CROP` control (main output) and `region_crop(index)` controls (regions,
`NativeIspConfig::regions`) do the same without the planner (`StyxConfig::native_crop`,
`native_regions` for the first rectangles).

- **Companions:** pyramid companions are cropped to the same region at their scale; overviews
  and other regions are kept whole. A hardware-only pyramid of an ISP-cropped region comes from
  an extra pass of it (one level); otherwise levels are box-filtered from the region (cheaper
  than a pass at any size on the CM5).
- **Mapping back:** `FrameMeta::crop` gives each frame's and region's position in the full
  frame.

### Overview

`FrameRequest::overview(w, h)` attaches the whole frame, scaled to the smallest even size with
its aspect ratio covering `w`x`h`, to every frame as a `CompanionKind::Overview` companion
(`frame.overview()`; crops leave it whole). A tracker crops to the regions around what it found
and searches the overview to find them again, without asking for full frames. On a native PiSP
it comes from the back end's second output in the same pass (`Delivered::hardware_overview`;
one for the capture, the largest any consumer asked for; it takes the second output, so a
pyramid is box-filtered or comes from an extra pass); elsewhere it is the frame's luma
box-filtered down on the CPU (halved while that still covers the size, then an area resize;
0.16 ms per megapixel halved), or the uncropped frame itself for frames without a luma plane.

```rust
let mut frames = Frames::gray()
    .regions([FrameRect::new(480, 260, 128, 128), FrameRect::new(100, 80, 128, 128)])
    .overview(320, 200)
    .skip_stale_regions()
    .open(&camera)?;
let roi = frames.roi();
while let RecvOutcome::Data(frame) = frames.next_frame(Duration::from_millis(500)) {
    for (i, region) in frame.regions() {     // region 0 is `frame` itself
        let at = region.meta().crop;         // where it is in the full frame
        // ... track in `region`
    }
    let overview = frame.overview();         // the whole frame, 320x200
    // ... reacquire in `overview`, then:
    roi.set_regions(&[FrameRect::new(0, 0, 256, 256), FrameRect::new(600, 300, 128, 128)]);
}
```

`examples/04_performance/native_roi.rs` moves one region through a planned stream, a camera
service client and the raw control, and checks each region against the overview. On the CM5
(OV9782 1280x720 at 30 fps) each region applies on the next processed frame and matches the
overview's mean luma within 0.4 levels. `examples/04_performance/native_regions.rs` measures
regions (`bench N`, `views N`), compares their pixels with a viewer's frame from the same
capture (`pixels`), runs two trackers (`trackers`), stale frames (`stale`) and two camera
service clients with two regions each (`service`).

## Limits

- Hardware decode paths other than the Raspberry Pi ISP (Rockchip MPP, VA-API, Jetson) are
  implemented but not yet validated on hardware.
- A consumer sharing its preparation gets its region cropped from the full decoded frame; alone,
  an MJPEG decode skips the rows below the region.
- Extra back end passes run one after another after the frame's pass (each waits for the
  previous); queueing them together would save their wake-ups (~0.03 ms of latency each).
- A region made by an extra pass is in the main output's format, at full resolution: scaled
  regions and other formats are supported by `PassSpec` but not planned yet.
