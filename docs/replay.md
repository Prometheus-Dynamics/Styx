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

## Playback

A replay capture plays like a video, with controls (`capture_api::replay_controls::ids`, also
listed and set through a camera service, so a `CameraService` of a replay device is a camera
clients bind to as to a live one):

| Control | Value |
|---|---|
| `replay_playing` | `Bool`: play or pause |
| `replay_speed` | `Float`: 0.25 to 4 times the recorded rate; 0: every frame as fast as consumers take them |
| `replay_loop` | `Bool` |
| `replay_seek_ms`, `replay_seek_frame` | `Uint` (write): the frame there comes next, also while paused |
| `replay_step` | write: one frame, then paused |
| `replay_position_ms`, `replay_frame` | `Uint` (read): the last frame delivered |
| `replay_duration_ms`, `replay_frame_count` | `Uint` (read): found by a scan at start (0 until then) |
| `replay_state` | menu (read): playing, paused, ended |

`ReplaySourceConfig::hold_at_end(true)` keeps the capture open at the end (paused there, ended,
for a seek), as a player wants; without it the capture ends and consumers see `Closed`.
`.timestamps(ReplayTimestamps::Rebased)` gives each frame its delivery time on
`CLOCK_MONOTONIC` (consistent with live frames, pauses, seeks and the speed) instead of the
recorded time. Either way the recorded per-frame metadata (`NativeFrameMeta` and its 3A record)
is kept. Seeking reads the recording from its start up to the frame.

### Raw recordings as processed frames

A recording of raw sensor frames (`Frames::sensor_raw()`: `BYR2`, `RG16`, ..., or packed `pBAA`)
replays as the camera would have processed it with `ReplaySourceConfig::soft_isp("ov9782")`
(feature `native`): each frame goes through the software ISP with the sensor's description and
tuning, at the frame's recorded exposure, gains and white balance (`NativeAaa::colour_gains`; AE
and AWB do not run), and comes out in NV12 with its recorded metadata, the 3A record included.
The replay device then offers an NV12 mode, so gray, NV12 and pyramids are planned as for the
live camera. Other recordings replay as they are.

## Recording on a device

`StreamRecorder::mcap(path, &header, RecorderOptions)` is for recording alongside a live
system:

- **Writer thread.** Each frame is copied and encoded on the caller's thread (no lease is
  held while the disk catches up), then written from a background thread.
- **Bounded queue.** `queue_bytes` (default 64 MiB) caps what waits for the disk. When it is
  full, frames and host messages are dropped instead of blocking the caller, and counted:
  `stats()` gives frames and messages written and dropped, bytes, the deepest the queue got,
  and the writer's time. Channels, metadata and attachments are never dropped. `queue_bytes: 0`
  writes on the caller's thread instead.
- **Chunk compression.** `RecordCompression::Lz4` (feature `replay-mcap-lz4`) or
  `::Zstd { level }` (feature `replay-mcap-zstd`). Each builds a C library, so a C compiler for
  the target is needed (for `scripts/cross-aarch64.sh --musl`, a musl one in
  `CC_aarch64_unknown_linux_musl`). Builds without these features do not grow. Replay reads
  compressed chunks with the same feature.
- **The host's own data.**
  - `add_channel(topic, schema, message_encoding, metadata)` returns a `ChannelId`, and
    `write(channel, log_time_ns, bytes)` writes to it. Use it for IMU samples, detections as
    JSON, and so on.
  - `add_metadata(name, map)` adds a metadata record, e.g. for calibration or a camera mount.
  - `add_attachment(name, media_type, log_time_ns, bytes)` stores a file whole, e.g. a field
    layout.
  - Replay skips channels it does not know.
- **Clocks.** `/styx/clock` (`styx/msg/ClockSample`) holds the monotonic, boot-time and
  real-time clocks read together, at the start, every `clock_period` (default 1 s) and at the
  end. Use it to put frames (native cameras stamp `CLOCK_MONOTONIC`) and data stamped on
  another clock (e.g. `CLOCK_BOOTTIME`) on one time line offline.

`examples/04_performance/record_bench.rs` measures what a device sustains: it records a camera
service's frames at several rates and compressions.

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
| `/styx/frame_meta` | `styx/msg/FrameMeta` | One message per image message: exact format, clock, backend sequence, crop and timing; for native cameras the exposure, gains and frame timing, the end of frame, and the 3A record (white balance gains, colour temperature, lux, AE/AWB state, flicker, lens). The definition is embedded in the file, so Foxglove shows the fields. |

- Messages of one frame share its MCAP `sequence` (the frame index) and `log_time` (the frame
  timestamp).
- A metadata record, `styx.recording`, holds the device identity, backend, format and interval.
- Compression is off unless `RecorderOptions::compression` asks for it.
- `/styx/clock` and the host's channels (above), when used.
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
