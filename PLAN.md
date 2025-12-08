# Styx Refactor Task Plan

## Capture Backends Performance
- [ ] **V4L2 zero-copy + timestamps**: rework `crates/styx/src/capture_api/v4l2_backend.rs` to reuse mmapped buffers via `ExternalBacking` (no Vec copy), compute plane layout per format, and propagate driver timestamps/metadata into `FrameMeta`; add regression bench for throughput/latency. (timestamps/bytesused/stride fixes done; true zero-copy still pending and may require lower-level queue/dequeue)
- [x] **Netcam pipeline efficiency**: reuse a shared buffer pool per worker, avoid per-part Vec reallocations in `mjpeg_loop`, propagate source timestamps/fps, and make the MJPEG boundary parsing robust; add connection backoff/timeout tuning hooks. (pool reuse + timestamp propagation done; MJPEG size cap + Content-Length read + backoff reset added)
- [x] **File backend caching**: cache decoded images/frames for static files, reuse pools across loops, propagate file/video timestamps, and avoid rebuilding RGBA buffers each iteration; dedupe ffmpeg decode/scaling logic with the netcam ffmpeg path. (timestamp propagation + RGBA cache added; ffmpeg dedupe pending)

## Async vs Blocking Story
- [x] Decide on the model: either true async (non-blocking HTTP/IO + async channels) or explicit sync-only. Remove the current per-frame `spawn_blocking` shims if staying sync, or replace blocking `reqwest::blocking`/ffmpeg loops with async-friendly workers if going async. (netcam now has async worker using streaming HTTP; ffmpeg remains in spawn_blocking)
- [x] Replace `MediaPipeline::process_frame_async` per-frame `spawn_blocking` with a long-lived worker + async receive path, or drop the `async` feature surface if not justified. (per-frame spawn removed; async worker loops on `next_async`)

## Queueing, Backpressure, and Metrics
- [x] Provide blocking/parking receive options in `CaptureHandle::recv` and `MediaPipeline::next` to avoid busy-yield loops; expose wait/backoff strategy as a tunable.
- [x] Make `StageMetrics` lock-free or per-thread aggregated to reduce mutex contention; ensure capture/decode/encode metrics include timestamps carried from backends. (atomics + OnceCell start; backends now propagate timestamps)

## Configuration and Ergonomics
- [x] Replace env-driven codec prefs (`STYX_CODEC_PREFS`) and hidden defaults (queue depth, pool sizes, netcam backoff/timeouts) with explicit builder/config structs; document defaults. (env prefs removed; `StyxConfig` builder added with exposed defaults; docs/examples still to be updated)
- [x] Improve API clarity: implement `Iterator`/`Stream`-like traits for pipeline consumption instead of the custom `next` with clippy allow; rename unclear methods (`backend_opt`) and clean redundant branches (`pick_id`). (Iterator impl added, `backend_preferred` added with `backend_opt` deprecated, redundant branch removed)

## Structure and Duplication
- [x] Split `crates/styx/src/capture_api.rs` into smaller modules (tunables, request/validation, handle/control) to get under ~300 lines per file.
- [x] Extract shared ffmpeg scaling/copy helpers used by netcam/file backends; centralize buffer-pool sizing logic.
- [ ] Trim example code to reflect the new APIs and ensure preview paths use the streamlined pipeline API.
- [ ] Plan: (examples remaining) refresh to use `StyxConfig`, iterator API, and new module paths.

## Dependency Audit
- [ ] Reevaluate `reqwest` with blocking + rustls; if moving to async or lightweight sync, swap to a smaller HTTP client or feature-set to reduce binary size.
- [ ] Drop any unused codec backends/features after refactors; ensure feature flags remain coherent (no overlapping default pulls).

## Testing and Benchmarks
- [ ] Add perf smoke tests/benchmarks for V4L2 zero-copy vs current copy, netcam throughput, and file replay loops.
- [ ] Extend unit tests for capture validation (intervals/controls), pipeline hooks, and codec selection without env vars.
