# API Ergonomics

This guide records the release review for Rust API surface, import size, common workflows, and typed boundaries.

## Import Surfaces

Use `styx::prelude::*` for examples, prototypes, and application code that crosses capture, codec, pipeline, metrics, and recording boundaries. It is intentionally broad so a user can build a small media workflow without learning the full crate layout first.

For smaller surfaces, prefer task-focused imports:

```rust,no_run
use styx::imports::capture::{CaptureRequest, RecvOutcome, StyxConfig};
use styx::imports::pipeline::MediaPipelineBuilder;
#[cfg(feature = "hooks")]
use styx::imports::recording::{FrameRecorder, RecordingOptions};
```

Use support-crate preludes when the application owns only one layer:

```rust,no_run
use styx_codec::prelude::{CodecKind, CodecRegistry, MjpegDecoder};
use styx_core::prelude::{FourCc, FrameLease, Resolution};
```

Feature-specific surfaces should stay local to the feature:

- graph workflows: `styx::graph` and the `daedalus-plugin` feature
- watch workflows: `styx::watch`
- service event workflows: `styx::service`
- preview windows: `styx::extras::preview_window` with the `preview-window` feature

Avoid adding more symbols to the facade prelude unless the type is needed for a short, common workflow. Prefer documenting focused module imports when a workflow is advanced or feature-specific.

## Common Task Recipes

Capture one virtual frame:

```rust,no_run
use std::time::Duration;
use styx::imports::capture::{RecvOutcome, open_virtual_rgb};

let handle = open_virtual_rgb("smoke", 640, 360, 30)?;
let frame = match handle.recv_blocking(Duration::from_millis(50)) {
    RecvOutcome::Data(frame) => Some(frame),
    RecvOutcome::Empty | RecvOutcome::Closed => None,
};
handle.stop();
# Ok::<(), styx::capture_api::CaptureError>(())
```

Start a raw frame pipeline:

```rust,no_run
use std::time::Duration;
use styx::imports::capture::make_virtual_rgb_device;
use styx::imports::pipeline::MediaPipelineBuilder;

let device = make_virtual_rgb_device("raw", 640, 360, 30);
let mut pipeline = MediaPipelineBuilder::new(device.capture_request())
    .raw_frames()
    .start()?;
let _ = pipeline.next_blocking_result(Duration::from_millis(50))?;
pipeline.stop();
# Ok::<(), styx::capture_api::CaptureError>(())
```

Decode MJPEG to `RG24`:

```rust,no_run
use std::sync::Arc;
use styx::imports::capture::{CaptureRequest, VirtualSourceConfig};
use styx::imports::codec::{FourCc, MjpegDecoder};
use styx::imports::pipeline::MediaPipelineBuilder;

let decoder = Arc::new(MjpegDecoder::new(FourCc::RG24));
let device = CaptureRequest::virtual_source(
    VirtualSourceConfig::new()
        .name("mjpeg")
        .format(FourCc::MJPG)
        .resolution(640, 360)
        .fps(30),
)
.into_device();
let mut pipeline = MediaPipelineBuilder::new(device.capture_request())
    .decoder(decoder)
    .start()?;
pipeline.stop();
# Ok::<(), styx::capture_api::CaptureError>(())
```

Record frames:

```rust,no_run
use styx::imports::capture::make_virtual_rgb_device;
use styx::imports::pipeline::MediaPipelineBuilder;
use styx::imports::recording::{FrameRecorder, RecordingOptions};

let device = make_virtual_rgb_device("record", 640, 360, 30);
let recorder = FrameRecorder::new("./recordings", RecordingOptions::default())?;
let mut pipeline = MediaPipelineBuilder::new(device.capture_request())
    .raw_frames()
    .sink("recording", recorder)
    .start()?;
let recorder = pipeline.stop_with_recorder().expect("recorder");
let _paths = recorder.into_paths();
# Ok::<(), Box<dyn std::error::Error>>(())
```

Start async netcam capture:

```rust,no_run
use styx::imports::capture::{
    CaptureRequest, CaptureStartPolicy, NetcamSourceConfig, RecvOutcome,
};

let request = CaptureRequest::netcam_source(
    NetcamSourceConfig::new("http://camera/mjpeg")
        .name("netcam")
        .resolution(640, 480)
        .fps(30),
);
let handle = request.start_with_policy_async(CaptureStartPolicy::default()).await?;
let _ = handle.recv_async().await;
handle.stop_async().await;
# Ok::<(), styx::capture_api::CaptureError>(())
```

For decode, encode, graph, hook, or sink work from async services, prefer `MediaPipeline::spawn_tokio_worker` or `spawn_blocking_worker` so CPU-heavy stages do not run on Tokio core workers.

## Typed Boundaries

The release API already has typed selectors for the common places where callers would otherwise pass strings:

- `BackendKind` implements `Display` and `FromStr`.
- `CodecSelector` implements `Display`, `FromStr`, `TryFrom<&str>`, `TryFrom<String>`, and `AsRef<str>`.
- `CodecImplementationId` implements `Display`, `FromStr`, `From<&str>`, `From<String>`, `From<&String>`, and `AsRef<str>`.
- `SinkKind` implements `Display` and `FromStr`.
- `FourCc` implements conversions from `u32`, `[u8; 4]`, `&[u8; 4]`, `TryFrom<&str>`, `TryFrom<String>`, `Display`, and `FromStr`.
- `Resolution`, `Interval`, and `StyxConfig` use typed constructors and builder/default surfaces instead of string maps.

Keep string parsing at CLI, config-file, graph-registration, and wire-format boundaries. Add new `FromStr`, `Display`, `TryFrom`, `AsRef`, or `Default` impls only when they remove real caller code without weakening typed invariants.
