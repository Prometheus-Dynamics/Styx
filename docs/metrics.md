# Camera metrics

Every capture records its health and performance while it runs, cheaply enough to leave on:
relaxed atomic counters and fixed rings of the last 128 samples, written on the frame path
without locks or allocation (262 ns per frame on the CM5, see [Cost](#cost)). Snapshots are taken
on demand. The counters, rings and 3A/AF/still state are the portable runtime's
(`styx_core::metrics`, re-exported as `styx_runtime::metrics`, `no_std`, the same on a
microcontroller); `styx` feeds them from frame
metadata and adds what a Linux process has (consumers, buffers held, worker CPU time).

```rust
use styx::prelude::*;

let frames = Frames::nv12().size(1280, 800).fps(30).open(&camera)?;
// ...
let m = frames.metrics();                 // this capture (CaptureHandle::camera_metrics() too)
println!("{:.1} fps, {} dropped, {:?} ms", m.fps.measured.unwrap_or(0.0), m.drops.total,
         m.latency.sensor_to_delivery.p95_ms);

let all = styx::metrics::snapshot();      // every capture of the process, and the process
print!("{}", all.prometheus_text());      // Prometheus text exposition (version 0.0.4)
```

| API | what |
|---|---|
| `CaptureHandle::camera_metrics()` | `CameraMetrics` of that capture |
| `Frames::metrics()` | the same for a `Frames` stream (`Frames::nv12()...open(&camera)?.metrics()`); on a shared capture every consumer is listed |
| `Frames::consumer_metrics()` | that stream's own row on a shared capture: frames received, frames it did not take in time |
| `CaptureHandle::live_metrics()` | the live `CaptureMetrics` (clone it, `snapshot()` any time, also after the handle is gone) |
| `styx::metrics::snapshot()` | `MetricsSnapshot`: every running capture (opened through `CaptureRequest`, the planner, shared captures, a camera service) and `ProcessMetrics` |
| `styx::metrics::process()` | `ProcessMetrics` alone |
| `MetricsSnapshot::prometheus_text()`, `ServiceMetrics::prometheus_text()` | Prometheus/OpenMetrics text, for an application to serve |
| `styx::metrics::serve_http(addr)` (feature `metrics-http`) | a tiny HTTP endpoint answering every request with `snapshot().prometheus_text()` |
| `MetricsSnapshot::to_json()`, serde on every type (feature `metrics-serde`, on with `serde`, `frame-socket` and `native`) | JSON |
| `CameraServiceHandle::metrics()` | `ServiceMetrics` of a camera service run in this process |
| `FrameClient::service_metrics(path)` (`metrics-serde`), `FrameClient::service_metrics_text(path)` | the same from another process (below) |
| `FrameSocket::metrics()`; `frame_socket::fetch_metrics(path)`, `fetch_metrics_text(path)` | a frame socket's statistics, in its process or from another (its `<path>.stats` endpoint, below) |
| `CameraMetrics::path`, `ConsumerMetrics::hops`, `FrameFetcher::hop_metrics()`, `FrameClient::hop_metrics()` | hop times and copies of the frames a capture's consumers took, a service client got, a consumer process imported ([Hops](#hops-where-each-frames-time-goes)) |
| `styx::metrics::path()`, `ProcessMetrics::path` | the process's copies by site, dma-buf syncs and exhausted pools |
| `FrameMeta::hops`, `FrameMeta::hop_record()` | one frame's hops, and its record for joining with other tools' |

`CaptureHandle::metrics()` (the receive-wait `StageMetrics`) and `health_report()` are unchanged.

`examples/05_apps/metrics_top.rs` prints a live table for every open camera (`metrics_top`), a
camera service's metrics (`metrics_top --service [socket]`), Prometheus text (`--prometheus`) or
JSON (`--json`); `--consumers N --slow MS` shares each capture between consumers, the last a slow
one; `--verify` has the consumers measure rate, sequence gaps and latency themselves for
comparison; `--overhead` measures the frame path cost; `--frame-socket PATH` shows a frame
socket's statistics. Under each camera it prints the hop table of the frames its consumers took,
on the process line the copies, dma-buf syncs and exhausted pools.
`examples/04_performance/hop_breakdown.rs` prints a frame's path hop by hop for a consumer in the
capturing process, behind a frame socket and behind a camera service.

## Per capture: `CameraMetrics`

Windows (`Window`) hold the last `WINDOW` = 128 samples (about 4 s at 30 fps, 1 s at 120 fps):
`samples` in the window, `total` since the start, `p50_ms`, `p95_ms`, `p99_ms`, `max_ms` over the
window, `max_ever_ms` since the start.

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
| `aaa.af_state` | `idle`, `scanning`, `focused` or `failed` | the loop's `AfStatus::state` for the latest frame; only for cameras with a focus lens (`AfStatus::active`), all AF fields `None` otherwise |
| `aaa.af_mode` | `manual`, `auto` or `continuous` | `AfStatus::mode` |
| `aaa.lens_position_dioptres` | the lens position AF commanded, in dioptres (0: infinity) | `AfStatus::lens_position` |
| `aaa.lens_settled` | the lens had settled at its commanded position for the latest frame's whole exposure (`false`: moving) | the lens control's prediction for that frame (`FrameControls::lens`, from the moves written and the lens's move time model; VCMs have no read-back), looked up for the frame's sequence |
| `aaa.af_scans` | AF scans started since the capture started (continuous scans after scene changes, and each trigger in auto mode) | counted when the state enters `scanning` |
| `restarts.*` | start retries, reconnect attempts, reconnects, idle stops; the last worker error (or retry error) | `CaptureRetryStats` and the worker error of the capture |
| `buffers.queue_depth`, `queue_capacity` | frames waiting for the consumer, and room | the consumer queue |
| `buffers.held`, `held_bytes`, `peak_held` | capture buffers held by frames (queued or with consumers, in this or other processes) | native and PiSP/software ISP buffers are counted from lease creation to the last share's drop; V4L2 and libcamera from their backing trackers |
| `buffers.hold` | delivery to release of each capture buffer | the same leases |
| `consumers` | each consumer of a shared capture (below) | |
| `path` | hop times and copies of the frames consumers took: sensor, dequeued, ISP done, queued, taken (a `HopMetrics`, [Hops](#hops-where-each-frames-time-goes)) | each frame's `FrameMeta::hops`, recorded when a consumer takes it |

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
| `path` | copies by site, `DMA_BUF_IOCTL_SYNC` calls and time, exhausted pools (a `PathMetrics`, [below](#copies-dma-buf-syncs-exhausted-pools)) |

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
`FrameClient::service_metrics_text(path)` asks for Prometheus text and needs no serde. Each
client's `hops` is the whole path of the frames sent to it: the service's hops up to the send,
and the client's receive and import times, which come back with its releases.

## Previews

`Preview::metrics()` (`styx::preview`, feature `preview`) and `snapshot().previews` return one
`PreviewMetrics` per preview in the process ([preview.md](preview.md)):

| field | meaning | how it is measured |
|---|---|---|
| `name`, `encoder` | `PreviewConfig::name`, the JPEG encoder (`turbojpeg`, `mozjpeg`, `image`) | |
| `frames_in` | frames offered (`Preview::offer`) or received from the camera service | counted on arrival |
| `encoded`, `passthrough` | JPEG frames published; of those, camera JPEG passed through | |
| `dropped_rate`, `dropped_busy`, `dropped_unwatched`, `errors` | frames not published: over the frame rate cap, replaced by a newer one while the encoder was busy (latest-frame semantics), nobody watching, could not be scaled or encoded (`last_error`) | counted where the frame is let go, before any work on it |
| `encode` | JPEG encode time per frame (window) | around the encoder call |
| `scale` | choosing the source and scaling it to the preview size (the frame is held only for this) | from taking the frame to releasing it |
| `capture_to_encoded` | capture timestamp to JPEG published | `FrameMeta::latency()` when taken, plus the time to publish |
| `bytes_total`, `bytes_per_frame`, `bytes_p95` | JPEG bytes: total, mean and 95th percentile over the window | |
| `size`, `source_size` | the preview's size and the frame it was scaled from (a share of the vision client's frames shows its size here) | |
| `cpu_ns`, `cpu_percent` | CPU time of the preview thread, and as a share of one core since it started | `clock_gettime(CLOCK_THREAD_CPUTIME_ID)` on the preview thread after each frame |
| `subscribers` | viewers subscribed now (camera service previews) | |

Prometheus (labels `preview`, `encoder`): `styx_preview_frames_in_total`,
`styx_preview_frames_encoded_total`, `styx_preview_frames_dropped_total{cause}` (rate, busy,
unwatched, error), `styx_preview_bytes_total`, `styx_preview_bytes_per_frame`,
`styx_preview_encode_ms{quantile}`, `styx_preview_scale_ms{quantile}`,
`styx_preview_capture_to_encoded_ms{quantile}`, `styx_preview_cpu_seconds_total`,
`styx_preview_subscribers`.

## Hops: where each frame's time goes

Every frame carries a hop record from the sensor to its consumer (`FrameMeta::hops`, a
`styx_core::buffer::FrameHops`: fixed size, `Copy`, no allocation, `no_std`): when it passed each
step, in `CLOCK_MONOTONIC` nanoseconds, and the copies made of its pixels on the way. Backends fill
the hops they know; a hop a frame did not pass stays `None`. The record travels with the frame
through queues, into other processes (frame socket, camera service) and back (camera service
releases), so every consumer has the frame's whole path, and tools that keep their own per-frame
records (Daedalus, Eidos) join on the frame's sequence number with its sensor timestamp
(`FrameMeta::hop_record()`, a serde `HopRecord`).

| hop | when | set by |
|---|---|---|
| `sensor` | the sensor timestamp, on `CLOCK_MONOTONIC`: frame start (first line received) on `rp1-cfe` and `unicam` (native captures); what the driver reports for V4L2 (`uvcvideo`: first payload or PTS-based); libcamera's `SensorTimestamp` (start of exposure), converted from boottime | the frame's timestamp when it enters the queue |
| `dequeued` | Styx took the frame from the kernel: `VIDIOC_DQBUF` returned (native raw and processed, V4L2), the request completed (libcamera) | the backend (PiSP and software ISP: the raw frame's dequeue), else the frame's capture instant |
| `isp_done` | the ISP finished: PiSP back end job and its extra passes done (`dequeued + PispTimes::total`), software/GPU ISP pass done | the PiSP and software ISP workers |
| `queued` | the frame entered the capture's consumer queue (its buffer leased to consumers) | `deliver` |
| `taken` | a consumer in the capturing process took it (`recv*`, `Frames::next_frame`, a camera service's thread for a client) | `CaptureHandle::recv*` |
| `sent` | a frame socket or camera service wrote the message (just before `sendmsg`) | the frame socket's thread, the service's thread for the client |
| `received` | the consumer process received the message and its descriptors | `FrameFetcher`/`fetch_frame`, `FrameClient::recv` |
| `imported` | the consumer built the frame over the descriptors (camera service clients and frame socket fetches: the buffer's mapping comes from a cache of each buffer's mapping, made on the first read of a buffer not seen before) | the same |

Hop deltas are aggregated where a frame's record is complete for a path, in windows of the last
`WINDOW` frames (p50/p95/p99/max, as `Window`):

| where | what | API |
|---|---|---|
| a capture | sensor → dequeued → ISP done → queued → taken, for the frames its consumers took | `CameraMetrics::path` (`HopMetrics`) |
| a frame socket | the server's hops up to `sent`, per frame sent | `FrameSocket::metrics().hops`, `<path>.stats` |
| a frame socket consumer | the whole path to `imported` | `FrameFetcher::hop_metrics()` |
| a camera service, per client | the whole path to `imported` (the client reports `received` and `imported` with its release) | `ServiceMetrics::client_metrics[..].hops` |
| a camera service client | the whole path to `imported` | `FrameClient::hop_metrics()` |

`HopMetrics`: `frames`, `zero_copy`, `copied`, `copies`, `copied_bytes`, `hops` (each
`HopWindow { from, to, window }`, in path order: the time into `to` from the hop before it) and
`total` (first to last hop). `styx::metrics::hop_lines` prints one as text.

## Copies, dma-buf syncs, exhausted pools

Every place that copies pixels calls one helper (`styx_core::metrics::copied_frame(meta, site,
bytes)`, or `copied(site, bytes)` where no output frame is at hand): a process-wide counter per
site and, with `copied_frame`, the frame's own hop record, so the consumer of a copied frame (and
the per-capture, per-socket and per-client `HopMetrics`) count it as copied. Every
`DMA_BUF_IOCTL_SYNC` is counted with its time (styx-core's `dmabuf_*_cpu_*` and styx-kernel's
`dma_heap::sync`, which every Styx sync goes through). `ProcessMetrics::path` (`PathMetrics`):

| field | what | counted at |
|---|---|---|
| `copies[site]`, `bytes` | `softisp`: raw input rows staged by the software ISP for uncached receiver buffers; `conversion`: a planner conversion or decode into new memory (YUYV to NV12, NV12 to RGB, JPEG); `region`: a crop or luma view realigned into new memory; `memfd_export`: a frame not in shareable memory copied into a memfd to send it (frame socket, camera service, frame server); `materialize`: `materialize_owned`, `copy_visible_to_slice`, `to_visible_vec`; `capture`: a backend copying out of the driver's buffer (V4L2 without zero copy, UVC payload assembly); `raw`: raw frames kept for stills; `other`: scaling into new memory (overviews the ISP does not make) | the copy itself |
| `dmabuf_syncs`, `dmabuf_sync_ns` | `DMA_BUF_IOCTL_SYNC` calls (start and end each count) and their time | the ioctl |
| `pool_exhausted` | a pool or a capture found every buffer held: the PiSP back end with all its output buffers held (the frame is dropped, `drops.isp_skipped`), a shared memfd pool allocating another buffer | the pool |

Prometheus: `styx_camera_hop_ms{from,to,quantile}`, `styx_camera_path_ms{quantile}`,
`styx_camera_path_frames_total{kind="zero_copy"|"copied"}`, `styx_camera_path_copied_bytes_total`;
the same as `styx_consumer_*` for camera service clients and frame sockets
(`consumer="frame_socket"`); `styx_process_copies_total{site}`,
`styx_process_copied_bytes_total{site}`, `styx_process_dmabuf_syncs_total`,
`styx_process_dmabuf_sync_seconds_total`, `styx_process_pool_exhausted_total`. Windows have a
`quantile="0.99"` sample now, and `Window::p99_ms`.

## A frame socket's statistics

`FrameSocket::metrics()` (`FrameSocketMetrics`): the counters (`published`, `copied`, `served`,
`leases`, `held_frames`, `revoked`, `unserved`), the lease hold times (send to close),
`HopMetrics` of the frames sent and the snapshot of the serving process. Another process (HeliOS's
profiler) reads them while the socket serves, without touching the frame socket itself (a
connection to it is always a frame lease): the sibling endpoint `<path>.stats`
(`frame_socket::stats_path`) answers each connection that writes `json` or `prometheus` with
the statistics and closes; `frame_socket::fetch_metrics(path)` and `fetch_metrics_text(path)` do
that (or `printf prometheus | socat - UNIX-CONNECT:<path>.stats`).

## No copies, no allocations on the way to a consumer

Counted with a counting allocator over hundreds of frames after a warm-up, and with the copy
counters, on the host (tests that run with `cargo test`):

| path | per frame | test |
|---|---|---|
| in-process, native PiSP: lease an output buffer, deliver, `recv`, read the pixels, drop (the buffer goes back) | 1 allocation (the lease record) in the lease; 0 in delivering, taking, reading, giving back; 0 copies | `native_isp::pisp_lease::alloc_tests` |
| frame socket: publish, send (the socket's thread), fetch and import (`FrameFetcher`), read, release | 0 publishing, 0 sending, 1 fetching (the lease record), 0 reading and releasing; 0 copies | `crates/styx/tests/zero_alloc.rs` |
| camera service: the service's thread for a client (take, export, send, take releases), the client receiving, importing, reading, releasing | 0 on the service's thread, 1 on the client (the release record); 0 copies | `crates/styx/tests/zero_alloc.rs` |

The one allocation per frame on each side is the frame's lease record: a frame handed out is
the consumer's to keep for as long as it likes, and its buffer must go back (to the back end, to
the server) the moment its last share drops. That needs a reference-counted record whose drop
says so, one per frame; everything else (message buffers, descriptor lists, frame records, the
mapping of each buffer, the PiSP's buffer returns) is reused. Fixed on the way: the frame socket
consumer mapped every frame (`mmap` and `munmap`, about 0.5 ms for a 1280x800 NV12 frame on the
CM5) and camera service clients mapped every memfd frame; the frame socket, the service and its
clients built their messages, descriptor lists and frame records anew for every frame; the
PiSP's buffer returns went through a channel that allocates in blocks.

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
`styx_camera_af_state{state,mode}` (1), `styx_camera_lens_position_dioptres`,
`styx_camera_lens_settled`, `styx_camera_af_scans_total`,
`styx_camera_reconnects_total`, `styx_camera_reconnect_attempts_total`,
`styx_camera_queue_depth`, `styx_camera_queue_capacity`, `styx_camera_buffers_held`,
`styx_camera_buffers_held_bytes`, `styx_camera_buffer_hold_ms{quantile}`,
`styx_camera_hop_ms{from,to,quantile}`, `styx_camera_path_ms{quantile}`,
`styx_camera_path_frames_total{kind}` (zero_copy, copied), `styx_camera_path_copied_bytes_total`.
Windows are exported at quantile 0.5, 0.95, 0.99 and 1 (the window's maximum).

Per consumer (the camera's labels and `consumer`; service clients: `service="camera"`):
`styx_consumer_frames_total`, `styx_consumer_dropped_total`, `styx_consumer_held`,
`styx_consumer_hold_ms{quantile}`, and for camera service clients `styx_consumer_hop_ms`,
`styx_consumer_path_ms`, `styx_consumer_path_frames_total`,
`styx_consumer_path_copied_bytes_total`. Service: `styx_service_clients`,
`styx_service_events_total{event}`. Process: `styx_process_copies_total{site}`,
`styx_process_copied_bytes_total{site}`, `styx_process_dmabuf_syncs_total`,
`styx_process_dmabuf_sync_seconds_total`, `styx_process_pool_exhausted_total`. Frame socket
(label `socket`): `styx_frame_socket_events_total{event}` (published, copied, served, revoked,
unserved), `styx_frame_socket_leases`, `styx_frame_socket_held_frames`,
`styx_frame_socket_hold_ms{quantile}`, its hops as `styx_consumer_*{consumer="frame_socket"}`.

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
262 ns per frame on the CM5 (Cortex-A76 2.4 GHz), 116 ns on a Zen 3 desktop (270 ns on the CM5
before and after the counters moved into `styx-runtime`, measured on the same day). At 120 fps that is
31 µs per second, 0.003% of a core.

Hops add per frame: the queue and take stamps (`Instant::now`, one each), the sensor timestamp
and dequeue/ISP times the backend already has, and a record into the capture's windows (one
ring write per hop, a relaxed `fetch_add` and a store); a frame sent to another process adds
the send, receive and import stamps and one record each at the server and the consumer. Copy
and sync counters are a relaxed `fetch_add` where a copy or an ioctl happens anyway (an ioctl
also reads the clock twice). `metrics_top --overhead` measures both: the frame path above with
its hops, and the hops alone for a frame sent to another process (every stamp and the three
records): on a Zen 3 desktop 182 ns per frame for the frame path (116 ns before the hops) and
244 ns for all of a sent frame's hops. Without `path-metrics` (a `no_std` build that does not
enable it) `FrameHops` is empty and every stamp compiles to nothing: the MCU images' flash is
what [mcu.md](mcu.md) lists (`scripts/mcu-size.sh --check`, A-D on Cortex-M7/M4F and M0+).

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

### Measuring the hop breakdown on the CM5

`hop_breakdown` prints, for native PiSP NV12 1280x800 at 30 fps, each hop's p50/p99/max, the
copies and dma-buf syncs per frame of each process and their heap allocations per frame (every
thread), for a consumer in the capturing process, behind a frame socket and behind a camera
service. Built static, so it runs on the device as it is:

```sh
CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER=rust-lld cargo build --release \
    --target aarch64-unknown-linux-musl -p styx-examples --features native,v4l2 \
    --bin hop_breakdown --bin metrics_top
scp target/aarch64-unknown-linux-musl/release/{hop_breakdown,metrics_top} root@helios:/tmp/
DMESG_LOG=target/helios-dmesg-hops.log WAIT=1 scripts/with-device-lock.sh hops '
  cd /tmp && ./metrics_top --overhead && ./hop_breakdown inproc 20 &&
  { ./hop_breakdown socket-serve /tmp/hops.sock 30 & sleep 3; ./hop_breakdown socket-fetch /tmp/hops.sock 20; wait; } &&
  { ./hop_breakdown service-serve /tmp/hops-svc.sock 32 & sleep 3; ./hop_breakdown service-client /tmp/hops-svc.sock 20; wait; }'
```
