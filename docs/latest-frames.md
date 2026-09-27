# Latest-Frame Delivery

A camera produces frames at a fixed rate. When the consumer is slower, frames have to be dropped
somewhere. Styx controls where, and therefore how old the frames you receive are.

```rust
use styx::prelude::*;

// Newest frame only: best for detection, tracking and control loops.
let config = StyxConfig::new().latest_frame_only();

// Or tune it directly.
let config = StyxConfig::new()
    .capture_queue_depth(2)
    .capture_queue_overflow(QueueOverflow::DropOldest) // the default
    .capture_extra_buffers(2);                         // the default
```

- **`CaptureConfig::queue_overflow`**
  - `DropOldest` (default): a frame arriving at a full queue replaces the oldest queued frame.
    The capture worker never blocks. Replaced frames are counted as
    `FrameDropReason::CaptureQueueEviction`.
  - `Backpressure`: the worker waits up to `queue_send_timeout_ms` for room, then drops the
    arriving frame.
- **`CaptureConfig::extra_buffers`:** device buffers (libcamera, V4L2) allocated beyond the
  queue depth. The headroom covers one frame held by the consumer and one in flight with the
  driver. On a CM5, 2 gives the same frame age as 3 with a consumer taking 100 ms per frame;
  with 1, libcamera falls back to stale frames (474 ms). Without it, a full queue leaves libcamera with no requests. It then fills the next
  request from its backlog of old raw frames, which is what made frames hundreds of milliseconds
  old.
- **`latest_frame_only()`:** a queue depth of 1 with `DropOldest`. The planner uses this for
  `Priority::Latency`.

The defaults (queue depth 2, 2 extra buffers) use 4 device buffers, and `latest_frame_only()`
uses 3. Buffers are the main memory cost of a capture. Each 1280x720 NV12 libcamera buffer is
1.4 MB, and libcamera's own internal buffers grow with the request count too (15.3 MB at 4
requests, 19.3 MB at 7).

## Before and after

Measured on a Raspberry Pi CM5 at 1280x720. "Age" is sensor timestamp to `recv`, p50 / p95. The
consumer spins for the given time on each frame.

**OV9782, libcamera, 30 fps**

| Consumer work | Before (depth 4, backpressure, no headroom) | Drop-oldest at depth 4 | `latest_frame_only()` |
|---|---|---|---|
| 0 ms | 8.0 / 8.5 ms | 8.1 / 8.3 ms | 8.0 / 8.2 ms |
| 50 ms | 324 / 341 ms | 125 / 141 ms | **25 / 41 ms** |
| 100 ms | 674 / 675 ms | 141 / 141 ms | **41 / 41 ms** |

**Logitech C270, V4L2 MJPEG, about 11 fps in office light**

| Consumer work | Before | Drop-oldest at depth 4 | `latest_frame_only()` |
|---|---|---|---|
| 0 ms | 68 / 68 ms | 68 / 68 ms | 68 / 68 ms |
| 100 ms | 236 / 272 ms | 276 / 308 ms | **100 / 108 ms** |

- **Fast consumer:** nothing changes. The C270's 68 ms is the camera's own exposure and transfer
  time.
- **Slow consumer:** delivered frame rates are the same in every configuration. Only the frame
  age changes.
- **Queue depth:** a deeper queue always holds that many older frames. On the C270 at depth 4,
  the new default is slightly worse than before. Previously the driver dropped frames once its
  4 buffers were full; now the extra buffers keep 4 queued frames. Use `latest_frame_only()`
  when frame age matters.
- **Planner:** at `Priority::Latency` it matches `latest_frame_only()` (OV9782: 41 ms, C270:
  102 ms at 100 ms of work).
