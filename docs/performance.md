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

### Custom perf harnesses

Two additional feature-gated commands cover paths that are easier to validate as runnable smoke
programs than generic Criterion benches:

- finite file replay:
  `cargo run -p styx --features file-backend --example file_replay_perf`
- MJPEG encode with mozjpeg:
  `cargo run -p styx --features codec-mozjpeg --example encode_perf`

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
cargo run -p styx --features file-backend --example file_replay_perf --quiet
cargo run -p styx --features codec-mozjpeg --example encode_perf --quiet
```

Observed timings:

| Case | Time |
| --- | ---: |
| File replay finite PNG set | p50 35.96 ms, p95 36.32 ms |
| Mozjpeg encode 720p RG24 -> MJPG | p50 379.21 ms, p95 403.06 ms |

Interpretation:

- File replay now has an explicit in-repo measurement surface instead of relying on manual preview examples.
- The current mozjpeg encode path is substantially slower than decode/transform on the same class of input, so encode remains a clear optimization target.

## What This Does Not Prove Yet

The current microbench does not replace real backend validation. It does not
measure:

- actual V4L2 dequeue/requeue latency
- driver behavior under sustained capture
- end-to-end pipeline latency
- per-device CPU utilization

Those are still part of the remaining item 1 tasks.

## Next Performance Work

- run the in-repo hardware example on real webcams:
  `cargo run -p styx --example v4l2_hardware_bench --features v4l2`
- record copy count, median latency, and p95 latency before/after the zero-copy
  path
- add a stable perf smoke test surface suitable for CI

## Runtime Metrics Surface

The pipeline metrics API now exposes:

- rolling stage timing snapshots, including `avg`, `p50`, and `p95`
- end-to-end pipeline latency from pipeline ingress to final output
- source-to-sink latency from capture-time instant to final output when the backend attaches capture timing
- copy/materialization counters via `PipelineMetrics::copies`
- recent residency transitions via `PipelineMetrics::residency`
- a structured `health_report()` view for live diagnostics

Typical usage:

```rust,ignore
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

```rust,ignore
let report = pipeline.health_report();
println!(
    "fps={:.1?} queue={}/{} drops={} copies={} p50={:.2?}ms last_transition={:?}",
    report.output_fps,
    report.capture_queue_depth,
    report.capture_queue_capacity,
    report.drop_count,
    report.copy_count,
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
cargo run -p styx --example perf_smoke --release
```

It currently checks loose p95 thresholds for:

- MJPEG decode at 640x360
- packed `RG24` rotate90 at 720p
- packed `RG24` mirror at 720p

The thresholds are intentionally conservative so they can be used as regression guards on mixed
developer machines and later in CI without turning into noise.
