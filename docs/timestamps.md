# Timestamps and Clocks

Every frame carries `FrameMeta::timestamp` (nanoseconds) and `FrameMeta::clock`, the clock that
timestamp is expressed in.

| Clock | Meaning |
|---|---|
| `Monotonic` | `CLOCK_MONOTONIC`: steady; stops during suspend. Same clock as `std::time::Instant`. |
| `Boottime` | `CLOCK_BOOTTIME`: steady; keeps counting through suspend. |
| `Realtime` | `CLOCK_REALTIME`: wall clock (Unix epoch); can jump when the time is set. |
| `StreamRelative` | Time since the stream started. Used for files, simulation, and live sources without a clock. |

## What each backend reports

| Backend | Native clock | Timestamp marks |
|---|---|---|
| libcamera | `Boottime` | `SensorTimestamp`, which libcamera documents as the start of exposure; on Raspberry Pi pipelines it is the CSI-2 receiver's buffer timestamp, the start of frame (`pipeline/rpi/pisp/pisp.cpp`: `sensorTimestamp = buffer->metadata().timestamp`), as for native; pyramid companions share it |
| V4L2 | `Monotonic` when the driver flags it, otherwise `clock` is `None` | driver capture time (start of frame for UVC) |
| native | `Monotonic` (converted when `timestamp_clock` asks) | the V4L2 buffer timestamp, `CLOCK_MONOTONIC` from the kernel: start of frame on `rp1-cfe` (measured: 10.9 ms before dequeue at 30 fps); the PiSP and software ISP outputs carry the raw frame's timestamp |
| netcam, virtual | `StreamRelative` | arrival in Styx |
| file, simulation | `StreamRelative` | media time (presentation time or frame index / fps) |

## Choosing a clock

```rust
use styx::prelude::*;

let config = StyxConfig::new().timestamp_clock(ClockSource::Monotonic);
```

- `ClockSource::Native` (the default) keeps each backend's clock.
- `Monotonic`, `Boottime` and `Realtime` convert live sources (libcamera, V4L2, netcam, virtual)
  into that clock.
  - libcamera and V4L2 frames are shifted by the current offset between the two clocks, sampled
    once per frame, so a frame and its companions stay equal.
  - Sources without their own clock (netcam, virtual) are stamped with that clock when the
    frame arrives.
  - Native frames are `CLOCK_MONOTONIC` from the kernel and converted the same way as V4L2 frames,
    raw and processed (PiSP and software ISP) alike.
- The native backend's buffer timestamps mark the start of the frame on `rp1-cfe`. Measured on a
  CM5 at 30 fps: the raw buffer dequeues 10.9 ms after its timestamp, which is the sensor's
  readout time. Other receivers may stamp the end of the frame; check before trusting a latency
  figure built on the timestamp from one.
- File and simulation sources always report media time, so seeking and replay stay
  meaningful.

## When the exposure happened

`FrameMeta::timestamp` on the native and libcamera backends is the receiver's start of frame:
the first line arriving over CSI-2, right after the exposure ends. To line a frame up with
other sensors (odometry, an IMU), use the middle of its exposure instead:

```rust
let mid = frame.meta().exposure_midpoint_in(TimestampClock::Monotonic); // Option<u64>
```

`FrameMeta::exposure_midpoint()` is `timestamp - exposure / 2`, with the exposure the frame was
actually taken with (`NativeFrameMeta::exposure_ns`, libcamera's per-frame `ExposureTime`), on
the frame's clock; `exposure_midpoint_in(clock)` converts it like `timestamp_in`. It is `None`
when the backend does not report the exposure per frame (V4L2, UVC) or the timestamp is
stream-relative.

- **Global shutter** (the OV9782): every row is exposed at once, so this is the frame's
  exposure midpoint. Checked on the CM5: a native frame's age on arrival does not move with the
  exposure (9.56 ms p50 at 1, 8 and 20 ms, 1280x800 at 30 fps), so the timestamp is after the
  exposure, not before it.
- **Rolling shutter** (IMX219, IMX477, IMX708, OV5647): it is the first row's exposure midpoint;
  row `r` is `r` line times later.
- Any fixed delay between the end of exposure and the receiver's frame start (the sensor's own
  frame-valid delay) is not subtracted: the OV9282 driver and the documentation Styx has give
  none, and it is at most a few line times (9.1 µs each on the OV9782).

The same applies to latency: `now - timestamp` measures from the end of the exposure; from the
midpoint add half the exposure (4 ms at 8 ms).

To compare a single frame against another clock, use `FrameMeta::timestamp_in(clock)`. It returns
`None` for stream-relative timestamps. `FrameMeta::latency()` reports how old a frame is,
whatever its clock.

## Dropped frames

`HealthReport::drop_reasons` includes `FrameDropReason::SensorSequenceGap`. This counts frames
the sensor produced that never reached Styx, for example because the consumer fell behind and
the driver or libcamera had no free buffer.

- **V4L2:** gaps in the driver's frame sequence numbers.
- **libcamera:** sequence numbers count completed requests, not sensor frames (on a Raspberry
  Pi they stay consecutive across a 1.3 s stall). Styx instead compares consecutive sensor
  timestamps with the `FrameDuration` the sensor reported. If `FrameDuration` is missing, it
  falls back to sequence numbers.
