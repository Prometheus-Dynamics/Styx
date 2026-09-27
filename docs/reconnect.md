# Camera Disconnect Recovery

libcamera and V4L2 captures recover on their own when their camera disconnects or stops
delivering frames. The `CaptureHandle`, and any pipeline or planner built on it, stays open.
Frames stop while the camera is gone and resume on the same handle once it is back.

```rust
use styx::prelude::*;

// On by default. Tune or disable per capture:
let config = StyxConfig::new().capture_reconnect(ReconnectPolicy {
    stall_timeout_ms: 2_000,   // restart after this long without frames (0: disconnects only)
    initial_backoff_ms: 100,   // first delay between attempts; doubles...
    max_backoff_ms: 1_000,     // ...up to this
    ..ReconnectPolicy::default()
});
let config = StyxConfig::new().capture_reconnect(ReconnectPolicy::disabled());
```

## How it works

- **Supervision:** a supervisor thread runs the backend capture and feeds the consumer's queue.
  The queue belongs to the handle, so restarting the backend capture adds no queue hop.
- **Detection:**
  - **Reported disconnects:** V4L2 `ENODEV` ends the backend capture with
    `CaptureError::Disconnected`. Before this change, the V4L2 worker retried forever and the
    consumer saw empty receives with no error.
  - **Stalls:** no frames for `stall_timeout_ms`, raised to at least four frame intervals. This
    catches libcamera cameras that disappear, which stop completing requests without an error,
    and hung drivers. Frames the consumer is still holding don't count as a stall: holding
    frames can starve the device of buffers, and restarting would not help.
- **Recovery:**
  - Styx probes again and picks the camera sharing the most identity keys (USB vendor/product,
    bus path, sensor path). The `/dev/video` node may change.
  - It restarts with the same backend, mode, interval, config and controls, including controls
    set with `set_control` since the start.
  - Failed attempts back off from 100 ms to 1 s. A warm probe takes under 1 ms on a CM5.
- **Reporting:** `HealthReport::capture_retries` records:
  - `reconnect_attempts`;
  - `reconnects` (times frames resumed);
  - `last_reconnect_downtime_ms` (last frame before the disconnect to the first frame after);
  - `last_retry_error`.

  `recent_stage_errors` keeps the last disconnect reason. Sequence gaps from replaced backend
  captures are still counted.

Netcam sources have their own reconnect loop and report into the same fields. File, virtual and
simulation sources don't disconnect and are not supervised.

## Stopping idle cameras

A supervised capture can also stop while nobody pulls frames, to free the camera's buffers and
power:

```rust
let config = StyxConfig::new().stop_when_idle(Duration::from_secs(5));
```

- **Stop:** when no receive is in progress and the last one is older than the timeout, the
  supervisor releases queued frames and stops the backend capture. The stall watchdog is
  paused meanwhile.
- **Restart:** the next `recv*` call restarts it on the caller's thread and then waits for a
  frame as usual. If the restart fails, the normal reconnect loop takes over.
- **Controls:** `set_control` while stopped is remembered and applied on restart; `get_control`
  returns the remembered value.
- **Reporting:** `CaptureRetryStats::{idle_stops, idle_resumes}`.

On a CM5:

| Camera | Released while idle | First frame after restart |
|---|---|---|
| C270, V4L2 | all buffers (2.3 MB PSS) | ~0.6 s |
| OV9782, libcamera | 9 MB PSS and 2.5 of 4.4 MB CMA (libcamera keeps the rest) | ~1.4 s |

## Measured on a Raspberry Pi CM5

A Logitech C270 was unbound from USB for 3 s (`/sys/bus/usb/drivers/usb/unbind`, then `bind`):

| Path | Detected by | Frames resumed after rebind | Downtime |
|---|---|---|---|
| V4L2 MJPEG capture | `ENODEV` | ~2 s (camera re-enumeration and first frame) | 5.5 s |
| libcamera (UVC) YUYV capture | stall watchdog | ~1 s | 5.0 s |
| Planner (V4L2 YUYV + luma) | `ENODEV` | ~1 s | 4.4 s |

The OV9782 CSI camera ran for 20 s with no false reconnects.
