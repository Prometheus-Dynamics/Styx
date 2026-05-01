# Backend Loop Stop Audit

Rust backend worker loops reviewed for release readiness. Non-Rust FFI internals are out of scope.

| Backend | Stop path | Bounded waits | Runtime observability |
| --- | --- | --- | --- |
| `netcam` | `CaptureHandle` sends stop through a watcher that flips an `AtomicBool`; sync, async, MJPEG, and FFmpeg paths check it. | HTTP request/connect/read timeouts, interruptible retry backoff, stop-poll sleeps, async `select!` around request start, FFmpeg interrupt callback. | `HealthReport.capture_shutdown`, `capture_retries`, and `last_error()`. |
| `v4l2` | Worker checks stop channel every dequeue loop and stops the stream before exit. | mmap dequeue uses configurable poll timeout; enqueue wait and error backoff are configurable. | `HealthReport.capture_shutdown`, queue stats, memory stats, V4L2 frame metadata. |
| `libcamera` | Worker checks stop channel on every completed-request timeout and marks the backing shutdown flag. | camera lookup, request polling, requeue stall, queue send, idle drain, and control response waits are configurable. | `HealthReport.capture_shutdown`, control errors, external backing stats, worker errors. |
| `file` | Worker checks stop channel between files, frames, image durations, and video frame delays. | frame delay waits are interruptible; enqueue wait is configurable; video stop waits are covered by a unit test. | `HealthReport.capture_shutdown`, queue stats, control state, file/video worker debug logs. |

Current release conclusion: all long-running Rust backend loops have an explicit stop path, avoid unbounded sleeps in normal operation, and expose delayed shutdown through the common capture health report.
