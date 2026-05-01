# Runtime Debugging

Use this checklist when a capture or media pipeline behaves unexpectedly at runtime.

## Enable Tracing

Styx emits structured diagnostics through `tracing`. Applications should install a tracing
subscriber at process startup and set an appropriate filter, for example:

```rust
tracing_subscriber::fmt()
    .with_env_filter("styx=debug")
    .init();
```

Useful targets and fields include:

- Capture worker lifecycle: backend name, startup/teardown latency, stop signaling, worker joins.
- Queue pressure: send timeouts, async waits/wakes, queue depth, and dropped frames.
- Codec and pipeline stages: capture receive mode, sync/async worker kind, stage errors.
- External backing state: libcamera/V4L2 backing counts and drain timeouts.
- Control operations: backend, control id, operation, latency, and failure messages.

## Capture Handle State

For a running `CaptureHandle`, inspect:

- `queue_stats()` for queue depth, capacity, send backpressure, send timeouts, receive timeouts,
  and async wait/wake counters.
- `memory_stats()` for capture queue occupancy, transform pool state, and external backing counts.
- `last_error()` for the most recent worker failure observed after startup.
- `last_control_error()` for the most recent control-plane failure.
- `health_report()` for a combined snapshot suitable for logs, diagnostics endpoints, or support
  bundles.

## Pipeline State

For `MediaPipeline`, prefer the result-returning APIs while debugging:

- `try_next_result()`
- `next_blocking_result(...)`
- `next_forever_result()`
- `next_async_result()` when the `async` feature is enabled

The infallible helpers map processing errors to `RecvOutcome::Closed`, which is ergonomic for simple
loops but hides stage-specific failures.

## Async Services

In Tokio applications:

- Use `MediaPipeline::spawn_tokio_worker()` for normal decode, encode, graph, hook, or sink work.
- Reserve `next_async()` and `next_async_result()` for lightweight pipelines where synchronous
  processing on the current task is acceptable.
- Call `CaptureHandle::stop_async()` or `stop_async_in_place()` before dropping handles when
  teardown latency matters.

## Backpressure And Drops

Frame drops caused by full queues are reported as capture queue send timeouts. When these grow:

- Increase capture queue depth with `StyxConfig::capture_queue_depth`.
- Increase send timeouts with backend-specific config methods.
- Move CPU-heavy pipeline work to `spawn_tokio_worker()` or a dedicated blocking worker.
- Inspect `HealthReport::drop_reasons`, `capture_backpressure_count`, and queue depth/capacity.

## External Backings

V4L2 and libcamera zero-copy paths keep external buffers alive until all frame leases release them.
Use `memory_stats().external_backings` or `health_report()` to confirm that external backing counts
return to zero after stopping a capture session. Nonzero counts after teardown usually mean a caller
is still holding `FrameLease` values.
