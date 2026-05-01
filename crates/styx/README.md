# styx (facade crate)

The end-user entry point that re-exports the layered stack (`styx-core`, `styx-capture`, `styx-codec`, optional backends) behind a single prelude. It adds capture requests, pipeline sessions, graph integration, service events, probing helpers, metrics, watch runtime support, recording, and feature-gated preview support.

## Documentation
- <https://docs.rs/styx>

## Install
```toml
[dependencies]
styx = "2.0.0"
```

## What it provides
- `styx::prelude`: re-exports core/capture/codec preludes plus capture API (`CaptureRequest`, `CaptureHandle`, `StyxConfig`, `start_capture`, etc.), pipeline types (`MediaPipelineBuilder`, `MediaPipeline`), metrics, and backend handles.
- `probe_all`: merge v4l2/libcamera probe results when enabled.
- `watch`: inventory watch/runtime layer with retained events, blocking subscriptions, hotplug watchers, and async wrappers when `async` is enabled.
- `BackendHandle/BackendKind`, `ProbedDevice`, `ProbedBackend`: describe discovered devices and selected backends.
- `capture_api`: request/source builders for virtual, netcam, file, and physical capture plus tunables.
- `session`: `MediaPipeline` for capture→decode→hook→encode flows (sync-first; async helpers when `async` is enabled).
- `graph` (feature `graph-pipeline`): Daedalus-backed `FrameLease` transport, source/sink nodes, media edge policies, and graph telemetry.
- `service`: retained runtime event stream for inventory, health, sink, recording, and graph-control events.
- `capabilities`: capture/codec/transform/backing inventory plus path explanation helpers for planner integration.
- `recording` (feature `hooks`): record encoded frames directly, or attach an encoder to record raw frames without the `image` crate.
- `preview` (feature `preview-window`): simple RGBA/RGB preview window for examples.

## Typical usage
Capture with a chosen backend/mode:
```rust,no_run
use styx::prelude::*;

let device = CaptureRequest::virtual_source(
    VirtualSourceConfig::new()
        .name("virtual")
        .resolution(640, 360)
        .fps(30),
)
.into_device();

let handle = CaptureRequest::new(&device)
    .backend_preferred(Some(BackendKind::Virtual)) // or V4l2/Libcamera when enabled
    .start()?;

match handle.recv() {
    RecvOutcome::Data(frame) => println!("got frame {:?}", frame.meta().format),
    RecvOutcome::Empty => {}
    RecvOutcome::Closed => {}
}
handle.stop();
# Ok::<(), Box<dyn std::error::Error>>(())
```

Capture → decode pipeline with hooks and optional preview:
```rust,ignore
use std::sync::Arc;
use styx::prelude::*;

let device = CaptureRequest::virtual_source(VirtualSourceConfig::new().name("virtual").resolution(640, 360).fps(30)).into_device();
let decoder = Arc::new(PassthroughDecoder::new(device.backends[0].descriptor.modes[0].format.code));
let mut pipeline = MediaPipelineBuilder::new(CaptureRequest::new(&device))
    .decoder(decoder)
    .rotate(Rotation90::Deg90)    // Optional 90-degree rotation
    .mirror(true)                 // Optional horizontal mirror
    .frame_hook(|frame| frame)     // works on FrameLease without image materialization
    .hook(|frame| frame.flipv())   // FrameLease-native transform helpers
    .start()?;

loop {
    match pipeline.next_forever_result()? {
        RecvOutcome::Data(frame) => println!("pipeline frame {:?}", frame.meta().format),
        RecvOutcome::Empty => {}
        RecvOutcome::Closed => break,
    }
}
# Ok::<(), styx::capture_api::CaptureError>(())
```

The pipeline hook/runtime path operates directly on `FrameLease`; Styx does not
materialize a generic image object as part of the media pipeline. Enable
`image` only when hook closures need image-crate/DynamicImage helpers or when
recording raw frames through the PNG/JPEG image-crate fallback.

Request-local tunables for queues/pools/netcam backoff:
```rust,no_run
use styx::prelude::*;

let device = CaptureRequest::virtual_source(VirtualSourceConfig::new().name("virtual").resolution(640, 360).fps(30)).into_device();
let config = StyxConfig::new()
    .capture_queue_depth(8)
    .capture_pool(4, 1 << 20, 8)
    .libcamera_probe_cache_ttl(1_000)
    .netcam_http_timeouts(10, 1_000, 2_000)
    .netcam_backoff(500, 5_000);
let handle = device.capture_request().config(config).start()?;
# Ok::<(), styx::capture_api::CaptureError>(())
```

Use `probe_all_with_config(&config)` when probe-time behavior, such as libcamera probe cache TTL,
must match the same runtime configuration.

Service event retention:
```rust,no_run
use styx::prelude::*;

let device = CaptureRequest::virtual_source(VirtualSourceConfig::new().name("virtual").resolution(640, 360).fps(30)).into_device();
let mut pipeline = MediaPipelineBuilder::new(CaptureRequest::new(&device))
    .service_runtime_config(StyxServiceConfig {
        max_retained_events: 1024,
    })
    .start()?;
let service = pipeline.service_runtime().expect("service runtime");
let mut cursor = service.lock().expect("service lock").subscribe_from_start();
let _ = pipeline.health_report();
let service = service.lock().expect("service lock");
let poll = service.poll_events(&mut cursor);
println!("events={} truncated={}", poll.events().len(), poll.was_truncated());
# Ok::<(), Box<dyn std::error::Error>>(())
```

Record output frames for replay with the file backend:
```rust,ignore
use styx::prelude::*;

let device = CaptureRequest::virtual_source(VirtualSourceConfig::new().name("virtual").resolution(640, 360).fps(30)).into_device();
let recorder = FrameRecorder::new("./recordings", RecordingOptions::default())?;
let mut pipeline = MediaPipelineBuilder::new(CaptureRequest::new(&device))
    .sink("recording", recorder)
    .start()?;

loop {
    match pipeline.next_forever_result()? {
        RecvOutcome::Data(_) => {}
        RecvOutcome::Empty => {}
        RecvOutcome::Closed => break,
    }
}
let recorder = pipeline.stop_with_recorder().expect("recorder");
let _paths = recorder.into_paths();
# Ok::<(), Box<dyn std::error::Error>>(())
```

## Examples
All examples live in the top-level `examples` workspace package and run through `styx-examples`; see the workspace README for feature flags.

- Canonical facade flows:
  - `quickstart_capture_virtual`
  - `low_latency_preview`
  - `reliable_recording`
  - `latest_frame_fanout`
  - `file_replay`
  - `netcam_capture`
- `watch_inventory`: synchronous inventory watch loop
- `async_watch_inventory`: Tokio-backed inventory watch loop
