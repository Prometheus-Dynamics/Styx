# Performance

This document tracks performance validation for Styx.

The immediate goal is to make the V4L2 zero-copy work measurable rather than
describing it qualitatively.

## Current Benchmark Surface

### V4L2-style frame construction microbench

The first benchmark compares:

- copy path: copy bytes from a simulated V4L2 mmap buffer into a pooled
  `BufferLease`, then build a `FrameLease`
- external path: build a `FrameLease` directly from an external backing that
  represents the mmap buffer

This benchmark is synthetic by design. It isolates the frame construction cost
that changed during the V4L2 zero-copy work.

Run it with:

```bash
cargo bench -p styx --bench v4l2_capture_paths
```

Covered frame sizes:

- 720p YUYV
- 1080p YUYV
- 4K YUYV

### Pipeline stage microbenches

The second benchmark covers two common CPU-heavy stages:

- MJPEG decode into `RG24`
- packed-frame transform on `RG24` input (`rotate90`, `mirror`)

Run it with:

```bash
cargo bench -p styx --bench pipeline_stage_perf
```

### Pipeline worker and queue microbenches

The third benchmark covers worker-facing paths that affect idle/backpressure latency:

- raw virtual capture pipeline drain through `MediaPipeline::next_blocking`
- bounded capture queue send timeout when full
- async queue receive overhead when built with `--features async`

Run it with:

```bash
cargo bench -p styx --bench pipeline_worker_perf
cargo bench -p styx --features async --bench pipeline_worker_perf
```

### Custom perf harnesses

Two additional feature-gated commands cover paths that are easier to validate as runnable smoke
programs than generic Criterion benches:

- finite file replay:
  `cargo run -p styx-examples --no-default-features --features file-backend --bin file_replay_perf`
- MJPEG encode with mozjpeg:
  `cargo run -p styx-examples --no-default-features --features codec-mozjpeg --bin encode_perf`

### Current results

Measured on April 23, 2026 with:

```bash
cargo bench -p styx --bench v4l2_capture_paths -- --noplot
```

Observed median-ish timings from Criterion:

| Case | Copy path | External path |
| --- | ---: | ---: |
| 720p YUYV | 52.35 us | 21.14 ns |
| 1080p YUYV | 112.24 us | 20.75 ns |
| 4K YUYV | 1.261 ms | 20.96 ns |

Interpretation:

- the copy path scales with frame size, as expected
- the external-backed path is effectively measuring frame-wrapper construction
  plus borrowed plane access, not driver dequeue cost
- this confirms the architectural win at the frame-construction boundary, but
  it is not a substitute for end-to-end capture benchmarks on real hardware

### Pipeline stage results

Measured on April 23, 2026 with:

```bash
cargo bench -p styx --bench pipeline_stage_perf -- --noplot
```

Observed timings:

| Case | Time | Throughput |
| --- | ---: | ---: |
| MJPEG decode 640x360 -> RG24 | 3.74-3.85 ms | 56.2-57.8 MiB/s |
| MJPEG decode 1280x720 -> RG24 | 13.70-14.00 ms | 61.7-63.0 MiB/s |
| Packed transform rotate90 720p RG24 | 2.40-2.46 ms | 1.05-1.07 GiB/s |
| Packed transform mirror 720p RG24 | 2.27-2.32 ms | 1.11-1.13 GiB/s |

Interpretation:

- MJPEG decode scales roughly with output size and is now benchmarked directly in-repo.
- Packed transforms are meaningfully faster than decode on the same 720p-class workload, which
  is useful when prioritizing optimization work.

### Custom harness results

Measured on April 23, 2026 with:

```bash
cargo run -p styx-examples --no-default-features --features file-backend --bin file_replay_perf --quiet
cargo run -p styx-examples --no-default-features --features codec-mozjpeg --bin encode_perf --quiet
```

Observed timings:

| Case | Time |
| --- | ---: |
| File replay finite PNG set | p50 35.96 ms, p95 36.32 ms |
| Mozjpeg encode 720p RG24 -> MJPG | p50 379.21 ms, p95 403.06 ms |

Interpretation:

- File replay now has an explicit in-repo measurement surface instead of relying on manual preview examples.
- File replay decodes images lazily and bounds decoded RGB cache memory with
  `StyxConfig::file_image_cache_bytes`; set it to `0` to disable decoded image caching.
- The current mozjpeg encode path is substantially slower than decode/transform on the same class of input, so encode remains a clear optimization target.

## What This Does Not Prove Yet

The current microbench does not replace real backend validation. It does not
measure:

- actual V4L2 dequeue/requeue latency
- driver behavior under sustained capture
- end-to-end pipeline latency
- per-device CPU utilization

Those are still part of the remaining item 1 tasks.

## V4L2 Zero-Copy Capture Proof

The V4L2 backend now builds frame layout from the negotiated driver format
returned by `VIDIOC_S_FMT`/`set_format`, including negotiated width, height,
FourCC, bytes-per-line, and `sizeimage`. The mmap path only exports an external
`FrameLease` when the dequeued buffer can represent the planned layout safely;
otherwise it falls back to the copied shared backing path when a safe copied
layout can still be represented.

Synthetic construction benchmark:

```sh
cargo bench -p styx --bench v4l2_capture_paths -- --sample-size 10
```

Observed release timings:

| Case | Copy path | External mmap path |
| --- | ---: | ---: |
| 720p YUYV | 53.64 us | 25.51 ns |
| 1080p YUYV | 116.88 us | 25.76 ns |
| 4K YUYV | 1.27 ms | 26.44 ns |

Hardware graph-backed V4L2 run on the plugged-in `046d:0825` camera:

```sh
cargo run --release -p styx-examples --features "v4l2 graph-pipeline" --bin v4l2_hardware_bench
```

Representative result:

| Mode | Frames | CPU | Median delta | p95 delta | Capture copies | Graph copied bytes | Handoff |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | --- |
| YUYV 1280x720 @ 7.5 fps target | 14 | 0.0% | 136.00 ms | 136.00 ms | 0 | 0 | zero_copy_graph |
| MJPG 1280x720 @ 30 fps target | 0 | 0.0% | n/a | n/a | 0 | 0 | no frames observed |

The available hardware only exposed representative 720p modes in this run, so
this table intentionally covers only modes the camera advertised.

## Hardware Coverage

- Run the in-repo graph-backed hardware example on real webcams:
  `cargo run -p styx-examples --no-default-features --features "v4l2 graph-pipeline" --bin v4l2_hardware_bench`
- Record CPU, copy count, bytes moved, Daedalus copied bytes, median latency,
  and p95 latency at 720p, 1080p, and 4K when the camera advertises those modes
- Keep the stable perf smoke baseline aligned with supported non-hardware
  representatives.

## Runtime Metrics Surface

The pipeline metrics API now exposes:

- rolling stage timing snapshots, including `avg`, `p50`, and `p95`
- end-to-end pipeline latency from pipeline ingress to final output
- source-to-sink latency from capture-time instant to final output when the backend attaches capture timing
- copy/materialization counters via `PipelineMetrics::copies`
- recent residency transitions via `PipelineMetrics::residency`
- machine-readable frame drop reasons in `HealthReport::drop_reasons`
- a structured `health_report()` view for live diagnostics

Typical usage:

```rust,no_run
let metrics = pipeline.metrics();
let end_to_end = metrics.end_to_end.snapshot();
let source_to_sink = metrics.source_to_sink.snapshot();
let copies = metrics.copies.snapshot();

println!(
    "e2e_p50_ms={:.2?} e2e_p95_ms={:.2?} source_p50_ms={:.2?} source_p95_ms={:.2?} copies={} bytes_moved={}",
    end_to_end.p50_millis,
    end_to_end.p95_millis,
    source_to_sink.p50_millis,
    source_to_sink.p95_millis,
    copies.copies,
    copies.bytes_moved,
);
```

Health-report usage:

```rust,no_run
let report = pipeline.health_report();
println!(
    "fps={:.1?} queue={}/{} backpressure={} drops={} drop_reasons={:?} async_waits={}/{} copies={} bytes_moved={} graph_copied_bytes={} p50={:.2?}ms last_transition={:?}",
    report.output_fps,
    report.capture_queue_depth,
    report.capture_queue_capacity,
    report.capture_backpressure_count,
    report.drop_count,
    report.drop_reasons,
    report.capture_async_send_waits,
    report.capture_async_recv_waits,
    report.copy_count,
    report.bytes_moved,
    report.graph.as_ref().map(|g| g.copied_bytes).unwrap_or(0),
    report.latency_p50_ms,
    report.recent_residency_transitions.last(),
);
```

Each transition reports:

- `from` residency
- `to` residency
- `reason`
- whether the transition forced a copy

Recommended consumers:

- `tracing` subscribers for stage-level spans around capture, decode, transform, encode, and sink
- periodic log/TTY health reports for long-running preview or recording sessions
- CI perf smoke commands for coarse regression detection

## Perf Smoke Surface

There is now a lightweight smoke command for common CPU-heavy paths:

```bash
cargo run -p styx-examples --no-default-features --features codec-jpeg-decoder --bin perf_smoke --release
```

It currently checks loose p95 thresholds for:

- MJPEG decode at 640x360
- packed `RG24` rotate90 at 720p
- packed `RG24` mirror at 720p

The thresholds are intentionally conservative so they can be used as regression guards on mixed
developer machines and later in CI without turning into noise.

CI uses the aggregate baseline checker:

```bash
./scripts/check-perf-smoke.sh
```

That script runs the decode/transform smoke path, file replay, and mozjpeg
encode, then compares p95 timings against
`testing/perf/baseline.txt`. These checks deliberately avoid the graph feature
so Daedalus runtime work cannot block the non-graph media performance surface.

## Runtime Troubleshooting Signals

When a capture or pipeline session stalls, drops frames, or closes unexpectedly, inspect
`MediaPipeline::health_report()` before changing queue sizes or worker placement:

- `capture_queue_depth` and `capture_queue_capacity`: a queue that stays at capacity means the
  consumer or pipeline worker is not keeping up with the capture backend.
- `capture_backpressure_count`: increments when the bounded capture queue is full; sustained growth
  points to downstream decode, graph, hook, encode, or sink work being slower than capture.
- `drop_count` and `drop_reasons`: count intentionally dropped frames. `capture_queue_send_timeout`
  means the backend waited for queue capacity and dropped when the configured timeout expired.
- `recent_stage_errors`: preserves decode, encode, graph, hook, transform, and sink failures that
  the infallible receive helpers expose as closure. Call `last_stage_error()` for the newest error.
- `capture_async_send_waits` and `capture_async_recv_waits`: show async queue wake/wait activity when
  diagnosing Tokio scheduling or worker placement.
- `CaptureRequest::start_with_policy_async` yields during retry backoff only. If camera startup or
  probing is slow in an async service, perform startup on a blocking task/thread and use async receive
  or `spawn_tokio_worker` after the handle has been created.
- `copy_count`, `bytes_moved`, and `recent_residency_transitions`: identify unexpected host copies or
  residency transitions that can explain latency spikes.

For live logs, enable a `tracing` subscriber and filter on spans/events with fields such as `backend`,
`stage`, `worker`, `receive`, `processing`, and `drop_reason`. Capture queue drops are emitted at
`debug`; per-frame hot-path timings and stage spans are intentionally lower-volume unless tracing is
configured for them.

## Queue Policy Presets

Queue depth is a latency/completeness policy, not a universal tuning knob:

- `StyxConfig::low_latency_preview()` uses a shallow capture queue so preview consumers prefer the
  freshest frame and expose downstream stalls quickly.
- `StyxConfig::reliable_recording()` uses a deeper capture queue and larger pool so short sink or
  encode stalls are less likely to drop frames.
- `StyxConfig::netcam_preview()` keeps moderate capture buffering while extending netcam timeouts and
  retry backoff for bursty MJPEG-over-HTTP sources.

Start with the preset that matches the workflow, then use health-report queue pressure, drop reasons,
and stage timings to justify changing `capture_queue_depth` or pool limits.
