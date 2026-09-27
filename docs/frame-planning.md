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

On a Raspberry Pi CM5, Logitech C270 MJPEG at 1280x720 delivered at 320x180:

| Output | Full size | 320x180 |
|---|---|---|
| Luma | 0.87 ms, 3.95 MB | 0.64 ms, 2.42 MB |
| RGB | 6.03 ms, 12.75 MB | 1.97 ms, 2.34 MB |

Decode time per frame (p50) and the process's memory while capturing (PSS). Scaled decodes of
frames with restart markers are split across cores like full-size ones. The full-size RGB plan
decodes with FFmpeg, whose libraries add to its memory; asking for a smaller size selects a
decoder that can scale.

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

- One consumer per camera. `plan_many` returns `MultipleConsumersUnsupported` for more than one
  set of requirements. Shared captures with per-consumer branches (e.g. Y8 for a detector plus
  MJPEG for a recorder) are the planned extension; the requirement types are already
  per-consumer.
- Hardware decode paths other than the Raspberry Pi ISP (Rockchip MPP, VA-API, Jetson) are
  implemented but not yet validated on hardware.
- Planned frames are for in-process consumers; the pipeline does not export them as memfd or
  dma-buf for other processes.
