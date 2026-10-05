# Recording and Replay

`StreamRecorder` writes frames to a recording without loss. The replay backend plays the file
back as a camera. Pipelines, the planner and your own code then run on recorded data
exactly as they would on the live camera. Use it to reproduce a bug, write regression tests
against real footage, or tune a vision pipeline away from the hardware.

Recording needs a format feature; MCAP is the usual one:

```toml
styx = { version = "2.0.0", features = ["replay-mcap"] }
```

```rust
use std::time::Duration;
use styx::prelude::*;

// Record whatever you receive: raw captures, pipeline output or planner frames.
let handle = CaptureRequest::new(&device).start()?;
let mut recorder = StreamRecorder::create("run.mcap", &device, &handle)?;
while let RecvOutcome::Data(frame) = handle.recv_blocking(Duration::from_millis(500)) {
    recorder.record(&frame)?;
}
recorder.finish()?;

// Replay it as a camera.
let replay = CaptureRequest::replay_source(
    ReplaySourceConfig::new("run.mcap")
        .pacing(ReplayPacing::Unpaced)   // or Realtime (default)
        .loop_forever(false),
)?;
let handle = replay.open()?;             // or: Frames::nv12().open(replay.device())
```

For frames whose format differs from the capture mode (pipeline or planner output), create
the recorder with `StreamRecorder::with_header` and the output format.

## Formats

- **MCAP** (`.mcap`, feature `replay-mcap`, opt-in): the open recording format used by
  ROS 2 and Foxglove. Recordings open in the Foxglove app and in the `mcap` CLI, and can be read
  from Python, C++ and other languages.
- **`.styxrec`** (feature `replay-styxrec`, experimental and opt-in): a compact Styx-only
  format. It records about 20% faster, but only Styx can read it. Choose it with
  `StreamRecorder::with_format(path, &header, StreamFormat::Styxrec)`.

Replay detects the format from the file, so `replay_source` and `open_recording` read either.

### MCAP layout

The profile is `ros2`, and messages are CDR-encoded with `ros2msg` schemas:

| Topic | Type | Contents |
|---|---|---|
| `/styx/image` | `sensor_msgs/msg/Image` | Raw frames. `encoding` is the ROS name (`mono8`, `rgb8`, `yuv422_yuy2`, ...) or `styx:<FOURCC>` for formats without one, e.g. NV12. The data is the visible rows, tightly packed, plane after plane. |
| `/styx/image/compressed` | `sensor_msgs/msg/CompressedImage` | Compressed frames (`jpeg`, `h264`, `h265`) as captured. |
| `/styx/pyramid/<level>` | `sensor_msgs/msg/Image` | Pyramid levels. |
| `/styx/frame_meta` | `styx/msg/FrameMeta` | One message per image message: exact format, clock, backend sequence, crop and timing. The definition is embedded in the file, so Foxglove shows the fields. |

- Messages of one frame share its MCAP `sequence` (the frame index) and `log_time` (the frame
  timestamp).
- A metadata record, `styx.recording`, holds the device identity, backend, format and interval.
- Compression is off.
- Checked with Foxglove's reference Python reader and ROS 2 decoder (`mcap`,
  `mcap-ros2-support`): every message in the CM5 recordings decodes, including `FrameMeta`.

### Damaged files

Recordings cut short (the process died, the disk filled) replay up to the last complete frame.
Damaged ones end with an error instead of a crash, and never make the reader allocate much more
than the file holds, which matters on small devices. Each MCAP record is read and bounds-checked
by Styx; the `mcap` crate only parses the few record types Styx uses. `open_recording_reader`
reads a recording from any `Read` (memory, network) with a size bound. The parsers are covered by
corruption tests in CI and by cargo-fuzz targets (`fuzz/`).

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
  last complete frame. MCAP groups messages into chunks; without compression, frames written
  before the cut still read back.

## Measured on a Raspberry Pi CM5

| Recording | Cost to record (MCAP / styxrec) | Size | Replay (both formats) |
|---|---|---|---|
| OV9782 planner output, Y8 1280x720 + ½ and ¼ pyramid levels | 1.12 / 0.91 ms per frame | 1.2 MB/frame | 90/90 frames bit-identical (pixels, companions, timestamps, clock, sequence) |
| C270 MJPEG 1280x720, raw capture | 0.043 / 0.026 ms per frame | 40 KB/frame | 30/30 identical; the planner decodes the replay through turbojpeg |

A real-time replay of a 3.54 s recording took 3.54 s. The planner produced output identical to
the live run from the replayed Y8 recording.

The `.styxrec` layout is documented in `crates/styx/src/replay/styxrec.rs`.
