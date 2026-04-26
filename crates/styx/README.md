# styx (facade crate)

The end-user entry point that re-exports the layered stack (`styx-core`, `styx-capture`, `styx-codec`, optional backends) behind a single prelude. It adds a capture API, pipeline builder, metrics, probing helpers, and feature-gated preview support.

## Documentation
- <https://docs.rs/styx>

## Install
```toml
[dependencies]
styx = "1.0.0"
```

## What it provides
- `styx::prelude`: re-exports core/capture/codec preludes plus capture API (`CaptureRequest`, `CaptureHandle`, `StyxConfig`, `start_capture`, `set_capture_tunables`, etc.), pipeline types (`MediaPipelineBuilder`, `MediaPipeline`), metrics, and backend handles.
- `probe_all`: merge v4l2/libcamera probe results when enabled.
- `watch`: inventory watch/runtime layer with retained events, blocking subscriptions, hotplug watchers, and async wrappers when `async` is enabled.
- `BackendHandle/BackendKind`, `ProbedDevice`, `ProbedBackend`: describe discovered devices and selected backends.
- `capture_api`: helpers for synthetic backends (`make_netcam_device`, `make_file_device`) and tunables.
- `session`: `MediaPipeline` for capture→decode→hook→encode flows (sync-first; async helpers when `async` is enabled).
- `recording` (feature `hooks`): record pipeline output frames to disk for replay.
- `preview` (feature `preview-window`): simple RGBA/RGB preview window for examples.

## Typical usage
Capture with a chosen backend/mode:
```rust,ignore
use styx::prelude::*;

let devices = probe_all();
let device = devices.first().expect("no devices");

let handle = CaptureRequest::new(device)
    .backend_preferred(Some(BackendKind::Virtual)) // or V4l2/Libcamera when enabled
    .start()?;

match handle.recv() {
    RecvOutcome::Data(frame) => println!("got frame {:?}", frame.meta().format),
    RecvOutcome::Empty => {}
    RecvOutcome::Closed => {}
}
handle.stop();
```

Capture → decode pipeline with hooks and optional preview:
```rust,ignore
use std::sync::Arc;
use styx::prelude::*;

let device = /* virtual or probed device */;
let decoder = Arc::new(PassthroughDecoder::new(device.backends[0].descriptor.modes[0].format.code));
let mut pipeline = MediaPipelineBuilder::new(CaptureRequest::new(device))
    .decoder(decoder)
    .rotate(Rotation90::Deg90)    // Optional 90-degree rotation
    .mirror(true)                 // Optional horizontal mirror
    .frame_hook(|frame| frame)     // works on FrameLease
    .hook(|frame| frame.flipv())   // image helpers on FrameLease (feature `hooks`)
    .start()?;

while let RecvOutcome::Data(frame) = pipeline.next() {
    println!("pipeline frame {:?}", frame.meta().format);
}
```

`DynamicImage` compatibility is available only through the optional
`dynamic-image` feature; the default hook/runtime path operates directly on `FrameLease`.

Global tunables for queues/pools/netcam backoff:
```rust,ignore
use styx::prelude::*;
StyxConfig::new()
    .capture_queue_depth(8)
    .capture_pool(4, 1 << 20, 8)
    .netcam_timeouts(10)
    .apply();
```

Record output frames for replay with the file backend:
```rust,ignore
use styx::prelude::*;

let recorder = FrameRecorder::new("./recordings", RecordingOptions::default())?;
let mut pipeline = MediaPipelineBuilder::new(CaptureRequest::new(&device))
    .record_output(recorder)
    .start()?;

while let RecvOutcome::Data(_) = pipeline.next() {}
let recorder = pipeline.stop_with_recorder().expect("recorder");
let replay = make_file_device("replay", recorder.into_paths(), 30, true);
# Ok::<(), Box<dyn std::error::Error>>(())
```

## Examples
All examples live in `crates/styx/examples` and are runnable from this crate; see the workspace README for the full list and required feature flags.

- Canonical facade flows:
  - `capture_virtual`
  - `low_latency_preview`
  - `reliable_recording`
  - `latest_frame_fanout`
  - `file_replay`
  - `netcam_capture`
- `watch_inventory`: synchronous inventory watch loop
- `async_watch_inventory`: Tokio-backed inventory watch loop
