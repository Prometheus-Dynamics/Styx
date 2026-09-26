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
| libcamera | `Boottime` | start of exposure (`SensorTimestamp`); pyramid companions share it |
| V4L2 | `Monotonic` when the driver flags it, otherwise `clock` is `None` | driver capture time (start of frame for UVC) |
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
- File and simulation sources always report media time, so seeking and replay stay
  meaningful.

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
