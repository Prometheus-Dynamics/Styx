# Recording and Replay

`StreamRecorder` writes frames to a `.styxrec` file without loss. The replay backend plays the
file back as a camera. Pipelines, the planner and your own code then run on recorded data
exactly as they would on the live camera. Use it to reproduce a bug, write regression tests
against real footage, or tune a vision pipeline away from the hardware.

```rust
use std::time::Duration;
use styx::prelude::*;

// Record whatever you receive: raw captures, pipeline output or planner frames.
let handle = CaptureRequest::new(&device).start()?;
let mut recorder = StreamRecorder::create("run.styxrec", &device, &handle)?;
while let RecvOutcome::Data(frame) = handle.recv_blocking(Duration::from_millis(500)) {
    recorder.record(&frame)?;
}
recorder.finish()?;

// Replay it as a camera.
let replay = CaptureRequest::replay_source(
    ReplaySourceConfig::new("run.styxrec")
        .pacing(ReplayPacing::Unpaced)   // or Realtime (default)
        .loop_forever(false),
)?;
let handle = replay.open()?;             // or: plan_frames(replay.device(), &requirements)
```

For frames whose format differs from the capture mode (pipeline or planner output), create
the recorder with `StreamRecorder::with_header` and the output format.

## What is kept

| | |
|---|---|
| Pixels | Every plane's visible rows, bit-exact. Strides are not kept: replayed frames are tightly packed, and the planner realigns them if asked to. |
| Compressed frames | The bitstream as captured (MJPEG, H.264, ...). |
| Timestamp and clock | Unchanged, e.g. libcamera's `Boottime` sensor time. |
| Backend metadata | libcamera and V4L2 sequence numbers, V4L2 flags, buffer memory. |
| Crop | `FrameMeta::crop` of ROI views. |
| Timing | `sensor_to_capture` and per-stage durations as recorded. |
| Companions | Pyramid levels, with their own pixels and crop. The planner reuses them instead of recomputing. |

`capture_instant` is the replay time, so latency measured downstream reflects the replay.

## Pacing

- **`Realtime`** (default): frames are delivered at the gaps between their recorded timestamps.
  The capture queue behaves as it does live, so a slow consumer loses frames the same way.
- **`Unpaced`**: every frame, as fast as the consumer takes them. Use it for offline processing
  and tests.
- **Looping:** `loop_forever` starts again at the end. Timestamps keep increasing: each loop
  starts one frame interval after the previous loop's last frame.
- **End of recording:** without looping, queued frames drain and the handle then reports
  `Closed`.
- **Cut-short recordings:** a recording whose process died while recording plays up to its
  last complete frame.

## Measured on a Raspberry Pi CM5

| Recording | Cost to record | Size | Replay |
|---|---|---|---|
| OV9782 planner output, Y8 1280x720 + ½ and ¼ pyramid levels | 0.95 ms/frame | 1.2 MB/frame | 90/90 frames bit-identical (pixels, companions, timestamps, clock, sequence) |
| C270 MJPEG 1280x720, raw capture | 0.025 ms/frame | 43 KB/frame | 30/30 identical; the planner decodes the replay through turbojpeg |

A real-time replay of a 3.54 s recording took 3.54 s. The planner produced output identical to
the live run from the replayed Y8 recording.

## File format

Version 1. A header records the device identity, the source backend, the format and the
interval, followed by one record per frame. The layout is documented in
`crates/styx/src/replay/format.rs`.
