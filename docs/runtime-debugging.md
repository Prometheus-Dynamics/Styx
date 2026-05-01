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

## Runtime Memory Report

Use `styx::memory::runtime_memory_report()` for a process-level snapshot, or
`MediaPipeline::runtime_memory_report()` when a running pipeline should attach Styx capture stats,
copy counters, residency transitions, and graph telemetry to the process snapshot. The report
combines:

- `/proc/self/smaps_rollup` process RSS, PSS, private, shared, and swap totals.
- `/proc/self/smaps` mapping groups such as heap, stack, shared libraries, anonymous mappings,
  memfd, PiSP memfd, libcamera/IPA mappings, DMA heap, device mappings, files, and unknown
  mappings.
- `/proc/self/fd` counts by fd class, including sockets, pipes, anon inodes, memfd, PiSP memfd,
  DMA-related fds, and media/video device fds. On Linux, DMA-BUF fdinfo is also deduplicated by
  inode when available so reports can show current-process DMA-BUF fd count, unique buffer count,
  total bytes, and exporter totals without requiring debugfs.
- Styx-tracked capture and pipeline pools, including libcamera request pools, TDN request pools,
  outstanding DMABUF frame leases, CPU-mapped DMABUF leases, transform pools, and shared codec
  pools.
- Best-effort kernel DMA-BUF debugfs availability, per-exporter DMA-BUF totals when
  `/sys/kernel/debug/dma_buf/bufinfo` is readable, and CMA total/free/used bytes when the kernel
  exposes `CmaTotal` and `CmaFree` in `/proc/meminfo`.

For target validation runs, use `scripts/run-runtime-memory-validation.sh`. It executes the probe
binary modes and writes a markdown report to `target/runtime-memory-validation.md` by default. The
report starts with a summary table for PSS/RSS, Styx tracked backings, libcamera request pools,
outstanding leases, PiSP memfd PSS, fd counts, and unexplained PSS. Override
`STYX_RUNTIME_MEMORY_FEATURES` and `STYX_RUNTIME_MEMORY_FRAMES` to match the target build and
capture duration.

Interpret the major fields this way:

- Process PSS/RSS is what the OS can attribute to this process. Use PSS for memory-budget
  comparisons because shared pages are proportionally accounted.
- Styx tracked backing bytes are buffers Styx knows it owns or is keeping alive through frame
  leases. These explain only Styx-visible pools, not every libcamera/PiSP internal allocation.
- Process DMA-BUF fdinfo totals show DMA-BUF objects currently referenced by this process. This is
  the right local check when Styx external backing bytes are low but libcamera/PiSP still has many
  request or ISP buffers open. These totals are buffer sizes, not PSS; mapped residency is still
  visible through smaps categories.
- PiSP memfd mappings usually come from the live libcamera/PiSP pipeline. They can appear in PSS
  while capture is active even when the Styx heap is small.
- `libcamera_or_ipa` mappings are best-effort process mappings for libcamera and IPA shared
  objects. They improve attribution of mapped code/data pages, but do not expose libcamera's
  private allocator ownership model.
- Copy and residency counters identify host copies introduced by decode, encode, transforms, graph
  transport, hooks, or sinks.
- Encoder heap cost is mostly normal process memory, not external backing. Compare process PSS and
  the anonymous/shared-library smaps categories before and after enabling an encoder to attribute
  codec context, conversion frames, thread buffers, and library residency.
- Unexplained PSS is a diagnostic delta. It is process PSS minus currently tracked Styx pools and
  graph copy/transport bytes, so it can include allocator overhead, thread stacks, library pages,
  libcamera internals, service state, watchers, API clients, and mappings that Styx cannot classify
  as owned.
- Kernel DMA/CMA memory may not be visible in normal process PSS. If debugfs DMA-BUF telemetry is
  unavailable, use the report's CMA totals as a coarse system-pressure signal, not per-process
  ownership proof.
- DMA-BUF exporter totals come from debugfs and are kernel-wide. They explain pressure by exporter
  name, but they do not prove that every buffer belongs to the current Styx process.

For low-memory deployments, start with these knobs:

- Lower `StyxConfig::capture_queue_depth` to reduce libcamera/V4L2 request-pool pressure.
- Disable libcamera request-pool prefaulting with `StyxConfig::libcamera_prefault_request_pools(false)`
  or `STYX_LIBCAMERA_PREFAULT_REQUEST_POOLS=0`.
- Enable libcamera idle stop with `StyxConfig::libcamera_stop_when_idle(true)` or
  `STYX_LIBCAMERA_STOP_WHEN_IDLE=1` when idle memory matters more than warm-start latency.
- Avoid TDN output unless a requested control requires it.
- Avoid unnecessary packed transforms, RGB conversions, host decode/encode fallbacks, and sink paths
  that force DMABUF frames into host memory.
- For FFmpeg preview encoders, prefer explicit `FfmpegEncoderOptions` when the application can
  accept constrained output, for example `thread_count: Some(1)` or a smaller `output_resolution`.
  These are encoder choices, not Styx memory modes.
