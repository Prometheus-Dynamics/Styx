# Camera metrics

Every capture records its health and performance while it runs, cheaply enough to leave on:
relaxed atomic counters and fixed rings of the last 128 samples, written on the frame path
without locks or allocation (262 ns per frame on the CM5, see [Cost](#cost)). Snapshots are taken
on demand.

```rust
use styx::prelude::*;

let handle = CaptureRequest::new(&device).start()?;
// ...
let m = handle.camera_metrics();          // this capture
println!("{:.1} fps, {} dropped, {:?} ms", m.fps.measured.unwrap_or(0.0), m.drops.total,
         m.latency.sensor_to_delivery.p95_ms);

let all = styx::metrics::snapshot();      // every capture of the process, and the process
print!("{}", all.prometheus_text());      // Prometheus text exposition (version 0.0.4)
```

| API | what |
|---|---|
| `CaptureHandle::camera_metrics()` | `CameraMetrics` of that capture |
| `CaptureHandle::live_metrics()` | the live `CaptureMetrics` (clone it, `snapshot()` any time, also after the handle is gone) |
| `styx::metrics::snapshot()` | `MetricsSnapshot`: every running capture (opened through `CaptureRequest`, the planner, shared captures, a camera service) and `ProcessMetrics` |
| `styx::metrics::process()` | `ProcessMetrics` alone |
| `MetricsSnapshot::prometheus_text()`, `ServiceMetrics::prometheus_text()` | Prometheus/OpenMetrics text, for an application to serve |
| `styx::metrics::serve_http(addr)` (feature `metrics-http`) | a tiny HTTP endpoint answering every request with `snapshot().prometheus_text()` |
| `MetricsSnapshot::to_json()`, serde on every type (feature `metrics-serde`, on with `serde`, `frame-socket` and `native`) | JSON |
| `CameraServiceHandle::metrics()` | `ServiceMetrics` of a camera service run in this process |
| `FrameClient::service_metrics(path)` (`metrics-serde`), `FrameClient::service_metrics_text(path)` | the same from another process (below) |

`CaptureHandle::metrics()` (the receive-wait `StageMetrics`) and `health_report()` are unchanged.

`examples/05_apps/metrics_top.rs` prints a live table for every open camera (`metrics_top`), a
camera service's metrics (`metrics_top --service [socket]`), Prometheus text (`--prometheus`) or
JSON (`--json`); `--consumers N --slow MS` shares each capture between consumers, the last a slow
one; `--verify` has the consumers measure rate, sequence gaps and latency themselves for
comparison; `--overhead` measures the frame path cost.

## Per capture: `CameraMetrics`

Windows (`Window`) hold the last `WINDOW` = 128 samples (about 4 s at 30 fps, 1 s at 120 fps):
`samples` in the window, `total` since the start, `p50_ms`, `p95_ms`, `max_ms` over the window,
`max_ever_ms` since the start.

| field | meaning | how it is measured |
|---|---|---|
| `id`, `name`, `backend`, `mode`, `uptime_ms` | process-unique capture id, the device's display name, backend, format and size | set when the request starts the capture |
| `frames.captured` | frames the backend produced | counted where the backend hands a frame to the consumer queue (for backends not instrumented yet: as received) |
| `frames.delivered` | frames that entered the consumer queue | the queue's own `sent` counter |
| `frames.received` | frames the consumer took | counted in `recv`, `recv_blocking`, `recv_timeout`, `recv_forever`, `recv_async` |
| `fps.configured` | the frame interval the capture was started with | the request's (or the planner's default) interval |
| `fps.measured` | the rate the sensor delivers now | 1 / mean of the differences between consecutive sensor timestamps over the window |
| `fps.average` | since the start | `captured` / uptime (includes start-up) |
| `drops.sensor_sequence_gaps` | frames lost before Styx got them (sensor, receiver, driver) | gaps in the sensor sequence numbers (native, V4L2; libcamera from timestamps), less frames the ISP skipped |
| `drops.queue_overflow` | the consumer was too slow | consumer queue send timeouts (`Backpressure`) plus frames replaced by newer ones (`DropOldest`) |
| `drops.corrupted` | corrupt frames | receiver error flag (native `error`, V4L2 `V4L2_BUF_FLAG_ERROR`, UVC `error`), and UVC frames dropped as damaged (gaps in its sequence) |
| `drops.isp_skipped` | frames the ISP dropped because consumers held all its output buffers | PiSP worker, `OutputsHeld` |
| `drops.total` | the sum | |
| `latency.sensor_to_delivery` | sensor timestamp to the frame entering the consumer queue (exposure end/readout, receiver, ISP, Styx) | `clock_gettime` of the frame's timestamp clock (`FrameMeta::clock`) minus the timestamp, when the clock is a system clock (monotonic, boottime, realtime; not stream-relative sources) |
| `latency.sensor_to_receive` | the same up to the consumer taking the frame (adds the time in the queue) | the same, in `recv*` |
| `isp.kind` | `pisp`, `software` or `gpu` | the ISP the processed capture runs |
| `isp.isp` | PiSP back end job (config queued to output dequeued), or the software/GPU ISP pass | `PispTimes::be_job`, `SoftTiming::isp` |
| `isp.processing` | raw frame dequeued to outputs ready: statistics, algorithms, ISP | `PispTimes::total`; software: settings + ISP + statistics + algorithms |
| `cpu.threads`, `cpu.total_ns` | CPU time of the capture's worker threads | each worker registers its thread id; a snapshot reads `/proc/self/task/<tid>/schedstat` (falls back to `stat` ticks), keeping the last reading of threads that ended. Threads of the software ISP's own pool and kernel time of drivers are not included; `ProcessMetrics::cpu_ns` covers the process |
| `cpu.per_frame_us` | `total_ns` / `captured`, since the start | `metrics_top` shows it since the previous refresh |
| `aaa.ae_state` | `searching` or `converged` | the 3A loop's AE for the latest frame (processed native captures) |
| `aaa.exposure_us`, `analogue_gain`, `digital_gain` | the sensor's exposure and gains for the latest frame | `NativeFrameMeta` (native captures, raw and processed) |
| `aaa.colour_temperature_k`, `lux`, `awb_converged` | AWB's colour temperature, the scene illuminance estimate, AWB settled | the loop's `Params` |
| `aaa.flicker_hz` | light flicker AE detected (100 for 50 Hz mains) | `AeStatus::flicker_detected` |
| `aaa.af_state` | autofocus state | not reported yet (no AF in the loop) |
| `restarts.*` | start retries, reconnect attempts, reconnects, idle stops; the last worker error (or retry error) | `CaptureRetryStats` and the worker error of the capture |
| `buffers.queue_depth`, `queue_capacity` | frames waiting for the consumer, and room | the consumer queue |
| `buffers.held`, `held_bytes`, `peak_held` | capture buffers held by frames (queued or with consumers, in this or other processes) | native and PiSP/software ISP buffers are counted from lease creation to the last share's drop; V4L2 and libcamera from their backing trackers |
| `buffers.hold` | delivery to release of each capture buffer | the same leases |
| `consumers` | each consumer of a shared capture (below) | |

### Consumers of a shared capture

A shared capture (`plan_many(..).start()`, `SharedFramePlan::start_session`, the camera service)
lists one `ConsumerMetrics` per consumer, labelled `consumer <group>.<member>`:

| field | meaning |
|---|---|
| `received` | frames the consumer received |
| `dropped` | frames it did not get while the others did: frames its queue (and its group's queue) replaced by newer ones because it did not take them in time |
| `held`, `hold` | camera service clients only (below) |

## Process: `ProcessMetrics`

| field | how |
|---|---|
| `pid` | |
| `cameras_open` | captures listed in `snapshot()` |
| `service_clients` | clients connected to camera services running in the process |
| `cpu_ns` | `/proc/self/stat` user + system |
| `rss_bytes`, `threads` | `/proc/self/statm`, `/proc/self/status` |
| `dmabufs`, `dmabuf_bytes` | distinct dma-bufs among the process's descriptors (`/proc/self/fd`, `fdinfo` `ino:` and `size:`): camera, ISP, exported and imported buffers |

## Camera service

`CameraServiceHandle::metrics()` and, from any other process, `FrameClient::service_metrics(path)`
return `ServiceMetrics`: the service counters (`clients`, `rejected`, `unauthorized`,
`restarts`, `sent`, `copied`, `skipped`, `revoked`), one `ConsumerMetrics` per client
(`client <id> pid <pid> on <camera>`: frames sent, frames skipped because its socket was full,
frames it holds now, and send-to-release hold times from its release messages) and the service
process's `snapshot()` (its cameras with their consumers). A metrics request is a message of its
own on the service socket (kind 9, the format: JSON or Prometheus text); the answer (kind 10)
carries the format and length, and the text in an attached memfd, so its size is not bounded by
the socket's message size. Older services ignore the request (the client times out after 5 s).
`FrameClient::service_metrics_text(path)` asks for Prometheus text and needs no serde.

## Prometheus names

Process: `styx_process_cameras_open`, `styx_process_service_clients`,
`styx_process_cpu_seconds_total`, `styx_process_rss_bytes`, `styx_process_threads`,
`styx_process_dmabufs`, `styx_process_dmabuf_bytes`.

Per camera (labels `camera`, `id`, `backend`): `styx_camera_frames_total{stage}` (captured,
delivered, received), `styx_camera_fps{kind}` (configured, measured, average),
`styx_camera_drops_total{cause}` (sensor_sequence_gap, queue_overflow, corrupted, isp_skipped),
`styx_camera_latency_ms{path,quantile}` (sensor_to_delivery, sensor_to_receive; quantile 0.5,
0.95, 1 = window max), `styx_camera_isp_ms{isp,stage,quantile}` (isp, processing),
`styx_camera_cpu_seconds_total`, `styx_camera_cpu_per_frame_us`, `styx_camera_ae_converged`,
`styx_camera_exposure_us`, `styx_camera_analogue_gain`, `styx_camera_digital_gain`,
`styx_camera_colour_temperature_kelvin`, `styx_camera_lux`, `styx_camera_flicker_hz`,
`styx_camera_reconnects_total`, `styx_camera_reconnect_attempts_total`,
`styx_camera_queue_depth`, `styx_camera_queue_capacity`, `styx_camera_buffers_held`,
`styx_camera_buffers_held_bytes`, `styx_camera_buffer_hold_ms{quantile}`.

Per consumer (the camera's labels and `consumer`; service clients: `service="camera"`):
`styx_consumer_frames_total`, `styx_consumer_dropped_total`, `styx_consumer_held`,
`styx_consumer_hold_ms{quantile}`. Service: `styx_service_clients`,
`styx_service_events_total{event}`.

The older `HealthReport::metric_samples()` / `render_prometheus` (pipeline health reports) remain.

## Backends

| backend | frames, fps, delivery latency, CPU | sequence gaps | corrupt | ISP, 3A | buffers held / hold times |
|---|---|---|---|---|---|
| native raw | yes | yes | yes | exposure and gains | yes |
| native processed (PiSP, software, GPU ISP) | yes | yes (less ISP skips) | yes | yes | yes |
| V4L2 | yes | yes | yes | — | held (tracker), no hold times |
| UVC (userspace) | yes | (damaged frames: corrupt) | yes | — | — |
| virtual | yes | — | — | — | — |
| libcamera, file, replay, netcam, simulation | as received (count, rate, receive latency) | libcamera: yes | — | — | libcamera: held (tracker) |

A reconnecting capture (`ReconnectPolicy`) keeps one `CameraMetrics`: counters of the backend
captures it replaced are added up, windows and 3A come from the running one.

## Tracing

No per-frame events. `info`: `capture started` (camera, backend, mode, rate) when a request
starts a capture, `capture closed` (frames, received, drops by cause, seconds) when its metrics
go away, `frames missing from the sequence` at 1, 2, 4, 8, ... lost frames, and the existing
reconnect, ISP buffer and teardown events.

## Cost

Recording a frame: two counters, a frame interval, a delivery latency (one `clock_gettime`),
sequence and error checks, the sensor's exposure and gains; per buffer one tracked lease (an
`Arc` clone and two `Instant::now`); per receive a counter and a latency; for processed captures
the ISP times and 3A values. Ring writes are a relaxed `fetch_add` and a store; the maximum is
read before it is updated.

`metrics_top --overhead` (all of the above for one frame, 7 runs of a million frames):
262 ns per frame on the CM5 (Cortex-A76 2.4 GHz), 116 ns on a Zen 3 desktop. At 120 fps that is
31 µs per second, 0.003% of a core.

The PiSP path, `native_processed 900` (NV12, luma view, RGB at 120 fps, 1280x800, whole process
CPU from `/proc/self/stat` over each 7.5 s run, two rounds alternating), before (native-stack
20f7445) and after: 4.4, 3.7, 3.9, 3.9, 4.1, 4.1 % of a core before, 4.1, 4.3, 4.0, 4.4, 3.9,
3.7 % after (means 4.02 and 4.07 %): no difference beyond the tick resolution of the measurement.

## Verified on the CM5

OV9782 (native, PiSP NV12 1280x800 at 30 fps) and a Logitech C270 (V4L2, YUYV 1280x960,
converted to NV12), `metrics_top --verify`, where each consumer measures from the frames it
receives (sensor timestamps, sequence numbers, `clock_gettime` at receipt) independently:

| | metrics | consumer's own measurement |
|---|---|---|
| OV9782 frames received | 303 | 303 |
| OV9782 rate (last 128 frames) | 30.00 fps | 30.00 fps |
| OV9782 drops / sequence gaps | 0 | 0 |
| OV9782 sensor to receive, p50/p95/max | 9.39 / 9.47 / 10.38 ms | 9.39 / 9.48 / 10.38 ms |
| C270 frames, rate | 69, 6.90 fps (configured 7.5: auto exposure in a dim room) | 69, 6.90 fps |
| C270 sensor to receive, p50 | 136.01 ms (at `recv`) | 136.89 ms (after the YUYV to NV12 conversion) |

The OV9782 also showed PiSP back end 1.8 ms per frame, 216-314 µs of worker CPU per frame, AE
converged at 21.4 ms × 1.5, AWB 6160 K, 705 lux, 15 dma-bufs (24.4 MiB) in the process.

Shared capture, two consumers, the second taking a frame every 100 ms (`--consumers 2 --slow
100`, 10 s): the capture 300 frames, no camera drops; consumer 0.0 received 301, dropped 0;
consumer 0.1 received 100, dropped 200 (its own count of sequence gaps: 198, the difference
being the frames at either end of its run).

Camera service (`camera_service serve ov9782`, two `camera_service client`s, luma 640x400 and
NV12 1280x800) queried by `metrics_top --service` from a third process: 2 clients, each 147-148
frames sent, 0 dropped, 1 held, hold p50 33.3 ms (each client keeps a frame until the next);
the capture's sensor-to-receive 9.69 ms where the clients measured a frame age of 9.7 ms
themselves; `service_clients 2`, `cameras_open 1`.
